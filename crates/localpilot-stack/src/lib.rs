//! The LocalX release train and the shared logic to install or refresh it as a
//! set.
//!
//! One release cut stamps every tool with the same version and tag, so the only
//! safe way to install them is together — a stack assembled from different tags
//! is a configuration nobody tested, and a mismatched store schema between an
//! installed CLI and the engine it talks to fails silently, not loudly.
//!
//! This crate owns the *orchestration* — which tools, at which tag, over which
//! channel — and leans on `localpilot-dist` for the on-disk contract (cache
//! layout, download, verify, activate). Both `localpilot` (via `update --all`)
//! and the `localx` umbrella binary route through here, so there is exactly one
//! copy of the install loop.
#![forbid(unsafe_code)]

use std::io::Write;
use std::path::{Path, PathBuf};
use std::time::Duration;

use localpilot_dist::{Cache, ReleaseManifest, Version};

/// One tool in the release train.
///
/// `tool` is the executable name (what lands on `PATH`); `package` is the cargo
/// package that builds it (they differ for `localmind`, whose binary comes from
/// the `localmind-cli` package). `manifest` is the per-release index filename —
/// `manifest.json` for a repo that publishes one binary, or a per-tool name when
/// two binaries ship from the same repo (`localx` rides LocalPilot's release).
#[derive(Debug, Clone, Copy)]
pub struct StackTool {
    pub tool: &'static str,
    pub repo: &'static str,
    pub manifest: &'static str,
    pub package: &'static str,
    pub features: &'static [&'static str],
}

/// The tools one release train cuts together, in install order.
///
/// `localx` is last: it is the umbrella that drives the others, and updating it
/// after them keeps a self-replace from cutting the run short.
pub const TRAIN: &[StackTool] = &[
    StackTool {
        tool: "localpilot",
        repo: "https://github.com/C0deGeek-dev/LocalPilot",
        manifest: "manifest.json",
        package: "localpilot",
        features: &["tui"],
    },
    StackTool {
        tool: "localmind",
        repo: "https://github.com/C0deGeek-dev/LocalMind",
        manifest: "manifest.json",
        package: "localmind-cli",
        features: &[],
    },
    StackTool {
        tool: "localbox",
        repo: "https://github.com/C0deGeek-dev/LocalBox",
        manifest: "manifest.json",
        package: "localbox",
        features: &[],
    },
    StackTool {
        tool: "localbench",
        repo: "https://github.com/C0deGeek-dev/LocalBench",
        manifest: "manifest.json",
        package: "localbench",
        features: &[],
    },
    StackTool {
        tool: "localx",
        repo: "https://github.com/C0deGeek-dev/LocalPilot",
        manifest: "manifest-localx.json",
        package: "localx",
        features: &[],
    },
];

/// Look up a train tool by its executable name.
#[must_use]
pub fn tool(name: &str) -> Option<&'static StackTool> {
    TRAIN.iter().find(|t| t.tool == name)
}

/// Where the install comes from.
///
/// `Release` downloads the published, checksum-verified binary (no toolchain).
/// `Prerelease` builds the newest `main` commit from source with cargo — the
/// developer channel for testing pushed-but-uncut work.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Channel {
    Release,
    Prerelease,
}

/// Which tools to act on.
pub enum Selection {
    /// Every tool in the train.
    All,
    /// One named tool (validate the name with [`tool`] first).
    One(&'static StackTool),
}

impl Selection {
    fn tools(&self) -> Vec<&'static StackTool> {
        match self {
            Selection::All => TRAIN.iter().collect(),
            Selection::One(t) => vec![t],
        }
    }
}

/// The tool this process *is* — the running binary. Its identity (the `tool`
/// name, always known) decides the self-replace route and the refresh of the
/// copy the shell resolves; neither is knowable for the other tools from this
/// process.
///
/// `version` carries the running version *when it parses*, for the cache's
/// strictly-newer activation rule (which stops a stale cache downgrading a
/// from-source build) and the sweep's protection of the running version. It is
/// `None` for a build whose stamp is not a semver — a bare git sha from a
/// tagless cargo checkout, a shallow clone, a source archive — and that absence
/// must never switch off the identity-driven behaviour, which does not need it.
pub struct Running {
    pub tool: &'static str,
    pub version: Option<Version>,
}

const TAGS_API: &str = "https://api.github.com/repos/C0deGeek-dev/LocalPilot/tags";

/// How many cached versions to keep beyond the running and pinned ones.
const KEEP_VERSIONS: usize = 2;

/// The one directory every train tool's executable is published into, so a user
/// adds a single entry to `PATH` rather than one per tool.
#[must_use]
pub fn shared_bin_dir() -> Option<PathBuf> {
    Cache::default_root("localx").map(|root| localpilot_dist::bin_dir(&root))
}

/// The newest published release tag across the train (queried from LocalPilot,
/// which the train is cut in lockstep with). `None` when nothing is published.
///
/// # Errors
/// Returns an error if the tag list cannot be reached or parsed.
pub async fn newest_published_tag() -> anyhow::Result<Option<String>> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let body: serde_json::Value = client
        .get(TAGS_API)
        // GitHub requires a User-Agent; it serves anonymous tag listings.
        .header("User-Agent", "localx-stack-install")
        .header("Accept", "application/vnd.github+json")
        .send()
        .await?
        .error_for_status()?
        .json()
        .await?;

    let mut best: Option<(Version, String)> = None;
    for tag in body.as_array().into_iter().flatten() {
        let Some(name) = tag.get("name").and_then(serde_json::Value::as_str) else {
            continue;
        };
        let Some(version) = Version::parse(name) else {
            continue;
        };
        if best.as_ref().is_none_or(|(b, _)| version.key() > b.key()) {
            best = Some((version, name.to_string()));
        }
    }
    Ok(best.map(|(_, name)| name))
}

/// The release tag that matches a running version — `v<dir-name>`.
#[must_use]
pub fn tag_for_version(version: &Version) -> String {
    format!("v{}", version.to_dir_name())
}

/// Install or refresh the selected tools over the chosen channel.
///
/// `tag` is used only for [`Channel::Release`]; `None` resolves the newest
/// published tag. A tool that fails to install is reported and skipped rather
/// than aborting the others — a partial stack the user can see beats an install
/// that stops halfway without saying where.
///
/// # Errors
/// Returns an error only if output cannot be written or a required release tag
/// cannot be resolved; a single tool's install failure is reported, not returned.
pub async fn install(
    selection: &Selection,
    tag: Option<&str>,
    channel: Channel,
    running: Option<&Running>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let tools = selection.tools();

    let resolved_tag = match channel {
        Channel::Release => Some(match tag {
            Some(t) => t.to_string(),
            None => newest_published_tag()
                .await?
                .ok_or_else(|| anyhow::anyhow!("no published release found to install"))?,
        }),
        Channel::Prerelease => None,
    };

    if let Some(t) = &resolved_tag {
        writeln!(out, "installing the stack at {t} ...\n")?;
    } else {
        writeln!(
            out,
            "building the stack from the latest main (prerelease) ...\n"
        )?;
    }

    let mut failed = Vec::new();
    for t in &tools {
        let (is_self, running_version) = self_view(running, t.tool);
        let ok = match channel {
            Channel::Release => {
                let tag = resolved_tag.as_deref().unwrap_or_default();
                install_release(t, tag, is_self, running_version, out).await?
            }
            Channel::Prerelease => source_install(t, is_self, out)?,
        };
        if !ok {
            failed.push(t.tool);
        }
    }

    report(
        &tools,
        &failed,
        resolved_tag.as_deref(),
        channel,
        running.map(|r| r.tool),
        out,
    )
}

/// Split a caller's running marker into what the install loop actually needs:
/// whether this process *is* `tool` (identity — decides the self-replace route
/// and the running-copy refresh), and the running version when it is known
/// (decides the strictly-newer activation rule and the sweep's protection).
///
/// The two are returned separately on purpose: a build stamped with a bare git
/// sha has a certain identity but no parseable version, and identity must never
/// hinge on the version parsing — that gate is exactly what LocalHub#79 traced
/// the unreachable self-replace to.
fn self_view<'a>(running: Option<&'a Running>, tool: &str) -> (bool, Option<&'a Version>) {
    match running {
        Some(r) if r.tool == tool => (true, r.version.as_ref()),
        _ => (false, None),
    }
}

/// Install one train tool's published archive at `tag`, verifying it first and
/// then making it the version on `PATH`.
///
/// # Errors
/// Returns an error only if output cannot be written; a failed install is
/// reported and leaves the previously installed version untouched.
pub async fn install_release(
    t: &StackTool,
    tag: &str,
    is_self: bool,
    running: Option<&Version>,
    out: &mut dyn Write,
) -> anyhow::Result<bool> {
    let Some(cache) = tool_cache(t.tool) else {
        writeln!(
            out,
            "no per-user data directory on this platform; use --prerelease"
        )?;
        return Ok(false);
    };
    let target = localpilot_dist::current_target();
    if target.is_empty() {
        writeln!(
            out,
            "{}: this platform has no published build; use --prerelease to build it from source",
            t.tool
        )?;
        return Ok(false);
    }

    let base = release_assets_url(t.repo, tag);
    writeln!(out, "{}: fetching {tag} manifest…", t.tool)?;
    let manifest_bytes = match localpilot_dist::download(&format!("{base}/{}", t.manifest)).await {
        Ok(bytes) => bytes,
        Err(error) => {
            writeln!(
                out,
                "{}: could not fetch the {tag} manifest ({error}); use --prerelease",
                t.tool
            )?;
            return Ok(false);
        }
    };
    let manifest = match String::from_utf8(manifest_bytes)
        .map_err(|e| e.to_string())
        .and_then(|text| ReleaseManifest::parse(&text).map_err(|e| e.to_string()))
    {
        Ok(manifest) => manifest,
        Err(error) => {
            writeln!(
                out,
                "{}: manifest for {tag} is unusable ({error}); use --prerelease",
                t.tool
            )?;
            return Ok(false);
        }
    };

    writeln!(out, "{}: downloading and verifying {target}…", t.tool)?;
    match localpilot_dist::install_release(&cache, &manifest, target, t.tool, &base).await {
        Ok(dir) => {
            writeln!(out, "{}: installed {tag} to {}", t.tool, dir.display())?;
            // Name the property that was actually checked. The digest proves the
            // bytes are intact; origin comes from the release's build
            // attestation, which this updater does not verify in-process — so
            // point at the command that does, rather than implying it was done.
            writeln!(
                out,
                "verified against the checksum published with the release (integrity)"
            )?;
            writeln!(
                out,
                "to confirm it was built by this repository: \
                 gh attestation verify <archive> --repo {}",
                repo_slug(t.repo)
            )?;
            if let Some(path) = activate(&cache, t.tool, running, out)? {
                writeln!(out, "{}: on PATH at {}", t.tool, path.display())?;
                // Only the tool this process *is* has a running copy to refresh.
                // When it runs from somewhere other than the managed copy (a
                // source bootstrap in cargo's bin directory, earlier on PATH),
                // refresh that copy too — otherwise the shell keeps resolving
                // the stale one and the update is invisible. Gated on identity,
                // not on a parseable version: a bare-sha build is still the
                // running executable and still needs its copy refreshed.
                if is_self {
                    refresh_running_copy(t.tool, &path, out)?;
                }
            }
            let swept = cache.sweep(KEEP_VERSIONS, &protected(&cache, running));
            if !swept.is_empty() {
                let names: Vec<String> = swept.iter().map(Version::to_dir_name).collect();
                writeln!(out, "removed older cached version(s): {}", names.join(", "))?;
            }
            Ok(true)
        }
        Err(error) => {
            writeln!(out, "{}: install failed: {error}", t.tool)?;
            writeln!(out, "the previously installed version is untouched")?;
            Ok(false)
        }
    }
}

/// Build one train tool from its repo's `main` HEAD with cargo.
///
/// This is the developer channel: it needs a Rust toolchain and installs into
/// cargo's own bin directory (`~/.cargo/bin`), the standard home for a
/// source-built binary — the same path `localpilot`'s from-source update has
/// always used.
///
/// The one exception is the tool this process *is* (`is_self`): cargo's final
/// step is a plain move onto the destination, and Windows refuses to overwrite
/// an executing image no matter who asks (an elevated shell changes nothing —
/// the lock is mandatory, not an ACL). So the running tool is built into a
/// staging root that cargo owns entirely, and the built executable is then
/// swapped in over the running one with the same rename-then-copy the release
/// channel uses, which Windows does permit. The staging root is removed after a
/// successful swap and kept — with its path printed — when the swap fails, so
/// the build is never lost.
///
/// # Errors
/// Returns an error only if output cannot be written or cargo cannot be spawned.
pub fn source_install(t: &StackTool, is_self: bool, out: &mut dyn Write) -> anyhow::Result<bool> {
    if !cargo_available() {
        writeln!(
            out,
            "{}: cargo (the Rust toolchain) is required for --prerelease; \
             install it from https://rustup.rs, or drop --prerelease to use the release channel",
            t.tool
        )?;
        return Ok(false);
    }

    let staging = if is_self {
        source_build_dir(t.tool)
    } else {
        None
    };
    let Some(staging) = staging else {
        // A companion tool, or a platform with no per-user data directory: the
        // classic install straight into cargo's bin directory.
        writeln!(
            out,
            "{}: building {} from {} (main)…",
            t.tool, t.package, t.repo
        )?;
        let built = run_cargo(t, &source_args(t, None))?;
        if built {
            writeln!(
                out,
                "{}: installed from main into cargo's bin directory",
                t.tool
            )?;
        } else if let Some(hint) = install_failure_hint(t.tool) {
            writeln!(out, "{hint}")?;
        } else {
            writeln!(out, "{}: cargo install failed", t.tool)?;
        }
        return Ok(built);
    };

    let Ok(running_exe) = std::env::current_exe() else {
        writeln!(
            out,
            "{}: cannot locate the running executable to replace it; \
             building into cargo's bin directory instead",
            t.tool
        )?;
        let built = run_cargo(t, &source_args(t, None))?;
        return Ok(built);
    };
    writeln!(
        out,
        "{}: building {} from {} (main) into a staging directory, \
         then replacing the running executable…",
        t.tool, t.package, t.repo
    )?;
    let outcome = stage_and_replace(t.tool, &staging, &running_exe, &mut |root| {
        run_cargo(t, &source_args(t, Some(root)))
    })?;
    describe_self_install(t.tool, &running_exe, &outcome, out)?;
    Ok(matches!(outcome, SelfInstall::Replaced(_)))
}

/// What a self-install of the running tool ended with. Every variant names its
/// actual cause; `Retained` is reserved for a build that verifiably exists on
/// disk, so the "copy it over after exit" advice never points at nothing.
#[derive(Debug)]
enum SelfInstall {
    /// The staging directory could not be prepared; nothing was built.
    StagingFailed(String),
    /// cargo ran and reported failure; nothing was staged.
    BuildFailed,
    /// The build succeeded but produced no executable where cargo should have
    /// put it (`<staging>/bin/<tool>`).
    MissingArtifact(PathBuf),
    /// The build succeeded and the running executable now holds it.
    Replaced(PathBuf),
    /// The build succeeded but the running executable could not be replaced;
    /// the built executable is retained at `built` (verified to exist).
    Retained { built: PathBuf, error: String },
}

/// Build into `staging` (cargo owns it wholesale), then swap the built
/// executable in over `running_exe` with rename-then-copy. Independent of
/// cargo — the build step is injected — so the placement policy is testable
/// without a toolchain.
///
/// # Errors
/// Propagates a build-step error (cargo could not be spawned); a build that
/// ran and failed is an outcome, not an error.
fn stage_and_replace(
    tool: &str,
    staging: &Path,
    running_exe: &Path,
    build: &mut dyn FnMut(&Path) -> anyhow::Result<bool>,
) -> anyhow::Result<SelfInstall> {
    // A fresh staging root every time: a stale artifact must never be mistaken
    // for this build's output.
    let _ = std::fs::remove_dir_all(staging);
    if let Err(error) = std::fs::create_dir_all(staging) {
        return Ok(SelfInstall::StagingFailed(format!(
            "could not create {}: {error}",
            staging.display()
        )));
    }
    if !build(staging)? {
        let _ = std::fs::remove_dir_all(staging);
        return Ok(SelfInstall::BuildFailed);
    }
    let built = staging
        .join("bin")
        .join(localpilot_dist::executable_name(tool));
    if !built.is_file() {
        return Ok(SelfInstall::MissingArtifact(built));
    }
    let Some(dest_dir) = running_exe.parent() else {
        return Ok(SelfInstall::Retained {
            built,
            error: "the running executable has no parent directory".to_string(),
        });
    };
    Ok(match localpilot_dist::place(dest_dir, tool, &built) {
        Ok(placed) => {
            // The payload now lives at the destination; the staging copy is
            // redundant. Best effort — a leftover is harmless and rebuilt fresh.
            let _ = std::fs::remove_dir_all(staging);
            SelfInstall::Replaced(placed)
        }
        Err(error) => SelfInstall::Retained {
            built,
            error: error.to_string(),
        },
    })
}

/// Say what happened to a self-install in terms the user can act on. A refused
/// swap of the running executable is named as exactly that: the file is in use
/// by this very process, an elevated shell does not change it, and the built
/// executable is kept so the next step is a copy after exit — never "re-run".
fn describe_self_install(
    tool: &str,
    running_exe: &Path,
    outcome: &SelfInstall,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    match outcome {
        SelfInstall::StagingFailed(reason) => writeln!(
            out,
            "{tool}: could not prepare the staging directory for the build: {reason}"
        ),
        SelfInstall::BuildFailed => match install_failure_hint(tool) {
            Some(hint) => writeln!(out, "{hint}"),
            None => writeln!(out, "{tool}: cargo install failed"),
        },
        SelfInstall::MissingArtifact(expected) => writeln!(
            out,
            "{tool}: cargo reported success but produced no executable at {}",
            expected.display()
        ),
        SelfInstall::Replaced(path) => writeln!(
            out,
            "{tool}: replaced the running executable at {} with the build from main \
             (the previous copy is displaced beside it and swept on the next run)",
            path.display()
        ),
        SelfInstall::Retained { built, error } => {
            writeln!(
                out,
                "{tool}: built from main, but could not replace the running executable at {}: {error}",
                running_exe.display()
            )?;
            writeln!(out, "  {}", running_image_hint())?;
            writeln!(
                out,
                "  the new build is kept at {} — after this process exits, copy it over {} \
                 (or run `{}` from there).",
                built.display(),
                running_exe.display(),
                built.display()
            )
        }
    }?;
    Ok(())
}

/// The one platform fact worth adding to a refused swap of the running
/// executable. The raw error above it stays authoritative — the swap can also
/// fail on permissions, disk, or an antivirus hold — so this only explains what
/// an image lock means where one exists, and never claims it was the cause.
fn running_image_hint() -> &'static str {
    if cfg!(windows) {
        "if the error is an access-denied on the running file, that is Windows' lock on an \
         executing image — an elevated shell does not lift it; exiting does."
    } else {
        "the safe fallback is to copy the new build over the running file after this process \
         exits."
    }
}

/// Where a self-install stages its build: beside the tool's release cache, in
/// the per-user data directory — durable across the swap, so a retained build
/// can be picked up after this process exits.
fn source_build_dir(tool: &str) -> Option<PathBuf> {
    Cache::default_root(tool).map(|root| root.join("source-build"))
}

/// Run `cargo install` with `args`, reporting only whether it succeeded.
fn run_cargo(t: &StackTool, args: &[String]) -> anyhow::Result<bool> {
    let mut command = std::process::Command::new("cargo");
    // The interactive TUI is unstable on the windows-gnu toolchain; force MSVC
    // when building a tool that links it.
    if cfg!(windows) && t.features.contains(&"tui") {
        command.arg("+stable-x86_64-pc-windows-msvc");
    }
    command.args(args);
    let status = command
        .status()
        .map_err(|e| anyhow::anyhow!("could not run cargo: {e}"))?;
    Ok(status.success())
}

/// Build the source-install arguments separately from process execution so the
/// package selection cannot regress unnoticed. With a `root`, cargo installs
/// into that directory instead of its own bin directory (the self-install
/// staging path); without one it is the classic `--force` refresh in place.
fn source_args(t: &StackTool, root: Option<&Path>) -> Vec<String> {
    let mut args = vec![
        "install".to_string(),
        "--git".to_string(),
        t.repo.to_string(),
        t.package.to_string(),
        "--branch".to_string(),
        "main".to_string(),
        "--locked".to_string(),
    ];
    match root {
        Some(root) => {
            args.push("--root".to_string());
            args.push(root.display().to_string());
        }
        None => args.push("--force".to_string()),
    }
    if !t.features.is_empty() {
        args.push("--features".to_string());
        args.push(t.features.join(","));
    }
    args
}

/// After the managed copy of the running tool was refreshed, refresh the copy
/// this process actually runs from too, when that is a different file — a
/// source bootstrap in cargo's bin directory sits earlier on `PATH` for every
/// installer-created setup, and would otherwise shadow the update forever.
/// The swap uses the same rename-then-copy the managed copy uses.
fn refresh_running_copy(tool: &str, managed: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let Ok(running_exe) = std::env::current_exe() else {
        return Ok(());
    };
    if same_file(&running_exe, managed) {
        return Ok(());
    }
    let Some(dest_dir) = running_exe.parent() else {
        return Ok(());
    };
    match localpilot_dist::place(dest_dir, tool, managed) {
        Ok(path) => writeln!(
            out,
            "{tool}: also refreshed the copy this command ran from, {} \
             (it is earlier on PATH than the managed copy)",
            path.display()
        )?,
        Err(error) => {
            writeln!(
                out,
                "{tool}: could not refresh the copy this command ran from, {}: {error}",
                running_exe.display()
            )?;
            writeln!(out, "  {}", running_image_hint())?;
            writeln!(
                out,
                "  alternatively remove that copy, or put {} earlier on PATH, so the managed \
                 copy is the one that runs.",
                managed.parent().unwrap_or(managed).display()
            )?;
        }
    }
    Ok(())
}

/// Whether two paths name the same file, tolerant of the `\\?\` prefix and
/// case differences `canonicalize` normalizes.
fn same_file(a: &Path, b: &Path) -> bool {
    match (std::fs::canonicalize(a), std::fs::canonicalize(b)) {
        (Ok(a), Ok(b)) => a == b,
        _ => a == b,
    }
}

/// Where the shell would find `tool` — the first match walking `PATH` in order,
/// exactly as a command lookup does.
///
/// This is what a user actually runs, which is not the same question as "what
/// did the installer put down". A copy earlier on `PATH` wins regardless of how
/// it got there or how old it is.
#[must_use]
pub fn path_resolved(tool: &str) -> Option<PathBuf> {
    let name = localpilot_dist::executable_name(tool);
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|dir| dir.join(&name))
        .find(|candidate| candidate.is_file())
}

/// The managed copy of `tool` when something **earlier on `PATH`** would win
/// instead. `None` when the managed copy is what the shell resolves, or when
/// there is no managed copy at all.
///
/// The difference from [`shadowed_managed_copy`] is whose perspective it takes.
/// That one asks whether *this running process* came from somewhere else, which
/// only answers for the tool doing the asking. This asks what the shell would do
/// for *any* tool — the question `localx status` has to answer when it reports
/// on four binaries it is not.
#[must_use]
pub fn shadowed_on_path(tool: &str) -> Option<(PathBuf, PathBuf)> {
    let managed = shared_bin_dir()?.join(localpilot_dist::executable_name(tool));
    if !managed.is_file() {
        return None;
    }
    let resolved = path_resolved(tool)?;
    if same_file(&resolved, &managed) {
        return None;
    }
    Some((resolved, managed))
}

/// The managed copy of `tool` (`<shared bin>/<tool>`) when it exists and is a
/// *different* file from the running executable — i.e. when this process was
/// resolved from somewhere else on `PATH`. `None` when there is no managed
/// copy, when the running executable is that copy, or when the running path is
/// unknown.
#[must_use]
pub fn shadowed_managed_copy(tool: &str) -> Option<PathBuf> {
    let running_exe = std::env::current_exe().ok()?;
    let managed = shared_bin_dir()?.join(localpilot_dist::executable_name(tool));
    if !managed.is_file() || same_file(&running_exe, &managed) {
        return None;
    }
    Some(managed)
}

/// A one-line note when the running `tool` is not the managed copy on `PATH`
/// (`<shared bin>/<tool>`), so a shell resolving a stale bootstrap copy is
/// visible rather than silent. `None` when there is nothing to say.
#[must_use]
pub fn running_binary_note(tool: &str) -> Option<String> {
    let running_exe = std::env::current_exe().ok()?;
    let managed = shadowed_managed_copy(tool)?;
    Some(format!(
        "note: this {tool} runs from {}, not the managed copy at {}; \
         `localx install` refreshes both, and the managed copy is the one to keep on PATH.",
        running_exe.display(),
        managed.display()
    ))
}

/// Whether `path` looks like a binary a running process is holding open.
///
/// Windows refuses to replace a running executable, and `cargo install` reports
/// that as a bare `Access is denied` after a successful two-minute build — which
/// reads as a build failure and is not one. Opening the file for write is the
/// cheapest way to tell the two apart, and it needs no process-enumeration
/// dependency.
///
/// Returns `false` on platforms that allow replacing a running binary, which is
/// the honest answer there: nothing is blocking the install.
#[must_use]
fn looks_locked_by_a_running_process(path: &std::path::Path) -> bool {
    if !path.is_file() {
        return false;
    }
    !cfg!(unix)
        && std::fs::OpenOptions::new()
            .write(true)
            .open(path)
            .is_err_and(|error| error.kind() == std::io::ErrorKind::PermissionDenied)
}

/// The reason a source install of `tool` failed, when the reason is knowable and
/// actionable — otherwise `None` and the caller says what it always said.
fn install_failure_hint(tool: &str) -> Option<String> {
    // cargo's own bin directory, where a source install lands.
    let home = std::env::var_os("CARGO_HOME")
        .map(PathBuf::from)
        .or_else(|| {
            std::env::var_os(if cfg!(windows) { "USERPROFILE" } else { "HOME" })
                .map(|home| PathBuf::from(home).join(".cargo"))
        })?;
    let destination = home
        .join("bin")
        .join(localpilot_dist::executable_name(tool));
    looks_locked_by_a_running_process(&destination).then(|| {
        format!(
            "{tool}: the build succeeded; replacing {} was refused because a \
             running process holds it. Close any running {tool} — an editor \
             session's `{tool} mcp serve` is the usual one, and it can outlive \
             the window that started it — then re-run.",
            destination.display()
        )
    })
}

fn cargo_available() -> bool {
    std::process::Command::new("cargo")
        .arg("--version")
        .output()
        .is_ok_and(|o| o.status.success())
}

/// Point `<bin>/<tool>` at whatever the resolver now chooses.
///
/// For the running tool the strictly-newer rule applies (a stale cache must not
/// downgrade a from-source build). For the companions the pin — or the newest
/// install — is the whole answer.
fn activate(
    cache: &Cache,
    tool: &str,
    running: Option<&Version>,
    out: &mut dyn Write,
) -> anyhow::Result<Option<PathBuf>> {
    let Some(bin) = shared_bin_dir() else {
        return Ok(None);
    };
    let placed = match running {
        Some(running) => localpilot_dist::activate(cache, &bin, tool, running),
        None => {
            let chosen = cache
                .pin()
                .and_then(|pinned| cache.get(&pinned))
                .or_else(|| cache.newest());
            match chosen {
                Some(cached) => localpilot_dist::place(&bin, tool, &cached.executable()).map(Some),
                None => Ok(None),
            }
        }
    };
    match placed {
        Ok(path) => Ok(path),
        Err(error) => {
            // The payload is installed and resolvable; only the PATH-visible copy
            // failed. Say which half worked so the fix is obvious.
            writeln!(
                out,
                "installed, but could not update {tool} on PATH: {error}"
            )?;
            Ok(None)
        }
    }
}

/// Versions the sweep must never remove: the running version (only knowable for
/// the running tool) and the pin.
fn protected(cache: &Cache, running: Option<&Version>) -> Vec<Version> {
    let mut protected = Vec::new();
    if let Some(running) = running {
        protected.push(running.clone());
    }
    if let Some(pinned) = cache.pin() {
        protected.push(pinned);
    }
    protected
}

/// The install cache for one train tool, when the platform reports a data
/// directory.
fn tool_cache(tool: &str) -> Option<Cache> {
    Cache::default_root(tool).map(Cache::new)
}

/// Base URL of a release's downloadable assets.
fn release_assets_url(repo_url: &str, tag: &str) -> String {
    // A repo URL may carry a `.git` suffix; the releases path must not.
    let repo = repo_url.trim_end_matches(".git");
    format!("{repo}/releases/download/{tag}")
}

/// The `owner/name` a repository URL points at, for the attestation hint.
fn repo_slug(repo_url: &str) -> &str {
    repo_url
        .trim_end_matches(".git")
        .rsplit_once("github.com/")
        .map_or(repo_url, |(_, slug)| slug)
}

fn report(
    tools: &[&StackTool],
    failed: &[&str],
    tag: Option<&str>,
    channel: Channel,
    running_tool: Option<&str>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let where_at = match tag {
        Some(t) => format!(" at {t}"),
        None => " from main".to_string(),
    };
    if failed.is_empty() {
        writeln!(out, "\nthe stack is installed{where_at}")?;
    } else if failed.len() == tools.len() {
        match channel {
            Channel::Release => {
                writeln!(
                    out,
                    "\nnothing was installed: no tool published a usable build for this platform."
                )?;
                writeln!(
                    out,
                    "try --prerelease to build from source, or check your network."
                )?;
            }
            Channel::Prerelease => {
                writeln!(out, "\nnothing was installed: every source build failed.")?;
            }
        }
        return Ok(());
    } else if running_tool.is_some_and(|running| failed.contains(&running)) {
        // A refused self-replace does not clear on a re-run — the same binary
        // would be running again. Its own message above names the next step.
        writeln!(
            out,
            "\ninstalled{where_at}, except: {}. See the message above for the next step.",
            failed.join(", ")
        )?;
    } else {
        writeln!(
            out,
            "\ninstalled{where_at}, except: {}. Re-run to retry.",
            failed.join(", ")
        )?;
    }
    // The shared-bin PATH advice only applies to the release channel; a source
    // build lands in cargo's bin directory instead — except the running tool,
    // which was swapped in place, so a self-only run has nothing to add.
    let only_self = running_tool.is_some_and(|r| tools.iter().all(|t| t.tool == r));
    if channel == Channel::Release {
        path_notice(out)?;
    } else if !only_self {
        writeln!(
            out,
            "\nsource-built binaries are in cargo's bin directory; ensure it is on PATH:"
        )?;
        writeln!(
            out,
            "    ~/.cargo/bin   (or %USERPROFILE%\\.cargo\\bin on Windows)"
        )?;
    }
    Ok(())
}

/// Tell the user how to reach the executables when the shared bin directory is
/// not already on `PATH`, and warn when a copy earlier on `PATH` will be run
/// instead of the one just installed.
///
/// **Release channel only.** On the prerelease channel `cargo install` writes
/// into cargo's own bin directory *by design* (see [`source_args`], which passes
/// `--force` and no `--root`), so a copy there is the install, not a stale
/// shadow — calling this on that channel reports correct state as a defect.
/// [`report`] gates the call accordingly, and any new caller must do the same or
/// establish the channel first. A caller that cannot know the channel needs the
/// stronger test this deliberately does not do: compare the shadowing binary's
/// *version* against the managed copy, which means executing it.
pub fn path_notice(out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(bin) = shared_bin_dir() else {
        return Ok(());
    };
    let entries: Vec<PathBuf> = std::env::var_os("PATH")
        .map(|path| std::env::split_paths(&path).collect())
        .unwrap_or_default();
    if !entries.iter().any(|entry| *entry == bin) {
        writeln!(out, "\nadd this directory to PATH to use them:")?;
        writeln!(out, "    {}", bin.display())?;
        if cfg!(windows) {
            writeln!(
                out,
                "    setx PATH \"$env:PATH;{}\"   (PowerShell, new terminals only)",
                bin.display()
            )?;
        } else {
            writeln!(
                out,
                "    export PATH=\"{}:$PATH\"   (add to your shell profile)",
                bin.display()
            )?;
        }
    }
    // Being on PATH is not the same as winning it. Report the shadow either way:
    // an older copy in an earlier entry beats a correct PATH, and it also beats
    // the entry the advice above tells the user to append.
    shadow_notice(out, &bin, &entries, &|candidate| candidate.is_file())
}

/// A train tool an earlier `PATH` entry resolves before the shared bin
/// directory, so the freshly installed copy is not the one that runs.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Shadowed {
    tool: &'static str,
    found_at: PathBuf,
}

/// Which train tools an earlier `PATH` entry wins over `bin`.
///
/// Pure over its inputs so the resolution order is testable without a real
/// `PATH` or filesystem. A `bin` that is absent from `entries` shadows from
/// every entry: appending it later still loses to all of them.
fn shadowed_before(
    bin: &Path,
    entries: &[PathBuf],
    exists: &dyn Fn(&Path) -> bool,
) -> Vec<Shadowed> {
    let cutoff = entries
        .iter()
        .position(|entry| entry == bin)
        .unwrap_or(entries.len());
    TRAIN
        .iter()
        .filter_map(|stack_tool| {
            let name = localpilot_dist::executable_name(stack_tool.tool);
            entries[..cutoff]
                .iter()
                .map(|entry| entry.join(&name))
                .find(|candidate| exists(candidate))
                .map(|found_at| Shadowed {
                    tool: stack_tool.tool,
                    found_at,
                })
        })
        .collect()
}

/// Name the older copies that will run instead, and the two ways out.
///
/// Silent when nothing is shadowed, which is the ordinary case.
fn shadow_notice(
    out: &mut dyn Write,
    bin: &Path,
    entries: &[PathBuf],
    exists: &dyn Fn(&Path) -> bool,
) -> anyhow::Result<()> {
    let shadowed = shadowed_before(bin, entries, exists);
    if shadowed.is_empty() {
        return Ok(());
    }
    // Neutral about *when*: this prints after an install and from `status`, and
    // "what was just installed" is a lie in the second case.
    writeln!(
        out,
        "\nwarning: PATH resolves these before the managed copies, so they are what actually runs:"
    )?;
    for entry in &shadowed {
        writeln!(out, "    {} -> {}", entry.tool, entry.found_at.display())?;
    }
    writeln!(
        out,
        "\nuntil this is resolved, running {} uses the copy above, not the one in {}.",
        shadowed[0].tool,
        bin.display()
    )?;
    writeln!(out, "resolve it either way:")?;
    writeln!(
        out,
        "    put {} ahead of the directories above on PATH, or",
        bin.display()
    )?;
    writeln!(out, "    delete the older copies listed above")?;
    // A cargo-installed copy is the common cause: it predates this installer and
    // `~/.cargo/bin` is on PATH by default, so it silently keeps winning. Name
    // the exact remedy only when the path really is cargo's, so the hint cannot
    // send someone to `cargo uninstall` for a binary cargo never installed.
    if shadowed.iter().any(|entry| {
        entry
            .found_at
            .components()
            .any(|c| c.as_os_str() == ".cargo")
    }) {
        writeln!(
            out,
            "\nthe copies under .cargo came from `cargo install`; `cargo uninstall <tool>` removes them."
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{
        report, self_view, shadow_notice, shadowed_before, source_args, stage_and_replace, tool,
        Channel, Running, SelfInstall, Version, TRAIN,
    };
    use std::path::{Path, PathBuf};

    /// `PATH` entries from plain strings, so the ordering under test is obvious.
    fn entries(raw: &[&str]) -> Vec<PathBuf> {
        raw.iter().map(PathBuf::from).collect()
    }

    /// The executable this platform would actually look for.
    fn exe(tool: &str) -> String {
        localpilot_dist::executable_name(tool)
    }

    #[test]
    fn an_earlier_path_entry_shadows_the_shared_bin_directory() {
        // The live 2026-07-27 failure: a pre-installer `cargo install` copy sits
        // in an earlier entry, so the installer writes a correct binary that
        // never runs. Being on PATH is not the same as winning it.
        let cargo = PathBuf::from("/home/u/.cargo/bin");
        let bin = PathBuf::from("/home/u/.local/share/localx/bin");
        let stale = cargo.join(exe("localpilot"));

        let found = shadowed_before(
            &bin,
            &entries(&["/home/u/.cargo/bin", "/home/u/.local/share/localx/bin"]),
            &|candidate| candidate == stale,
        );

        assert_eq!(found.len(), 1, "one tool is shadowed");
        assert_eq!(found[0].tool, "localpilot");
        assert_eq!(found[0].found_at, stale);
    }

    #[test]
    fn the_shared_bin_directory_winning_is_not_a_shadow() {
        // Same two directories, opposite order: the installed copy resolves
        // first, so a copy behind it is irrelevant and must stay silent.
        let bin = PathBuf::from("/home/u/.local/share/localx/bin");
        let behind = PathBuf::from("/home/u/.cargo/bin").join(exe("localpilot"));

        let found = shadowed_before(
            &bin,
            &entries(&["/home/u/.local/share/localx/bin", "/home/u/.cargo/bin"]),
            &|candidate| candidate == behind,
        );

        assert!(found.is_empty(), "a copy after the shared bin never runs");
    }

    #[test]
    fn a_shared_bin_absent_from_path_is_shadowed_by_every_entry() {
        // The worst case, and the one the old advice made silently useless:
        // appending the directory still loses to everything already on PATH.
        let bin = PathBuf::from("/home/u/.local/share/localx/bin");
        let stale = PathBuf::from("/usr/local/bin").join(exe("localmind"));

        let found = shadowed_before(&bin, &entries(&["/usr/local/bin"]), &|candidate| {
            candidate == stale
        });

        assert_eq!(found.len(), 1);
        assert_eq!(found[0].tool, "localmind");
    }

    #[test]
    fn the_first_shadowing_entry_wins_and_every_train_tool_is_checked() {
        // Resolution order matters: the earliest copy is the one that runs, so
        // that is the path the user must be told about. And the sweep covers the
        // whole train, not just the tool that happened to be noticed.
        let bin = PathBuf::from("/opt/localx/bin");
        let first = PathBuf::from("/a").join(exe("localpilot"));
        let second = PathBuf::from("/b").join(exe("localpilot"));
        let other = PathBuf::from("/b").join(exe("localbox"));

        let found = shadowed_before(&bin, &entries(&["/a", "/b", "/opt/localx/bin"]), &|c| {
            c == first || c == second || c == other
        });

        let localpilot = found
            .iter()
            .find(|s| s.tool == "localpilot")
            .expect("localpilot shadowed");
        assert_eq!(
            localpilot.found_at, first,
            "the earliest copy is the one run"
        );
        assert!(
            found.iter().any(|s| s.tool == "localbox"),
            "every train tool is checked, not only the first"
        );
    }

    #[test]
    fn the_notice_names_the_shadowing_path_and_both_remedies() {
        // The old PATH notice said where the new binary is, never that something
        // else would win — which is why the failure was invisible. The message
        // has to name the offender and both ways out.
        let bin = PathBuf::from("/home/u/.local/share/localx/bin");
        let stale = PathBuf::from("/home/u/.cargo/bin").join(exe("localpilot"));
        let mut out = Vec::new();

        shadow_notice(
            &mut out,
            &bin,
            &entries(&["/home/u/.cargo/bin", "/home/u/.local/share/localx/bin"]),
            &|candidate| candidate == stale,
        )
        .expect("notice writes");

        let text = String::from_utf8(out).expect("utf-8");
        assert!(
            text.contains(&stale.display().to_string()),
            "names the offender"
        );
        assert!(text.contains("PATH"), "offers the reorder remedy");
        assert!(text.contains("delete"), "offers the removal remedy");
        assert!(
            text.contains("cargo uninstall"),
            "a .cargo copy gets its exact remedy"
        );
    }

    #[test]
    fn the_notice_is_silent_when_nothing_is_shadowed() {
        // Every ordinary install prints this path. It must add nothing.
        let bin = PathBuf::from("/opt/localx/bin");
        let mut out = Vec::new();

        shadow_notice(&mut out, &bin, &entries(&["/opt/localx/bin"]), &|_| true)
            .expect("notice writes");

        assert!(out.is_empty(), "no shadow, no output");
    }

    #[test]
    fn a_non_cargo_shadow_does_not_suggest_cargo_uninstall() {
        // Sending someone to `cargo uninstall` for a binary cargo never
        // installed is worse than saying nothing.
        let bin = PathBuf::from("/opt/localx/bin");
        let stale = PathBuf::from("/usr/local/bin").join(exe("localpilot"));
        let mut out = Vec::new();

        shadow_notice(
            &mut out,
            &bin,
            &entries(&["/usr/local/bin", "/opt/localx/bin"]),
            &|candidate| candidate == stale,
        )
        .expect("notice writes");

        let text = String::from_utf8(out).expect("utf-8");
        assert!(text.contains("/usr/local/bin"), "still names the offender");
        assert!(!text.contains("cargo uninstall"), "no cargo remedy offered");
    }

    #[test]
    fn self_view_keys_on_identity_not_on_a_parseable_version() {
        let localx = tool("localx").expect("localx in train");

        // A running localx whose stamp is a bare sha: no parseable version, but
        // the identity is certain — so the self route stays on. This is exactly
        // the LocalHub#79 case the old version gate switched off in production.
        let sha_stamped = Running {
            tool: "localx",
            version: None,
        };
        let (is_self, version) = self_view(Some(&sha_stamped), localx.tool);
        assert!(
            is_self,
            "a sha-stamped localx must still be recognised as self"
        );
        assert!(version.is_none());

        // A parseable version is carried through, for the strictly-newer rule.
        let tagged = Running {
            tool: "localx",
            version: Version::parse("3.3.1"),
        };
        let (is_self, version) = self_view(Some(&tagged), localx.tool);
        assert!(is_self);
        assert_eq!(version, Version::parse("3.3.1").as_ref());

        // A companion tool is not self, and carries no running version here.
        let (is_self, version) = self_view(Some(&sha_stamped), "localbox");
        assert!(!is_self);
        assert!(version.is_none());

        // No marker at all: not self.
        let (is_self, version) = self_view(None, localx.tool);
        assert!(!is_self);
        assert!(version.is_none());
    }

    #[test]
    fn train_has_five_tools_with_localx_last() {
        assert_eq!(TRAIN.len(), 5);
        assert_eq!(TRAIN.last().map(|t| t.tool), Some("localx"));
    }

    #[test]
    fn localx_rides_localpilot_release_with_its_own_manifest() {
        let localx = tool("localx").expect("localx in train");
        assert!(localx.repo.ends_with("/LocalPilot"));
        assert_eq!(localx.manifest, "manifest-localx.json");
    }

    #[test]
    fn localmind_source_build_uses_the_cli_package_not_the_binary_name() {
        let localmind = tool("localmind").expect("localmind in train");
        let args = source_args(localmind, None);
        // `cargo install <package>` needs the package name, which is not the
        // binary name for localmind.
        assert!(args.contains(&"localmind-cli".to_string()));
        assert!(args.contains(&"--branch".to_string()));
        assert!(args.contains(&"main".to_string()));
    }

    #[test]
    fn localpilot_source_build_carries_its_features() {
        let localpilot = tool("localpilot").expect("localpilot in train");
        let args = source_args(localpilot, None);
        let features_idx = args
            .iter()
            .position(|a| a == "--features")
            .expect("features");
        assert_eq!(args[features_idx + 1], "tui");
    }

    #[test]
    fn a_companion_source_build_still_forces_into_cargos_bin_directory() {
        // The self-replace route must not leak into the other four tools: with
        // no staging root the arguments are the classic in-place refresh.
        let localbox = tool("localbox").expect("localbox in train");
        let args = source_args(localbox, None);
        assert!(args.contains(&"--force".to_string()));
        assert!(!args.contains(&"--root".to_string()));
    }

    #[test]
    fn a_self_source_build_targets_a_staging_root_and_never_forces_in_place() {
        let localx = tool("localx").expect("localx in train");
        let root = Path::new("staging-root");
        let args = source_args(localx, Some(root));
        let root_idx = args.iter().position(|a| a == "--root").expect("--root");
        assert_eq!(args[root_idx + 1], root.display().to_string());
        assert!(!args.contains(&"--force".to_string()));
        assert!(args.contains(&"--locked".to_string()));
    }

    #[test]
    fn a_successful_self_build_replaces_the_running_executable_and_clears_staging() {
        let temp = tempfile::tempdir().expect("temp");
        let staging = temp.path().join("source-build");
        let running = temp.path().join("bin").join("tool");
        std::fs::create_dir_all(running.parent().unwrap()).unwrap();
        std::fs::write(&running, "old").unwrap();

        let outcome = stage_and_replace("tool", &staging, &running, &mut |root| {
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join(localpilot_dist::executable_name("tool")), "new").unwrap();
            Ok(true)
        })
        .unwrap();

        let SelfInstall::Replaced(path) = outcome else {
            panic!("expected a replacement, got {outcome:?}");
        };
        assert_eq!(std::fs::read_to_string(&path).unwrap(), "new");
        assert!(
            !staging.exists(),
            "staging is cleared after a successful swap"
        );
    }

    #[test]
    fn a_refused_swap_keeps_the_built_executable_where_the_message_says() {
        let temp = tempfile::tempdir().expect("temp");
        let staging = temp.path().join("source-build");
        // A running executable whose parent is a plain file: placement cannot
        // create the destination directory, so the swap is refused.
        let blocker = temp.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let running = blocker.join("tool");

        let outcome = stage_and_replace("tool", &staging, &running, &mut |root| {
            let bin = root.join("bin");
            std::fs::create_dir_all(&bin).unwrap();
            std::fs::write(bin.join(localpilot_dist::executable_name("tool")), "new").unwrap();
            Ok(true)
        })
        .unwrap();

        let SelfInstall::Retained { built, error } = outcome else {
            panic!("expected the build to be retained, got {outcome:?}");
        };
        assert!(
            built.is_file(),
            "the retained build must still exist: {}",
            built.display()
        );
        assert!(!error.is_empty());
    }

    #[test]
    fn a_failed_self_build_leaves_nothing_behind() {
        let temp = tempfile::tempdir().expect("temp");
        let staging = temp.path().join("source-build");
        let running = temp.path().join("tool");
        std::fs::write(&running, "old").unwrap();

        let outcome = stage_and_replace("tool", &staging, &running, &mut |_| Ok(false)).unwrap();

        assert!(matches!(outcome, SelfInstall::BuildFailed));
        assert!(!staging.exists());
        assert_eq!(std::fs::read_to_string(&running).unwrap(), "old");
    }

    #[test]
    fn a_build_that_cannot_be_spawned_is_an_error_not_a_retained_build() {
        let temp = tempfile::tempdir().expect("temp");
        let staging = temp.path().join("source-build");
        let running = temp.path().join("tool");
        std::fs::write(&running, "old").unwrap();

        let result = stage_and_replace("tool", &staging, &running, &mut |_| {
            Err(anyhow::anyhow!("could not run cargo: not found"))
        });

        let error = result.expect_err("a spawn failure propagates");
        assert!(error.to_string().contains("could not run cargo"));
        assert_eq!(std::fs::read_to_string(&running).unwrap(), "old");
    }

    #[test]
    fn a_refused_swap_keeps_the_raw_error_authoritative_and_never_claims_a_cause() {
        let mut out = Vec::new();
        super::describe_self_install(
            "tool",
            Path::new("/opt/bin/tool"),
            &SelfInstall::Retained {
                built: std::path::PathBuf::from("/data/tool/source-build/bin/tool"),
                error: "permission denied (os error 13)".to_string(),
            },
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("permission denied (os error 13)"), "{text}");
        assert!(text.contains("copy it over"), "{text}");
        assert!(
            !text.contains("that file is in use by this process"),
            "{text}"
        );
        assert!(!text.contains("Re-run"), "{text}");
        if cfg!(windows) {
            assert!(text.contains("if the error is an access-denied"), "{text}");
        } else {
            assert!(!text.contains("elevated"), "{text}");
        }
    }

    #[test]
    fn a_staging_directory_that_cannot_be_created_names_that_and_nothing_else() {
        let temp = tempfile::tempdir().expect("temp");
        // Staging nested under a plain file cannot be created.
        let blocker = temp.path().join("not-a-dir");
        std::fs::write(&blocker, "x").unwrap();
        let staging = blocker.join("source-build");
        let running = temp.path().join("tool");
        std::fs::write(&running, "old").unwrap();
        let mut built = false;

        let outcome = stage_and_replace("tool", &staging, &running, &mut |_| {
            built = true;
            Ok(true)
        })
        .unwrap();

        let SelfInstall::StagingFailed(reason) = outcome else {
            panic!("expected a staging failure, got {outcome:?}");
        };
        assert!(reason.contains("could not create"), "{reason}");
        assert!(!built, "no build runs without a staging directory");
        let mut out = Vec::new();
        super::describe_self_install(
            "tool",
            &running,
            &SelfInstall::StagingFailed(reason),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("access-denied"), "{text}");
        assert!(!text.contains("copy it over"), "{text}");
    }

    #[test]
    fn the_report_never_tells_a_refused_self_replace_to_re_run() {
        let localx = tool("localx").expect("localx in train");
        let localbox = tool("localbox").expect("localbox in train");
        let tools = [localx, localbox];

        let mut out = Vec::new();
        report(
            &tools,
            &["localx"],
            None,
            Channel::Prerelease,
            Some("localx"),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(!text.contains("Re-run to retry"), "{text}");
        assert!(text.contains("See the message above"), "{text}");

        let mut out = Vec::new();
        report(
            &tools,
            &["localbox"],
            None,
            Channel::Prerelease,
            Some("localx"),
            &mut out,
        )
        .unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("Re-run to retry"), "{text}");
    }
}
