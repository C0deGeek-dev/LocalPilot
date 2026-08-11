//! Self-update: check the project repository for a newer release tag and, on the
//! user's confirmation, install it. The stack-install machinery (the release
//! train, the per-tool install loop, source builds, PATH activation) lives in
//! `localpilot-stack` so both this command and the `localx` umbrella share one
//! copy; what stays here is localpilot's own version cache (list/pin/rollback)
//! and the once-a-day update notice.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use localpilot_dist::{Cache, Version};
use localpilot_stack::{Channel, Running, Selection};
use localpilot_store::Store;

const TAGS_API: &str = "https://api.github.com/repos/C0deGeek-dev/LocalPilot/tags";
const CACHE_KEY: &str = "update-check.json";
const CHECK_INTERVAL_SECS: u64 = 86_400;

/// The running binary's version, embedded at build time (a `git describe` of the
/// source, or the release tag).
#[must_use]
pub fn current_version() -> &'static str {
    env!("LOCALPILOT_VERSION")
}

/// Query the repository for the newest tag. Returns the tag name when it is
/// strictly newer than the running version, else `None`.
///
/// # Errors
/// Returns an error if the repository cannot be reached or parsed.
pub async fn newer_release() -> anyhow::Result<Option<String>> {
    let current = Version::parse(current_version());
    Ok(match (latest_release().await?, current) {
        (Some((latest, name)), Some(cur)) if latest.key() > cur.key() => Some(name),
        // Unparseable local version: surface the latest tag so the user can decide.
        (Some((_, name)), None) => Some(name),
        _ => None,
    })
}

/// The newest release tag the repository publishes, whatever the running version
/// is. `newer_release` filters this; callers that need "the current release"
/// rather than "an upgrade" use it directly.
///
/// # Errors
/// Returns an error if the repository cannot be reached or parsed.
pub async fn latest_release() -> anyhow::Result<Option<(Version, String)>> {
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

    Ok(best)
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
/// install — a single tool by default, or the whole stack with `--all`.
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
    let running = Version::parse(current);

    if all {
        // `--all` means "make the stack match this binary". It deliberately does
        // not ask about a newer release: the user picked a version by installing
        // it, and the bootstrap installer runs this with stdin bound to a pipe,
        // where a confirmation prompt reads EOF and cancels. A from-source build
        // has no published tag of its own, so its running version resolves to the
        // base release tag (the describe suffix is dropped); an unparseable
        // version falls back to the newest published release.
        let channel = if from_source {
            Channel::Prerelease
        } else {
            Channel::Release
        };
        let tag = running.as_ref().map(localpilot_stack::tag_for_version);
        let marker = running.as_ref().map(|v| Running {
            tool: "localpilot",
            version: v.clone(),
        });
        return localpilot_stack::install(
            &Selection::All,
            tag.as_deref(),
            channel,
            marker.as_ref(),
            out,
        )
        .await;
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
            let Some(localpilot) = localpilot_stack::tool("localpilot") else {
                writeln!(
                    out,
                    "internal error: localpilot is not in the release train"
                )?;
                return Ok(());
            };
            let marker = running.as_ref().map(|v| Running {
                tool: "localpilot",
                version: v.clone(),
            });
            if from_source {
                // Prefer the published binary: it needs no toolchain and takes
                // seconds. Compiling stays available on request, and is the
                // automatic fallback when a platform has no published archive.
                localpilot_stack::source_install(localpilot, out)?;
            } else if !localpilot_stack::install_release(
                localpilot,
                &tag,
                marker.as_ref().map(|m| &m.version),
                out,
            )
            .await?
            {
                writeln!(out, "falling back to building from source")?;
                localpilot_stack::source_install(localpilot, out)?;
            }
        }
        Ok(None) => {
            writeln!(out, "up to date ({current})")?;
        }
        Err(error) => writeln!(out, "update check failed: {error}")?,
    }
    Ok(())
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

/// The install cache for localpilot, when the platform reports a data directory.
fn cache() -> Option<Cache> {
    Cache::default_root("localpilot").map(Cache::new)
}

/// Refresh the `PATH`-visible localpilot executable after a change to what should
/// run, and say where it landed. A pin that only `version list` can see is not a
/// pin.
fn report_active(cache: &Cache, out: &mut dyn Write) -> anyhow::Result<()> {
    let Some(running) = Version::parse(current_version()) else {
        return Ok(());
    };
    let Some(bin) = localpilot_stack::shared_bin_dir() else {
        return Ok(());
    };
    match localpilot_dist::activate(cache, &bin, "localpilot", &running) {
        Ok(Some(path)) => writeln!(out, "active binary: {}", path.display())?,
        Ok(None) => {}
        Err(error) => writeln!(out, "could not update localpilot on PATH: {error}")?,
    }
    Ok(())
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
    let running = Version::parse(current_version());
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
            let Some(version) = Version::parse(text) else {
                writeln!(out, "{text:?} is not a version like 2.5.0")?;
                return Ok(());
            };
            if cache.get(&version).is_none()
                && Version::parse(current_version()).as_ref() != Some(&version)
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
    let running = Version::parse(current_version());
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
                "rolled back to {} (pinned)",
                cached.version.to_dir_name()
            )?;
            writeln!(out, "to undo: localpilot version pin --clear")?;
            // The version just rolled back to may predate `version pin` — in which
            // case the command above does not exist there and the pin file is the
            // only way out. Name it rather than leave the user stuck.
            writeln!(
                out,
                "  (or delete {} if that release has no `version` command)",
                cache.pin_path().display()
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
