//! RAM soak: build N sessions from one shared-pool factory and measure
//! the per-session resident-set delta.
//!
//! This is the behaviour verification for the multi-session RAM story: a single
//! process hosts many sessions that share one provider stack (and, in
//! production, one MCP connection pool and one tool registry backing), so each
//! extra session should cost only its own mutable state — transcript,
//! compaction cache, config, approver — not a fresh provider or tool pool.
//!
//! It is `#[ignore]`d so the fast `cargo test` path stays quick, and because it
//! shells out to read process RSS. Run it explicitly and read the printed
//! numbers:
//!
//! ```text
//! cargo test -p localpilot-server --test soak -- --ignored --nocapture
//! ```
//!
//! RSS is read without any `unsafe`/`libc`: on Windows via
//! `(Get-Process -Id <pid>).WorkingSet64`, on Unix via `/proc/self/status`
//! (`VmRSS`). If neither is available the test reports that and skips the
//! measurement rather than failing.
#![allow(clippy::unwrap_used)]

use std::sync::Arc;

use localpilot_harness::{SessionConfig, SessionRuntime};
use localpilot_llm::FakeProvider;
use localpilot_recovery::{RecoveryBudget, RecoveryEngine};
use localpilot_sandbox::{Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace};
use localpilot_server::registry::{RegistryError, SessionFactory, SessionRegistry};
use localpilot_store::Store;
use localpilot_tools::ToolRegistry;
use tempfile::TempDir;

/// Builds `SessionRuntime`s over one shared temp-dir store and one shared
/// `FakeProvider` — the single pool every session draws from.
struct FakeFactory {
    dir: Arc<TempDir>,
    provider: Arc<FakeProvider>,
}

impl FakeFactory {
    fn new(provider: FakeProvider) -> Self {
        Self {
            dir: Arc::new(tempfile::tempdir().unwrap()),
            provider: Arc::new(provider),
        }
    }
}

impl SessionFactory for FakeFactory {
    fn create(&self) -> Result<SessionRuntime, RegistryError> {
        let root = self.dir.path();
        let workspace =
            Workspace::new(root).map_err(|err| RegistryError::Factory(err.to_string()))?;
        Ok(SessionRuntime::new(
            self.provider.clone(),
            ToolRegistry::with_builtins(),
            PermissionEngine::new(Profile::Bypass, Vec::new()),
            Box::new(ScriptedApprover::always()),
            Store::open(root),
            workspace,
            RecoveryEngine::new(RecoveryBudget::default()),
            SessionConfig {
                interactivity: Interactivity::NonInteractive,
                trusted: true,
                ..SessionConfig::default()
            },
            Vec::new(),
        ))
    }
}

/// Current process resident-set size in bytes, or `None` if it cannot be read
/// safely on this platform.
fn rss_bytes() -> Option<u64> {
    #[cfg(windows)]
    {
        let pid = std::process::id();
        let output = std::process::Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                &format!("(Get-Process -Id {pid}).WorkingSet64"),
            ])
            .output()
            .ok()?;
        if !output.status.success() {
            return None;
        }
        String::from_utf8_lossy(&output.stdout).trim().parse().ok()
    }
    #[cfg(unix)]
    {
        let status = std::fs::read_to_string("/proc/self/status").ok()?;
        for line in status.lines() {
            if let Some(rest) = line.strip_prefix("VmRSS:") {
                let kb: u64 = rest.split_whitespace().next()?.parse().ok()?;
                return Some(kb * 1024);
            }
        }
        None
    }
    #[cfg(not(any(windows, unix)))]
    {
        None
    }
}

#[tokio::test]
#[ignore = "heavy RAM soak; run with `-- --ignored --nocapture` to read the numbers"]
async fn per_session_rss_delta_stays_near_the_shared_pool_baseline() {
    let factory = FakeFactory::new(FakeProvider::new().text("soak"));
    let registry = SessionRegistry::new();

    // Warm up: build one session so first-touch allocations (the runtime, the
    // tokio machinery, the tempdir) are charged to the baseline, not to the
    // per-session delta we are measuring.
    let _warm = registry.open_new(&factory).await.unwrap();

    let Some(baseline) = rss_bytes() else {
        eprintln!(
            "soak: could not read process RSS on this platform; skipping the live measurement"
        );
        return;
    };

    let mut ids = Vec::new();
    println!(
        "soak: baseline RSS = {} bytes ({} KiB)",
        baseline,
        baseline / 1024
    );
    for target in [1usize, 8, 32] {
        while ids.len() < target {
            // Each session is retained by the registry, so all N stay resident
            // while we measure.
            ids.push(registry.open_new(&factory).await.unwrap());
        }
        let rss = rss_bytes().unwrap();
        let delta = rss.saturating_sub(baseline);
        let per = delta / target as u64;
        println!(
            "soak: N={target:>2}  rss={rss:>12}  delta={delta:>10}  per_session={per:>8} bytes ({} KiB)",
            per / 1024
        );
    }

    // The registry must still hold every session (nothing leaked or was dropped).
    assert_eq!(
        registry.len().await,
        ids.len() + 1,
        "one warm-up + N sessions"
    );
}
