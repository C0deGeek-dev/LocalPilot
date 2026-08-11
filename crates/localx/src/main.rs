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
        /// `all` (default), `engine`, or a tool name (localpilot, localmind,
        /// localbox, localbench, localx).
        #[arg(default_value = "all")]
        target: String,
        /// Build from the latest `main` instead of the newest release (app tools
        /// only; needs a Rust toolchain).
        #[arg(long)]
        prerelease: bool,
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
        Command::Install { target, prerelease } => {
            install(&target, channel(prerelease), &mut out).await
        }
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

/// This binary as a [`Running`] marker, so its own cache gets the strictly-newer
/// activation rule and its running version is protected from the sweep.
fn running() -> Option<Running> {
    Version::parse(VERSION).map(|version| Running {
        tool: "localx",
        version,
    })
}

/// Update every app tool to the newest release (or latest `main`), then refresh
/// the engine. The full stack in one command.
async fn update(channel: Channel, out: &mut dyn Write) -> Result<()> {
    let marker = running();
    localpilot_stack::install(&Selection::All, None, channel, marker.as_ref(), out).await?;
    update_engine(out)?;
    Ok(())
}

/// Install the whole stack, a single named tool, or just the engine.
async fn install(target: &str, channel: Channel, out: &mut dyn Write) -> Result<()> {
    match target {
        "all" => {
            let marker = running();
            localpilot_stack::install(&Selection::All, None, channel, marker.as_ref(), out).await?;
            update_engine(out)?;
        }
        "engine" => update_engine(out)?,
        name => {
            let Some(tool) = localpilot_stack::tool(name) else {
                anyhow::bail!(
                    "unknown target {name:?}: expected `all`, `engine`, or one of \
                     localpilot, localmind, localbox, localbench, localx"
                );
            };
            let marker = running();
            localpilot_stack::install(&Selection::One(tool), None, channel, marker.as_ref(), out)
                .await?;
        }
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
        writeln!(out, "  {:<11} {}", tool.tool, tool_version(tool.tool))?;
    }
    writeln!(out, "  {:<11} {}", "engine", engine_version())?;

    if let Some(bin) = localpilot_stack::shared_bin_dir() {
        let on_path = std::env::var_os("PATH")
            .is_some_and(|path| std::env::split_paths(&path).any(|entry| entry == bin));
        if !on_path {
            writeln!(out, "\nnote: {} is not on PATH.", bin.display())?;
        }
    }
    Ok(())
}

/// The installed version of a tool. Prefers the release cache (a clean version
/// with no per-tool `--version` format to parse); falls back to probing the
/// binary on `PATH` for a from-source install, pulling the first version-shaped
/// token out of its output since each tool formats that line differently (e.g.
/// localbox answers `--version` with JSON).
fn tool_version(tool: &str) -> String {
    if let Some(cache) = localpilot_dist::Cache::default_root(tool).map(localpilot_dist::Cache::new)
    {
        if let Some(newest) = cache.newest() {
            return newest.version.to_dir_name();
        }
    }
    let path = tool_path(tool);
    match std::process::Command::new(&path).arg("--version").output() {
        Ok(output) if output.status.success() => {
            let text = String::from_utf8_lossy(&output.stdout);
            text.split_whitespace()
                .find(|token| Version::parse(token).is_some())
                .map(|token| format!("{} (source)", token.trim_start_matches('v')))
                .unwrap_or_else(|| "installed".to_string())
        }
        _ => "not installed".to_string(),
    }
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
    use super::{channel, Channel, Cli, Command};
    use clap::{CommandFactory, Parser};

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
    fn an_unknown_subcommand_becomes_a_passthrough() {
        // `localx <tool> …` is not a named subcommand; clap routes it to Tool.
        let cli = Cli::try_parse_from(["localx", "localpilot", "version", "list"]).unwrap();
        match cli.command {
            Command::Tool(args) => assert_eq!(args, ["localpilot", "version", "list"]),
            _ => panic!("expected passthrough"),
        }
    }
}
