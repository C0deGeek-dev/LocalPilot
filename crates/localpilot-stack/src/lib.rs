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
        features: &["tui", "learning"],
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

/// The tool whose version is known because it is the running binary. Its cache
/// gets the strictly-newer activation rule (which stops a stale cache
/// downgrading a from-source build) and its running version is protected from
/// the sweep — neither is knowable for the other tools from this process.
pub struct Running {
    pub tool: &'static str,
    pub version: Version,
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
        let ok = match channel {
            Channel::Release => {
                let tag = resolved_tag.as_deref().unwrap_or_default();
                let running_version = running.filter(|r| r.tool == t.tool).map(|r| &r.version);
                install_release(t, tag, running_version, out).await?
            }
            Channel::Prerelease => source_install(t, running, out)?,
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

/// Install one train tool's published archive at `tag`, verifying it first and
/// then making it the version on `PATH`.
///
/// # Errors
/// Returns an error only if output cannot be written; a failed install is
/// reported and leaves the previously installed version untouched.
pub async fn install_release(
    t: &StackTool,
    tag: &str,
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
                // `running` is only ever `Some` for the tool this process *is*.
                // When it runs from somewhere other than the managed copy (a
                // source bootstrap in cargo's bin directory, earlier on PATH),
                // refresh that copy too — otherwise the shell keeps resolving
                // the stale one and the update is invisible.
                if running.is_some() {
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
/// The one exception is the tool this process *is* (`running`): cargo's final
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
pub fn source_install(
    t: &StackTool,
    running: Option<&Running>,
    out: &mut dyn Write,
) -> anyhow::Result<bool> {
    if !cargo_available() {
        writeln!(
            out,
            "{}: cargo (the Rust toolchain) is required for --prerelease; \
             install it from https://rustup.rs, or drop --prerelease to use the release channel",
            t.tool
        )?;
        return Ok(false);
    }

    let is_self = running.is_some_and(|r| r.tool == t.tool);
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
        SelfInstall::BuildFailed => writeln!(out, "{tool}: cargo install failed"),
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
/// not already on `PATH`.
pub fn path_notice(out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(bin) = shared_bin_dir() else {
        return Ok(());
    };
    let on_path = std::env::var_os("PATH")
        .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == bin));
    if on_path {
        return Ok(());
    }
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
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::{report, source_args, stage_and_replace, tool, Channel, SelfInstall, TRAIN};
    use std::path::Path;

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
        assert_eq!(args[features_idx + 1], "tui,learning");
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
