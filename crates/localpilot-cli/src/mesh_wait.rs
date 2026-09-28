//! `localpilot mesh wait`: a one-shot waiter a host runs in place of
//! `pair.py watch` (spec §8b, the host side of a push).
//!
//! It registers the role's delivery endpoint from its own process, looks at
//! the mailbox, and then waits for a wake (or the safety poll) instead of
//! polling every second. With mail it prints exactly what `watch` prints and
//! exits 0; at its timeout it exits 1, silent, as `watch` does. It retires the
//! endpoint on the way out. If it cannot hold the endpoint (another process
//! holds it live, or a crashed waiter's lease has not yet run out), it says so
//! on stderr and behaves as a plain polling watch, so it is never worse than
//! `watch`.
//!
//! `--nudge-only` is for a notifier that must not read the mail itself: it
//! moves no cursor and writes no receipt, and exits 0 with one line,
//! `NUDGE sender:N[,sender:N]`, once some sender's newest unacknowledged
//! message to the role is above that sender's `--after` value.

use std::process::ExitCode;
use std::sync::Arc;
use std::time::Duration;

use clap::Parser;
use localpilot_mesh::ops::WatchArgs;
use localpilot_mesh::Mesh;
use tokio::sync::Notify;

use crate::mesh_cmd::{native_writer, resolve_anchor, MeshArgs};
use crate::mesh_listen::Listening;

#[derive(Debug, Parser)]
#[command(name = "localpilot mesh wait", no_binary_name = true)]
struct WaitCli {
    /// The participant waiting, e.g. `claude` or `codex`.
    #[arg(long)]
    role: String,
    /// Seconds before giving up (exit 1); 0 waits for ever.
    #[arg(long, default_value_t = 0)]
    timeout: u64,
    /// Seconds between looks at the mailbox while no wake arrives.
    #[arg(long, default_value_t = 30.0)]
    poll: f64,
    /// Acknowledge through this point first, as `watch --ack-through` does.
    #[arg(long, conflicts_with = "nudge_only")]
    ack_through: Option<String>,
    /// Read nothing: report newer unacknowledged mail, move no cursor.
    #[arg(long)]
    nudge_only: bool,
    /// With `--nudge-only`: `sender:N[,sender:N]`, the mail already nudged
    /// about; only a newer message ends the wait.
    #[arg(long, requires = "nudge_only")]
    after: Option<String>,
}

/// Whether these mesh arguments ask for the waiter.
pub(crate) fn is_wait(args: &MeshArgs) -> bool {
    args.rest.first().is_some_and(|op| op == "wait")
}

/// Run the waiter: 0 with mail (or a nudge), 1 at the timeout or on an error,
/// 2 on a usage error.
pub(crate) async fn run(args: MeshArgs) -> ExitCode {
    let cli = match WaitCli::try_parse_from(&args.rest[1..]) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let after = match parse_after(cli.after.as_deref()) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("localpilot mesh wait: {msg}");
            return ExitCode::from(2);
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
            eprintln!("localpilot mesh wait needs the native writer; it does not run under [mesh] writer = \"delegate\"");
            return ExitCode::from(2);
        }
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    }
    let mesh = Mesh::at(&anchor, source);
    let sid = match mesh.session_for(&cli.role) {
        Ok(s) => s,
        Err(e) => {
            eprintln!("{e} [anchor={}]", anchor.display());
            return ExitCode::from(1);
        }
    };
    let wake = Arc::new(Notify::new());
    let listening =
        match Listening::start_with(&mesh, &cli.role, &sid, wake.clone(), !cli.nudge_only).await {
            Ok(l) => Some(l),
            Err(why) => {
                eprintln!("WARN not listening ({why}); waiting by polling");
                None
            }
        };
    let code = tokio::select! {
        c = wait(&mesh, &cli, &sid, &after, &wake) => c,
        _ = tokio::signal::ctrl_c() => 1,
    };
    if let Some(l) = listening {
        l.stop(&mesh).await;
    }
    ExitCode::from(code)
}

/// The waiting loop; returns the exit code. Every look is bound to `sid`,
/// the session the waiter registered in: if another session becomes active,
/// the next look refuses (`SESSION_SWITCHED`) and the waiter stops, moving
/// nothing in the new session.
async fn wait(mesh: &Mesh, cli: &WaitCli, sid: &str, after: &[(String, i64)], wake: &Notify) -> u8 {
    let deadline =
        (cli.timeout > 0).then(|| tokio::time::Instant::now() + Duration::from_secs(cli.timeout));
    let poll = Duration::try_from_secs_f64(cli.poll.clamp(0.05, 3600.0))
        .unwrap_or(Duration::from_secs(30));
    let mut ack_through = cli.ack_through.clone();
    loop {
        let look = if cli.nudge_only {
            nudge(mesh, &cli.role, sid, after).await
        } else {
            deliver(mesh, &cli.role, sid, ack_through.take()).await
        };
        match look {
            Ok(true) => return 0,
            Ok(false) => {}
            Err(e) => {
                eprintln!("{e}");
                return 1;
            }
        }
        let nap = match deadline {
            Some(d) => {
                let now = tokio::time::Instant::now();
                if now >= d {
                    return 1;
                }
                poll.min(d - now)
            }
            None => poll,
        };
        tokio::select! {
            () = wake.notified() => {}
            () = tokio::time::sleep(nap) => {}
        }
    }
}

/// One look with `watch` semantics; `true` when mail was printed.
async fn deliver(
    mesh: &Mesh,
    role: &str,
    sid: &str,
    ack_through: Option<String>,
) -> Result<bool, String> {
    let (mesh, role, sid) = (mesh.clone(), role.to_owned(), sid.to_owned());
    tokio::task::spawn_blocking(move || {
        if let Some(through) = ack_through {
            mesh.ack_in(&role, &through, &sid)
                .map_err(|e| e.to_string())?;
        }
        // `receive` is `watch`'s one look, bound to `sid`: it refuses rather
        // than deliver from, or move a cursor in, a session that replaced it.
        let stale_after = WatchArgs::default().stale_after;
        match mesh
            .receive(&role, stale_after, Some(&sid))
            .map_err(|e| e.to_string())?
        {
            // One line, as `watch` writes each delivery.
            Some(delivery) => {
                println!("{}", delivery.render());
                Ok(true)
            }
            None => Ok(false),
        }
    })
    .await
    .map_err(|e| e.to_string())?
}

/// One read-only look; `true` when a nudge was printed.
async fn nudge(
    mesh: &Mesh,
    role: &str,
    sid: &str,
    after: &[(String, i64)],
) -> Result<bool, String> {
    let (mesh, role, sid) = (mesh.clone(), role.to_owned(), sid.to_owned());
    let heads = tokio::task::spawn_blocking(move || mesh.unacknowledged_heads(&role, &sid))
        .await
        .map_err(|e| e.to_string())?
        .map_err(|e| e.to_string())?;
    let floor = |sender: &str| {
        after
            .iter()
            .find(|(s, _)| s == sender)
            .map_or(0, |(_, n)| *n)
    };
    if !heads.iter().any(|(s, n)| *n > floor(s)) {
        return Ok(false);
    }
    let line: Vec<String> = heads.iter().map(|(s, n)| format!("{s}:{n}")).collect();
    println!("NUDGE {}", line.join(","));
    Ok(true)
}

/// `sender:N[,sender:N]` as pairs.
fn parse_after(raw: Option<&str>) -> Result<Vec<(String, i64)>, String> {
    let Some(raw) = raw.filter(|r| !r.trim().is_empty()) else {
        return Ok(Vec::new());
    };
    raw.split(',')
        .map(|part| {
            let (s, n) = part
                .split_once(':')
                .ok_or_else(|| format!("--after wants sender:N, not {part:?}"))?;
            let n: i64 = n
                .parse()
                .map_err(|_| format!("--after wants a number after {s}:, not {n:?}"))?;
            if s.is_empty() || n < 0 {
                return Err(format!("--after wants sender:N, not {part:?}"));
            }
            Ok((s.to_owned(), n))
        })
        .collect()
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used, clippy::expect_used)]

    use super::*;

    #[test]
    fn the_after_list_parses_and_refuses_bad_parts() {
        assert_eq!(parse_after(None).unwrap(), vec![]);
        assert_eq!(
            parse_after(Some("claude:3,localpilot:1")).unwrap(),
            vec![("claude".to_owned(), 3), ("localpilot".to_owned(), 1)]
        );
        for bad in ["claude", "claude:x", ":3", "claude:-1"] {
            assert!(parse_after(Some(bad)).is_err(), "{bad}");
        }
    }
}
