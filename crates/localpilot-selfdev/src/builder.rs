//! Building the CLI from its own source, on purpose and reproducibly.
//!
//! A self-dev build is not an ordinary `cargo build`, and the differences are
//! all policy rather than cleverness:
//!
//! - **Its own target directory.** A self-dev build that shared `target/` would
//!   invalidate the artefacts the running session and `rust-analyzer` depend on,
//!   turning every reload into a full rebuild of the developer's inner loop.
//! - **Its own profile.** Optimising a binary that exists to be replaced in the
//!   next minute is wasted time; `selfdev` inherits `release` for the runtime
//!   shape but drops the optimiser.
//! - **A job count that leaves room.** The session that started the build is
//!   still running, and still has to stay responsive.
//! - **Source identity passed in as environment.** The build script embeds the
//!   hash and fingerprint it is *told*, so the produced binary can be checked
//!   against the tree it claims to come from (the subject-03 gauntlet), and so
//!   the build script does not have to watch `.git` to stay truthful.
//!
//! The plan is computed by a pure function so the policy is unit-testable
//! without running a compiler.

use std::path::{Path, PathBuf};
use std::process::{Command, Stdio};

use crate::error::SelfDevError;
use crate::source::SourceState;

/// The cargo profile a self-dev build uses.
pub const SELFDEV_PROFILE: &str = "selfdev";
/// The package built for a self-dev reload.
pub const SELFDEV_PACKAGE: &str = "localpilot";
/// The tool name the built binary is published under.
pub const TOOL: &str = "localpilot";

/// Environment variable carrying the commit hash to embed.
pub const ENV_GIT_HASH: &str = "LOCALPILOT_GIT_HASH";
/// Environment variable carrying the source fingerprint to embed.
pub const ENV_SOURCE_FINGERPRINT: &str = "LOCALPILOT_SOURCE_FINGERPRINT";
/// Environment variable carrying the version string to embed.
pub const ENV_VERSION: &str = "LOCALPILOT_VERSION";

/// What to build, and how hard to work at it.
#[derive(Debug, Clone)]
pub struct BuildOptions {
    /// Cargo profile. Defaults to [`SELFDEV_PROFILE`].
    pub profile: String,
    /// Package to build. Defaults to [`SELFDEV_PACKAGE`].
    pub package: String,
    /// Where cargo writes its artefacts. Isolated from the workspace `target/`.
    pub target_dir: PathBuf,
    /// Parallel jobs. `None` applies the leave-a-core policy.
    pub jobs: Option<usize>,
    /// Cargo features to enable, if any.
    pub features: Vec<String>,
}

impl BuildOptions {
    /// Defaults for a self-dev build writing into `target_dir`.
    #[must_use]
    pub fn new(target_dir: impl Into<PathBuf>) -> Self {
        Self {
            profile: SELFDEV_PROFILE.to_string(),
            package: SELFDEV_PACKAGE.to_string(),
            target_dir: target_dir.into(),
            jobs: None,
            features: Vec::new(),
        }
    }
}

/// A fully resolved build invocation: everything needed to run it, decided
/// before anything is spawned.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct BuildPlan {
    /// Arguments to `cargo`.
    pub args: Vec<String>,
    /// Environment the build script reads, as ordered pairs.
    pub env: Vec<(String, String)>,
    /// A best-effort guess at where the executable will land, for a caller with
    /// no other handle and for the tests. It is **not** authoritative: when a
    /// `build.target` triple is configured (globally or per-project), cargo
    /// inserts a `<triple>/` component this guess cannot know, so [`build`]
    /// takes the real path from cargo's own artifact messages instead.
    pub predicted_executable: PathBuf,
}

/// The result of a completed build: the plan that produced it and the path cargo
/// actually wrote the executable to.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Built {
    /// The invocation that ran, including the embedded source identity in
    /// [`BuildPlan::env`] — the subject-03 gauntlet checks the produced binary
    /// against exactly this.
    pub plan: BuildPlan,
    /// Where the executable is, as cargo reported it.
    pub executable: PathBuf,
}

/// Resolve what a self-dev build of `source` should run.
///
/// Pure: it spawns nothing and touches no filesystem, so the policy above can be
/// asserted directly.
#[must_use]
pub fn plan(source: &SourceState, options: &BuildOptions) -> BuildPlan {
    let jobs = options.jobs.unwrap_or_else(default_jobs);
    let mut args = vec![
        "build".to_string(),
        "--locked".to_string(),
        "--package".to_string(),
        options.package.clone(),
        "--profile".to_string(),
        options.profile.clone(),
        "--target-dir".to_string(),
        options.target_dir.display().to_string(),
        "--jobs".to_string(),
        jobs.to_string(),
    ];
    if !options.features.is_empty() {
        args.push("--features".to_string());
        args.push(options.features.join(","));
    }

    BuildPlan {
        args,
        env: vec![
            (ENV_GIT_HASH.to_string(), source.embedded_hash().to_string()),
            (
                ENV_SOURCE_FINGERPRINT.to_string(),
                source.fingerprint.clone(),
            ),
            (ENV_VERSION.to_string(), embedded_version(source)),
        ],
        predicted_executable: options
            .target_dir
            .join(profile_dir(&options.profile))
            .join(localpilot_dist::executable_name(TOOL)),
    }
}

/// Build `source` and return the plan plus the path cargo actually wrote the
/// executable to.
///
/// The path is read from cargo's `compiler-artifact` messages, not guessed: a
/// configured `build.target` triple inserts a directory component no static
/// guess can know. `--message-format=json-render-diagnostics` keeps human
/// diagnostics on stderr (so a build the operator asked for still shows its
/// errors) while the machine-readable artefact records arrive on stdout.
///
/// # Errors
/// Returns [`SelfDevError::Build`] when cargo cannot be spawned, exits non-zero,
/// or succeeds without reporting a binary artefact for the built package.
pub fn build(source: &SourceState, options: &BuildOptions) -> Result<Built, SelfDevError> {
    use std::io::{BufRead, BufReader};

    let plan = plan(source, options);
    let mut child = Command::new("cargo")
        .args(&plan.args)
        .arg("--message-format=json-render-diagnostics")
        .current_dir(&source.root)
        .envs(plan.env.iter().map(|(k, v)| (k.as_str(), v.as_str())))
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .spawn()
        .map_err(|error| SelfDevError::Build {
            status: "not started".to_string(),
            detail: error.to_string(),
        })?;

    // Read the artefact stream as it arrives so a long build does not fill the
    // pipe buffer and deadlock. The last binary artefact naming the built
    // package is the one we want.
    let mut executable: Option<PathBuf> = None;
    if let Some(stdout) = child.stdout.take() {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if let Some(path) = binary_artifact(&line, &options.package) {
                executable = Some(path);
            }
        }
    }

    let status = child.wait().map_err(|error| SelfDevError::Build {
        status: "wait failed".to_string(),
        detail: error.to_string(),
    })?;
    if !status.success() {
        return Err(SelfDevError::Build {
            status: status
                .code()
                .map_or("signal".to_string(), |c| c.to_string()),
            detail: format!("cargo {}", plan.args.join(" ")),
        });
    }

    let executable = executable.ok_or_else(|| SelfDevError::Build {
        status: "0".to_string(),
        detail: format!(
            "the build succeeded but reported no binary artefact for package {:?}",
            options.package
        ),
    })?;
    if !executable.is_file() {
        return Err(SelfDevError::Build {
            status: "0".to_string(),
            detail: format!(
                "cargo named an executable at {} but nothing is there",
                executable.display()
            ),
        });
    }
    Ok(Built { plan, executable })
}

/// The executable path from one `compiler-artifact` line, when it is a binary of
/// `package`.
///
/// Cargo emits one JSON object per line; a `compiler-artifact` for a binary
/// carries a non-null `executable` and a `target` whose `kind` includes `"bin"`.
/// Matching the target *name* to the package keeps a package's own bin from
/// being confused with a build-dependency's.
fn binary_artifact(line: &str, package: &str) -> Option<PathBuf> {
    let value: serde_json::Value = serde_json::from_str(line).ok()?;
    if value.get("reason")?.as_str()? != "compiler-artifact" {
        return None;
    }
    let executable = value.get("executable")?.as_str()?;
    let target = value.get("target")?;
    let is_bin = target
        .get("kind")?
        .as_array()?
        .iter()
        .any(|kind| kind.as_str() == Some("bin"));
    let names_package = target.get("name")?.as_str()? == package;
    (is_bin && names_package).then(|| PathBuf::from(executable))
}

/// The directory cargo writes a profile's artefacts into (for the best-effort
/// prediction only).
///
/// Every profile uses its own name except `dev`, which cargo writes to `debug`.
fn profile_dir(profile: &str) -> &str {
    if profile == "dev" {
        "debug"
    } else {
        profile
    }
}

/// How many jobs a self-dev build gets by default.
///
/// One fewer than the machine has, floored at one: the session that asked for
/// the build is still running and still has to answer. This is a deliberately
/// dependency-free policy — reading physical memory would need a new crate, and
/// leaving a core free is the constraint that actually keeps a live session
/// responsive during a rebuild.
fn default_jobs() -> usize {
    std::thread::available_parallelism()
        .map(|n| n.get().saturating_sub(1).max(1))
        .unwrap_or(1)
}

/// The version string a self-dev build embeds.
///
/// It is the release version this source tree belongs to, suffixed with the
/// source label, so a self-dev binary can never be mistaken for the release it
/// was built from — while still parsing as that release for ordering purposes.
fn embedded_version(source: &SourceState) -> String {
    format!(
        "{}-{SELFDEV_PROFILE}-{}",
        env!("CARGO_PKG_VERSION"),
        source.version_label
    )
}

/// The default isolated target directory for self-dev builds under `root`.
#[must_use]
pub fn default_target_dir(root: &Path) -> PathBuf {
    root.join("build-target")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{commit_all, init_repo, write};

    fn state() -> SourceState {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "first");
        let state = SourceState::read(repo.path()).expect("read");
        // The temp repo is dropped here; the state is a value, and nothing in
        // `plan` touches the tree.
        drop(repo);
        state
    }

    #[test]
    fn the_plan_isolates_the_target_directory_and_uses_the_selfdev_profile() {
        let source = state();
        let options = BuildOptions::new("/tmp/isolated");
        let plan = plan(&source, &options);

        assert!(plan.args.contains(&"--profile".to_string()));
        assert!(plan.args.contains(&SELFDEV_PROFILE.to_string()));
        assert!(plan.args.contains(&"--target-dir".to_string()));
        assert!(
            plan.args.contains(&"--locked".to_string()),
            "a self-dev build must not silently move the lockfile"
        );
        assert!(
            plan.predicted_executable.starts_with("/tmp/isolated"),
            "the predicted path must sit inside the isolated target dir, not target/"
        );
        assert!(plan
            .predicted_executable
            .ends_with(localpilot_dist::executable_name(TOOL)));
    }

    #[test]
    fn a_binary_artifact_line_yields_the_executable_a_triple_and_all() {
        // A real cargo line with a configured target triple in the path — the
        // exact case a static guess gets wrong.
        let line = r#"{"reason":"compiler-artifact","package_id":"localpilot 2.6.0","target":{"kind":["bin"],"name":"localpilot"},"executable":"/w/build-target/x86_64-unknown-linux-gnu/selfdev/localpilot","fresh":false}"#;
        assert_eq!(
            binary_artifact(line, "localpilot"),
            Some(PathBuf::from(
                "/w/build-target/x86_64-unknown-linux-gnu/selfdev/localpilot"
            )),
            "the real path (triple included) must come from cargo, not a guess"
        );
    }

    #[test]
    fn non_binary_and_foreign_artifacts_are_ignored() {
        // A library artefact carries a null executable.
        let lib = r#"{"reason":"compiler-artifact","target":{"kind":["lib"],"name":"localpilot"},"executable":null}"#;
        assert_eq!(binary_artifact(lib, "localpilot"), None);

        // A dependency's own binary must not be mistaken for ours.
        let other = r#"{"reason":"compiler-artifact","target":{"kind":["bin"],"name":"some-dep"},"executable":"/w/some-dep"}"#;
        assert_eq!(binary_artifact(other, "localpilot"), None);

        // A build-script-executed message is not an artefact.
        let build = r#"{"reason":"build-script-executed","package_id":"x"}"#;
        assert_eq!(binary_artifact(build, "localpilot"), None);

        // Non-JSON (a stray render-diagnostics line, were one to reach stdout).
        assert_eq!(binary_artifact("Compiling localpilot", "localpilot"), None);
    }

    #[test]
    fn the_plan_passes_the_source_identity_as_environment() {
        let source = state();
        let plan = plan(&source, &BuildOptions::new("/tmp/isolated"));

        let env: std::collections::HashMap<_, _> = plan.env.iter().cloned().collect();
        assert_eq!(
            env.get(ENV_GIT_HASH).map(String::as_str),
            Some(source.embedded_hash())
        );
        assert_eq!(
            env.get(ENV_SOURCE_FINGERPRINT),
            Some(&source.fingerprint),
            "the gauntlet compares this against the tree it was built from"
        );
        let version = env.get(ENV_VERSION).expect("a version is embedded");
        assert!(
            version.contains(&source.version_label),
            "a self-dev binary must not be mistakable for the release it came from"
        );
    }

    #[test]
    fn a_default_job_count_leaves_a_core_for_the_running_session() {
        let jobs = default_jobs();
        assert!(jobs >= 1);
        if let Ok(available) = std::thread::available_parallelism() {
            assert!(
                jobs < available.get() || available.get() == 1,
                "a build must not claim every core while a session is live"
            );
        }
    }

    #[test]
    fn an_explicit_job_count_wins_over_the_policy() {
        let source = state();
        let mut options = BuildOptions::new("/tmp/isolated");
        options.jobs = Some(3);
        let plan = plan(&source, &options);

        let jobs_index = plan
            .args
            .iter()
            .position(|arg| arg == "--jobs")
            .expect("jobs flag");
        assert_eq!(plan.args[jobs_index + 1], "3");
    }

    #[test]
    fn the_dev_profile_still_resolves_to_cargos_debug_directory() {
        assert_eq!(profile_dir("dev"), "debug");
        assert_eq!(profile_dir(SELFDEV_PROFILE), SELFDEV_PROFILE);
        assert_eq!(profile_dir("release"), "release");
    }
}
