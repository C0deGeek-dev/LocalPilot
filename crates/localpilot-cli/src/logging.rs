//! CLI logging setup.
//!
//! Two modes, chosen by the environment:
//!
//! * Default — no `LOCALPILOT_LOG` set: logging follows `RUST_LOG` and writes to
//!   the terminal, matching the historical `tracing_subscriber::fmt::init`
//!   behaviour. This is left untouched so non-interactive commands keep their
//!   stderr diagnostics.
//! * File — `LOCALPILOT_LOG` set to a filter (e.g. `debug`): logs are written to
//!   a per-run file under `<cwd>/.localpilot/logs/` and nothing is emitted to the
//!   terminal. The interactive `chat` TUI owns stdout, so terminal logging would
//!   corrupt its display; routing to a file keeps the screen clean and lets the
//!   user `tail -f` the log from another terminal.
//!
//! A bare level (no `=` target) is scoped so that the noisy transport crates
//! (`hyper`, `reqwest`, ...) stay at `info`. Those crates log request headers at
//! debug/trace, and a request header carries the API key — pinning them keeps the
//! credential out of the log file. A value that already contains target
//! directives is passed through verbatim for power users.

use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::{SystemTime, UNIX_EPOCH};

use tracing_subscriber::EnvFilter;

/// Environment variable that enables file logging and supplies the filter.
const ENV_VAR: &str = "LOCALPILOT_LOG";

/// Transport crates pinned to `info` so they cannot emit request headers (which
/// carry the API key) when a bare debug/trace level is requested.
const TRANSPORT_DIRECTIVES: &str = "hyper=info,hyper_util=info,h2=info,reqwest=info,rustls=info";

/// Initialise logging. Returns the path of the active log file when file logging
/// is enabled, so the caller can tell the user where to look; returns `None` in
/// the default terminal mode.
pub fn init(cwd: &Path) -> Option<PathBuf> {
    match std::env::var(ENV_VAR) {
        Ok(value) if !value.trim().is_empty() => init_file(cwd, value.trim()),
        _ => {
            // Historical behaviour: terminal logging driven by `RUST_LOG`.
            let _ = tracing_subscriber::fmt().try_init();
            None
        }
    }
}

fn init_file(cwd: &Path, value: &str) -> Option<PathBuf> {
    let dir = cwd.join(".localpilot").join("logs");
    std::fs::create_dir_all(&dir).ok()?;
    let path = dir.join(log_file_name(unix_stamp()));
    let file = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&path)
        .ok()?;
    let writer = Mutex::new(file);
    let installed = tracing_subscriber::fmt()
        .with_env_filter(EnvFilter::new(scoped_filter(value)))
        .with_ansi(false)
        .with_target(true)
        .with_writer(writer)
        .try_init()
        .is_ok();
    installed.then_some(path)
}

/// Build the env-filter directive string for a `LOCALPILOT_LOG` value.
fn scoped_filter(value: &str) -> String {
    // A value with an explicit target (`crate=level`) is the power-user path:
    // honour it exactly.
    if value.contains('=') {
        value.to_string()
    } else {
        format!("{value},{TRANSPORT_DIRECTIVES}")
    }
}

/// Per-run log file name. Seconds-resolution is enough to separate runs without
/// pulling in a date/time dependency.
fn log_file_name(stamp: u64) -> String {
    format!("localpilot-{stamp}.log")
}

fn unix_stamp() -> u64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs())
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn bare_level_is_scoped_to_keep_transport_quiet() {
        let filter = scoped_filter("debug");
        assert!(filter.starts_with("debug,"));
        // The API key rides in a request header; transport crates must not log
        // headers at debug/trace.
        assert!(filter.contains("hyper=info"));
        assert!(filter.contains("reqwest=info"));
    }

    #[test]
    fn explicit_targets_pass_through_unchanged() {
        let value = "localpilot_llm=trace,localpilot_harness=debug";
        assert_eq!(scoped_filter(value), value);
    }

    #[test]
    fn log_file_name_is_unique_per_stamp_and_ends_in_log() {
        assert_eq!(log_file_name(42), "localpilot-42.log");
        assert_ne!(log_file_name(1), log_file_name(2));
        assert!(log_file_name(unix_stamp()).ends_with(".log"));
    }
}
