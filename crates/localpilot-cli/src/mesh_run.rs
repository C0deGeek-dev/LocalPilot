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

use std::path::{Path, PathBuf};
use std::process::{Command, ExitCode};
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use localpilot_mesh::ops::engine::{escalation, parse_answer, validate, Need, Request, Step};
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
    /// Seconds to wait for a session that names the role; 0 waits for ever.
    #[arg(long, default_value_t = 60)]
    timeout: u64,
    /// Seconds between looks for mail, from 0.05 to 3600.
    #[arg(long, default_value_t = 1.0, value_parser = poll_seconds)]
    poll: f64,
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
    let poll = Duration::from_secs_f64(cli.poll);
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
    let sid = mesh.engine_ready(&cli.role)?;
    println!("ENGINE role={} session={sid}", cli.role);
    loop {
        match mesh.receive(&cli.role, 900, Some(&sid))? {
            None if cli.once => return Ok(()),
            None => tokio::time::sleep(poll).await,
            Some(delivery) => {
                for step in mesh.plan(&cli.role, &delivery)? {
                    if !execute(mesh, &cli.role, step, judge).await? {
                        return Ok(());
                    }
                }
                if cli.once {
                    return Ok(());
                }
            }
        }
    }
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
        Step::Judge(request) => {
            let mut args = judgement(judge, &request).await;
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
        Err(e) => return Err(e.into()),
    }
    mesh.ack(role, ack)?;
    Ok(())
}

/// A model's answer as a post: one retry with the reason it was refused,
/// then an escalation that carries none of its text.
async fn judgement(judge: &mut dyn Judge, request: &Request) -> PostArgs {
    let mut feedback: Option<String> = None;
    for _ in 0..2 {
        let answer = judge
            .judge(request, feedback.as_deref())
            .await
            .map_err(|e| format!("the model turn failed: {e}"))
            .and_then(|text| parse_answer(&text))
            .and_then(|answer| validate(request, &answer));
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
        let (events, _keep) = broadcast::channel(1024);
        let cancel = CancellationToken::new();
        let prompt = brief(&self.anchor, &self.role, request, feedback);
        let stop = runtime.run_turn(&prompt, &events, &cancel).await;
        runtime
            .current_turn_assistant_text()
            .ok_or_else(|| anyhow::anyhow!("the turn ended ({stop:?}) without an answer"))
    }
}

fn bounded(text: &str, budget: usize) -> String {
    if text.chars().count() <= budget {
        return text.to_owned();
    }
    let head: String = text.chars().take(budget).collect();
    format!("{head}\n[... cut at {budget} characters]")
}

/// What the model is shown: the message, for a review the diff of the
/// verified files, and the one answer shape it may give.
fn brief(anchor: &Path, role: &str, request: &Request, feedback: Option<&str>) -> String {
    let mut out = format!(
        "You are {role}, a participant in a pair-programming session on the repository at {}. \
         You may read files and run read-only commands; you cannot change anything.\n\n\
         {} sent {} {}:\n<<<\n{}\n>>>\n",
        anchor.display(),
        request.from,
        request.kind,
        request.msg_id,
        bounded(&request.body, BODY_BUDGET)
    );
    let shape = match request.need {
        Need::Review => {
            out.push_str(&format!(
                "\nYou are a required reviewer. The engine has verified the request's fingerprints; the files under review are: {}.\n\nTheir diff against HEAD:\n```diff\n{}\n```\n",
                request.files.join(", "),
                review_diff(anchor, &request.files)
            ));
            "{\"kind\": \"VERDICT\", \"decision\": \"AGREE\" or \"REVISE\", \"findings\": [{\"file\": \"path\", \"line\": 12, \"severity\": \"blocking\" or \"important\" or \"minor\", \"text\": \"what is wrong and why\"}], \"body\": \"your summary\"}\n\
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

    fn request(need: Need) -> Request {
        Request {
            need,
            msg_id: "claude:3".into(),
            kind: "REVIEW_REQUEST".into(),
            from: "claude".into(),
            body: "please".into(),
            round: 1,
            files: vec!["a.txt".into()],
            expect: Expect {
                session_id: "s".into(),
                unit_id: Some("1-a".into()),
                reviewer: need == Need::Review,
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
        let post = judgement(&mut judge, &request(Need::Review)).await;
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
        let post = judgement(&mut judge, &request(Need::Review)).await;
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
        assert!(b.contains("\"decision\": \"AGREE\" or \"REVISE\""));
        assert!(b.contains("Do not write the verdict header"));
        assert!(b.contains("refused: the body is empty"));
        let r = brief(dir.path(), "localpilot", &request(Need::Reply), None);
        assert!(r.contains("\"DESIGN_AGREED\""));
        assert!(!r.contains("verdict header"));
        assert!(bounded(&"x".repeat(10), 4).ends_with("[... cut at 4 characters]"));
    }
}
