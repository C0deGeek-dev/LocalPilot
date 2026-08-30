//! LocalPilot-owned embedding server lifecycle over LocalMind's neutral leases.
//!
//! LocalPilot is the only product in this relationship that starts or stops
//! LocalBox. The shared registry supplies exact endpoint/PID ownership,
//! machine-global locking, stale-client pruning, and unique RAII leases. If a
//! standalone LocalMind client outlives this process, a detached LocalPilot
//! reaper waits for that lease to drain and then receives the stop token.

use localmind_inference::embedding_lease::{
    endpoints_match, EmbeddingLease, EmbeddingLeaseRegistry, OwnerPreparation, ReleaseOutcome,
    StopPreparation,
};
use serde::Deserialize;
use std::path::PathBuf;
use std::process::{Command, Stdio};
use std::time::Duration;

const OWNER: &str = "localpilot";
const REAPER_POLL: Duration = Duration::from_millis(250);

/// One LocalPilot process's ownership-side lease. Drop is the single release
/// path for ordinary return, error propagation, cancellation, and unwinding.
pub struct OwnerEmbeddingLease {
    _inner: LifecycleLease<LocalBoxEffects>,
}

struct LifecycleLease<E: EmbedEffects> {
    lease: Option<EmbeddingLease>,
    endpoint: String,
    server_pid: u32,
    effects: E,
}

impl<E: EmbedEffects> Drop for LifecycleLease<E> {
    fn drop(&mut self) {
        let Some(lease) = self.lease.take() else {
            return;
        };
        match lease.release() {
            Ok(ReleaseOutcome::StopOwned(token)) => {
                let stopped = self.effects.stop();
                let _ = token.complete(stopped);
                if stopped {
                    eprintln!("embeddings: stopped the embedding server this run started");
                }
            }
            Ok(ReleaseOutcome::OthersRemain) => {
                if let Err(error) = self.effects.defer_stop(&self.endpoint, self.server_pid) {
                    eprintln!(
                        "embeddings: could not start the lease-drain reaper ({error}); \
                         the owned server will be reconciled by the next LocalPilot run"
                    );
                }
            }
            Ok(ReleaseOutcome::Unowned) | Err(_) => {}
        }
    }
}

/// Acquire or start the configured embedding endpoint for this run.
#[must_use]
pub fn ensure_for_run() -> Option<OwnerEmbeddingLease> {
    let cwd = std::env::current_dir().ok()?;
    let base_url = localpilot_localmind::configured_embedding_endpoint(&cwd)?;
    ensure_for_process(&base_url)
}

/// Owner-side lifecycle entry point. A reachable user-managed endpoint remains
/// untouched and receives no lease. Ordinary failures preserve lexical recall.
#[must_use]
pub fn ensure_for_process(base_url: &str) -> Option<OwnerEmbeddingLease> {
    let registry = EmbeddingLeaseRegistry::machine_default()?;
    let effects = LocalBoxEffects::resolve()?;
    ensure_with(&registry, base_url, effects).map(|inner| OwnerEmbeddingLease { _inner: inner })
}

fn ensure_with<E: EmbedEffects>(
    registry: &EmbeddingLeaseRegistry,
    base_url: &str,
    effects: E,
) -> Option<LifecycleLease<E>> {
    match registry.prepare_owner(base_url) {
        Ok(OwnerPreparation::Acquired(lease)) => {
            let state = effects.runtime_state()?;
            guard_for(lease, base_url, state, effects)
        }
        Ok(OwnerPreparation::Legacy(permit)) => {
            let state = effects.runtime_state()?;
            if !state.matches(base_url) {
                return None;
            }
            match permit.migrate(OWNER, state.pid) {
                Ok(lease) => guard_for(lease, base_url, state, effects),
                Err(_) => None,
            }
        }
        Ok(OwnerPreparation::Start(permit)) => match effects.start() {
            Ok(()) => {
                let Some(state) = effects
                    .runtime_state()
                    .filter(|state| state.matches(base_url))
                else {
                    let _ = effects.stop();
                    eprintln!(
                        "embeddings: LocalBox started without verifiable endpoint/PID state; \
                         it was stopped and retrieval stays lexical"
                    );
                    return None;
                };
                match permit.claim(OWNER, state.pid) {
                    Ok(lease) => {
                        eprintln!("embeddings: started the embedding server for this run");
                        guard_for(lease, base_url, state, effects)
                    }
                    Err(error) => {
                        let _ = effects.stop();
                        eprintln!(
                            "embeddings: could not register the started server ({error}); \
                             it was stopped and retrieval stays lexical"
                        );
                        None
                    }
                }
            }
            Err(error) => {
                eprintln!(
                    "embeddings: could not start the embedding server ({error}); \
                     retrieval stays lexical"
                );
                None
            }
        },
        Ok(OwnerPreparation::UserManaged) => None,
        Ok(OwnerPreparation::Stopping) => {
            eprintln!(
                "embeddings: the owned embedding server is stopping; retrieval stays lexical"
            );
            None
        }
        Err(error) => {
            eprintln!("embeddings: lifecycle state unavailable ({error}); retrieval stays lexical");
            None
        }
    }
}

fn guard_for<E: EmbedEffects>(
    lease: EmbeddingLease,
    endpoint: &str,
    state: EmbedRuntimeState,
    effects: E,
) -> Option<LifecycleLease<E>> {
    if !state.matches(endpoint) {
        return None;
    }
    Some(LifecycleLease {
        lease: Some(lease),
        endpoint: endpoint.to_string(),
        server_pid: state.pid,
        effects,
    })
}

/// Hidden detached-owner entry point. It has no session and emits nothing on
/// normal operation. The canonical registry decides whether this exact owner is
/// still valid and when all live client leases have drained.
pub fn reap(endpoint: &str, server_pid: u32) {
    let Some(registry) = EmbeddingLeaseRegistry::machine_default() else {
        return;
    };
    let Some(effects) = LocalBoxEffects::resolve() else {
        return;
    };
    loop {
        match registry.prepare_stop_when_unleased(endpoint, OWNER, server_pid) {
            Ok(StopPreparation::Waiting) => std::thread::sleep(REAPER_POLL),
            Ok(StopPreparation::Ready(token)) => {
                let stopped = effects.stop();
                let _ = token.complete(stopped);
                return;
            }
            Ok(StopPreparation::Unowned | StopPreparation::Stopping) | Err(_) => return,
        }
    }
}

fn spawn_reaper(endpoint: &str, server_pid: u32) -> Result<(), std::io::Error> {
    let executable = std::env::current_exe()?;
    let mut command = Command::new(executable);
    command
        .arg("__embed-reap")
        .arg("--endpoint")
        .arg(endpoint)
        .arg("--server-pid")
        .arg(server_pid.to_string())
        .stdin(Stdio::null())
        .stdout(Stdio::null())
        .stderr(Stdio::null());
    configure_detached(&mut command);
    command.spawn().map(|_| ())
}

#[cfg(unix)]
fn configure_detached(command: &mut Command) {
    use std::os::unix::process::CommandExt;
    command.process_group(0);
}

#[cfg(windows)]
fn configure_detached(command: &mut Command) {
    use std::os::windows::process::CommandExt;
    const CREATE_NEW_PROCESS_GROUP: u32 = 0x0000_0200;
    const DETACHED_PROCESS: u32 = 0x0000_0008;
    const CREATE_NO_WINDOW: u32 = 0x0800_0000;
    command.creation_flags(CREATE_NEW_PROCESS_GROUP | DETACHED_PROCESS | CREATE_NO_WINDOW);
}

#[derive(Clone)]
struct LocalBoxEffects {
    executable: PathBuf,
    home: PathBuf,
}

impl LocalBoxEffects {
    fn resolve() -> Option<Self> {
        Some(Self {
            executable: localbox(),
            home: user_home()?,
        })
    }
}

trait EmbedEffects: Clone {
    fn start(&self) -> Result<(), String>;
    fn stop(&self) -> bool;
    fn runtime_state(&self) -> Option<EmbedRuntimeState>;
    fn defer_stop(&self, endpoint: &str, server_pid: u32) -> Result<(), String>;
}

impl EmbedEffects for LocalBoxEffects {
    fn start(&self) -> Result<(), String> {
        match Command::new(&self.executable).arg("embed-serve").output() {
            Ok(output) if output.status.success() => Ok(()),
            Ok(output) => Err(String::from_utf8_lossy(&output.stderr).trim().to_string()),
            Err(error) => Err(error.to_string()),
        }
    }

    fn stop(&self) -> bool {
        Command::new(&self.executable)
            .arg("embed-stop")
            .output()
            .is_ok_and(|output| output.status.success())
    }

    fn runtime_state(&self) -> Option<EmbedRuntimeState> {
        let raw =
            std::fs::read_to_string(self.home.join(".local-llm").join("embed-server.json")).ok()?;
        serde_json::from_str(raw.trim_start_matches('\u{feff}')).ok()
    }

    fn defer_stop(&self, endpoint: &str, server_pid: u32) -> Result<(), String> {
        spawn_reaper(endpoint, server_pid).map_err(|error| error.to_string())
    }
}

#[derive(Clone, Debug, Deserialize, Eq, PartialEq)]
#[serde(rename_all = "PascalCase")]
struct EmbedRuntimeState {
    pid: u32,
    base_url: String,
}

impl EmbedRuntimeState {
    fn matches(&self, configured_endpoint: &str) -> bool {
        self.pid > 0 && endpoints_match(&self.base_url, configured_endpoint).is_ok_and(|same| same)
    }
}

fn user_home() -> Option<PathBuf> {
    std::env::var_os("USERPROFILE")
        .or_else(|| std::env::var_os("HOME"))
        .map(PathBuf::from)
}

fn localbox() -> PathBuf {
    if let Some(bin) = localpilot_stack::shared_bin_dir() {
        let candidate = bin.join(localpilot_dist::executable_name("localbox"));
        if candidate.is_file() {
            return candidate;
        }
    }
    PathBuf::from("localbox")
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use localmind_inference::embedding_lease::JoinOutcome;
    use std::net::TcpListener;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use std::sync::{Arc, Mutex};

    #[derive(Clone)]
    struct FakeEffects {
        state: Arc<FakeState>,
    }

    struct FakeState {
        endpoint: String,
        listener: Mutex<Option<TcpListener>>,
        starts: AtomicUsize,
        stops: AtomicUsize,
        deferred: AtomicUsize,
    }

    impl FakeEffects {
        fn stopped(endpoint: String) -> Self {
            Self {
                state: Arc::new(FakeState {
                    endpoint,
                    listener: Mutex::new(None),
                    starts: AtomicUsize::new(0),
                    stops: AtomicUsize::new(0),
                    deferred: AtomicUsize::new(0),
                }),
            }
        }

        fn running(listener: TcpListener) -> Self {
            let endpoint = format!("http://{}", listener.local_addr().unwrap());
            Self {
                state: Arc::new(FakeState {
                    endpoint,
                    listener: Mutex::new(Some(listener)),
                    starts: AtomicUsize::new(0),
                    stops: AtomicUsize::new(0),
                    deferred: AtomicUsize::new(0),
                }),
            }
        }

        fn endpoint(&self) -> &str {
            &self.state.endpoint
        }

        fn count(counter: &AtomicUsize) -> usize {
            counter.load(Ordering::SeqCst)
        }
    }

    impl EmbedEffects for FakeEffects {
        fn start(&self) -> Result<(), String> {
            let address = self
                .state
                .endpoint
                .strip_prefix("http://")
                .ok_or_else(|| "bad test endpoint".to_string())?;
            let listener = TcpListener::bind(address).map_err(|error| error.to_string())?;
            *self.state.listener.lock().unwrap() = Some(listener);
            self.state.starts.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }

        fn stop(&self) -> bool {
            self.state.stops.fetch_add(1, Ordering::SeqCst);
            self.state.listener.lock().unwrap().take().is_some()
        }

        fn runtime_state(&self) -> Option<EmbedRuntimeState> {
            self.state.listener.lock().unwrap().as_ref()?;
            Some(EmbedRuntimeState {
                pid: std::process::id(),
                base_url: self.state.endpoint.clone(),
            })
        }

        fn defer_stop(&self, _endpoint: &str, _server_pid: u32) -> Result<(), String> {
            self.state.deferred.fetch_add(1, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn runtime_state_must_match_the_configured_socket_exactly() {
        let state = EmbedRuntimeState {
            pid: std::process::id(),
            base_url: "http://127.0.0.1:8090".to_string(),
        };
        assert!(state.matches("http://localhost:8090/"));
        assert!(!state.matches("http://127.0.0.1:8091"));
        assert!(!state.matches("https://example.com:8090"));
    }

    #[test]
    fn runtime_state_reads_the_localbox_pascal_case_shape() {
        let state: EmbedRuntimeState = serde_json::from_str(
            r#"{"Pid":4242,"Port":8090,"BaseUrl":"http://127.0.0.1:8090","Model":"m","Pooling":"last"}"#,
        )
        .unwrap();
        assert_eq!(state.pid, 4242);
        assert_eq!(state.base_url, "http://127.0.0.1:8090");
    }

    #[test]
    fn missing_endpoint_starts_registers_and_last_drop_stops_exact_owner() {
        let root = tempfile::tempdir().unwrap();
        let registry = EmbeddingLeaseRegistry::at(root.path());
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", probe.local_addr().unwrap());
        drop(probe);
        let effects = FakeEffects::stopped(endpoint.clone());

        let guard = ensure_with(&registry, &endpoint, effects.clone()).expect("owner guard");
        assert_eq!(FakeEffects::count(&effects.state.starts), 1);
        assert!(root.path().join("started-by-localpilot").is_file());
        drop(guard);

        assert_eq!(FakeEffects::count(&effects.state.stops), 1);
        assert!(!root.path().join("started-by-localpilot").exists());
    }

    #[test]
    fn reachable_user_server_is_neither_started_leased_nor_stopped() {
        let root = tempfile::tempdir().unwrap();
        let registry = EmbeddingLeaseRegistry::at(root.path());
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", listener.local_addr().unwrap());
        let effects = FakeEffects::stopped(endpoint.clone());

        assert!(ensure_with(&registry, &endpoint, effects.clone()).is_none());
        assert_eq!(FakeEffects::count(&effects.state.starts), 0);
        assert_eq!(FakeEffects::count(&effects.state.stops), 0);
        assert!(listener.local_addr().is_ok(), "user listener remains alive");
    }

    #[test]
    fn legacy_marker_migrates_only_after_runtime_state_matches() {
        let root = tempfile::tempdir().unwrap();
        let registry = EmbeddingLeaseRegistry::at(root.path());
        std::fs::create_dir_all(root.path()).unwrap();
        std::fs::write(root.path().join("started-by-localpilot"), "localpilot\n").unwrap();
        let effects = FakeEffects::running(TcpListener::bind("127.0.0.1:0").unwrap());

        let guard = ensure_with(&registry, effects.endpoint(), effects.clone()).expect("migration");
        let marker = std::fs::read_to_string(root.path().join("started-by-localpilot")).unwrap();
        assert!(marker.contains("\"schema\": 1"));
        assert_eq!(FakeEffects::count(&effects.state.starts), 0);
        drop(guard);
        assert_eq!(FakeEffects::count(&effects.state.stops), 1);
    }

    #[test]
    fn owner_drop_defers_until_overlapping_localmind_client_drains() {
        let root = tempfile::tempdir().unwrap();
        let registry = EmbeddingLeaseRegistry::at(root.path());
        let probe = TcpListener::bind("127.0.0.1:0").unwrap();
        let endpoint = format!("http://{}", probe.local_addr().unwrap());
        drop(probe);
        let effects = FakeEffects::stopped(endpoint.clone());
        let owner = ensure_with(&registry, &endpoint, effects.clone()).expect("owner");
        let JoinOutcome::Acquired(client) = registry.join_existing(&endpoint).unwrap() else {
            panic!("client lease");
        };

        drop(owner);
        assert_eq!(FakeEffects::count(&effects.state.stops), 0);
        assert_eq!(FakeEffects::count(&effects.state.deferred), 1);
        drop(client);

        let StopPreparation::Ready(token) = registry
            .prepare_stop_when_unleased(&endpoint, OWNER, std::process::id())
            .unwrap()
        else {
            panic!("reaper must receive stop after client drain");
        };
        let stopped = effects.stop();
        token.complete(stopped).unwrap();
        assert_eq!(FakeEffects::count(&effects.state.stops), 1);
        assert!(!root.path().join("started-by-localpilot").exists());
    }
}
