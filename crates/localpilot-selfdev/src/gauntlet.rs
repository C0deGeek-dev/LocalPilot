//! The gate a candidate binary must pass before a channel is allowed to point
//! at it: refuse to promote a stale or broken build.
//!
//! Three checks, in order of cost:
//!
//! 1. **Identity.** The candidate reports its embedded build identity
//!    (`version --json`); its commit hash and source fingerprint must equal the
//!    tree it was supposed to be built from. A binary that does not match its
//!    source is stale by definition.
//! 2. **Freshness.** The source tree is re-read *after* the build. If it changed
//!    while the build ran, the candidate is already behind the tree it claims,
//!    and is superseded rather than promotable.
//! 3. **Handshake.** The candidate is spawned in RPC mode and must complete a
//!    real init round-trip within a deadline — proof it can construct its config,
//!    provider, tools, and session and answer on the wire, not merely print a
//!    version string.
//!
//! The identity and freshness checks are pure and unit-tested here. The two that
//! must run a real binary — reading the reported identity and the handshake — are
//! exercised by the crate's behaviour verification against a freshly built
//! `localpilot`, because a unit test cannot conjure one.

use std::io::{BufRead, BufReader, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::sync::mpsc;
use std::time::Duration;

use serde::Deserialize;

use crate::error::SelfDevError;
use crate::source::SourceState;

/// How long the handshake smoke test waits for the candidate to answer.
pub const DEFAULT_HANDSHAKE_TIMEOUT: Duration = Duration::from_secs(30);

/// Provider and model names the throwaway smoke config uses. They exist only so
/// the RPC session can be constructed; the handshake never contacts them.
const SMOKE_PROVIDER: &str = "selfdev-smoke";
const SMOKE_MODEL: &str = "selfdev-smoke";

/// Run the whole gauntlet against `executable`, built from `expected`.
///
/// Returns `Ok` only when the candidate's embedded identity matches the source
/// it was built from, the source has not changed since, and the candidate
/// completes a real RPC handshake. `scratch_dir` is a caller-owned working
/// directory the throwaway smoke config is written into.
///
/// # Errors
/// Returns the first check to fail — a stale identity, a superseded tree, or a
/// broken handshake — so a channel is never pointed at a bad build.
pub fn vet(
    executable: &Path,
    expected: &SourceState,
    scratch_dir: &Path,
    timeout: Duration,
) -> Result<ReportedIdentity, SelfDevError> {
    let reported = read_reported_identity(executable)?;
    check_identity(&reported, expected)?;
    // Re-read the tree the build came from; a change mid-build supersedes it.
    let after = SourceState::read(&expected.root)?;
    check_fresh(expected, &after)?;
    write_smoke_config(scratch_dir, SMOKE_PROVIDER, SMOKE_MODEL)?;
    smoke_handshake(
        executable,
        scratch_dir,
        SMOKE_PROVIDER,
        SMOKE_MODEL,
        timeout,
    )?;
    Ok(reported)
}

/// The build identity a candidate reports through `version --json`.
#[derive(Debug, Clone, PartialEq, Eq, Deserialize)]
pub struct ReportedIdentity {
    /// The version string the binary reports.
    pub version: String,
    /// The commit hash embedded at build time.
    pub git_hash: String,
    /// The source fingerprint embedded at build time.
    pub fingerprint: String,
}

/// Confirm a reported identity matches the source it should have been built from.
///
/// Both the commit hash and the fingerprint must match: the hash alone would
/// pass a dirty rebuild of a different working tree at the same commit, which is
/// exactly the stale-binary case the fingerprint exists to catch.
///
/// # Errors
/// Returns [`SelfDevError::Invalid`] naming the first field that disagrees.
pub fn check_identity(
    reported: &ReportedIdentity,
    expected: &SourceState,
) -> Result<(), SelfDevError> {
    if reported.git_hash != expected.embedded_hash() {
        return Err(SelfDevError::Invalid(format!(
            "candidate reports git hash {:?} but was built from {:?}",
            reported.git_hash,
            expected.embedded_hash()
        )));
    }
    if reported.fingerprint != expected.fingerprint {
        return Err(SelfDevError::Invalid(format!(
            "candidate reports fingerprint {:?} but the source fingerprint is {:?}",
            reported.fingerprint, expected.fingerprint
        )));
    }
    Ok(())
}

/// Confirm the source tree did not change while the build ran.
///
/// # Errors
/// Returns [`SelfDevError::Invalid`] when the fingerprint moved between `before`
/// and `after` — the candidate is behind the tree it claims and is superseded.
pub fn check_fresh(before: &SourceState, after: &SourceState) -> Result<(), SelfDevError> {
    if before.fingerprint != after.fingerprint {
        return Err(SelfDevError::Invalid(format!(
            "the source tree changed during the build ({} -> {}); the candidate is superseded",
            &before.version_label, &after.version_label
        )));
    }
    Ok(())
}

/// Ask a candidate binary for its embedded build identity.
///
/// # Errors
/// Returns [`SelfDevError::Build`] when the binary cannot be run or does not
/// emit the `version --json` contract.
pub fn read_reported_identity(executable: &Path) -> Result<ReportedIdentity, SelfDevError> {
    let output = Command::new(executable)
        .args(["version", "--json"])
        .stdin(Stdio::null())
        .output()
        .map_err(|error| SelfDevError::Build {
            status: "not started".to_string(),
            detail: format!("running `version --json`: {error}"),
        })?;
    if !output.status.success() {
        return Err(SelfDevError::Build {
            status: output
                .status
                .code()
                .map_or("signal".into(), |c| c.to_string()),
            detail: "candidate exited non-zero for `version --json`".to_string(),
        });
    }
    let text = String::from_utf8_lossy(&output.stdout);
    serde_json::from_str(text.trim()).map_err(|error| SelfDevError::Build {
        status: "0".to_string(),
        detail: format!("candidate did not emit the version --json contract: {error}"),
    })
}

/// Spawn a candidate in RPC mode and require a real init round-trip.
///
/// The candidate is treated as a black box spoken to over its published wire
/// protocol (newline-delimited JSON): a `hello` command in, a `hello` event out.
/// A dedicated reader thread feeds parsed lines to a channel so the wait can be
/// bounded — a hung candidate is killed at the deadline rather than blocking the
/// caller. `config_dir` must contain a `.localpilot.toml` naming the provider and
/// model passed here; the handshake never calls the provider, so a stub endpoint
/// that is never contacted is enough.
///
/// # Errors
/// Returns [`SelfDevError::Build`] when the candidate cannot be spawned, does not
/// answer within `timeout`, or answers with the wrong record.
pub fn smoke_handshake(
    executable: &Path,
    config_dir: &Path,
    provider: &str,
    model: &str,
    timeout: Duration,
) -> Result<(), SelfDevError> {
    let mut child = Command::new(executable)
        .args(["rpc", "--provider", provider, "--model", model])
        .current_dir(config_dir)
        .stdin(Stdio::piped())
        .stdout(Stdio::piped())
        .stderr(Stdio::null())
        .spawn()
        .map_err(|error| SelfDevError::Build {
            status: "not started".to_string(),
            detail: format!("spawning `rpc`: {error}"),
        })?;

    let result = handshake_round_trip(&mut child, timeout);
    // Always reap the child: a candidate that passed must not linger, and one
    // that failed must not be left running.
    let _ = child.kill();
    let _ = child.wait();
    result
}

/// The wire round-trip, factored out so the child is always reaped by the caller
/// whatever happens here.
fn handshake_round_trip(
    child: &mut std::process::Child,
    timeout: Duration,
) -> Result<(), SelfDevError> {
    let mut stdin = child.stdin.take().ok_or_else(|| SelfDevError::Build {
        status: "0".to_string(),
        detail: "candidate rpc has no stdin".to_string(),
    })?;
    let stdout = child.stdout.take().ok_or_else(|| SelfDevError::Build {
        status: "0".to_string(),
        detail: "candidate rpc has no stdout".to_string(),
    })?;

    // A reader thread turns blocking line reads into timed channel receives, so a
    // candidate that never answers is bounded by the deadline, not unbounded.
    let (tx, rx) = mpsc::channel();
    std::thread::spawn(move || {
        for line in BufReader::new(stdout).lines().map_while(Result::ok) {
            if tx.send(line).is_err() {
                break;
            }
        }
    });

    // A `hello` command: the published protocol is version 1, internally tagged.
    stdin
        .write_all(b"{\"v\":1,\"command\":{\"type\":\"hello\"}}\n")
        .and_then(|()| stdin.flush())
        .map_err(|error| SelfDevError::Build {
            status: "0".to_string(),
            detail: format!("writing the hello command: {error}"),
        })?;

    let deadline = std::time::Instant::now() + timeout;
    loop {
        let remaining = deadline.saturating_duration_since(std::time::Instant::now());
        if remaining.is_zero() {
            return Err(SelfDevError::Build {
                status: "0".to_string(),
                detail: "candidate did not complete the rpc handshake before the deadline"
                    .to_string(),
            });
        }
        match rx.recv_timeout(remaining) {
            Ok(line) if is_hello_event(&line) => return Ok(()),
            // A non-hello record (a stray ask, a warning) is skipped; keep waiting
            // for the handshake reply within the same deadline.
            Ok(_) => {}
            Err(mpsc::RecvTimeoutError::Timeout) => {
                return Err(SelfDevError::Build {
                    status: "0".to_string(),
                    detail: "candidate did not complete the rpc handshake before the deadline"
                        .to_string(),
                });
            }
            Err(mpsc::RecvTimeoutError::Disconnected) => {
                return Err(SelfDevError::Build {
                    status: "0".to_string(),
                    detail: "candidate rpc closed its output before answering the handshake"
                        .to_string(),
                });
            }
        }
    }
}

/// Whether one server record is the `hello` event that answers the handshake.
fn is_hello_event(line: &str) -> bool {
    let Ok(value) = serde_json::from_str::<serde_json::Value>(line) else {
        return false;
    };
    value
        .get("event")
        .and_then(|event| event.get("type"))
        .and_then(serde_json::Value::as_str)
        == Some("hello")
}

/// Write the throwaway config the handshake needs: one provider the RPC session
/// can construct, pointed at an endpoint the handshake never contacts.
///
/// # Errors
/// Returns [`SelfDevError::Io`] when the file cannot be written.
pub fn write_smoke_config(dir: &Path, provider: &str, model: &str) -> Result<(), SelfDevError> {
    let body = format!(
        "[providers.{provider}]\n\
         kind = \"local\"\n\
         base_url = \"http://127.0.0.1:1\"\n\
         model = \"{model}\"\n"
    );
    std::fs::write(dir.join(".localpilot.toml"), body).map_err(SelfDevError::io)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::git::tests::{commit_all, init_repo, write};

    fn source() -> SourceState {
        let repo = init_repo();
        write(repo.path(), "a.txt", "one");
        commit_all(repo.path(), "first");
        let state = SourceState::read(repo.path()).expect("read");
        drop(repo);
        state
    }

    fn matching_identity(source: &SourceState) -> ReportedIdentity {
        ReportedIdentity {
            version: format!("2.6.0-selfdev-{}", source.version_label),
            git_hash: source.embedded_hash().to_string(),
            fingerprint: source.fingerprint.clone(),
        }
    }

    #[test]
    fn a_matching_identity_passes() {
        let source = source();
        assert!(check_identity(&matching_identity(&source), &source).is_ok());
    }

    #[test]
    fn a_wrong_commit_hash_is_rejected() {
        let source = source();
        let mut reported = matching_identity(&source);
        reported.git_hash = "0000000deadbeef".to_string();
        let err = check_identity(&reported, &source).expect_err("must reject");
        assert!(matches!(err, SelfDevError::Invalid(m) if m.contains("git hash")));
    }

    #[test]
    fn a_right_hash_but_wrong_fingerprint_is_rejected() {
        // The stale-binary case the fingerprint exists for: same commit, but the
        // binary was built from different working-tree bytes.
        let source = source();
        let mut reported = matching_identity(&source);
        reported.fingerprint = "deadbeef".to_string();
        let err = check_identity(&reported, &source).expect_err("must reject");
        assert!(matches!(err, SelfDevError::Invalid(m) if m.contains("fingerprint")));
    }

    #[test]
    fn an_unchanged_tree_is_fresh() {
        let source = source();
        assert!(check_fresh(&source, &source).is_ok());
    }

    #[test]
    fn a_tree_that_changed_during_the_build_is_superseded() {
        let mut before = source();
        let mut after = before.clone();
        before.fingerprint = "aaaa".to_string();
        before.version_label = "aaaa".to_string();
        after.fingerprint = "bbbb".to_string();
        after.version_label = "bbbb".to_string();
        let err = check_fresh(&before, &after).expect_err("must reject");
        assert!(matches!(err, SelfDevError::Invalid(m) if m.contains("superseded")));
    }

    #[test]
    fn the_hello_event_is_recognised_and_others_are_not() {
        assert!(is_hello_event(
            r#"{"v":1,"event":{"type":"hello","protocol_version":1,"session_id":"s","model":"m"}}"#
        ));
        assert!(!is_hello_event(
            r#"{"v":1,"event":{"type":"text_delta","text":"hi"}}"#
        ));
        assert!(!is_hello_event(r#"{"v":1,"event":{"type":"error"}}"#));
        assert!(!is_hello_event("not json"));
    }
}
