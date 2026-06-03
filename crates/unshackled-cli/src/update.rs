//! Self-update: check the project repository for a newer release tag and, on the
//! user's confirmation, reinstall from source with the same feature set.

use std::io::Write;
use std::path::Path;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use unshackled_store::Store;

const REPO_URL: &str = "https://github.com/David-c0degeek/Unshackled-Rust";
const TAGS_API: &str = "https://api.github.com/repos/David-c0degeek/Unshackled-Rust/tags";
const CACHE_KEY: &str = "update-check.json";
const CHECK_INTERVAL_SECS: u64 = 86_400;

/// The running binary's version, embedded at build time (a `git describe` of the
/// source, or the release tag).
#[must_use]
pub fn current_version() -> &'static str {
    env!("UNSHACKLED_VERSION")
}

/// A parsed `major.minor.patch[-alpha.N]` version. A release (no pre-release)
/// sorts above its pre-releases.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Version {
    major: u64,
    minor: u64,
    patch: u64,
    alpha: Option<u64>,
}

impl Version {
    fn parse(text: &str) -> Option<Self> {
        let core = text.trim().trim_start_matches('v');
        let (release, alpha) = match core.split_once("-alpha.") {
            Some((release, rest)) => {
                let digits: String = rest.chars().take_while(char::is_ascii_digit).collect();
                (release, Some(digits.parse().ok()?))
            }
            // Drop any `git describe` suffix (e.g. `-2-gabc1234`).
            None => (core.split('-').next()?, None),
        };
        let mut parts = release.split('.');
        Some(Version {
            major: parts.next()?.parse().ok()?,
            minor: parts.next()?.parse().ok()?,
            patch: parts.next().unwrap_or("0").parse().ok()?,
            alpha,
        })
    }

    /// Sort key: a release (`alpha = None`) is newer than any of its alphas.
    fn key(&self) -> (u64, u64, u64, u64) {
        (
            self.major,
            self.minor,
            self.patch,
            self.alpha.unwrap_or(u64::MAX),
        )
    }
}

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
        .header("User-Agent", "unshackled-update-check")
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
/// Disabled by `UNSHACKLED_NO_UPDATE_CHECK`, and compiled out on the windows-gnu
/// toolchain whose TLS stack is unstable (the explicit `update` command still
/// works there).
pub async fn cached_notice(root: &Path) -> Option<String> {
    if cfg!(all(windows, target_env = "gnu")) {
        return None;
    }
    if std::env::var_os("UNSHACKLED_NO_UPDATE_CHECK").is_some() {
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
pub async fn run(check_only: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let current = current_version();
    match newer_release().await {
        Ok(Some(tag)) => {
            writeln!(out, "update available: {tag}  (current: {current})")?;
            if check_only {
                writeln!(out, "run `unshackled update` to install it")?;
                return Ok(());
            }
            if !confirm(&format!("update to {tag} now?"))? {
                writeln!(out, "cancelled")?;
                return Ok(());
            }
            reinstall(&tag, out)?;
        }
        Ok(None) => writeln!(out, "up to date ({current})")?,
        Err(error) => writeln!(out, "update check failed: {error}")?,
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
    if cfg!(feature = "learning") {
        features.push("learning");
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
    fn rejects_garbage() {
        assert!(Version::parse("not-a-version").is_none());
    }
}
