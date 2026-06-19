//! The `run_background` builtin tool and its session-scoped process registry.
//!
//! A long-running command — a dev server like `npm run dev` or `bun serve`, or a
//! watcher — never exits, so `run_shell` (which waits for completion) only blocks
//! until its timeout and then kills it. `run_background` instead starts the
//! command detached from the turn, waits a short grace period to confirm it
//! actually stayed up, captures its startup output, and keeps the child in an
//! in-memory [`BackgroundProcesses`] registry so later turns can read its logs,
//! list it, or stop it.
//!
//! The registry is **session-scoped**: it lives on the runtime for the session
//! and every child is started with `kill_on_drop(true)`, so dropping the registry
//! (or calling [`BackgroundProcesses::kill_all`] at session close) terminates the
//! processes — no daemon survives the session.

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::time::{Duration, Instant};

use async_trait::async_trait;
use localpilot_sandbox::{CommandClass, Effect};
use parking_lot::Mutex;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use tokio::io::AsyncReadExt;

use crate::builtins::cap;
use crate::builtins_shell::{
    execution_class, execution_text, normalize_execution, render_stream, shell_program_and_args,
    RunShellExecution,
};
use crate::contract::{
    Idempotency, Reversibility, SideEffectClass, ToolContract, VerificationMethod,
};
use crate::error::ToolError;
use crate::tool::{detail_preview, parse_input, schema_for, Tool, ToolContext, ToolOutput};

/// Default grace period: how long [`BackgroundProcesses::start`] waits before
/// deciding a process stayed up.
const DEFAULT_GRACE_SECS: u64 = 2;

/// Cap on a process's retained log, keeping the most recent bytes. Matches the
/// per-call output cap so a chatty server cannot grow memory without bound.
const LOG_CAP_BYTES: usize = 64 * 1024;

/// A bounded, FIFO byte buffer holding the tail of a process's combined
/// stdout/stderr. Oldest bytes are dropped once the cap is exceeded.
#[derive(Default)]
struct RollingLog {
    buf: Vec<u8>,
}

impl RollingLog {
    fn push(&mut self, bytes: &[u8]) {
        self.buf.extend_from_slice(bytes);
        if self.buf.len() > LOG_CAP_BYTES {
            let overflow = self.buf.len() - LOG_CAP_BYTES;
            self.buf.drain(..overflow);
        }
    }

    /// Render the buffer as text, substituting a placeholder for binary bytes so
    /// raw control bytes never reach the model context.
    fn render(&self) -> String {
        render_stream(&self.buf)
    }
}

/// One tracked background process.
struct ProcEntry {
    command: String,
    child: tokio::process::Child,
    log: Arc<Mutex<RollingLog>>,
    started_at: Instant,
}

/// A snapshot of a tracked process for display.
pub struct ProcStatus {
    pub id: String,
    pub command: String,
    pub age_secs: u64,
    pub alive: bool,
}

#[derive(Default)]
struct State {
    procs: HashMap<String, ProcEntry>,
    next_id: u64,
}

/// The outcome of starting a background process.
pub(crate) enum StartOutcome {
    /// The process stayed up past the grace period and is now tracked under `id`.
    Running { id: String, log: String },
    /// The process exited within the grace period; it is not tracked.
    ExitedEarly { code: i32, log: String },
}

/// A session-scoped registry of running background processes. Cheap to share by
/// reference into a [`ToolContext`]; interior-mutable behind a single lock.
#[derive(Default)]
pub struct BackgroundProcesses {
    state: Mutex<State>,
}

impl BackgroundProcesses {
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// Start `execution` in `cwd`, drain its output into a rolling log, and wait
    /// `grace` to see whether it stays up. A process still running after the
    /// grace period is tracked and its id returned; one that exited is reported
    /// but not tracked.
    ///
    /// # Errors
    /// Returns [`ToolError::Failed`] if the process cannot be spawned or its exit
    /// status cannot be polled.
    pub(crate) async fn start(
        &self,
        execution: RunShellExecution,
        cwd: &Path,
        grace: Duration,
    ) -> Result<StartOutcome, ToolError> {
        let command_line = execution_text(&execution);
        let (program, args) = match execution {
            RunShellExecution::Direct { program, args } => (program, args),
            RunShellExecution::Shell { command } => shell_program_and_args(&command),
        };

        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(cwd)
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let mut child = command
            .spawn()
            .map_err(|e| ToolError::Failed(format!("failed to start {program}: {e}")))?;

        // Continuously drain both pipes into the shared log so a full OS pipe can
        // never block (and so wedge) the child. The tasks end when the pipes
        // close, i.e. when the process exits or is killed.
        let log = Arc::new(Mutex::new(RollingLog::default()));
        if let Some(stdout) = child.stdout.take() {
            spawn_drain(stdout, Arc::clone(&log));
        }
        if let Some(stderr) = child.stderr.take() {
            spawn_drain(stderr, Arc::clone(&log));
        }

        tokio::time::sleep(grace).await;

        match child.try_wait() {
            Ok(Some(status)) => Ok(StartOutcome::ExitedEarly {
                code: status.code().unwrap_or(-1),
                log: log.lock().render(),
            }),
            Ok(None) => {
                let started_at = Instant::now();
                let snapshot = log.lock().render();
                let mut state = self.state.lock();
                state.next_id += 1;
                let id = format!("bg-{}", state.next_id);
                state.procs.insert(
                    id.clone(),
                    ProcEntry {
                        command: command_line,
                        child,
                        log,
                        started_at,
                    },
                );
                Ok(StartOutcome::Running { id, log: snapshot })
            }
            Err(e) => Err(ToolError::Failed(format!("could not poll {program}: {e}"))),
        }
    }

    /// A snapshot of every tracked process, sorted by id.
    #[must_use]
    pub fn list(&self) -> Vec<ProcStatus> {
        let mut state = self.state.lock();
        let mut out: Vec<ProcStatus> = state
            .procs
            .iter_mut()
            .map(|(id, entry)| ProcStatus {
                id: id.clone(),
                command: entry.command.clone(),
                age_secs: entry.started_at.elapsed().as_secs(),
                alive: matches!(entry.child.try_wait(), Ok(None)),
            })
            .collect();
        out.sort_by(|a, b| a.id.cmp(&b.id));
        out
    }

    /// The retained log for `id`, or `None` when no such process is tracked.
    #[must_use]
    pub fn logs(&self, id: &str) -> Option<String> {
        let state = self.state.lock();
        state.procs.get(id).map(|entry| entry.log.lock().render())
    }

    /// Stop and forget the process `id`. Returns whether it was tracked.
    pub async fn stop(&self, id: &str) -> bool {
        // Remove under the lock, then await the kill outside it — never hold the
        // lock across an await.
        let entry = self.state.lock().procs.remove(id);
        if let Some(mut entry) = entry {
            let _ = entry.child.start_kill();
            let _ = tokio::time::timeout(Duration::from_secs(5), entry.child.wait()).await;
            true
        } else {
            false
        }
    }

    /// Stop and forget the process `id` without awaiting its exit. Returns
    /// whether it was tracked. Synchronous so a UI command (a `/bg stop`) can run
    /// it off the turn loop; `kill_on_drop` reaps the child as the entry drops.
    pub fn stop_now(&self, id: &str) -> bool {
        let mut state = self.state.lock();
        if let Some(mut entry) = state.procs.remove(id) {
            let _ = entry.child.start_kill();
            true
        } else {
            false
        }
    }

    /// Terminate and forget every tracked process. Synchronous so it can run from
    /// the session-close path; `kill_on_drop` reaps the children as they drop.
    pub fn kill_all(&self) {
        let mut state = self.state.lock();
        for entry in state.procs.values_mut() {
            let _ = entry.child.start_kill();
        }
        state.procs.clear();
    }
}

/// Spawn a detached task draining `reader` into `log` until end of stream.
fn spawn_drain<R>(mut reader: R, log: Arc<Mutex<RollingLog>>)
where
    R: AsyncReadExt + Unpin + Send + 'static,
{
    tokio::spawn(async move {
        let mut buf = [0u8; 4096];
        loop {
            match reader.read(&mut buf).await {
                Ok(0) | Err(_) => break,
                Ok(n) => log.lock().push(&buf[..n]),
            }
        }
    });
}

#[derive(Debug, Default, Deserialize, JsonSchema)]
#[serde(rename_all = "snake_case")]
enum BackgroundAction {
    /// Start a long-running command as a background process (the default).
    #[default]
    Start,
    /// List the tracked background processes.
    List,
    /// Read the captured output of a tracked process by `id`.
    Logs,
    /// Stop and forget a tracked process by `id`.
    Stop,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct RunBackgroundInput {
    /// What to do. Defaults to starting a process.
    #[serde(default)]
    action: BackgroundAction,
    /// For `start`: a shell command line to run in the background.
    #[serde(default)]
    command: Option<String>,
    /// For `start`: a program to run directly when `args` is provided; otherwise
    /// treated as a shell command line.
    #[serde(default)]
    program: Option<String>,
    /// For `start`: arguments passed to `program` for direct execution.
    #[serde(default)]
    args: Vec<String>,
    /// For `logs`/`stop`: the id of the tracked process (as returned by `start`).
    #[serde(default)]
    id: Option<String>,
    /// For `start`: seconds to wait before judging the process stayed up.
    /// Defaults to 2.
    #[serde(default)]
    grace_secs: Option<u64>,
}

pub struct RunBackground;

impl RunBackground {
    fn execution(input: &RunBackgroundInput) -> Result<RunShellExecution, ToolError> {
        normalize_execution(
            input.command.clone(),
            input.program.clone(),
            input.args.clone(),
        )
    }
}

#[async_trait]
impl Tool for RunBackground {
    fn name(&self) -> &'static str {
        "run_background"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description:
                "Start a long-running command (dev server, watcher) as a background process, \
                 or list/read-logs-of/stop one.",
            side_effect: SideEffectClass::Destructive,
            reversibility: Reversibility::Irreversible,
            idempotency: Idempotency::Unknown,
            verification: VerificationMethod::Unverifiable,
            ..ToolContract::default()
        }
    }
    fn description(&self) -> &'static str {
        "Run a long-running command in the background, or list, read logs, or stop one."
    }
    fn schema(&self) -> Value {
        schema_for::<RunBackgroundInput>()
    }
    fn approval_detail(&self, input: &Value) -> String {
        // Only a `start` actually runs a command; show its command line.
        let Ok(parsed) = parse_input::<RunBackgroundInput>(input) else {
            return String::new();
        };
        if !matches!(parsed.action, BackgroundAction::Start) {
            return String::new();
        }
        Self::execution(&parsed)
            .map(|execution| detail_preview(&execution_text(&execution)))
            .unwrap_or_default()
    }
    fn effects(&self, input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: RunBackgroundInput = parse_input(input)?;
        // Only `start` runs a command; managing our own tracked processes
        // (list/logs/stop) has no external effect.
        if !matches!(input.action, BackgroundAction::Start) {
            return Ok(Vec::new());
        }
        let execution = Self::execution(&input)?;
        let class = execution_class(&execution)?;
        let mut effects = vec![Effect::RunCommand(class)];
        if class == CommandClass::Network {
            effects.push(Effect::Network);
        }
        Ok(effects)
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: RunBackgroundInput = parse_input(&input)?;
        let procs = ctx.processes.ok_or_else(|| {
            ToolError::Failed("background processes are not available in this session".to_string())
        })?;

        match input.action {
            BackgroundAction::Start => {
                let detail = {
                    let execution = Self::execution(&input)?;
                    execution_text(&execution)
                };
                let grace = Duration::from_secs(input.grace_secs.unwrap_or(DEFAULT_GRACE_SECS));
                let execution = Self::execution(&input)?;
                match procs.start(execution, ctx.workspace.root(), grace).await? {
                    StartOutcome::Running { id, log } => Ok(cap(format!(
                        "started background process `{id}` (`{detail}`). \
                         Use run_background with action `logs` and this id to read more \
                         output, or `stop` to end it.\n--- startup output ---\n{log}"
                    ))),
                    StartOutcome::ExitedEarly { code, log } => {
                        let mut out = cap(format!(
                            "`{detail}` exited within {}s with code {code}; it did not stay up \
                             as a background process.\n--- output ---\n{log}",
                            grace.as_secs()
                        ));
                        out.is_error = true;
                        Ok(out)
                    }
                }
            }
            BackgroundAction::List => {
                let list = procs.list();
                if list.is_empty() {
                    return Ok(ToolOutput::ok("no background processes"));
                }
                let mut text = String::new();
                for status in list {
                    let state = if status.alive { "running" } else { "exited" };
                    text.push_str(&format!(
                        "{}\t{}\t{}s\t{}\n",
                        status.id, state, status.age_secs, status.command
                    ));
                }
                Ok(cap(text))
            }
            BackgroundAction::Logs => {
                let id = require_id(&input.id)?;
                match procs.logs(&id) {
                    Some(log) => Ok(cap(format!("--- output of `{id}` ---\n{log}"))),
                    None => no_such_process(&id),
                }
            }
            BackgroundAction::Stop => {
                let id = require_id(&input.id)?;
                if procs.stop(&id).await {
                    Ok(ToolOutput::ok(format!("stopped background process `{id}`")))
                } else {
                    no_such_process(&id)
                }
            }
        }
    }
}

fn require_id(id: &Option<String>) -> Result<String, ToolError> {
    id.as_ref()
        .map(|s| s.trim().to_string())
        .filter(|s| !s.is_empty())
        .ok_or_else(|| ToolError::InvalidInput("this action requires `id`".to_string()))
}

fn no_such_process(id: &str) -> Result<ToolOutput, ToolError> {
    let mut out = ToolOutput::ok(format!("no background process `{id}`"));
    out.is_error = true;
    Ok(out)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A command that prints a line and then stays alive well past any test
    /// grace period, run through the platform shell.
    fn stays_up() -> RunShellExecution {
        #[cfg(windows)]
        let command = "Write-Output hello; Start-Sleep -Seconds 30".to_string();
        #[cfg(not(windows))]
        let command = "echo hello; sleep 30".to_string();
        RunShellExecution::Shell { command }
    }

    /// A command that exits immediately.
    fn exits_now() -> RunShellExecution {
        #[cfg(windows)]
        let command = "Write-Output bye".to_string();
        #[cfg(not(windows))]
        let command = "echo bye".to_string();
        RunShellExecution::Shell { command }
    }

    #[tokio::test]
    async fn a_process_that_stays_up_is_tracked_with_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let procs = BackgroundProcesses::new();

        let outcome = procs
            .start(stays_up(), dir.path(), Duration::from_millis(400))
            .await
            .unwrap();
        let id = match outcome {
            StartOutcome::Running { id, .. } => id,
            StartOutcome::ExitedEarly { .. } => panic!("a sleeping process must stay up"),
        };

        // The drain tasks capture the startup line shortly after launch.
        tokio::time::sleep(Duration::from_millis(200)).await;
        assert!(procs.logs(&id).unwrap().contains("hello"));

        let list = procs.list();
        assert_eq!(list.len(), 1);
        assert!(list[0].alive);

        assert!(
            procs.stop(&id).await,
            "stop reports the process was tracked"
        );
        assert!(procs.list().is_empty(), "a stopped process is forgotten");
        assert!(!procs.stop(&id).await, "stopping an unknown id is a no-op");
    }

    #[tokio::test]
    async fn a_process_that_exits_within_the_grace_is_not_tracked() {
        let dir = tempfile::tempdir().unwrap();
        let procs = BackgroundProcesses::new();

        let outcome = procs
            .start(exits_now(), dir.path(), Duration::from_millis(500))
            .await
            .unwrap();
        match outcome {
            StartOutcome::ExitedEarly { code, log } => {
                assert_eq!(code, 0);
                assert!(log.contains("bye"));
            }
            StartOutcome::Running { .. } => panic!("an immediate `echo` must not stay up"),
        }
        assert!(
            procs.list().is_empty(),
            "a process that exited early is never tracked"
        );
    }

    #[tokio::test]
    async fn kill_all_terminates_and_forgets_every_process() {
        let dir = tempfile::tempdir().unwrap();
        let procs = BackgroundProcesses::new();

        procs
            .start(stays_up(), dir.path(), Duration::from_millis(300))
            .await
            .unwrap();
        procs
            .start(stays_up(), dir.path(), Duration::from_millis(300))
            .await
            .unwrap();
        assert_eq!(procs.list().len(), 2);

        procs.kill_all();
        assert!(procs.list().is_empty());
    }

    #[test]
    fn logs_and_stop_report_an_unknown_id() {
        let procs = BackgroundProcesses::new();
        assert!(procs.logs("bg-99").is_none());
    }
}
