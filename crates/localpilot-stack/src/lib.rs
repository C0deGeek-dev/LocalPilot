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
use std::path::PathBuf;
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
            Channel::Prerelease => source_install(t, out)?,
        };
        if !ok {
            failed.push(t.tool);
        }
    }

    report(&tools, &failed, resolved_tag.as_deref(), channel, out)
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
/// # Errors
/// Returns an error only if output cannot be written or cargo cannot be spawned.
pub fn source_install(t: &StackTool, out: &mut dyn Write) -> anyhow::Result<bool> {
    if !cargo_available() {
        writeln!(
            out,
            "{}: cargo (the Rust toolchain) is required for --prerelease; \
             install it from https://rustup.rs, or drop --prerelease to use the release channel",
            t.tool
        )?;
        return Ok(false);
    }

    let mut command = std::process::Command::new("cargo");
    // The interactive TUI is unstable on the windows-gnu toolchain; force MSVC
    // when building a tool that links it.
    if cfg!(windows) && t.features.contains(&"tui") {
        command.arg("+stable-x86_64-pc-windows-msvc");
    }
    command.args(source_args(t));

    writeln!(
        out,
        "{}: building {} from {} (main)…",
        t.tool, t.package, t.repo
    )?;
    let status = command
        .status()
        .map_err(|e| anyhow::anyhow!("could not run cargo: {e}"))?;
    if status.success() {
        writeln!(
            out,
            "{}: installed from main into cargo's bin directory",
            t.tool
        )?;
        Ok(true)
    } else {
        writeln!(out, "{}: cargo install failed", t.tool)?;
        Ok(false)
    }
}

/// Build the source-install arguments separately from process execution so the
/// package selection cannot regress unnoticed.
fn source_args(t: &StackTool) -> Vec<String> {
    let mut args = vec![
        "install".to_string(),
        "--git".to_string(),
        t.repo.to_string(),
        t.package.to_string(),
        "--branch".to_string(),
        "main".to_string(),
        "--locked".to_string(),
        "--force".to_string(),
    ];
    if !t.features.is_empty() {
        args.push("--features".to_string());
        args.push(t.features.join(","));
    }
    args
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
    } else {
        writeln!(
            out,
            "\ninstalled{where_at}, except: {}. Re-run to retry.",
            failed.join(", ")
        )?;
    }
    // The shared-bin PATH advice only applies to the release channel; a source
    // build lands in cargo's bin directory instead.
    if channel == Channel::Release {
        path_notice(out)?;
    } else {
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
    use super::{source_args, tool, TRAIN};

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
        let args = source_args(localmind);
        // `cargo install <package>` needs the package name, which is not the
        // binary name for localmind.
        assert!(args.contains(&"localmind-cli".to_string()));
        assert!(args.contains(&"--branch".to_string()));
        assert!(args.contains(&"main".to_string()));
    }

    #[test]
    fn localpilot_source_build_carries_its_features() {
        let localpilot = tool("localpilot").expect("localpilot in train");
        let args = source_args(localpilot);
        let features_idx = args
            .iter()
            .position(|a| a == "--features")
            .expect("features");
        assert_eq!(args[features_idx + 1], "tui,learning");
    }
}
