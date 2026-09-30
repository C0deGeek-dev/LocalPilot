//! `localpilot mesh run`: LocalPilot as a native pair participant.
//!
//! The protocol runs in code (`localpilot_mesh::ops::engine`): the engine
//! joins, receives with acknowledged delivery, decides what each message
//! needs, posts, and acknowledges only after it has acted. A model is asked
//! only for judgement (a verdict or a reply), in a fresh headless turn that
//! runs `readonly` under the session's write lease, with an in-memory store,
//! so it cannot change the tree it is reviewing. Its answer must be one JSON
//! object the engine validates; after a second invalid answer the engine
//! escalates instead of posting it.
//!
//! With `--own` it also takes a unit over when one is handed to it. It
//! accepts the handoff through the protocol operation, has the model
//! implement the task in a turn that may write only while the session says
//! it owns the tree, builds and fingerprints the review request itself, and
//! closes the unit only when every required reviewer has agreed.

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use localpilot_mesh::ops::engine::{escalation, parse_answer, validate, Need, Request, Step};
use localpilot_mesh::ops::owner::{OwnerState, OwnerTask};
use localpilot_mesh::ops::{Expect, PostArgs, SessionLease};
use localpilot_mesh::{Mesh, MeshError};
use localpilot_sandbox::Profile;
use localpilot_store::Store;
use tokio::sync::broadcast;
use tokio_util::sync::CancellationToken;

use crate::mesh_cmd::{native_writer, resolve_anchor, MeshArgs};

/// How long a model turn may hold a write lease; a navigator's lease never
/// writes anyway, and a fresh one is taken for every turn.
const LEASE_TTL: Duration = Duration::from_secs(30 * 60);
/// The most of a request body put in front of the model.
const BODY_BUDGET: usize = 8_000;
/// The most of the reviewed diff put in front of the model.
const DIFF_BUDGET: usize = 30_000;
/// Rounds an owner may spend on one unit before it escalates.
const OWNER_ROUNDS: i64 = 3;

#[derive(Debug, Parser)]
#[command(name = "localpilot mesh run", no_binary_name = true)]
struct RunCli {
    /// The participant to play, e.g. `localpilot`.
    #[arg(long)]
    role: String,
    /// The model that judges.
    #[arg(long)]
    model: String,
    /// The configured provider; the default provider when omitted.
    #[arg(long)]
    provider: Option<String>,
    /// Handle one delivery, then exit.
    #[arg(long)]
    once: bool,
    /// Take a unit over when it is handed to this participant, and do the
    /// owner's work: implement, request review, close on agreement.
    #[arg(long)]
    own: bool,
    /// Seconds to wait for a session that names the role; 0 waits for ever.
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// Register a delivery endpoint and act as soon as a peer's post wakes
    /// it; the mailbox is still read every `--poll` seconds (30 by default
    /// when listening).
    #[arg(long)]
    listen: bool,
    /// Seconds between looks for mail, from 0.05 to 3600 (default 1, or 30
    /// with `--listen`).
    #[arg(long, value_parser = poll_seconds)]
    poll: Option<f64>,
}

impl RunCli {
    fn poll(&self) -> Duration {
        Duration::from_secs_f64(self.poll.unwrap_or(if self.listen { 30.0 } else { 1.0 }))
    }
}

fn poll_seconds(raw: &str) -> Result<f64, String> {
    let v: f64 = raw.parse().map_err(|e| format!("{e}"))?;
    if v.is_finite() && (0.05..=3600.0).contains(&v) {
        Ok(v)
    } else {
        Err("must be a number of seconds from 0.05 to 3600".into())
    }
}

/// Whether these mesh arguments ask for the engine rather than one operation.
pub(crate) fn is_run(args: &MeshArgs) -> bool {
    args.rest.first().is_some_and(|op| op == "run")
}

/// Run the participant engine; the exit code is 0 when it stopped cleanly,
/// 1 on an error, 2 on a usage error and 4 when the session refuses it.
pub(crate) async fn run(args: MeshArgs) -> ExitCode {
    let cli = match RunCli::try_parse_from(&args.rest[1..]) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let (anchor, source) = match resolve_anchor(args.repo.as_deref()) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    };
    match native_writer() {
        Ok(true) => {}
        Ok(false) => {
            eprintln!(
                "localpilot mesh run needs the native writer; it does not run under [mesh] writer = \"delegate\""
            );
            return ExitCode::from(2);
        }
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    }
    let mesh = Mesh::at(&anchor, source);
    let mut judge = ModelJudge {
        mesh: mesh.clone(),
        anchor: anchor.clone(),
        role: cli.role.clone(),
        model: cli.model.clone(),
        provider: cli.provider.clone(),
    };
    match engine(&mesh, &cli, &mut judge).await {
        Ok(()) => ExitCode::SUCCESS,
        Err(Failure::Refused(msg)) => {
            eprintln!("{msg} [anchor={}]", anchor.display());
            ExitCode::from(4)
        }
        Err(Failure::Error(msg)) => {
            eprintln!("{msg} [anchor={}]", anchor.display());
            ExitCode::from(1)
        }
    }
}

enum Failure {
    Refused(String),
    Error(String),
}

impl From<MeshError> for Failure {
    fn from(e: MeshError) -> Self {
        match e {
            MeshError::Refused(m) => Failure::Refused(m),
            other => Failure::Error(other.to_string()),
        }
    }
}

async fn engine(mesh: &Mesh, cli: &RunCli, judge: &mut dyn Judge) -> Result<(), Failure> {
    let poll = cli.poll();
    let joined = {
        let (mesh, role, timeout) = (mesh.clone(), cli.role.clone(), cli.timeout);
        tokio::task::spawn_blocking(move || mesh.join(&role, timeout, poll))
            .await
            .map_err(|e| Failure::Error(e.to_string()))??
    };
    if joined.code == 1 {
        return Err(Failure::Error(format!(
            "NO_SESSION no session named {} within {}s",
            cli.role, cli.timeout
        )));
    }
    print!("{}", joined.stdout);
    crate::mesh_push::push_all(mesh).await;
    let sid = mesh.engine_ready(&cli.role)?;
    if cli.own {
        mesh.owner_supported(&cli.role)?;
    }
    println!("ENGINE role={} session={sid}", cli.role);
    let wake = std::sync::Arc::new(tokio::sync::Notify::new());
    let listening = if cli.listen {
        match crate::mesh_listen::Listening::start(mesh, &cli.role, &sid, wake.clone()).await {
            Ok(l) => Some(l),
            Err(why) => {
                println!("WARN not listening: {why}; the engine polls only");
                None
            }
        }
    } else {
        None
    };
    let result = tokio::select! {
        r = drive(mesh, cli, judge, &sid, poll, &wake) => r,
        _ = tokio::signal::ctrl_c() => Ok(()),
    };
    // Every way out retires the endpoint this run registered.
    if let Some(l) = listening {
        l.stop(mesh).await;
    }
    result
}

/// The engine's loop: act on each delivery, then wait for a wake or the
/// next poll.
async fn drive(
    mesh: &Mesh,
    cli: &RunCli,
    judge: &mut dyn Judge,
    sid: &str,
    poll: Duration,
    wake: &tokio::sync::Notify,
) -> Result<(), Failure> {
    loop {
        let delivered = mesh.receive(&cli.role, 900, Some(sid))?;
        if let Some(delivery) = &delivered {
            for step in mesh.plan_as(&cli.role, delivery, cli.own)? {
                let go_on = execute(mesh, &cli.role, step, judge).await;
                crate::mesh_push::push_all(mesh).await;
                if !go_on? {
                    return Ok(());
                }
            }
        }
        if cli.own {
            let go_on = owner_step(mesh, &cli.role, judge).await;
            crate::mesh_push::push_all(mesh).await;
            if !go_on? {
                return Ok(());
            }
        }
        if cli.once {
            return Ok(());
        }
        if delivered.is_none() {
            tokio::select! {
                () = wake.notified() => {}
                () = tokio::time::sleep(poll) => {}
            }
        }
    }
}

/// What an owner does between deliveries; `false` when the engine must stop
/// (the unit closed, or the owner escalated).
async fn owner_step(mesh: &Mesh, role: &str, judge: &mut dyn Judge) -> Result<bool, Failure> {
    match mesh.owner_state(role)? {
        OwnerState::NotOwner | OwnerState::Waiting(_) => Ok(true),
        OwnerState::Escalated(why) => {
            println!("STOPPED as owner: {why}");
            Ok(false)
        }
        OwnerState::Agreed(request) => match mesh.complete(role) {
            Ok(out) => {
                print!("{}", out.stdout);
                println!("CLOSED on every reviewer's agreement with {request}");
                Ok(false)
            }
            // The protocol's own sign-off check has the last word.
            Err(MeshError::Refused(why)) => {
                println!("NOT_CLOSED {request}: {why}");
                Ok(true)
            }
            Err(e) => Err(e.into()),
        },
        OwnerState::Implement(task) => {
            if task.round > OWNER_ROUNDS {
                let args = owner_escalation(
                    mesh,
                    role,
                    &format!("no agreement after {OWNER_ROUNDS} review rounds"),
                )?;
                post_owner(mesh, role, &args, &task)?;
                return Ok(false);
            }
            let args = ownership(mesh, role, judge, &task).await?;
            let escalated = args.kind == "ESCALATE";
            post_owner(mesh, role, &args, &task)?;
            Ok(!escalated)
        }
    }
}

fn post_owner(mesh: &Mesh, role: &str, args: &PostArgs, task: &OwnerTask) -> Result<(), Failure> {
    match mesh.post_guarded(role, args, &task.expect) {
        Ok(m) => {
            let id = m.get("msg_id").and_then(|v| v.as_str()).unwrap_or_default();
            println!("POSTED {} {id} round={}", args.kind, task.round);
            Ok(())
        }
        Err(MeshError::Refused(why)) if why.starts_with("STALE") => {
            println!("SKIPPED round {}: {why}", task.round);
            Ok(())
        }
        // Posted; only its bookkeeping is incomplete (spec M-7). Never re-sent.
        Err(e @ MeshError::PostedIncomplete { .. }) => {
            println!("{e}");
            Ok(())
        }
        Err(e) => Err(e.into()),
    }
}

/// The owner's round as a post: the model implements; the engine builds the
/// request from the tree. A refused answer or request is fed back once, then
/// escalated.
async fn ownership(
    mesh: &Mesh,
    role: &str,
    judge: &mut dyn Judge,
    task: &OwnerTask,
) -> Result<PostArgs, Failure> {
    let mut feedback: Option<String> = None;
    for _ in 0..2 {
        let answer = judge
            .implement(task, feedback.as_deref())
            .await
            .map_err(|e| format!("the model turn failed: {e}"))
            .and_then(|text| parse_answer(&text));
        let why = match answer {
            Ok(a) if a.kind == "ESCALATE" && !a.body.trim().is_empty() => {
                return owner_escalation(mesh, role, a.body.trim());
            }
            Ok(a) if a.kind == "REVIEW_REQUEST" && !a.body.trim().is_empty() => {
                match mesh.review_request(role, &a.body)? {
                    Ok(args) => return Ok(args),
                    Err(why) => why,
                }
            }
            Ok(a) => format!(
                "answer with kind REVIEW_REQUEST (or ESCALATE) and a non-empty body, not {}",
                a.kind
            ),
            Err(why) => why,
        };
        feedback = Some(why);
    }
    owner_escalation(
        mesh,
        role,
        &format!(
            "could not produce a reviewable change: {}",
            feedback.as_deref().unwrap_or("no answer")
        ),
    )
}

fn owner_escalation(mesh: &Mesh, role: &str, why: &str) -> Result<PostArgs, Failure> {
    let reviewers = mesh
        .session_summary()?
        .map(|s| {
            s.participants
                .into_iter()
                .filter(|p| p != role)
                .collect::<Vec<_>>()
                .join(",")
        })
        .unwrap_or_default();
    Ok(PostArgs {
        kind: "ESCALATE".into(),
        body: format!("{role} (the owner) stops here: {why}. A person or another participant needs to take this unit on."),
        to: Some(reviewers),
        ..PostArgs::default()
    })
}

/// Carry out one step; `false` when the engine must stop.
async fn execute(
    mesh: &Mesh,
    role: &str,
    step: Step,
    judge: &mut dyn Judge,
) -> Result<bool, Failure> {
    match step {
        Step::Notice(line) => println!("NOTICE {line}"),
        Step::Ack { msg_id, why } => {
            mesh.ack(role, &msg_id)?;
            println!("ACKED {msg_id} ({why})");
        }
        Step::Stop { msg_id, reason } => {
            mesh.ack(role, &msg_id)?;
            println!("STOPPED by {msg_id}: {reason}");
            return Ok(false);
        }
        Step::Post { args, expect, ack } => post(mesh, role, &args, &expect, &ack)?,
        Step::Accept { msg_id } => match mesh.handoff_accept(role) {
            Ok(out) => {
                print!("{}", out.stdout);
                mesh.ack(role, &msg_id)?;
                println!("ACCEPTED {msg_id}");
            }
            // The operation refuses a tree that moved since the offer: say
            // so, never force it.
            Err(MeshError::Refused(why)) => {
                let note = PostArgs {
                    kind: "NOTE".into(),
                    body: format!("{role} could not accept this handoff: {why}"),
                    reply_to: Some(msg_id.clone()),
                    ..PostArgs::default()
                };
                mesh.post(role, &note)?;
                mesh.ack(role, &msg_id)?;
                println!("NOT_ACCEPTED {msg_id}: {why}");
            }
            Err(e) => return Err(e.into()),
        },
        Step::Judge(request) => {
            let mut args = judgement(judge, mesh.root(), &request).await;
            // The tree may have moved while the model judged it: a verdict
            // is posted only on the manifest it was asked about.
            if request.need == Need::Review {
                if let Some(revise) = mesh.recheck_review(role, &request)? {
                    println!("CHANGED {}: the tree moved during review", request.msg_id);
                    args = revise;
                }
            }
            post(mesh, role, &args, &request.expect, &request.msg_id)?;
        }
    }
    Ok(true)
}

/// Post, then acknowledge what it answers. A post the session no longer
/// wants (a later unit, a lost reviewer seat) is acknowledged unanswered.
fn post(
    mesh: &Mesh,
    role: &str,
    args: &PostArgs,
    expect: &Expect,
    ack: &str,
) -> Result<(), Failure> {
    match mesh.post_guarded(role, args, expect) {
        Ok(m) => {
            let id = m.get("msg_id").and_then(|v| v.as_str()).unwrap_or_default();
            println!("POSTED {} {id} reply_to={ack}", args.kind);
        }
        Err(MeshError::Refused(why)) if why.starts_with("STALE") => {
            println!("SKIPPED {ack}: {why}");
        }
        // Posted; only its bookkeeping is incomplete (spec M-7). Never re-sent.
        Err(e @ MeshError::PostedIncomplete { .. }) => {
            println!("{e}");
        }
        Err(e) => return Err(e.into()),
    }
    mesh.ack(role, ack)?;
    Ok(())
}

/// A model's answer as a post: one retry with the reason it was refused,
/// then an escalation that carries none of its text.
async fn judgement(judge: &mut dyn Judge, root: &Path, request: &Request) -> PostArgs {
    let mut feedback: Option<String> = None;
    for _ in 0..2 {
        let answer = judge
            .judge(request, feedback.as_deref())
            .await
            .map_err(|e| format!("the model turn failed: {e}"))
            .and_then(|text| parse_answer(&text))
            .and_then(|answer| validate(root, request, &answer));
        match answer {
            Ok(args) => return args,
            Err(why) => feedback = Some(why),
        }
    }
    escalation(request, feedback.as_deref().unwrap_or("no answer"))
}

/// Where a judgement comes from.
#[async_trait::async_trait]
pub(crate) trait Judge: Send {
    /// The model's final text for `request`; `feedback` says why its last
    /// answer was refused.
    async fn judge(&mut self, request: &Request, feedback: Option<&str>) -> anyhow::Result<String>;

    /// The model's final text after implementing `task` as the owner;
    /// `feedback` says why its last answer or request was refused.
    async fn implement(
        &mut self,
        task: &OwnerTask,
        feedback: Option<&str>,
    ) -> anyhow::Result<String>;
}

struct ModelJudge {
    mesh: Mesh,
    anchor: PathBuf,
    role: String,
    model: String,
    provider: Option<String>,
}

#[async_trait::async_trait]
impl Judge for ModelJudge {
    async fn judge(&mut self, request: &Request, feedback: Option<&str>) -> anyhow::Result<String> {
        let mut runtime = crate::session_cmd::build_runtime_with_store(
            &self.anchor,
            &self.model,
            self.provider.as_deref(),
            Profile::ReadOnly,
            true,
            Store::ephemeral(),
            false,
        )
        .await?;
        let handle = runtime.permission_engine_handle();
        let lease = SessionLease::acquire(self.mesh.clone(), &self.role, LEASE_TTL);
        // No standing command grants either: a listed command runs even
        // under `readonly`, and a review turn must not be able to run
        // anything that writes.
        handle.set(
            handle
                .snapshot()
                .with_allowed_commands(Vec::new())
                .with_lease(Arc::clone(&lease)),
        );
        let (events, rx) = broadcast::channel(1024);
        let tracer = tokio::spawn(trace(rx));
        let cancel = CancellationToken::new();
        let prompt = brief(&self.anchor, &self.role, request, feedback);
        let usage_scope = self.mesh.usage_scope(&self.role)?;
        let stop = runtime.run_turn(&prompt, &events, &cancel).await;
        self.record_turn_usage(runtime.current_turn_usage(), &usage_scope);
        drop(events);
        // The runtime may hold a sender of its own; never wait on it for long.
        let _ = tokio::time::timeout(Duration::from_millis(500), tracer).await;
        println!("  TURN ended {stop:?}");
        runtime
            .current_turn_assistant_text()
            .ok_or_else(|| anyhow::anyhow!("the turn ended ({stop:?}) without an answer"))
    }

    async fn implement(
        &mut self,
        task: &OwnerTask,
        feedback: Option<&str>,
    ) -> anyhow::Result<String> {
        // `bypass` so the owner can run its tests headless (every shell call
        // otherwise waits for a confirmation nobody can give). The session's
        // lease still decides: the moment this participant stops owning the
        // tree, every write and command above read-only is denied. File
        // tools keep the workspace boundary; shell commands are not
        // contained (see the docs). No standing grants and no MCP servers.
        let mut runtime = crate::session_cmd::build_runtime_with_store(
            &self.anchor,
            &self.model,
            self.provider.as_deref(),
            Profile::Bypass,
            true,
            Store::ephemeral(),
            false,
        )
        .await?;
        let handle = runtime.permission_engine_handle();
        let lease = SessionLease::acquire(self.mesh.clone(), &self.role, LEASE_TTL);
        handle.set(
            handle
                .snapshot()
                .with_allowed_commands(Vec::new())
                .with_lease(Arc::clone(&lease)),
        );
        let (events, rx) = broadcast::channel(1024);
        let tracer = tokio::spawn(trace(rx));
        let cancel = CancellationToken::new();
        let prompt = owner_brief(&self.anchor, &self.role, task, feedback);
        let usage_scope = self.mesh.usage_scope(&self.role)?;
        let stop = runtime.run_turn(&prompt, &events, &cancel).await;
        self.record_turn_usage(runtime.current_turn_usage(), &usage_scope);
        drop(events);
        // The runtime may hold a sender of its own; never wait on it for long.
        let _ = tokio::time::timeout(Duration::from_millis(500), tracer).await;
        println!("  TURN ended {stop:?}");
        runtime
            .current_turn_assistant_text()
            .ok_or_else(|| anyhow::anyhow!("the turn ended ({stop:?}) without an answer"))
    }
}

impl ModelJudge {
    fn record_turn_usage(&self, usage: localpilot_core::TokenUsage, scope: &(String, String)) {
        let report = localpilot_mesh::ops::UsageReport {
            report_id: uuid::Uuid::new_v4().to_string(),
            input_tokens: usage.input_tokens,
            output_tokens: usage.output_tokens,
            cache_creation_input_tokens: usage.cache_creation_input_tokens,
            cache_read_input_tokens: usage.cache_read_input_tokens,
            cost_microusd: None,
            limit_percent: None,
        };
        if let Err(e) = self
            .mesh
            .record_usage(&self.role, &report, "engine", Some(scope))
        {
            eprintln!("USAGE_NOT_RECORDED {e}");
        }
    }
}

/// What the owner's model is shown: the task, the recent conversation, any
/// findings to answer, and the one answer shape it may give.
fn owner_brief(anchor: &Path, role: &str, task: &OwnerTask, feedback: Option<&str>) -> String {
    let mut out = format!(
        "You are {role}, the owner of the current work unit in a pair-programming session on \
         the repository at {}. Do the work yourself: create and edit files in this repository \
         and run commands, such as its tests, to check it. Never touch the .pair-programming \
         directory; the engine sends and receives every protocol message.\n\nTask:\n{}\n",
        anchor.display(),
        bounded(&task.task, BODY_BUDGET)
    );
    // Local models invent absolute paths from other machines; name the
    // working directory and what is in it.
    out.push_str(&format!(
        "
Your working directory is the repository root, {}. Use paths relative to it (for          example `slug.py`), never an absolute path from anywhere else. It contains: {}.
",
        anchor.display(),
        tree_listing(anchor)
    ));
    if !task.context.is_empty() {
        out.push_str("\nRecent messages to you:\n");
        for line in &task.context {
            out.push_str(&format!("- {line}\n"));
        }
    }
    if !task.findings.is_empty() {
        out.push_str(&format!(
            "\nThis is round {}. Your reviewers asked for changes; address every finding:\n",
            task.round
        ));
        for f in &task.findings {
            out.push_str(&format!("{}\n", bounded(f, BODY_BUDGET)));
        }
    }
    out.push_str(
        "\nWhen the work is done and checked, reply with exactly one JSON object, and nothing \
         after it:\n{\"kind\": \"REVIEW_REQUEST\", \"body\": \"what you changed and how you checked it\"}\n\
         Do not list files or fingerprints; the engine adds them from the tree. If you cannot do \
         the task, reply {\"kind\": \"ESCALATE\", \"body\": \"why\"} instead.\n",
    );
    if let Some(why) = feedback {
        out.push_str(&format!(
            "\nYour previous answer was refused: {why}. Fix that, then answer again with one valid JSON object.\n"
        ));
    }
    out
}

/// One line per tool call and warning of a model turn, so a run can be read
/// back: which tools the model used, what failed, and why a turn stopped.
async fn trace(mut rx: broadcast::Receiver<localpilot_harness::RuntimeEvent>) {
    use localpilot_harness::RuntimeEvent;
    loop {
        match rx.recv().await {
            Ok(RuntimeEvent::ToolFinished {
                name,
                is_error,
                output,
                duration_ms,
                ..
            }) => {
                let first: String = output
                    .lines()
                    .next()
                    .unwrap_or_default()
                    .chars()
                    .take(160)
                    .collect();
                let status = if is_error { "error" } else { "ok" };
                println!("  TOOL {name} {status} {duration_ms}ms {first}");
            }
            Ok(RuntimeEvent::ToolStarted { name, detail, .. }) => {
                let detail: String = detail.chars().take(160).collect();
                println!("  CALL {name} {detail}");
            }
            Ok(RuntimeEvent::Warning(w)) => {
                let w: String = w.chars().take(300).collect();
                println!("  WARN {w}");
            }
            Ok(_) | Err(broadcast::error::RecvError::Lagged(_)) => {}
            Err(broadcast::error::RecvError::Closed) => return,
        }
    }
}

fn bounded(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_owned();
    }
    let head: String = text.chars().take(budget).collect();
    format!("{head}\n[... cut at {budget} characters]")
}

/// The repository's files (tracked and untracked, the mailbox excluded),
/// bounded, for an owner's brief.
fn tree_listing(anchor: &Path) -> String {
    let out = Command::new("git")
        .arg("-C")
        .arg(anchor)
        .args(["ls-files", "--cached", "--others", "--exclude-standard"])
        .output()
        .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
        .unwrap_or_default();
    let files: Vec<&str> = out
        .lines()
        .filter(|l| !l.starts_with(".pair-programming/"))
        .collect();
    match files.len() {
        0 => "no files yet".to_owned(),
        n if n > 60 => format!("{}, and {} more", files[..60].join(", "), n - 60),
        _ => files.join(", "),
    }
}

/// What the model is shown: the session's task, the message, for a review
/// the diff of the verified files, and the one answer shape it may give.
fn brief(anchor: &Path, role: &str, request: &Request, feedback: Option<&str>) -> String {
    let mut out = format!(
        "You are {role}, a participant in a pair-programming session on the repository at {}. \
         You may read files and run read-only commands; you cannot change anything.\n\n\
         The session's task:\n<<<\n{}\n>>>\n\n\
         {} sent {} {}:\n<<<\n{}\n>>>\n",
        anchor.display(),
        bounded(&request.task, BODY_BUDGET),
        request.from,
        request.kind,
        request.msg_id,
        bounded(&request.body, BODY_BUDGET)
    );
    let shape = match request.need {
        Need::Review => {
            out.push_str(&format!(
                "\nYou are a required reviewer. The engine has verified the request's fingerprints; the files under review are: {}.\n\nTheir diff against HEAD:\n```diff\n{}\n```\n\nJudge the change against the task as written, not only against its own tests: anything the task requires that the change does not do is a blocking finding, even when no test covers it.\n",
                request.files.join(", "),
                review_diff(anchor, &request.files)
            ));
            let anchors = hunk_anchors(anchor, &request.files);
            if !anchors.is_empty() {
                out.push_str(
                    "\nAnchors for the changed lines, as the engine read them. To cite lines, copy one of these objects exactly into the finding as \"anchor\" (its \"path\" must be the finding's \"file\"); the engine checks it when it writes your verdict. Never make one up:\n",
                );
                for a in &anchors {
                    out.push_str(&serde_json::to_string(a).unwrap_or_default());
                    out.push('\n');
                }
            }
            "{\"kind\": \"VERDICT\", \"decision\": \"AGREE\" or \"REVISE\", \"findings\": [{\"file\": \"path\", \"line\": 12, \"severity\": \"blocking\" or \"important\" or \"minor\", \"text\": \"what is wrong and why\", \"anchor\": optional, one of the anchors above}], \"body\": \"your summary\"}\n\
             Do not write the verdict header; the engine writes it from your findings. REVISE needs at least one finding. AGREE may not carry a blocking finding."
        }
        Need::Reply => {
            "{\"kind\": \"ANSWER\" or \"DESIGN_AGREED\" or \"CHALLENGE\" or \"QUESTION\" or \"ESCALATE\", \"body\": \"your reply\"}"
        }
    };
    out.push_str(&format!(
        "\nReply with exactly one JSON object, and nothing after it:\n{shape}\n"
    ));
    if let Some(why) = feedback {
        out.push_str(&format!(
            "\nYour previous answer was refused: {why}. Answer again with one valid JSON object.\n"
        ));
    }
    out
}

/// The most anchors a review brief lists.
const BRIEF_ANCHORS: usize = 40;

/// An anchor for each changed range on the new side of each file's diff
/// against HEAD, and for a file Git confirms is untracked, one for the whole
/// file. Every file is read through the evidence service's own bounded,
/// no-follow reads; a file it refuses, or one whose status Git cannot give,
/// gets no anchor.
fn hunk_anchors(anchor: &Path, files: &[String]) -> Vec<localpilot_mesh::evidence::Anchor> {
    use localpilot_mesh::evidence::{self as ev, Bounds};
    let bounds = Bounds::default();
    let git = |args: &[&str]| {
        Command::new("git")
            .arg("-C")
            .arg(anchor)
            .args(args)
            .output()
            .ok()
            .filter(|o| o.status.success())
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
    };
    let mut out = Vec::new();
    for file in files {
        if out.len() >= BRIEF_ANCHORS {
            break;
        }
        let untracked = git(&["ls-files", "--others", "--exclude-standard", "--", file])
            .is_some_and(|o| o.lines().any(|l| l == file));
        if untracked {
            if let Ok(a) = ev::anchor_file(anchor, file, &bounds) {
                out.push(a.anchor);
            }
            continue;
        }
        let Some(diff) = git(&["diff", "--no-ext-diff", "-U0", "HEAD", "--", file]) else {
            continue;
        };
        for (start, end) in diff.lines().filter_map(new_side) {
            if out.len() >= BRIEF_ANCHORS {
                return out;
            }
            if let Ok(a) = ev::anchor(anchor, file, start, end, &bounds) {
                out.push(a.anchor);
            }
        }
    }
    out
}

/// The new-side range of a `-U0` hunk header, `@@ -a[,b] +c[,d] @@`; none
/// for a hunk that only deletes.
fn new_side(line: &str) -> Option<(usize, usize)> {
    let rest = line.strip_prefix("@@ ")?;
    let plus = rest.split_whitespace().find(|w| w.starts_with('+'))?;
    let mut parts = plus[1..].splitn(2, ',');
    let start: usize = parts.next()?.parse().ok()?;
    let count: usize = match parts.next() {
        Some(c) => c.parse().ok()?,
        None => 1,
    };
    (count > 0 && start > 0).then(|| (start, start + count - 1))
}

/// The diff of each file against HEAD; a file Git does not track is shown
/// whole. Bounded, and said so when cut.
fn review_diff(anchor: &Path, files: &[String]) -> String {
    let mut out = String::new();
    for file in files {
        let diff = Command::new("git")
            .arg("-C")
            .arg(anchor)
            .args(["diff", "HEAD", "--", file])
            .output()
            .map(|o| String::from_utf8_lossy(&o.stdout).into_owned())
            .unwrap_or_default();
        if diff.trim().is_empty() {
            if let Ok(text) = std::fs::read_to_string(anchor.join(file)) {
                out.push_str(&format!("--- new file {file}\n{text}\n"));
            }
        } else {
            out.push_str(&diff);
        }
    }
    bounded(&out, DIFF_BUDGET)
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_mesh::ops::Expect;

    #[test]
    fn a_hunk_header_gives_its_new_side_range() {
        assert_eq!(new_side("@@ -3,2 +3,4 @@ fn x"), Some((3, 6)));
        assert_eq!(new_side("@@ -3 +3 @@"), Some((3, 3)));
        assert_eq!(new_side("@@ -3,2 +2,0 @@"), None);
        assert_eq!(new_side("+not a header"), None);
    }

    fn request(need: Need) -> Request {
        Request {
            need,
            msg_id: "claude:3".into(),
            kind: "REVIEW_REQUEST".into(),
            from: "claude".into(),
            body: "please".into(),
            task: "make a.txt say hi".into(),
            round: 1,
            files: vec!["a.txt".into()],
            expect: Expect {
                session_id: "s".into(),
                unit_id: Some("1-a".into()),
                reviewer: need == Need::Review,
                owner: false,
            },
        }
    }

    struct Scripted(Vec<anyhow::Result<String>>, Vec<Option<String>>);

    #[async_trait::async_trait]
    impl Judge for Scripted {
        async fn judge(&mut self, _r: &Request, feedback: Option<&str>) -> anyhow::Result<String> {
            self.1.push(feedback.map(str::to_owned));
            self.0.remove(0)
        }

        async fn implement(
            &mut self,
            _t: &OwnerTask,
            feedback: Option<&str>,
        ) -> anyhow::Result<String> {
            self.1.push(feedback.map(str::to_owned));
            self.0.remove(0)
        }
    }

    #[tokio::test]
    async fn a_refused_answer_is_retried_once_with_the_reason_then_escalated() {
        let mut judge = Scripted(
            vec![
                Ok("no json".into()),
                Ok("{\"kind\":\"VERDICT\",\"decision\":\"REVISE\",\"body\":\"x\"}".into()),
            ],
            Vec::new(),
        );
        let post = judgement(&mut judge, Path::new("."), &request(Need::Review)).await;
        assert_eq!(post.kind, "ESCALATE");
        assert_eq!(post.reply_to.as_deref(), Some("claude:3"));
        assert!(
            post.body.contains("REVISE needs at least one finding"),
            "{}",
            post.body
        );
        assert_eq!(judge.1[0], None);
        assert!(judge.1[1]
            .as_deref()
            .is_some_and(|f| f.contains("no JSON object")));
    }

    #[tokio::test]
    async fn a_valid_second_answer_is_posted() {
        let mut judge = Scripted(
            vec![
                Err(anyhow::anyhow!("provider down")),
                Ok("ok {\"kind\":\"VERDICT\",\"decision\":\"AGREE\",\"body\":\"fine\"}".into()),
            ],
            Vec::new(),
        );
        let post = judgement(&mut judge, Path::new("."), &request(Need::Review)).await;
        assert_eq!(post.kind, "VERDICT");
        assert_eq!(post.body, "AGREE round=1 blocking=0 important=0\nfine");
        assert!(judge.1[1]
            .as_deref()
            .is_some_and(|f| f.contains("provider down")));
    }

    #[test]
    fn the_brief_shows_the_message_the_shape_and_any_refusal() {
        let dir = tempfile::tempdir().unwrap();
        let b = brief(
            dir.path(),
            "localpilot",
            &request(Need::Review),
            Some("the body is empty"),
        );
        assert!(b.contains("claude sent REVIEW_REQUEST claude:3"));
        assert!(b.contains("The session's task:\n<<<\nmake a.txt say hi\n>>>"));
        assert!(b.contains("Judge the change against the task as written"));
        assert!(b.contains("\"decision\": \"AGREE\" or \"REVISE\""));
        assert!(b.contains("Do not write the verdict header"));
        assert!(b.contains("refused: the body is empty"));
        let r = brief(dir.path(), "localpilot", &request(Need::Reply), None);
        assert!(r.contains("\"DESIGN_AGREED\""));
        assert!(!r.contains("verdict header"));
        assert!(r.contains("make a.txt say hi"));
        assert!(!r.contains("Judge the change"));
        assert!(bounded(&"x".repeat(10), 4).ends_with("[... cut at 4 characters]"));
    }

    #[test]
    fn the_review_brief_lists_an_anchor_for_each_changed_range() {
        let dir = tempfile::tempdir().unwrap();
        let git = |args: &[&str]| {
            assert!(Command::new("git")
                .arg("-C")
                .arg(dir.path())
                .args(args)
                .output()
                .unwrap()
                .status
                .success());
        };
        git(&["init", "-q"]);
        git(&["config", "user.email", "t@example.invalid"]);
        git(&["config", "user.name", "t"]);
        git(&["config", "core.autocrlf", "false"]);
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        git(&["add", "a.txt"]);
        git(&["commit", "-qm", "base"]);
        std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\nFOUR\nfive\n").unwrap();
        let anchors = hunk_anchors(dir.path(), &["a.txt".to_owned()]);
        let ranges: Vec<(usize, usize)> = anchors.iter().map(|a| (a.start, a.end)).collect();
        assert_eq!(ranges, vec![(2, 2), (4, 5)]);
        // Each one checks out against the tree as the engine read it.
        let mut spent = 0;
        let bounds = localpilot_mesh::evidence::Bounds::default();
        for a in &anchors {
            assert_eq!(
                localpilot_mesh::evidence::verify(dir.path(), a, &bounds, &mut spent),
                localpilot_mesh::evidence::Check::Ok
            );
        }
        // Untracked files: the whole file, lines counted as the service
        // counts them, and only through its bounded, no-follow reads.
        std::fs::write(dir.path().join("new.txt"), "a\n\nb\n").unwrap();
        std::fs::write(dir.path().join("big.txt"), "x\n".repeat(600_000)).unwrap();
        let whole = hunk_anchors(dir.path(), &["new.txt".to_owned(), "big.txt".to_owned()]);
        assert_eq!(whole.len(), 1, "{whole:?}");
        assert_eq!(
            (whole[0].path.as_str(), whole[0].start, whole[0].end),
            ("new.txt", 1, 3)
        );
        // Neither tracked-and-unchanged nor untracked: nothing.
        std::fs::write(dir.path().join("a.txt"), "one\ntwo\nthree\nfour\n").unwrap();
        assert!(hunk_anchors(dir.path(), &["a.txt".to_owned()]).is_empty());
        std::fs::write(dir.path().join("a.txt"), "one\nTWO\nthree\nFOUR\nfive\n").unwrap();
        #[cfg(unix)]
        {
            let outside = tempfile::tempdir().unwrap();
            std::fs::write(outside.path().join("s.txt"), "secret\n").unwrap();
            std::os::unix::fs::symlink(outside.path().join("s.txt"), dir.path().join("l.txt"))
                .unwrap();
            assert!(hunk_anchors(dir.path(), &["l.txt".to_owned()]).is_empty());
        }
        let b = brief(dir.path(), "localpilot", &request(Need::Review), None);
        assert!(b.contains("Anchors for the changed lines"), "{b}");
        assert!(
            b.contains(&serde_json::to_string(&anchors[0]).unwrap()),
            "{b}"
        );
        assert!(b.contains("\"anchor\": optional"), "{b}");
    }
}
