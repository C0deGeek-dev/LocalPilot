//! Starting this workspace's background server for `/remote` (the start
//! choice behind "Start server and share" / "Start server and reconnect").
//!
//! A start is one physical operation, owned here past any view of it: the
//! child process is kept (never killed; dropping it does not stop it), and a
//! second one is never spawned while ours may still be starting. Readiness is
//! a session-free exchange, not just an open pipe: the server binds its
//! endpoint before it has finished setting up, so only an answered pairing
//! hello (which mints nothing) shows it is ready. Each attempt has one
//! deadline for all of its probes. What a result is allowed to do is decided
//! by the caller, against the sharing intent it was started for.

#![cfg_attr(not(feature = "tui"), allow(dead_code))]

use std::path::PathBuf;
use std::sync::{Arc, Mutex, PoisonError};
use std::time::Duration;

/// How long one start waits for the server to answer.
pub(crate) const START_WITHIN: Duration = Duration::from_secs(15);
/// Between probes.
const PROBE_EVERY: Duration = Duration::from_millis(250);

/// What one readiness probe found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Probe {
    /// The server answered the hello: it is up and compatible.
    Ready,
    /// Nothing listens for this workspace.
    NotRunning,
    /// Something holds the endpoint but has not answered yet (busy, or still
    /// setting up): wait, never start another.
    NotYet,
    /// It answered in a way that rules it out (an error, or a reply this
    /// version does not understand).
    Refused(String),
}

/// What is known about a started server process.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Status {
    Running,
    Exited(String),
    /// Its status could not be read: it may still be running.
    Unknown(String),
}

/// A started server process, as far as this terminal can see it.
pub(crate) trait Child: Send {
    fn status(&mut self) -> Status;
}

/// Whether an attempt may still act for the intent that began it (checked
/// immediately before anything is started).
pub(crate) type Authorized = Arc<dyn Fn() -> bool + Send + Sync>;

/// What a start needs from the system: a bounded probe and a spawn.
#[async_trait::async_trait]
pub(crate) trait Seams: Send + Sync {
    async fn probe(&self, within: Duration) -> Probe;
    fn spawn(&self) -> std::io::Result<Box<dyn Child>>;
}

/// How a start ended.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Outcome {
    /// The server answers; `started` says whether this terminal started it.
    Ready {
        started: bool,
    },
    Failed(String),
    /// It has not answered within the deadline; ours may still be starting.
    NotYet,
}

#[derive(Default)]
struct Slot {
    /// The server this terminal started last, kept so it is never started
    /// twice while it may still be booting.
    child: Option<Box<dyn Child>>,
    /// The attempt running now, if any.
    running: Option<u64>,
    /// The last finished attempt and how it ended.
    finished: Option<(u64, Outcome)>,
    attempts: u64,
}

/// One terminal's server starts.
pub(crate) struct ServerStart {
    seams: Arc<dyn Seams>,
    slot: Arc<Mutex<Slot>>,
    within: Duration,
}

impl ServerStart {
    pub(crate) fn new(seams: Arc<dyn Seams>) -> Self {
        Self {
            seams,
            slot: Arc::new(Mutex::new(Slot::default())),
            within: START_WITHIN,
        }
    }

    #[cfg(test)]
    pub(crate) fn within(mut self, within: Duration) -> Self {
        self.within = within;
        self
    }

    fn slot(&self) -> std::sync::MutexGuard<'_, Slot> {
        self.slot.lock().unwrap_or_else(PoisonError::into_inner)
    }

    /// Begin an attempt for an intent `authorized` still answers for, unless
    /// one is already running (then `None`: wait for it, and begin again for
    /// a fresh probe once it ends). The attempt starts a server only while
    /// `authorized` says yes at that moment.
    pub(crate) fn begin(&self, authorized: Authorized) -> Option<u64> {
        let mut slot = self.slot();
        if slot.running.is_some() {
            return None;
        }
        slot.attempts += 1;
        let attempt = slot.attempts;
        slot.running = Some(attempt);
        drop(slot);
        let seams = self.seams.clone();
        let shared = self.slot.clone();
        let within = self.within;
        tokio::spawn(async move {
            let outcome = run(&*seams, &shared, within, &*authorized).await;
            let mut slot = shared.lock().unwrap_or_else(PoisonError::into_inner);
            slot.running = None;
            slot.finished = Some((attempt, outcome));
        });
        Some(attempt)
    }

    /// How `attempt` ended, once it has.
    pub(crate) fn outcome(&self, attempt: u64) -> Option<Outcome> {
        match &self.slot().finished {
            Some((finished, outcome)) if *finished == attempt => Some(outcome.clone()),
            _ => None,
        }
    }
}

async fn run(
    seams: &dyn Seams,
    slot: &Mutex<Slot>,
    within: Duration,
    authorized: &(dyn Fn() -> bool + Send + Sync),
) -> Outcome {
    let deadline = tokio::time::Instant::now() + within;
    let left = || deadline.saturating_duration_since(tokio::time::Instant::now());
    let mut started = false;
    loop {
        if left().is_zero() {
            return Outcome::NotYet;
        }
        let probe = seams.probe(left()).await;
        match probe {
            Probe::Ready => return Outcome::Ready { started },
            Probe::Refused(why) => return Outcome::Failed(why),
            Probe::NotYet => {}
            Probe::NotRunning => {
                // Ours may still be starting (or its status is unknown): wait
                // on it, never a second one.
                let exited = {
                    let mut slot = slot.lock().unwrap_or_else(PoisonError::into_inner);
                    match slot.child.as_mut().map(|child| child.status()) {
                        Some(Status::Running | Status::Unknown(_)) => None,
                        Some(Status::Exited(status)) => {
                            slot.child = None;
                            Some(Some(status))
                        }
                        None => Some(None),
                    }
                };
                match exited {
                    // Alive, or not known to have exited: keep probing.
                    None => {}
                    // Ours exited: another server may have won the endpoint.
                    Some(Some(status)) if started => {
                        return match seams.probe(left()).await {
                            Probe::Ready => Outcome::Ready { started: false },
                            _ => Outcome::Failed(format!(
                                "the background server exited ({status}); run `localpilot \
                                 serve` in a terminal to see why"
                            )),
                        };
                    }
                    // None of ours runs: start one, but only for an intent
                    // that still stands and with time left to see it answer.
                    Some(_) if !authorized() => {
                        return Outcome::Failed("sharing changed in the meantime".to_string())
                    }
                    Some(_) if left().is_zero() => return Outcome::NotYet,
                    Some(_) => match seams.spawn() {
                        Ok(child) => {
                            slot.lock().unwrap_or_else(PoisonError::into_inner).child = Some(child);
                            started = true;
                        }
                        Err(error) => {
                            return Outcome::Failed(format!(
                                "the background server could not be started: {error}"
                            ))
                        }
                    },
                }
            }
        }
        if left().is_zero() {
            return Outcome::NotYet;
        }
        tokio::time::sleep(PROBE_EVERY.min(left())).await;
    }
}

// ------------------------------------------------------------- production

struct Process(std::process::Child);

impl Child for Process {
    fn status(&mut self) -> Status {
        match self.0.try_wait() {
            Ok(Some(status)) => Status::Exited(status.to_string()),
            Ok(None) => Status::Running,
            Err(error) => Status::Unknown(error.to_string()),
        }
    }
}

/// The workspace's own server: probed over the same dial sharing uses, and
/// started as the detached `__server-serve` in the workspace folder, bound to
/// the endpoint sharing dials (the endpoint override is not passed on).
pub(crate) struct Workspace {
    pub(crate) root: PathBuf,
    pub(crate) dial: crate::remote_share::Dial,
}

#[async_trait::async_trait]
impl Seams for Workspace {
    async fn probe(&self, within: Duration) -> Probe {
        match tokio::time::timeout(within, hello(&self.dial)).await {
            Ok(probe) => probe,
            Err(_) => Probe::NotYet,
        }
    }

    fn spawn(&self) -> std::io::Result<Box<dyn Child>> {
        let program = std::env::current_exe()?;
        let root = self.root.canonicalize()?;
        let mut command = localpilot_server::daemon::build_serve_command(
            &program,
            localpilot_server::daemon::SERVE_ARGV,
        );
        command
            .current_dir(root)
            .env_remove(localpilot_server::transport::ENDPOINT_ENV);
        Ok(Box::new(Process(command.spawn()?)))
    }
}

/// One session-free hello: connect, ask for the pairing instance, read the
/// answer. Nothing is created on the server.
async fn hello(dial: &crate::remote_share::Dial) -> Probe {
    use localpilot_rpc::{ClientCommand, JsonRecordReader, ServerEvent};
    use localpilot_server::TransportError;
    let link = match dial().await {
        Ok(link) => link,
        Err(TransportError::NotRunning) => return Probe::NotRunning,
        Err(TransportError::Busy) => return Probe::NotYet,
        Err(error) => return Probe::Refused(format!("it could not be reached: {error}")),
    };
    let (read, mut write) = tokio::io::split(link);
    let mut reader = JsonRecordReader::new(read);
    if crate::remote_pairing::send_record(&mut write, ClientCommand::PairHello)
        .await
        .is_err()
    {
        // It took the connection but is not answering yet.
        return Probe::NotYet;
    }
    match crate::remote_pairing::next_event(&mut reader).await {
        Some(ServerEvent::PairingReady { .. }) => Probe::Ready,
        Some(ServerEvent::Error { message }) => Probe::Refused(format!("it refused: {message}")),
        Some(_) => Probe::Refused(
            "it answered in a way this version does not understand; update LocalPilot".to_string(),
        ),
        // Closed before answering: still setting up, or it failed to.
        None => Probe::NotYet,
    }
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;
    use std::collections::VecDeque;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct FakeChild {
        exit_after: Option<usize>,
        unknown: bool,
        polls: Arc<AtomicUsize>,
    }

    impl Child for FakeChild {
        fn status(&mut self) -> Status {
            let polls = self.polls.fetch_add(1, Ordering::SeqCst) + 1;
            if self.unknown {
                return Status::Unknown("access denied".to_string());
            }
            match self.exit_after {
                Some(after) if polls >= after => Status::Exited("exit code 1".to_string()),
                _ => Status::Running,
            }
        }
    }

    fn yes() -> Authorized {
        Arc::new(|| true)
    }

    #[derive(Default)]
    struct Fake {
        probes: Mutex<VecDeque<Probe>>,
        /// What every probe answers once the scripted ones run out.
        then: Mutex<Option<Probe>>,
        spawns: AtomicUsize,
        exit_after: Option<usize>,
        unknown: bool,
        polls: Arc<AtomicUsize>,
        spawn_fails: bool,
        /// A probe that sleeps this long before answering.
        slow: Option<Duration>,
    }

    impl Fake {
        fn script(probes: &[Probe], then: Probe) -> Arc<Self> {
            Arc::new(Self {
                probes: Mutex::new(probes.iter().cloned().collect()),
                then: Mutex::new(Some(then)),
                ..Self::default()
            })
        }
    }

    #[async_trait::async_trait]
    impl Seams for Fake {
        async fn probe(&self, within: Duration) -> Probe {
            if let Some(slow) = self.slow {
                tokio::time::sleep(slow.min(within)).await;
            }
            let next = self.probes.lock().unwrap().pop_front();
            next.unwrap_or_else(|| self.then.lock().unwrap().clone().unwrap())
        }
        fn spawn(&self) -> std::io::Result<Box<dyn Child>> {
            if self.spawn_fails {
                return Err(std::io::Error::other("denied"));
            }
            self.spawns.fetch_add(1, Ordering::SeqCst);
            Ok(Box::new(FakeChild {
                exit_after: self.exit_after,
                unknown: self.unknown,
                polls: self.polls.clone(),
            }))
        }
    }

    async fn finish(start: &ServerStart, attempt: u64) -> Outcome {
        loop {
            if let Some(outcome) = start.outcome(attempt) {
                return outcome;
            }
            tokio::time::sleep(Duration::from_millis(50)).await;
        }
    }

    #[tokio::test(start_paused = true)]
    async fn a_running_server_is_reused_and_nothing_is_started() {
        let fake = Fake::script(&[], Probe::Ready);
        let start = ServerStart::new(fake.clone());
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(
            finish(&start, attempt).await,
            Outcome::Ready { started: false }
        );
        assert_eq!(fake.spawns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_absent_server_is_started_once_and_awaited() {
        let fake = Fake::script(
            &[Probe::NotRunning, Probe::NotRunning, Probe::NotYet],
            Probe::Ready,
        );
        let start = ServerStart::new(fake.clone());
        let attempt = start.begin(yes()).unwrap();
        assert!(start.begin(yes()).is_none(), "one attempt at a time");
        assert_eq!(
            finish(&start, attempt).await,
            Outcome::Ready { started: true }
        );
        assert_eq!(
            fake.spawns.load(Ordering::SeqCst),
            1,
            "never a second child"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_busy_endpoint_is_waited_on_never_doubled() {
        let fake = Fake::script(&[Probe::NotYet, Probe::NotYet, Probe::NotYet], Probe::Ready);
        let start = ServerStart::new(fake.clone());
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(
            finish(&start, attempt).await,
            Outcome::Ready { started: false }
        );
        assert_eq!(fake.spawns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn our_child_exiting_reuses_a_winner_or_reports_the_exit() {
        // Another server won the endpoint: ours exited, theirs answers.
        let fake = Arc::new(Fake {
            probes: Mutex::new([Probe::NotRunning, Probe::NotRunning].into_iter().collect()),
            then: Mutex::new(Some(Probe::Ready)),
            exit_after: Some(1),
            ..Fake::default()
        });
        let start = ServerStart::new(fake.clone());
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(
            finish(&start, attempt).await,
            Outcome::Ready { started: false }
        );

        // Nothing answers after ours exited: a failure that says so.
        let fake = Arc::new(Fake {
            then: Mutex::new(Some(Probe::NotRunning)),
            exit_after: Some(1),
            ..Fake::default()
        });
        let start = ServerStart::new(fake.clone());
        let attempt = start.begin(yes()).unwrap();
        match finish(&start, attempt).await {
            Outcome::Failed(why) => assert!(why.contains("exited (exit code 1)"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(fake.spawns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn a_slow_start_is_not_yet_and_a_retry_waits_on_the_same_child() {
        let fake = Fake::script(&[], Probe::NotRunning);
        let start = ServerStart::new(fake.clone()).within(Duration::from_secs(2));
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(finish(&start, attempt).await, Outcome::NotYet);
        assert_eq!(fake.spawns.load(Ordering::SeqCst), 1);
        // Retry: the child is still alive, so no second one.
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(finish(&start, attempt).await, Outcome::NotYet);
        assert_eq!(fake.spawns.load(Ordering::SeqCst), 1);
        // It comes up later: a fresh attempt sees it.
        *fake.then.lock().unwrap() = Some(Probe::Ready);
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(
            finish(&start, attempt).await,
            Outcome::Ready { started: false }
        );
        assert_eq!(fake.spawns.load(Ordering::SeqCst), 1);
    }

    #[tokio::test(start_paused = true)]
    async fn nothing_is_started_for_an_intent_that_moved_on_during_the_probe() {
        let fake = Arc::new(Fake {
            then: Mutex::new(Some(Probe::NotRunning)),
            slow: Some(Duration::from_secs(1)),
            ..Fake::default()
        });
        let start = ServerStart::new(fake.clone());
        let allowed = Arc::new(std::sync::atomic::AtomicBool::new(true));
        let check = allowed.clone();
        let attempt = start
            .begin(Arc::new(move || check.load(Ordering::SeqCst)))
            .unwrap();
        // Cancelled while the first probe is still out.
        tokio::time::sleep(Duration::from_millis(500)).await;
        allowed.store(false, Ordering::SeqCst);
        match finish(&start, attempt).await {
            Outcome::Failed(why) => assert!(why.contains("changed"), "{why}"),
            other => panic!("{other:?}"),
        }
        assert_eq!(fake.spawns.load(Ordering::SeqCst), 0);
    }

    #[tokio::test(start_paused = true)]
    async fn an_absent_server_at_the_deadline_is_not_started() {
        // The probe uses up the whole budget, then says nothing runs.
        let fake = Arc::new(Fake {
            then: Mutex::new(Some(Probe::NotRunning)),
            slow: Some(Duration::from_secs(60)),
            ..Fake::default()
        });
        let start = ServerStart::new(fake.clone()).within(Duration::from_secs(2));
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(finish(&start, attempt).await, Outcome::NotYet);
        assert_eq!(
            fake.spawns.load(Ordering::SeqCst),
            0,
            "too late to start one"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn an_unreadable_child_status_is_never_taken_for_an_exit() {
        let fake = Arc::new(Fake {
            then: Mutex::new(Some(Probe::NotRunning)),
            unknown: true,
            ..Fake::default()
        });
        let start = ServerStart::new(fake.clone()).within(Duration::from_secs(2));
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(finish(&start, attempt).await, Outcome::NotYet);
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(finish(&start, attempt).await, Outcome::NotYet);
        assert_eq!(
            fake.spawns.load(Ordering::SeqCst),
            1,
            "never a second child"
        );
    }

    #[tokio::test(start_paused = true)]
    async fn a_refusing_server_or_a_failed_spawn_fails_with_its_reason() {
        let fake = Fake::script(&[], Probe::Refused("it refused: no".into()));
        let start = ServerStart::new(fake.clone());
        let attempt = start.begin(yes()).unwrap();
        assert_eq!(
            finish(&start, attempt).await,
            Outcome::Failed("it refused: no".into())
        );
        let fake = Arc::new(Fake {
            then: Mutex::new(Some(Probe::NotRunning)),
            spawn_fails: true,
            ..Fake::default()
        });
        let start = ServerStart::new(fake);
        let attempt = start.begin(yes()).unwrap();
        match finish(&start, attempt).await {
            Outcome::Failed(why) => assert!(why.contains("could not be started: denied"), "{why}"),
            other => panic!("{other:?}"),
        }
    }
}
