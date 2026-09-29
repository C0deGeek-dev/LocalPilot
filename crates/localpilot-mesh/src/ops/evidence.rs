//! `evidence`: the read-only evidence service, as a participant asks for it.
//!
//! Each answer is a packet that names the session, the unit and the tree
//! state it was taken at, so a reader can tell what it was evidence of. The
//! diagnostics are a fixed set of Git reads, run as argv with no shell, a
//! time limit and an output limit, with Git's optional locks, filesystem
//! monitor, external diff and text conversion turned off. No build or test
//! command is run: that needs an OS sandbox, which this service does not have.

use std::io::Read;
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use serde_json::{json, Value};

use super::{refused, sid, str_of, Mesh, Obj};
use crate::error::MeshError;
use crate::evidence::{self, Anchor, Bounds, Pattern};
use crate::timefmt::utc_now;

/// How long one diagnostic may run.
const DIAGNOSTIC_TIME: Duration = Duration::from_secs(10);
/// The most output one diagnostic returns.
const DIAGNOSTIC_OUTPUT: usize = 256 << 10;

/// What a participant asks the evidence service for.
#[derive(Debug, Clone)]
pub enum EvidenceArgs {
    /// Find text in the tree.
    Locate {
        query: String,
        regex: bool,
        glob: Option<String>,
    },
    /// Pin lines of a file to their hash.
    Anchor {
        path: String,
        start: usize,
        end: usize,
    },
    /// Check anchors against the tree as it is now.
    Verify { anchors: Vec<Anchor> },
    /// The fixed Git reads, and whether each named path exists.
    Diagnostics { paths: Vec<String> },
}

impl Mesh {
    /// Answer an evidence request from participant `role` as a JSON packet.
    ///
    /// # Errors
    /// No active session, a role not in it, or a refused request.
    pub fn evidence(&self, role: &str, args: &EvidenceArgs) -> Result<Value, MeshError> {
        let s = self.require(role, true)?;
        let root = self.repo().to_path_buf();
        let bounds = Bounds::default();
        let no_vcs = self.no_vcs(&s)?;
        let (op, body) = match args {
            EvidenceArgs::Locate { query, regex, glob } => {
                let p = Pattern::new(query, *regex, &bounds).map_err(ev)?;
                let found = evidence::locate(&root, &p, glob.as_deref(), &bounds).map_err(ev)?;
                ("locate", to_value(&found))
            }
            EvidenceArgs::Anchor { path, start, end } => {
                let a = evidence::anchor(&root, path, *start, *end, &bounds).map_err(ev)?;
                ("anchor", to_value(&a))
            }
            EvidenceArgs::Verify { anchors } => {
                if anchors.len() > bounds.anchors {
                    return Err(refused(format!(
                        "at most {} anchors per request, not {}",
                        bounds.anchors,
                        anchors.len()
                    )));
                }
                let mut spent = 0;
                let checks: Vec<Value> = anchors
                    .iter()
                    .map(|a| {
                        let check = evidence::verify(&root, a, &bounds, &mut spent);
                        json!({"anchor": a, "check": to_value(&check)})
                    })
                    .collect();
                ("verify", json!({ "checks": checks }))
            }
            EvidenceArgs::Diagnostics { paths } => {
                ("diagnostics", diagnostics(&root, &s, no_vcs, paths))
            }
        };
        // Without version control the tree's digest would need a full,
        // unbounded read of the tree, so no packet computes one.
        let (head, head_unavailable) = if no_vcs {
            (
                Value::Null,
                Some("the session has no version control; evidence does not rescan the tree"),
            )
        } else {
            match git_text(&root, &["rev-parse", "--verify", "--quiet", "HEAD"]) {
                Ok(h) if !h.is_empty() => (json!(h), None),
                _ => (
                    Value::Null,
                    Some("git cannot resolve HEAD (unborn, or unreadable)"),
                ),
            }
        };
        let mut packet = json!({
            "op": op,
            "session_id": sid(&s),
            "unit_id": s.get("unit_id"),
            "work_unit": s.get("work_unit"),
            "head": head,
            "taken_at": utc_now(),
        });
        if let (Some(why), Some(p)) = (head_unavailable, packet.as_object_mut()) {
            p.insert("head_unavailable".into(), json!(why));
        }
        if let (Some(p), Some(b)) = (packet.as_object_mut(), body.as_object()) {
            for (k, v) in b {
                p.insert(k.clone(), v.clone());
            }
        }
        Ok(packet)
    }
}

fn ev(e: evidence::EvidenceError) -> MeshError {
    refused(e.to_string())
}

fn to_value<T: serde::Serialize>(v: &T) -> Value {
    serde_json::to_value(v).unwrap_or(Value::Null)
}

/// The fixed diagnostics. Without version control, the Git reads are
/// `unavailable`, never an empty answer.
fn diagnostics(root: &Path, s: &Obj, no_vcs: bool, paths: &[String]) -> Value {
    let exists: Vec<Value> = paths
        .iter()
        .map(|p| match evidence::checked_path(root, p) {
            Ok(full) => {
                let kind = match std::fs::symlink_metadata(&full) {
                    Ok(m) if m.is_file() => "file",
                    Ok(m) if m.is_dir() => "directory",
                    Ok(_) => "other",
                    Err(_) => "missing",
                };
                json!({"path": p, "exists": kind})
            }
            Err(e) => json!({"path": p, "refused": e.to_string()}),
        })
        .collect();
    if no_vcs {
        let unavailable = json!({"unavailable": "the session has no version control"});
        return json!({"status": unavailable, "diff_stat": unavailable, "exists": exists});
    }
    let status = bounded_git(root, &["status", "--porcelain=v1", "-z"]);
    let diff = match str_of(s, "base_head").filter(|b| !b.is_empty() && *b != "UNBORN") {
        Some(base) => bounded_git(
            root,
            &["diff", "--no-ext-diff", "--no-textconv", "--stat", base],
        ),
        None => json!({"unavailable": "the unit has no base commit"}),
    };
    json!({"status": status, "diff_stat": diff, "exists": exists})
}

/// Git as a read: argv only, no optional locks, no filesystem monitor, no
/// hooks, stdin closed.
fn git_command(root: &Path, args: &[&str]) -> Command {
    let mut cmd = Command::new("git");
    cmd.arg("-C")
        .arg(root)
        .args([
            "-c",
            "core.fsmonitor=false",
            "-c",
            "core.untrackedCache=false",
        ])
        .arg("--no-optional-locks")
        .args(args)
        .env("GIT_OPTIONAL_LOCKS", "0")
        .env_remove("GIT_EXTERNAL_DIFF")
        .stdin(Stdio::null())
        .stderr(Stdio::null())
        .stdout(Stdio::piped());
    cmd
}

fn git_text(root: &Path, args: &[&str]) -> Result<String, String> {
    let out = git_command(root, args)
        .output()
        .map_err(|e| e.to_string())?;
    Ok(String::from_utf8_lossy(&out.stdout).trim().to_owned())
}

/// Run one diagnostic within [`DIAGNOSTIC_TIME`] and [`DIAGNOSTIC_OUTPUT`].
fn bounded_git(root: &Path, args: &[&str]) -> Value {
    let mut child = match git_command(root, args).spawn() {
        Ok(c) => c,
        Err(e) => return json!({"error": format!("cannot run git: {e}")}),
    };
    let Some(mut stdout) = child.stdout.take() else {
        let _ = child.kill();
        return json!({"error": "no output pipe"});
    };
    let reader = std::thread::spawn(move || {
        let mut buf = Vec::new();
        let limit = u64::try_from(DIAGNOSTIC_OUTPUT + 1).unwrap_or(u64::MAX);
        let _ = (&mut stdout).take(limit).read_to_end(&mut buf);
        // Drain the rest so git is never blocked on a full pipe.
        let _ = std::io::copy(&mut stdout, &mut std::io::sink());
        buf
    });
    let started = Instant::now();
    let code = loop {
        match child.try_wait() {
            Ok(Some(st)) => break st.code(),
            Ok(None) if started.elapsed() >= DIAGNOSTIC_TIME => {
                let _ = child.kill();
                let _ = child.wait();
                return json!({"error": "timed out"});
            }
            Ok(None) => std::thread::sleep(Duration::from_millis(20)),
            Err(e) => return json!({"error": e.to_string()}),
        }
    };
    let mut out = reader.join().unwrap_or_default();
    let truncated = out.len() > DIAGNOSTIC_OUTPUT;
    out.truncate(DIAGNOSTIC_OUTPUT);
    json!({
        "exit": code,
        "output": String::from_utf8_lossy(&out),
        "truncated": truncated,
    })
}
