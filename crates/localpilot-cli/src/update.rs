//! Self-update: check the project repository for a newer release tag and, on the
//! user's confirmation, reinstall from source with the same feature set.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use localpilot_store::Store;

const REPO_URL: &str = "https://github.com/C0deGeek-dev/LocalPilot.git";
const TAGS_API: &str = "https://api.github.com/repos/C0deGeek-dev/LocalPilot/tags";
const CACHE_KEY: &str = "update-check.json";
const CHECK_INTERVAL_SECS: u64 = 86_400;

/// The running binary's version, embedded at build time (a `git describe` of the
/// source, or the release tag).
#[must_use]
pub fn current_version() -> &'static str {
    env!("LOCALPILOT_VERSION")
}

use localpilot_dist::Version;

/// Query the repository for the newest tag. Returns the tag name when it is
/// strictly newer than the running version, else `None`.
///
/// # Errors
/// Returns an error if the repository cannot be reached or parsed.
pub async fn newer_release() -> anyhow::Result<Option<String>> {
    let current = Version::parse(current_version());
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(5))
        .build()?;
    let body: serde_json::Value = client
        .get(TAGS_API)
        // GitHub requires a User-Agent; it serves anonymous tag listings.
        .header("User-Agent", "localpilot-update-check")
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

    Ok(match (best, current) {
        (Some((latest, name)), Some(cur)) if latest.key() > cur.key() => Some(name),
        // Unparseable local version: surface the latest tag so the user can decide.
        (Some((_, name)), None) => Some(name),
        _ => None,
    })
}

/// A best-effort, cached "update available" notice for app startup. Checks the
/// network at most once a day (result cached in the project store) and returns
/// the newer tag, if any. Never fails; returns `None` on any error.
///
/// Disabled by `LOCALPILOT_NO_UPDATE_CHECK`, and compiled out on the windows-gnu
/// toolchain whose TLS stack is unstable (the explicit `update` command still
/// works there).
pub async fn cached_notice(root: &Path) -> Option<String> {
    if cfg!(all(windows, target_env = "gnu")) {
        return None;
    }
    if std::env::var_os("LOCALPILOT_NO_UPDATE_CHECK").is_some() {
        return None;
    }

    let store = Store::open(root);
    let now = now_unix();

    if let Ok(Some(bytes)) = store.get_cache(CACHE_KEY) {
        if let Ok(value) = serde_json::from_slice::<serde_json::Value>(&bytes) {
            let checked_at = value.get("checked_at").and_then(serde_json::Value::as_u64);
            if checked_at.is_some_and(|t| now.saturating_sub(t) < CHECK_INTERVAL_SECS) {
                // Fresh cache: return the stored result without a network call.
                return value
                    .get("latest")
                    .and_then(serde_json::Value::as_str)
                    .map(String::from);
            }
        }
    }

    let latest = newer_release().await.ok().flatten();
    let record = serde_json::json!({
        "checked_at": now,
        "latest": latest.clone(),
    });
    if let Ok(bytes) = serde_json::to_vec(&record) {
        let _ = store.put_cache(CACHE_KEY, &bytes);
    }
    latest
}

fn now_unix() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

/// Run the `update` command: check, report, and (unless `check_only`) prompt and
/// reinstall from source.
///
/// # Errors
/// Returns an error only if writing output or running the installer fails; a
/// failed network check is reported, not returned.
pub async fn run(
    check_only: bool,
    from_source: bool,
    all: bool,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let current = current_version();
    if all {
        // `--all` means "make the stack match this binary". It deliberately does
        // not ask about a newer release: the user picked a version by installing
        // it, and the bootstrap installer runs this with stdin bound to a pipe,
        // where a confirmation prompt reads EOF and cancels.
        let tag = current_tag();
        writeln!(out, "installing the stack at {tag} ...")?;
        return install_stack(&tag, out).await;
    }
    match newer_release().await {
        Ok(Some(tag)) => {
            writeln!(out, "update available: {tag}  (current: {current})")?;
            if check_only {
                writeln!(out, "run `localpilot update` to install it")?;
                return Ok(());
            }
            if !confirm(&format!("update to {tag} now?"))? {
                writeln!(out, "cancelled")?;
                return Ok(());
            }
            // Prefer the published binary: it needs no toolchain and takes
            // seconds. Compiling stays available, and is the automatic fallback
            // when a platform has no published archive.
            let installed = !from_source && install_binary(&tag, out).await?;
            if !installed {
                if !from_source {
                    writeln!(out, "falling back to building from source")?;
                }
                reinstall(&tag, out)?;
            }
        }
        Ok(None) => {
            writeln!(out, "up to date ({current})")?;
        }
        Err(error) => writeln!(out, "update check failed: {error}")?,
    }
    Ok(())
}

/// The release tag matching the running build, for installing companions at the
/// version this binary was cut with.
fn current_tag() -> String {
    localpilot_dist::Version::parse(current_version()).map_or_else(
        || current_version().to_string(),
        |v| format!("v{}", v.to_dir_name()),
    )
}

/// Install the release train at `tag`.
///
/// The train cuts every tool to one version, so a stack assembled from different
/// tags is a configuration nobody tested. A tool that fails to install is
/// reported and skipped rather than aborting the others — a partial stack the
/// user can see beats an install that stops halfway without saying where.
async fn install_stack(tag: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut failed = Vec::new();
    for (tool, repo) in TRAIN {
        if !install_tool(tool, repo, tag, out).await? {
            failed.push(*tool);
        }
    }
    if failed.is_empty() {
        writeln!(out, "\nthe stack is installed at {tag}")?;
    } else {
        writeln!(
            out,
            "\ninstalled at {tag}, except: {}. Re-run `localpilot update --all` to retry.",
            failed.join(", ")
        )?;
    }
    path_notice(out)
}

/// Tell the user how to reach the executables, when the directory holding them is
/// not already on `PATH`.
fn path_notice(out: &mut dyn Write) -> anyhow::Result<()> {
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

/// Reinstall from source at `tag` via `cargo install --git`, matching the running
/// binary's feature set, and the MSVC toolchain on Windows when the TUI is built.
fn reinstall(tag: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let mut features: Vec<&str> = Vec::new();
    if cfg!(feature = "tui") {
        features.push("tui");
    }

    let mut command = std::process::Command::new("cargo");
    // The interactive TUI is unstable on the windows-gnu toolchain.
    if cfg!(all(windows, feature = "tui")) {
        command.arg("+stable-x86_64-pc-windows-msvc");
    }
    command.args([
        "install", "--git", REPO_URL, "--tag", tag, "--locked", "--force",
    ]);
    if !features.is_empty() {
        command.arg("--features").arg(features.join(","));
    }

    writeln!(out, "reinstalling from source at {tag} ...")?;
    let status = command
        .status()
        .map_err(|e| anyhow::anyhow!("could not run cargo: {e}"))?;
    if status.success() {
        writeln!(out, "updated to {tag}")?;
        Ok(())
    } else {
        Err(anyhow::anyhow!("cargo install failed"))
    }
}

fn confirm(prompt: &str) -> anyhow::Result<bool> {
    use std::io::Write as _;
    print!("{prompt} [y/N] ");
    std::io::stdout().flush()?;
    let mut answer = String::new();
    std::io::stdin().read_line(&mut answer)?;
    Ok(answer.trim().eq_ignore_ascii_case("y"))
}

// --- version cache ----------------------------------------------------------

/// Base URL of a release's downloadable assets.
fn release_assets_url(repo_url: &str, tag: &str) -> String {
    // `REPO_URL` carries the `.git` suffix cargo wants; the releases path does
    // not, and GitHub is stricter about it on some routes than others.
    let repo = repo_url.trim_end_matches(".git");
    format!("{repo}/releases/download/{tag}")
}

/// The `owner/name` a repository URL points at, for the attestation hint.
fn repo_slug(repo_url: &str) -> &str {
    repo_url
        .trim_end_matches(".git")
        .rsplit_once("github.com/")
        .map_or("C0deGeek-dev/LocalPilot", |(_, slug)| slug)
}

/// The install cache for this tool, when the platform reports a data directory.
fn cache() -> Option<localpilot_dist::Cache> {
    tool_cache("localpilot")
}

/// The install cache for any train tool.
fn tool_cache(tool: &str) -> Option<localpilot_dist::Cache> {
    localpilot_dist::Cache::default_root(tool).map(localpilot_dist::Cache::new)
}

/// The tools one release train cuts together.
///
/// They share a version and a tag by construction, so installing them as a set is
/// the only way to avoid a skew none of them can detect on its own — a mismatched
/// store schema between an installed CLI and the engine it talks to fails
/// silently, not loudly.
const TRAIN: &[(&str, &str)] = &[
    ("localpilot", "https://github.com/C0deGeek-dev/LocalPilot"),
    ("localmind", "https://github.com/C0deGeek-dev/LocalMind"),
    ("localbox", "https://github.com/C0deGeek-dev/LocalBox"),
    ("localbench", "https://github.com/C0deGeek-dev/LocalBench"),
];

/// The one directory every train tool's executable is published into, so a user
/// adds a single entry to `PATH` rather than one per tool.
#[must_use]
pub fn shared_bin_dir() -> Option<std::path::PathBuf> {
    localpilot_dist::Cache::default_root("localx").map(|root| localpilot_dist::bin_dir(&root))
}

/// Point `<bin>/<tool>` at whatever the resolver now chooses, so a version switch
/// is visible to the shell rather than only to `version list`.
///
/// Only this tool has a running build in this process. The resolver's
/// strictly-newer rule exists to stop a stale cache downgrading a from-source
/// build — a protection that is meaningful for `localpilot` and meaningless for
/// the companions, where the pin (or the newest install) is the whole answer.
fn activate(
    cache: &localpilot_dist::Cache,
    tool: &str,
    out: &mut dyn Write,
) -> anyhow::Result<Option<std::path::PathBuf>> {
    let Some(bin) = shared_bin_dir() else {
        return Ok(None);
    };
    let placed = if tool == "localpilot" {
        let Some(running) = localpilot_dist::Version::parse(current_version()) else {
            return Ok(None);
        };
        localpilot_dist::activate(cache, &bin, tool, &running)
    } else {
        let chosen = cache
            .pin()
            .and_then(|pinned| cache.get(&pinned))
            .or_else(|| cache.newest());
        match chosen {
            Some(cached) => localpilot_dist::place(&bin, tool, &cached.executable()).map(Some),
            None => Ok(None),
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

/// Install a released binary into the version cache, verifying it first.
///
/// This is the path that does **not** need a Rust toolchain. The from-source
/// reinstall stays available for a target with no published archive and for
/// anyone who prefers it.
///
/// # Errors
/// Returns an error only if output cannot be written; a failed install is
/// reported and leaves the previously installed version untouched.
pub async fn install_binary(tag: &str, out: &mut dyn Write) -> anyhow::Result<bool> {
    install_tool("localpilot", REPO_URL, tag, out).await
}

/// Install one train tool's published archive at `tag`, verifying it first and
/// then making it the version on `PATH`.
///
/// # Errors
/// Returns an error only if output cannot be written; a failed install is
/// reported and leaves the previously installed version untouched.
pub async fn install_tool(
    tool: &str,
    repo_url: &str,
    tag: &str,
    out: &mut dyn Write,
) -> anyhow::Result<bool> {
    let Some(cache) = tool_cache(tool) else {
        writeln!(
            out,
            "no per-user data directory on this platform; use --from-source"
        )?;
        return Ok(false);
    };
    let target = localpilot_dist::current_target();
    if target.is_empty() {
        writeln!(
            out,
            "this platform has no published build; use --from-source to compile it"
        )?;
        return Ok(false);
    }

    let base = release_assets_url(repo_url, tag);
    writeln!(out, "{tool}: fetching {tag} manifest…")?;
    let manifest_bytes = match localpilot_dist::download(&format!("{base}/manifest.json")).await {
        Ok(bytes) => bytes,
        Err(error) => {
            writeln!(out, "no manifest for {tag} ({error}); use --from-source")?;
            return Ok(false);
        }
    };
    let manifest = match String::from_utf8(manifest_bytes)
        .map_err(|e| e.to_string())
        .and_then(|text| localpilot_dist::ReleaseManifest::parse(&text).map_err(|e| e.to_string()))
    {
        Ok(manifest) => manifest,
        Err(error) => {
            writeln!(
                out,
                "manifest for {tag} is unusable ({error}); use --from-source"
            )?;
            return Ok(false);
        }
    };

    writeln!(out, "{tool}: downloading and verifying {target}…")?;
    match localpilot_dist::install_release(&cache, &manifest, target, tool, &base).await {
        Ok(dir) => {
            writeln!(out, "{tool}: installed {tag} to {}", dir.display())?;
            // Name the property that was actually checked. The digest proves the
            // bytes are intact; origin comes from the release's build attestation,
            // which this updater does not verify in-process — so point at the
            // command that does, rather than implying it was already done.
            writeln!(
                out,
                "verified against the checksum published with the release (integrity)"
            )?;
            writeln!(
                out,
                "to confirm it was built by this repository: \
                 gh attestation verify <archive> --repo {}",
                repo_slug(repo_url)
            )?;
            if let Some(path) = activate(&cache, tool, out)? {
                writeln!(out, "{tool}: on PATH at {}", path.display())?;
            }
            let swept = cache.sweep(KEEP_VERSIONS, &protected(&cache, tool));
            if !swept.is_empty() {
                let names: Vec<String> = swept
                    .iter()
                    .map(localpilot_dist::Version::to_dir_name)
                    .collect();
                writeln!(out, "removed older cached version(s): {}", names.join(", "))?;
            }
            Ok(true)
        }
        Err(error) => {
            writeln!(out, "install failed: {error}")?;
            writeln!(out, "the previously installed version is untouched")?;
            Ok(false)
        }
    }
}

/// Refresh the `PATH`-visible executable after a change to what should run, and
/// say where it landed. A pin that only `version list` can see is not a pin.
fn report_active(cache: &localpilot_dist::Cache, out: &mut dyn Write) -> anyhow::Result<()> {
    if let Some(path) = activate(cache, "localpilot", out)? {
        writeln!(out, "active binary: {}", path.display())?;
    }
    Ok(())
}

/// How many cached versions to keep, beyond the running and pinned ones. Two is
/// enough to roll back once without keeping every release ever installed.
const KEEP_VERSIONS: usize = 2;

/// Versions the sweep must never remove.
///
/// The running version is only ours to protect for our own cache; another tool's
/// running version is not something this process can know.
fn protected(cache: &localpilot_dist::Cache, tool: &str) -> Vec<localpilot_dist::Version> {
    let mut protected = Vec::new();
    if tool == "localpilot" {
        if let Some(running) = localpilot_dist::Version::parse(current_version()) {
            protected.push(running);
        }
    }
    if let Some(pinned) = cache.pin() {
        protected.push(pinned);
    }
    protected
}

/// List installed versions and say which one would run, and why.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn list_versions(out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(cache) = cache() else {
        writeln!(out, "no per-user data directory on this platform")?;
        return Ok(());
    };
    let running = localpilot_dist::Version::parse(current_version());
    let installed = cache.installed();
    if installed.is_empty() {
        writeln!(out, "no installed versions (running {})", current_version())?;
    } else {
        for cached in &installed {
            writeln!(
                out,
                "  {}  {}  {}",
                cached.version.to_dir_name(),
                cached.marker.target,
                cached.dir.display()
            )?;
        }
    }
    if let Some(running) = running {
        let resolution = localpilot_dist::resolve(&cache, &running);
        writeln!(
            out,
            "\nwould run {} — {}",
            resolution.version.to_dir_name(),
            resolution.reason.explain()
        )?;
    }
    Ok(())
}

/// Pin a version so the resolver stops preferring the newest, or clear the pin.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn set_pin(version: Option<&str>, out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(cache) = cache() else {
        writeln!(out, "no per-user data directory on this platform")?;
        return Ok(());
    };
    match version {
        None => {
            cache.clear_pin()?;
            writeln!(out, "pin cleared; the newest installed version will run")?;
            report_active(&cache, out)?;
        }
        Some(text) => {
            let Some(version) = localpilot_dist::Version::parse(text) else {
                writeln!(out, "{text:?} is not a version like 2.5.0")?;
                return Ok(());
            };
            if cache.get(&version).is_none()
                && localpilot_dist::Version::parse(current_version()).as_ref() != Some(&version)
            {
                writeln!(
                    out,
                    "{} is not installed; pinning it anyway would leave nothing to run",
                    version.to_dir_name()
                )?;
                return Ok(());
            }
            cache.set_pin(&version)?;
            writeln!(out, "pinned to {}", version.to_dir_name())?;
            report_active(&cache, out)?;
        }
    }
    Ok(())
}

/// Switch to an older installed version — a pin, not a download.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn rollback(out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(cache) = cache() else {
        writeln!(out, "no per-user data directory on this platform")?;
        return Ok(());
    };
    let running = localpilot_dist::Version::parse(current_version());
    let installed = cache.installed();
    // The newest version strictly older than what would run now.
    let previous = installed.iter().find(|cached| {
        running
            .as_ref()
            .is_some_and(|running| cached.version.key() < running.key())
    });
    match previous {
        Some(cached) => {
            cache.set_pin(&cached.version)?;
            writeln!(
                out,
                "rolled back to {} (pinned; `localpilot version pin --clear` to undo)",
                cached.version.to_dir_name()
            )?;
            report_active(&cache, out)?;
        }
        None => {
            writeln!(
                out,
                "nothing to roll back to — no older version is installed"
            )?;
            writeln!(out, "installed versions: `localpilot version list`")?;
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::Version;

    #[test]
    fn alpha_ordering_and_describe_suffix() {
        let a6 = Version::parse("v0.1.0-alpha.6").unwrap();
        let a7 = Version::parse("v0.1.0-alpha.7").unwrap();
        let release = Version::parse("0.1.0").unwrap();
        let dev = Version::parse("v0.1.0-alpha.6-2-gabc1234").unwrap();

        assert!(a7.key() > a6.key());
        // A full release is newer than any of its alphas.
        assert!(release.key() > a7.key());
        // A describe suffix is ignored: a dev build equals its base tag.
        assert_eq!(dev.key(), a6.key());
    }

    #[test]
    fn channel_ordering() {
        let alpha = Version::parse("v0.3.0-alpha.9").unwrap();
        let beta1 = Version::parse("v0.3.0-beta.1").unwrap();
        let beta2 = Version::parse("v0.3.0-beta.2").unwrap();
        let rc = Version::parse("v0.3.0-rc.1").unwrap();
        let release = Version::parse("0.3.0").unwrap();

        // alpha < beta < rc < release, and numbers order within a channel.
        assert!(beta1.key() > alpha.key());
        assert!(beta2.key() > beta1.key());
        assert!(rc.key() > beta2.key());
        assert!(release.key() > rc.key());
    }

    #[test]
    fn beta_describe_suffix_equals_base_tag() {
        // A dirty dev build off `v0.3.0-beta.2` must still read as that tag.
        let dev = Version::parse("v0.3.0-beta.2-26-g61f9559-dirty").unwrap();
        let tag = Version::parse("v0.3.0-beta.2").unwrap();
        assert_eq!(dev.key(), tag.key());
    }

    #[test]
    fn rejects_garbage() {
        assert!(Version::parse("not-a-version").is_none());
        // A non-version describe tag must not parse as a version.
        assert!(Version::parse("legacy-altscreen-tui-26-g61f9559-dirty").is_none());
    }
}
