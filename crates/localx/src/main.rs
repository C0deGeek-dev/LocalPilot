//! `localx` — the LocalX stack's front door.
//!
//! One command to provision and refresh the whole stack: the release-train apps
//! (localpilot, localmind, localbox, localbench, and localx itself) and the
//! llama.cpp engine. The install machinery lives in `localpilot-stack`; this
//! binary is the thin CLI over it, plus a passthrough so `localx <tool>` runs a
//! stack tool without hunting for it on `PATH`.
#![forbid(unsafe_code)]

use std::io::Write;
use std::path::PathBuf;
use std::process::ExitCode;

use anyhow::Result;
use clap::{Parser, Subcommand};
use localpilot_dist::Version;
use localpilot_stack::{Channel, Running, Selection};

mod powershell;

/// This binary's version, embedded at build time.
const VERSION: &str = env!("LOCALX_VERSION");

#[derive(Parser)]
#[command(
    name = "localx",
    version = VERSION,
    about = "Provision and update the LocalX stack as one.",
    subcommand_required = true,
    arg_required_else_help = true
)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Update the whole stack to the newest release (or `--prerelease` to build
    /// the latest `main` from source).
    Update {
        /// Build each app from its repo's latest `main` commit instead of the
        /// newest published release. Needs a Rust toolchain; app tools only.
        #[arg(long)]
        prerelease: bool,
    },
    /// Install the stack, a single tool, or the engine.
    Install {
        /// `all` (default), `engine`, `powershell-shortcuts`, or a tool name
        /// (localpilot, localmind, localbox, localbench, localx).
        #[arg(default_value = "all")]
        target: String,
        /// Build from the latest `main` instead of the newest release (app tools
        /// only; needs a Rust toolchain).
        #[arg(long)]
        prerelease: bool,
        /// Install the optional PowerShell `llm*` shortcuts after the selected
        /// stack target. Existing custom profiles are never edited silently.
        #[arg(long)]
        powershell_shortcuts: bool,
    },
    /// Show the installed version of every stack tool and the engine.
    Status,
    /// Run a stack tool: `localx <tool> [args…]`.
    #[command(external_subcommand)]
    Tool(Vec<String>),
}

#[tokio::main]
async fn main() -> ExitCode {
    let cli = Cli::parse();

    // Passthrough forwards the child's own exit code, so it returns directly.
    if let Command::Tool(args) = &cli.command {
        return passthrough(args);
    }

    let mut out = std::io::stdout();
    let result = match cli.command {
        Command::Update { prerelease } => update(channel(prerelease), &mut out).await,
        Command::Install {
            target,
            prerelease,
            powershell_shortcuts,
        } => install(&target, channel(prerelease), powershell_shortcuts, &mut out).await,
        Command::Status => status(&mut out),
        Command::Tool(_) => unreachable!("handled above"),
    };

    match result {
        Ok(()) => ExitCode::SUCCESS,
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "error: {error:#}");
            ExitCode::FAILURE
        }
    }
}

fn channel(prerelease: bool) -> Channel {
    if prerelease {
        Channel::Prerelease
    } else {
        Channel::Release
    }
}

/// This binary as a [`Running`] marker. The marker is *always* produced: its
/// identity ("this process is localx") is what enables the self-replace route
/// and the refresh of the copy the shell resolves. The version is carried only
/// when the stamp parses, for the cache's strictly-newer rule and the sweep.
///
/// The version does not always parse. Any stamp that is not a semver — a bare
/// git sha from an older build or a tagless checkout, an operator-supplied
/// `LOCALX_VERSION`, a source archive — yields `None` here while the binary is
/// still this running executable. Gating the whole marker on `Version::parse` is
/// what made the self-replace unreachable on exactly the prerelease channel
/// (LocalHub#79), so identity never depends on the version. This crate's own
/// `build.rs` now keeps the stamp parseable as defence in depth, but the marker
/// must not lean on that — the running binary may have been built any other way.
fn running_marker(version: &str) -> Running {
    Running {
        tool: "localx",
        version: Version::parse(version),
    }
}

/// Update every app tool to the newest release (or latest `main`), then refresh
/// the engine. The full stack in one command.
async fn update(channel: Channel, out: &mut dyn Write) -> Result<()> {
    let marker = running_marker(VERSION);
    localpilot_stack::install(&Selection::All, None, channel, Some(&marker), out).await?;
    update_engine(out)?;
    powershell::refresh_if_installed(out)?;
    Ok(())
}

/// Install the whole stack, a single named tool, or just the engine.
async fn install(
    target: &str,
    channel: Channel,
    powershell_shortcuts: bool,
    out: &mut dyn Write,
) -> Result<()> {
    if target == "powershell-shortcuts" {
        return powershell::install(out);
    }
    match target {
        "all" => {
            let marker = running_marker(VERSION);
            localpilot_stack::install(&Selection::All, None, channel, Some(&marker), out).await?;
            update_engine(out)?;
        }
        "engine" => update_engine(out)?,
        name => {
            let Some(tool) = localpilot_stack::tool(name) else {
                anyhow::bail!(
                    "unknown target {name:?}: expected `all`, `engine`, \
                     `powershell-shortcuts`, or one of localpilot, localmind, \
                     localbox, localbench, localx"
                );
            };
            let marker = running_marker(VERSION);
            localpilot_stack::install(&Selection::One(tool), None, channel, Some(&marker), out)
                .await?;
        }
    }
    if powershell_shortcuts {
        powershell::install(out)?;
    } else {
        powershell::refresh_if_installed(out)?;
    }
    Ok(())
}

/// Refresh the llama.cpp engine by delegating to the installed `localbox`.
///
/// The engine is upstream binaries owned by LocalBox, not part of the app source
/// tree, so the `--prerelease` (build-from-main) channel does not apply — the
/// engine always refreshes its release assets. A failure here is reported but
/// does not fail the whole run: the app stack is already installed.
fn update_engine(out: &mut dyn Write) -> Result<()> {
    let localbox = tool_path("localbox");
    writeln!(out, "\nengine: refreshing llama.cpp via `localbox update`…")?;
    match std::process::Command::new(&localbox).arg("update").status() {
        Ok(status) if status.success() => Ok(()),
        Ok(_) => {
            writeln!(
                out,
                "engine: `localbox update` reported a problem; the app stack is installed."
            )?;
            Ok(())
        }
        Err(error) => {
            writeln!(
                out,
                "engine: could not run localbox ({error}). Install the stack first, \
                 then run `localx install engine`."
            )?;
            Ok(())
        }
    }
}

/// Show the installed version of every stack tool and the engine.
fn status(out: &mut dyn Write) -> Result<()> {
    writeln!(out, "LocalX stack (localx {VERSION}):")?;
    for tool in localpilot_stack::TRAIN {
        // This binary's own row reports the version that is actually running —
        // known exactly from its stamp — and flags a managed copy that differs,
        // rather than printing the cache's newest as if it were the one in use.
        let shown = if tool.tool == "localx" {
            running_localx_version()
        } else {
            tool_version(tool.tool)
        };
        writeln!(out, "  {:<11} {}", tool.tool, shown)?;
    }
    writeln!(out, "  {:<11} {}", "engine", engine_version())?;

    if let Some(bin) = localpilot_stack::shared_bin_dir() {
        let on_path = std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == bin));
        if !on_path {
            writeln!(
                out,
                "
note: {} is not on PATH.",
                bin.display()
            )?;
        }
    }
    if let Some(note) = localpilot_stack::running_binary_note("localx") {
        writeln!(out, "\n{note}")?;
    }
    Ok(())
}

/// The running `localx` version, plus the managed copy's version when the two
/// disagree — the case where the shell resolves a stale bootstrap copy. The
/// managed copy is *probed*, not read from the cache: the cache's newest entry
/// may be pinned away, stale after a failed activation, or not what sits at the
/// managed path at all.
fn running_localx_version() -> String {
    describe_running_localx(VERSION, managed_localx_version().as_deref())
}

/// The version the managed copy of `localx` reports for itself, when there is
/// one and it is not the running executable. `unknown build` when it runs but
/// prints no version-shaped token.
fn managed_localx_version() -> Option<String> {
    let managed = localpilot_stack::shadowed_managed_copy("localx")?;
    let output = std::process::Command::new(&managed)
        .arg("--version")
        .output()
        .ok()?;
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(version_token(&text).unwrap_or_else(|| "unknown build".to_string()))
}

/// The first version-shaped token in a `--version` line, without a leading `v`.
fn version_token(text: &str) -> Option<String> {
    text.split_whitespace()
        .find(|token| Version::parse(token).is_some())
        .map(|token| token.trim_start_matches('v').to_string())
}

/// The status row text for this binary: its own stamp, and the managed copy's
/// version when that copy is a different build.
fn describe_running_localx(running: &str, managed: Option<&str>) -> String {
    match managed {
        Some(managed) if managed.trim_start_matches('v') != running.trim_start_matches('v') => {
            format!("{running} (running; managed copy is {managed})")
        }
        _ => format!("{running} (running)"),
    }
}

/// What a user would actually get if they typed `<tool>`, and a flag when that
/// is not the managed copy.
///
/// This used to prefer the release cache's newest entry, because that is a clean
/// version string with no per-tool `--version` format to parse. It was
/// convenient and it was not true: the cache records what was *downloaded*,
/// which says nothing about what the shell resolves. Measured on a real machine,
/// `localx status` reported `localpilot 3.3.2` from the cache while `PATH`
/// resolved a different binary stamped `5d1e23b` — a status command answering a
/// question nobody asked.
///
/// So the version is probed from the binary `PATH` actually resolves, and when a
/// different copy shadows the managed one, both are shown. Each tool formats its
/// version line differently (localbox answers `--version` with JSON), hence
/// pulling the first version-shaped token rather than parsing a fixed shape.
fn tool_version(tool: &str) -> String {
    match localpilot_stack::shadowed_on_path(tool) {
        Some((resolved, managed)) => {
            let running = probe_version(&resolved);
            let managed_version = probe_version(&managed);
            format!(
                "{running} (from {}) — managed copy is {managed_version}",
                resolved.display()
            )
        }
        None => probe_version(&tool_path(tool)),
    }
}

/// The version a binary reports for itself.
///
/// A semver token if there is one. Failing that, **whatever it did say** — a
/// bare commit sha from an older stamping format is exactly the signal someone
/// reading this line needs, and swallowing it as "installed" hides the one fact
/// that explains why two copies disagree.
///
/// `missing` when the file is not there; `will not run` when it is there and
/// fails, which is a different problem from not being installed and should not
/// wear the same words.
fn probe_version(path: &std::path::Path) -> String {
    if !path.is_file()
        && path
            .parent()
            .is_some_and(|parent| !parent.as_os_str().is_empty())
    {
        return "missing".to_string();
    }
    // Tools differ in how they are *asked*, not only in what they answer:
    // localbench takes a `version` subcommand and rejects `--version` outright.
    // Trying both is the difference between reporting a version and reporting a
    // tool as broken because it was asked the wrong question.
    let text = ["--version", "version"]
        .into_iter()
        .find_map(|arg| {
            let output = std::process::Command::new(path).arg(arg).output().ok()?;
            output
                .status
                .success()
                .then(|| String::from_utf8_lossy(&output.stdout).into_owned())
        })
        .unwrap_or_default();
    let name = path.file_stem().unwrap_or_default().to_string_lossy();
    version_from_output(&text, &name)
}

/// Read a version out of whatever a tool printed.
///
/// Pure, so the interesting cases can be tested without shelling out — and they
/// are interesting: a semver token, a JSON envelope (localbox), a bare commit
/// sha from an older stamping format, and nothing at all.
fn version_from_output(text: &str, tool_name: &str) -> String {
    if text.trim().is_empty() {
        return "will not run".to_string();
    }
    if let Some(token) = text
        .split_whitespace()
        .find(|token| Version::parse(token).is_some())
    {
        return token.trim_start_matches('v').to_string();
    }
    // Some tools answer with JSON; pull the field rather than reporting the
    // opening brace.
    if let Some((_, rest)) = text.split_once("\"version\"") {
        if let Some(value) = rest.split('"').nth(1).filter(|v| !v.trim().is_empty()) {
            return value.trim().to_string();
        }
    }
    // Whatever it printed, minus the program name it leads with. A bare sha from
    // an older stamping format is exactly the signal a reader needs; swallowing
    // it as "installed" hides the one fact that explains why two copies
    // disagree, which is the whole question `status` is being asked.
    text.split_whitespace()
        .find(|token| !token.trim_end_matches(':').eq_ignore_ascii_case(tool_name))
        .map_or_else(
            || "unrecognised build".to_string(),
            |token| format!("{token} (unstamped build)"),
        )
}

/// The engine's state. LocalBox owns the llama.cpp binaries and their version;
/// here we only confirm the delegate is reachable, then point at the command
/// that refreshes them. A precise engine tag would need an offline stamp seam in
/// localbox, which is not worth making `localbox --version` fragile for.
fn engine_version() -> String {
    let path = tool_path("localbox");
    match std::process::Command::new(&path).arg("--version").output() {
        Ok(output) if output.status.success() => {
            "llama.cpp managed by localbox (refresh: `localx install engine`)".to_string()
        }
        _ => "not installed (needs localbox)".to_string(),
    }
}

/// The path to a stack tool: the shared-bin copy when it exists, else the bare
/// name so `PATH` (including a from-source `cargo` install) resolves it.
fn tool_path(tool: &str) -> PathBuf {
    if let Some(bin) = localpilot_stack::shared_bin_dir() {
        let candidate = bin.join(localpilot_dist::executable_name(tool));
        if candidate.exists() {
            return candidate;
        }
    }
    PathBuf::from(localpilot_dist::executable_name(tool))
}

/// `localx <tool> [args…]` — run a stack tool, forwarding its exit code.
fn passthrough(args: &[String]) -> ExitCode {
    let Some((tool, rest)) = args.split_first() else {
        let _ = writeln!(std::io::stderr(), "usage: localx <tool> [args…]");
        return ExitCode::FAILURE;
    };
    if tool == "localx" {
        let _ = writeln!(std::io::stderr(), "localx cannot pass through to itself");
        return ExitCode::FAILURE;
    }
    if localpilot_stack::tool(tool).is_none() {
        let _ = writeln!(
            std::io::stderr(),
            "unknown tool {tool:?}: expected one of localpilot, localmind, localbox, localbench"
        );
        return ExitCode::FAILURE;
    }
    let path = tool_path(tool);
    match std::process::Command::new(&path).args(rest).status() {
        // Forward the child's exit code; a signal death has none, so use 1.
        Ok(status) => ExitCode::from(u8::try_from(status.code().unwrap_or(1)).unwrap_or(1)),
        Err(error) => {
            let _ = writeln!(std::io::stderr(), "could not run {tool}: {error}");
            ExitCode::FAILURE
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        channel, describe_running_localx, version_from_output, version_token, Channel, Cli, Command,
    };
    use clap::{CommandFactory, Parser};

    #[test]
    fn a_semver_token_wins_wherever_it_sits_in_the_line() {
        assert_eq!(
            version_from_output("localpilot v3.3.2", "localpilot"),
            "3.3.2"
        );
        assert_eq!(version_from_output("localmind 3.3.2", "localmind"), "3.3.2");
    }

    #[test]
    fn a_json_envelope_reports_its_field_not_its_opening_brace() {
        // localbox answers `--version` with JSON.
        assert_eq!(
            version_from_output(
                "{\n  \"version\": \"3.3.2\",\n  \"engine\": \"x\"\n}",
                "localbox"
            ),
            "3.3.2"
        );
    }

    #[test]
    fn a_bare_sha_is_shown_rather_than_swallowed() {
        // The case that made this worth fixing. A binary stamped by an older
        // build prints no semver; reporting it as "installed" hid the one fact
        // that explains why two copies disagree.
        assert_eq!(
            version_from_output("localpilot 5d1e23b", "localpilot"),
            "5d1e23b (unstamped build)"
        );
    }

    #[test]
    fn nothing_printed_is_a_tool_that_will_not_run() {
        // Distinct from "missing": the file is there and fails, which is a
        // different problem and should not wear the same words.
        assert_eq!(version_from_output("", "localbench"), "will not run");
        assert_eq!(version_from_output("   \n", "localbench"), "will not run");
    }

    #[test]
    fn status_flags_a_managed_copy_whose_probed_version_differs() {
        assert_eq!(
            describe_running_localx("3.1.0", Some("3.2.0")),
            "3.1.0 (running; managed copy is 3.2.0)"
        );
        assert_eq!(
            describe_running_localx("v3.1.0", Some("3.1.0")),
            "v3.1.0 (running)"
        );
        assert_eq!(describe_running_localx("3.1.0", None), "3.1.0 (running)");
        assert_eq!(
            describe_running_localx("3.1.0", Some("unknown build")),
            "3.1.0 (running; managed copy is unknown build)"
        );
    }

    #[test]
    fn the_managed_version_is_the_probed_token_not_a_cache_entry() {
        assert_eq!(version_token("localx 3.1.0").as_deref(), Some("3.1.0"));
        assert_eq!(
            version_token("localx v3.2.0-rc.1").as_deref(),
            Some("3.2.0-rc.1")
        );
        assert_eq!(version_token("localx bc388f9"), None);
    }

    #[test]
    fn the_running_marker_is_produced_even_for_a_non_semver_stamp() {
        // A prerelease build stamped with a bare git sha (a tagless cargo
        // checkout) still yields a marker — identity is certain — with no
        // version. This is the production entry point the stack-crate tests
        // bypass by hand-constructing a marker; gating it on the stamp is what
        // made the self-replace unreachable (LocalHub#79).
        let sha = super::running_marker("fa98397");
        assert_eq!(sha.tool, "localx");
        assert!(sha.version.is_none());

        // A normal semver stamp carries its version through for the cache rules.
        let tagged = super::running_marker("3.3.1");
        assert_eq!(tagged.tool, "localx");
        assert!(tagged.version.is_some());
    }

    #[test]
    fn cli_definition_is_valid() {
        Cli::command().debug_assert();
    }

    #[test]
    fn prerelease_flag_selects_the_source_channel() {
        assert_eq!(channel(true), Channel::Prerelease);
        assert_eq!(channel(false), Channel::Release);
    }

    #[test]
    fn prerelease_parses_on_update_and_install() {
        let cli = Cli::try_parse_from(["localx", "update", "--prerelease"]).unwrap();
        assert!(matches!(cli.command, Command::Update { prerelease: true }));

        let cli = Cli::try_parse_from(["localx", "install", "localbox", "--prerelease"]).unwrap();
        assert!(matches!(
            cli.command,
            Command::Install {
                prerelease: true,
                ..
            }
        ));
    }

    #[test]
    fn install_defaults_to_all() {
        let cli = Cli::try_parse_from(["localx", "install"]).unwrap();
        match cli.command {
            Command::Install { target, .. } => assert_eq!(target, "all"),
            _ => panic!("expected install"),
        }
    }

    #[test]
    fn powershell_shortcuts_parse_as_a_target_or_install_option() {
        let target = Cli::try_parse_from(["localx", "install", "powershell-shortcuts"]).unwrap();
        assert!(matches!(
            target.command,
            Command::Install {
                ref target,
                powershell_shortcuts: false,
                ..
            } if target == "powershell-shortcuts"
        ));

        let option = Cli::try_parse_from(["localx", "install", "--powershell-shortcuts"]).unwrap();
        assert!(matches!(
            option.command,
            Command::Install {
                ref target,
                powershell_shortcuts: true,
                ..
            } if target == "all"
        ));
    }

    #[test]
    fn an_unknown_subcommand_becomes_a_passthrough() {
        // `localx <tool> …` is not a named subcommand; clap routes it to Tool.
        let cli = Cli::try_parse_from(["localx", "localpilot", "version", "list"]).unwrap();
        match cli.command {
            Command::Tool(args) => assert_eq!(args, ["localpilot", "version", "list"]),
            _ => panic!("expected passthrough"),
        }
    }
}
