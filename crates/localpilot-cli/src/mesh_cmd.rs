//! `localpilot mesh`: LocalPilot's own participant in a pair-programming
//! mailbox shared with Claude Code and Codex.
//!
//! The arguments follow the reference `pair.py` command line for the
//! operations a participant performs, so the same conformance suite drives
//! either implementation. Creating, parking, resuming and retiring sessions
//! belong to a full implementation and are refused here with exit 2.
//!
//! Exit codes: 0 success; 1 a refusal or a watch that timed out; 2 a usage
//! error or an operation outside the participant profile; 4 a denied write
//! (`guard-write`); 5 a refused delivery-plumbing request.
//!
//! `[mesh] writer = "delegate"` (user config or environment only; a project's
//! `.localpilot.toml` cannot set it) is the rollback: every participant operation
//! is handed, arguments unchanged, to `delegate_command` (normally the
//! reference `pair.py`), and its exit code is returned. There is no fallback
//! to the native writer when the delegate is missing or fails to start: the
//! switch exists because the native writer is distrusted.

use std::ffi::OsString;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::ExitCode;
use std::time::Duration;

use clap::{Args, Parser, Subcommand};
use localpilot_mesh::ops::{EndpointArgs, NextUnitArgs, PostArgs, WatchArgs};
use localpilot_mesh::{Mesh, MeshError, Out};

/// Operations of the reference implementation that a participant does not
/// provide.
const FULL_ONLY: &[&str] = &[
    "start",
    "phase",
    "verify-request",
    "abandon",
    "purge",
    "park",
    "resume",
];

#[derive(Debug, Args)]
pub(crate) struct MeshArgs {
    /// The anchor working tree; defaults to `PAIR_REPO`, then the current
    /// directory.
    #[arg(long)]
    pub(crate) repo: Option<PathBuf>,
    /// The operation and its arguments, as for `pair.py`; or `run` and its
    /// options, for the participant engine.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, num_args = 0..)]
    pub(crate) rest: Vec<OsString>,
}

#[derive(Debug, Parser)]
#[command(name = "localpilot mesh", no_binary_name = true)]
struct OpCli {
    #[command(subcommand)]
    op: Op,
}

#[derive(Debug, Subcommand)]
enum Op {
    Join {
        #[arg(long)]
        role: String,
        #[arg(long, default_value_t = 0)]
        timeout: u64,
        #[arg(long, default_value_t = 1.0)]
        poll: f64,
    },
    Post {
        #[arg(long)]
        role: String,
        #[arg(long)]
        kind: String,
        #[arg(long)]
        body: Option<String>,
        #[arg(long)]
        body_file: Option<PathBuf>,
        #[arg(long)]
        expect_reply: bool,
        #[arg(long)]
        ack_through: Option<String>,
        #[arg(long)]
        to: Option<String>,
        #[arg(long)]
        reply_to: Option<String>,
        #[arg(long)]
        broadcast: bool,
        #[arg(long)]
        forward: bool,
        /// Who acted (self-asserted; never identity or authority).
        #[arg(long, value_parser = ["human"])]
        actor: Option<String>,
    },
    Watch(WatchOpts),
    Peek(WatchOpts),
    Ack {
        #[arg(long)]
        role: String,
        #[arg(long)]
        through: String,
    },
    Health {
        #[arg(long)]
        role: String,
        #[arg(long)]
        status: String,
        #[arg(long)]
        reason: Option<String>,
        #[arg(long)]
        resume_at: Option<String>,
    },
    Status,
    Transcript {
        #[arg(long)]
        session: Option<String>,
    },
    GuardWrite {
        #[arg(long)]
        role: String,
        #[arg(long)]
        path: Option<PathBuf>,
    },
    HandoffOffer {
        #[arg(long)]
        role: String,
        #[arg(long)]
        to: Option<String>,
    },
    HandoffAccept {
        #[arg(long)]
        role: String,
        #[arg(long, value_parser = ["human"])]
        actor: Option<String>,
    },
    HandoffDecline {
        #[arg(long)]
        role: String,
        #[arg(long, value_parser = ["human"])]
        actor: Option<String>,
    },
    HandoffWithdraw {
        #[arg(long)]
        role: String,
        #[arg(long, value_parser = ["human"])]
        actor: Option<String>,
    },
    Complete {
        #[arg(long)]
        role: String,
    },
    Endpoint {
        #[arg(long)]
        role: String,
        #[arg(
            long,
            required_unless_present = "unregister",
            conflicts_with = "unregister"
        )]
        register: bool,
        #[arg(long)]
        unregister: bool,
        #[arg(long)]
        transport: Option<String>,
        #[arg(long)]
        address: Option<String>,
        /// Seconds until the registration expires.
        #[arg(long)]
        ttl: Option<i64>,
    },
    Accept {
        #[arg(long)]
        role: String,
        #[arg(long)]
        msg_id: String,
        #[arg(long, allow_hyphen_values = true)]
        generation: i64,
    },
    RecordPush {
        #[arg(long)]
        role: String,
        #[arg(long)]
        msg_id: String,
        #[arg(long)]
        to: String,
        #[arg(long, allow_hyphen_values = true)]
        generation: i64,
        #[arg(long, value_parser = ["sent", "refused", "failed", "timeout"])]
        outcome: String,
    },
    NextUnit {
        #[arg(long)]
        role: String,
        #[arg(long)]
        task: Option<String>,
        #[arg(long)]
        task_file: Option<PathBuf>,
        #[arg(long)]
        work_unit: Option<String>,
        #[arg(long)]
        advisers: Option<String>,
    },
}

#[derive(Debug, Args)]
struct WatchOpts {
    #[arg(long)]
    role: String,
    #[arg(long, default_value_t = 0)]
    timeout: u64,
    #[arg(long, default_value_t = 1.0)]
    poll: f64,
    #[arg(long, default_value_t = 900)]
    stale_after: i64,
    #[arg(long)]
    ack_through: Option<String>,
}

/// Whether the user's configuration selects the native writer. Read from the
/// user config and the environment only, as `run` reads it.
///
/// # Errors
/// The configuration does not load.
pub(crate) fn native_writer() -> Result<bool, String> {
    let paths = localpilot_config::ConfigPaths {
        user: localpilot_config::user_config_path(),
        project: None,
    };
    localpilot_config::load(&paths, &localpilot_config::CliOverrides::default())
        .map(|c| c.mesh.writer != localpilot_config::MeshWriter::Delegate)
        .map_err(|e| format!("localpilot mesh: cannot load configuration: {e}"))
}

/// Run one mesh operation, writing its output, and return its exit code.
pub(crate) async fn run(args: MeshArgs) -> ExitCode {
    let name = args
        .rest
        .first()
        .map(|s| s.to_string_lossy().into_owned())
        .unwrap_or_default();
    if FULL_ONLY.contains(&name.as_str()) {
        eprintln!(
            "localpilot mesh: `{name}` is not provided by the participant profile; use pair.py"
        );
        return ExitCode::from(2);
    }
    let op = match OpCli::try_parse_from(&args.rest) {
        Ok(cli) => cli.op,
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
    // The writer, and above all the delegate's command line, come from the
    // user's own config and environment only. A repository's
    // `.localpilot.toml` is not trusted to choose a program this command runs:
    // otherwise `localpilot mesh status` in a cloned repository could execute
    // whatever that repository names.
    let paths = localpilot_config::ConfigPaths {
        user: localpilot_config::user_config_path(),
        project: None,
    };
    let config = match localpilot_config::load(&paths, &localpilot_config::CliOverrides::default())
    {
        Ok(c) => c.mesh,
        Err(e) => {
            // Never guess the writer: a delegate configured in a broken file
            // must not quietly become the native writer.
            eprintln!("localpilot mesh: cannot load configuration: {e}");
            return ExitCode::from(1);
        }
    };
    if config.writer == localpilot_config::MeshWriter::Delegate {
        // Parsed above only to keep the delegate to participant operations.
        drop(op);
        return delegate(&config.delegate_command, &anchor, &args.rest);
    }
    let mesh = Mesh::at(&anchor, source);
    let result = dispatch(&mesh, op);
    // Whatever the operation appended is pushed once it holds no lock, even
    // if it then failed: the message is in the journal (spec P-3).
    crate::mesh_push::push_all(&mesh).await;
    match result {
        Ok(code) => ExitCode::from(code),
        // Spec M-7: the message is posted; exit 6 tells the caller not to
        // post it again.
        Err(e @ MeshError::PostedIncomplete { .. }) => {
            eprintln!("{e}");
            ExitCode::from(6)
        }
        Err(e) => {
            eprintln!("{e} [anchor={} source={source}]", anchor.display());
            ExitCode::from(1)
        }
    }
}

/// The command line a delegate runs: its argv exactly as configured, then
/// `--repo <anchor>`, then the operation's arguments verbatim.
fn delegate_argv(command: &[String], anchor: &Path, rest: &[OsString]) -> Option<Vec<OsString>> {
    let (program, lead) = command.split_first()?;
    if program.is_empty() {
        return None;
    }
    let mut argv: Vec<OsString> = vec![program.into()];
    argv.extend(lead.iter().map(OsString::from));
    argv.push("--repo".into());
    argv.push(anchor.as_os_str().to_owned());
    argv.extend(rest.iter().cloned());
    Some(argv)
}

/// Run the operation on the delegate, with this process's stdio, and return
/// its exit code.
fn delegate(command: &[String], anchor: &Path, rest: &[OsString]) -> ExitCode {
    let Some(argv) = delegate_argv(command, anchor, rest) else {
        eprintln!(
            "localpilot mesh: [mesh] writer is \"delegate\" but delegate_command is empty; \
             set it to the delegate's argv, for example [\"python\", \"<path>/pair.py\"]"
        );
        return ExitCode::from(2);
    };
    match std::process::Command::new(&argv[0])
        .args(&argv[1..])
        .status()
    {
        Ok(status) => ExitCode::from(
            status
                .code()
                .and_then(|c| u8::try_from(c).ok())
                .unwrap_or(1),
        ),
        Err(e) => {
            eprintln!(
                "localpilot mesh: cannot start the delegate {:?} ([mesh] delegate_command): {e}",
                argv[0]
            );
            ExitCode::from(2)
        }
    }
}

fn poll(secs: f64) -> Duration {
    Duration::try_from_secs_f64(secs).unwrap_or(Duration::from_secs(1))
}

fn dispatch(mesh: &Mesh, op: Op) -> Result<u8, MeshError> {
    let out = match op {
        Op::Join {
            role,
            timeout,
            poll: p,
        } => mesh.join(&role, timeout, poll(p))?,
        Op::Post {
            role,
            kind,
            body,
            body_file,
            expect_reply,
            ack_through,
            to,
            reply_to,
            broadcast,
            forward,
            actor,
        } => {
            let body = text_arg(body_file, body)?;
            let a = PostArgs {
                kind,
                body,
                expect_reply,
                to,
                reply_to,
                broadcast,
                forward,
                ack_through,
                extra: actor
                    .map(|v| {
                        [("actor".to_owned(), serde_json::Value::String(v))]
                            .into_iter()
                            .collect()
                    })
                    .unwrap_or_default(),
            };
            mesh.post(&role, &a)?
        }
        Op::Watch(w) => return watch(mesh, w, true),
        Op::Peek(w) => return watch(mesh, w, false),
        Op::Ack { role, through } => mesh.ack(&role, &through)?,
        Op::Health {
            role,
            status,
            reason,
            resume_at,
        } => mesh.health(&role, &status, reason.as_deref(), resume_at.as_deref())?,
        Op::Status => mesh.status()?,
        Op::Transcript { session } => mesh.transcript(session.as_deref())?,
        Op::GuardWrite { role, path } => mesh.guard_write(&role, path.as_deref())?,
        Op::HandoffOffer { role, to } => mesh.handoff_offer(&role, to.as_deref())?,
        Op::HandoffAccept { role, actor } => mesh.handoff_accept_as(&role, actor.as_deref())?,
        Op::HandoffDecline { role, actor } => mesh.handoff_decline(&role, actor.as_deref())?,
        Op::HandoffWithdraw { role, actor } => mesh.handoff_withdraw(&role, actor.as_deref())?,
        Op::Complete { role } => mesh.complete(&role)?,
        Op::Endpoint {
            role,
            unregister,
            transport,
            address,
            ttl,
            ..
        } => {
            let a = if unregister {
                EndpointArgs::Unregister
            } else {
                EndpointArgs::Register {
                    transport,
                    address,
                    ttl,
                }
            };
            mesh.endpoint(&role, &a)?
        }
        Op::Accept {
            role,
            msg_id,
            generation,
        } => mesh.accept(&role, &msg_id, generation)?,
        Op::RecordPush {
            role,
            msg_id,
            to,
            generation,
            outcome,
        } => mesh.record_push(&role, &msg_id, &to, generation, &outcome)?,
        Op::NextUnit {
            role,
            task,
            task_file,
            work_unit,
            advisers,
        } => {
            let a = NextUnitArgs {
                task: text_arg(task_file, task)?,
                work_unit,
                advisers,
            };
            mesh.next_unit(&role, &a)?
        }
    };
    Ok(emit(&out))
}

/// A text given inline or as a file; the file wins, as in the reference.
fn text_arg(file: Option<PathBuf>, inline: Option<String>) -> Result<String, MeshError> {
    match (file, inline) {
        (Some(f), _) => std::fs::read_to_string(&f)
            .map_err(|e| MeshError::Refused(format!("cannot read {}: {e}", f.display()))),
        (None, Some(b)) => Ok(b),
        (None, None) => Err(MeshError::Refused("body required".into())),
    }
}

fn watch(mesh: &Mesh, w: WatchOpts, block: bool) -> Result<u8, MeshError> {
    let a = WatchArgs {
        block,
        timeout: w.timeout,
        poll: poll(w.poll),
        stale_after: w.stale_after,
        ack_through: w.ack_through,
    };
    let mut stdout = std::io::stdout();
    mesh.watch(&w.role, &a, &mut |text| {
        writeln!(stdout, "{text}")?;
        stdout.flush()
    })
}

fn emit(out: &Out) -> u8 {
    print!("{}", out.stdout);
    eprint!("{}", out.stderr);
    let _ = std::io::stdout().flush();
    out.code
}

/// The anchor tree, as `localpilot mesh` and `doctor` both resolve it.
pub(crate) fn resolve_anchor(repo: Option<&Path>) -> Result<(PathBuf, &'static str), String> {
    localpilot_mesh::anchor::resolve(repo)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn parse(args: &[&str]) -> Result<Op, clap::Error> {
        OpCli::try_parse_from(args).map(|c| c.op)
    }

    #[test]
    fn participant_arguments_follow_the_reference_command_line() {
        let op = parse(&[
            "post",
            "--role",
            "localpilot",
            "--kind",
            "ANSWER",
            "--body",
            "yes",
            "--reply-to",
            "codex:3",
            "--ack-through",
            "codex:3",
        ])
        .unwrap();
        assert!(
            matches!(op, Op::Post { ref reply_to, .. } if reply_to.as_deref() == Some("codex:3"))
        );
        assert!(matches!(
            parse(&["watch", "--role", "codex", "--timeout", "5"]).unwrap(),
            Op::Watch(WatchOpts {
                timeout: 5,
                stale_after: 900,
                ..
            })
        ));
        assert!(matches!(parse(&["status"]).unwrap(), Op::Status));
    }

    #[test]
    fn the_delegate_gets_its_argv_exactly_then_the_anchor_then_the_operation() {
        let cmd = vec![
            "py".to_owned(),
            "-3".to_owned(),
            "C:/Program Files/p/pair.py".to_owned(),
        ];
        let rest: Vec<OsString> = ["post", "--body", "a b"]
            .iter()
            .map(OsString::from)
            .collect();
        let argv = delegate_argv(&cmd, Path::new("D:/anchor with space"), &rest).unwrap();
        let want: Vec<OsString> = [
            "py",
            "-3",
            "C:/Program Files/p/pair.py",
            "--repo",
            "D:/anchor with space",
            "post",
            "--body",
            "a b",
        ]
        .iter()
        .map(OsString::from)
        .collect();
        assert_eq!(argv, want);
        assert!(delegate_argv(&[], Path::new("x"), &rest).is_none());
        assert!(delegate_argv(&[String::new()], Path::new("x"), &rest).is_none());
    }

    #[test]
    fn an_unknown_operation_is_a_usage_error() {
        let err = parse(&["dance", "--role", "codex"]).unwrap_err();
        assert_eq!(err.exit_code(), 2);
    }
}
