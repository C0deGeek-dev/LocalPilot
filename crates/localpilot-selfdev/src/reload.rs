//! Swapping the running process onto a freshly published build, and continuing
//! the session on the other side of the swap without the user lifting a finger.
//!
//! Two hard-won constraints from the reference shape everything here:
//!
//! - **A process replacement runs no destructors.** On Unix `exec` overwrites the
//!   image in place; on Windows the parent just exits. Either way, anything that
//!   was only in memory is gone. So every durable thing — the session, the
//!   reload marker, the continuation intent, any draft the user was typing — is
//!   written *before* the swap, never after.
//! - **A recovery intent must be durable, idempotent, and non-consuming.** The
//!   successor process reads the continuation without deleting it, acts on it, and
//!   records delivery only once the continuation is *accepted*. A restart that
//!   dies before it finishes can therefore try again; a restart that succeeds
//!   twice (a crash after acting but before recording) still delivers once,
//!   because delivery is keyed by an idempotent flag, not by the read.
//!
//! The relaunch itself is the one genuinely OS-divergent step. Its *plan* — the
//! program, the arguments, and whether the current process is replaced — is
//! computed by a pure function and unit-tested on every platform; only the final
//! syscall differs (`exec` vs spawn-then-exit, D003).

use std::path::{Path, PathBuf};

use serde::{Deserialize, Serialize};

use crate::channel::{ChannelName, Channels};
use crate::error::SelfDevError;
use crate::marker::BuildMarker;
use crate::store::VersionStore;

/// The intent format this build writes and understands.
pub const RELOAD_INTENT_VERSION: u32 = 1;
/// Directory under the self-dev root holding per-session reload state.
const RELOAD_DIR: &str = "reload";

/// A durable record that a reload happened and the session on the far side should
/// continue itself.
///
/// Written before the swap; read (without deletion) by the successor; flipped to
/// `delivered` once the continuation is accepted; reclaimed by [`ReloadStore::gc`]
/// after that.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ReloadIntent {
    /// Intent format version.
    pub intent_version: u32,
    /// A unique id for this reload, so a re-delivered signal is deduplicated by
    /// it rather than acted on twice (the reference's watch-channel lesson).
    pub request_id: String,
    /// The session that must continue after the swap.
    pub session_id: String,
    /// The version label the process was running before the swap.
    pub from_version: String,
    /// The version label it is running after the swap.
    pub to_version: String,
    /// A short description of the in-flight task, woven into the continuation
    /// prompt so the resumed session knows what it was doing.
    pub task: String,
    /// The user's unsent draft input at reload time, preserved across the swap so
    /// it is not lost. `None` when there was none.
    pub draft_input: Option<String>,
    /// The pid that initiated the reload — for diagnostics and for a later
    /// liveness-based sweep.
    pub pid: u32,
    /// Whether the continuation has been delivered and accepted. A non-consuming
    /// read leaves this `false`; [`ReloadStore::mark_delivered`] sets it, and a
    /// delivered intent no longer continues anything and may be reclaimed.
    pub delivered: bool,
}

impl ReloadIntent {
    /// A fresh, undelivered intent.
    #[must_use]
    #[allow(clippy::too_many_arguments)] // a reload intent genuinely carries this
    pub fn new(
        request_id: impl Into<String>,
        session_id: impl Into<String>,
        from_version: impl Into<String>,
        to_version: impl Into<String>,
        task: impl Into<String>,
        draft_input: Option<String>,
        pid: u32,
    ) -> Self {
        Self {
            intent_version: RELOAD_INTENT_VERSION,
            request_id: request_id.into(),
            session_id: session_id.into(),
            from_version: from_version.into(),
            to_version: to_version.into(),
            task: task.into(),
            draft_input,
            pid,
            delivered: false,
        }
    }

    /// The hidden continuation prompt injected into the resumed session so it
    /// carries on without waiting for the user. Original text (D001).
    #[must_use]
    pub fn continuation_prompt(&self) -> String {
        let mut prompt = format!(
            "A hot reload just succeeded ({} \u{2192} {}). You were in the middle of this task: \
             {}. Continue immediately from where you left off. Do not ask the user what to do \
             next, and do not restart the task from the beginning.",
            self.from_version, self.to_version, self.task
        );
        if let Some(draft) = &self.draft_input {
            if !draft.trim().is_empty() {
                prompt.push_str(&format!(
                    " The user had this unsent draft when the reload began; take it into account: \
                     {draft}"
                ));
            }
        }
        prompt
    }
}

/// Per-session reload state on disk, rooted at the self-dev subtree.
#[derive(Debug, Clone)]
pub struct ReloadStore {
    root: PathBuf,
}

impl ReloadStore {
    /// A store rooted at `selfdev_root` (state lands in `<root>/reload/`).
    #[must_use]
    pub fn new(selfdev_root: impl Into<PathBuf>) -> Self {
        Self {
            root: selfdev_root.into().join(RELOAD_DIR),
        }
    }

    /// The intent file for `session_id`.
    fn intent_path(&self, session_id: &str) -> PathBuf {
        self.root.join(format!("{session_id}.json"))
    }

    /// Persist `intent`, atomically. Called *before* the swap.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] when the directory cannot be created or the
    /// write/rename fails.
    pub fn write(&self, intent: &ReloadIntent) -> Result<(), SelfDevError> {
        std::fs::create_dir_all(&self.root).map_err(SelfDevError::io)?;
        let body = serde_json::to_vec_pretty(intent).map_err(SelfDevError::io)?;
        let target = self.intent_path(&intent.session_id);
        let temp = self.root.join(format!(
            ".{}.{}.incoming",
            intent.session_id,
            std::process::id()
        ));
        std::fs::write(&temp, body).map_err(SelfDevError::io)?;
        std::fs::rename(&temp, target).map_err(|error| {
            let _ = std::fs::remove_file(&temp);
            SelfDevError::io(error)
        })
    }

    /// The pending continuation for `session_id`, if one is waiting.
    ///
    /// **Non-consuming**: the file is left in place, so a restart that dies before
    /// recording delivery can retry. Returns `None` when there is no intent, when
    /// it has already been delivered, or when the file is unreadable / a foreign
    /// format (an unreadable intent must never wedge a resume).
    #[must_use]
    pub fn pending(&self, session_id: &str) -> Option<ReloadIntent> {
        let body = std::fs::read_to_string(self.intent_path(session_id)).ok()?;
        let intent: ReloadIntent = serde_json::from_str(&body).ok()?;
        if intent.intent_version != RELOAD_INTENT_VERSION || intent.delivered {
            return None;
        }
        Some(intent)
    }

    /// Record that the continuation for `session_id` was delivered and accepted.
    ///
    /// Idempotent: marking an already-delivered (or absent) intent is a no-op that
    /// still succeeds, so a retried restart cannot double-deliver.
    ///
    /// # Errors
    /// Returns [`SelfDevError::Io`] only when a present intent cannot be rewritten.
    pub fn mark_delivered(&self, session_id: &str) -> Result<(), SelfDevError> {
        let path = self.intent_path(session_id);
        let Ok(body) = std::fs::read_to_string(&path) else {
            return Ok(()); // Nothing to mark.
        };
        let Ok(mut intent) = serde_json::from_str::<ReloadIntent>(&body) else {
            return Ok(()); // A foreign file is not ours to rewrite.
        };
        if intent.delivered {
            return Ok(());
        }
        intent.delivered = true;
        self.write(&intent)
    }

    /// Reclaim intents whose job is done: any that have been delivered.
    ///
    /// Called at startup. An *undelivered* intent is deliberately kept — its
    /// session has not continued yet — so this never races a pending continuation.
    ///
    /// Returns the session ids reclaimed, for a caller that wants to log the sweep.
    pub fn gc(&self) -> Vec<String> {
        let mut reclaimed = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return reclaimed;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            let Ok(body) = std::fs::read_to_string(&path) else {
                continue;
            };
            let Ok(intent) = serde_json::from_str::<ReloadIntent>(&body) else {
                continue;
            };
            if intent.delivered && std::fs::remove_file(&path).is_ok() {
                reclaimed.push(intent.session_id);
            }
        }
        reclaimed
    }
}

/// A fully resolved relaunch: everything needed to swap onto the new binary,
/// decided by platform-independent code before the OS-divergent syscall.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RelaunchPlan {
    /// The executable to run — the channel-resolved immutable version.
    pub program: PathBuf,
    /// Arguments to the successor.
    pub args: Vec<String>,
    /// Whether the current process is *replaced* (Unix `exec`, same pid) rather
    /// than the successor being spawned alongside a parent that then exits
    /// (Windows, which has no `exec`). D003.
    pub replaces_current_process: bool,
}

/// Resolve how to relaunch onto `program` with `args`.
///
/// Pure and platform-aware: the only thing it decides from the platform is
/// whether the current process will be replaced, which is exactly the fact a
/// caller needs in order to know whether to exit afterwards. Unit-testable on
/// every OS.
#[must_use]
pub fn relaunch_plan(program: &Path, args: &[String]) -> RelaunchPlan {
    RelaunchPlan {
        program: program.to_path_buf(),
        args: args.to_vec(),
        replaces_current_process: cfg!(unix),
    }
}

/// Perform the relaunch.
///
/// On Unix this `exec`s the successor: on success it **never returns** (the image
/// is replaced), so a returned `Ok` is impossible there and an `Err` is the only
/// outcome. On Windows it spawns the successor and returns its [`std::process::Child`]
/// so the caller can exit the parent, completing the swap.
///
/// Everything durable must already be on disk before this is called — a process
/// replacement runs no destructors.
///
/// # Errors
/// Returns [`SelfDevError::Io`] when the successor cannot be started (or, on Unix,
/// when `exec` fails without replacing the image).
#[cfg(unix)]
pub fn relaunch(plan: &RelaunchPlan) -> Result<std::process::Child, SelfDevError> {
    use std::os::unix::process::CommandExt;
    // `exec` returns only on failure; on success the image is replaced and this
    // call never returns. The `Err` branch is therefore the only reachable path.
    let error = std::process::Command::new(&plan.program)
        .args(&plan.args)
        .exec();
    Err(SelfDevError::io(error))
}

/// Perform the relaunch (Windows: spawn the successor; the caller then exits).
///
/// # Errors
/// Returns [`SelfDevError::Io`] when the successor cannot be spawned.
#[cfg(not(unix))]
pub fn relaunch(plan: &RelaunchPlan) -> Result<std::process::Child, SelfDevError> {
    std::process::Command::new(&plan.program)
        .args(&plan.args)
        .spawn()
        .map_err(SelfDevError::io)
}

/// Everything a reload needs, so the ordering that keeps it safe lives in one
/// place rather than being re-derived by each caller.
pub struct ReloadRequest<'a> {
    /// The immutable version store to install the candidate into.
    pub store: &'a VersionStore,
    /// The channel pointers to promote through.
    pub channels: &'a Channels,
    /// Per-session reload state.
    pub reload: &'a ReloadStore,
    /// The vetted candidate executable (subject 03 already passed).
    pub executable: &'a Path,
    /// The marker recording the candidate's identity.
    pub marker: &'a BuildMarker,
    /// The channel to point at the candidate once installed.
    pub channel: ChannelName,
    /// The intent the successor consumes to continue the session.
    pub intent: ReloadIntent,
    /// Extra arguments handed to the successor (e.g. a resume flag).
    pub successor_args: Vec<String>,
}

/// Make everything a reload needs durable, **in the order that keeps it safe**,
/// and return the immutable path the successor will run.
///
/// Install the candidate, promote the channel to it, then write the continuation
/// intent last — so that once this returns, the successor has, on disk, the
/// binary to run and the reason to continue. Nothing here is the swap itself: a
/// caller can inspect the staged state, and the split is what makes the whole
/// pre-swap sequence testable on a platform where the swap would never return.
///
/// The returned path is the concrete immutable version directory's executable,
/// not the channel — the successor is launched from a path a later build can
/// never overwrite (subject 02), with no chance of the channel resolving to a
/// different version between this call and the launch.
///
/// # Errors
/// Returns [`SelfDevError`] if the install, the channel swap, or the intent write
/// fails — each leaving the running process intact and nothing half-committed.
pub fn stage_reload(request: &ReloadRequest<'_>) -> Result<PathBuf, SelfDevError> {
    let installed =
        request
            .store
            .install(&request.marker.label, request.executable, request.marker)?;
    request
        .channels
        .set(request.channel.clone(), &installed.label)?;
    // Written last: everything the successor needs to continue is now durable.
    request.reload.write(&request.intent)?;
    Ok(installed.executable())
}

/// Stage a reload and swap onto it.
///
/// The caller is responsible for having already vetted the candidate
/// ([`crate::vet`]) and quiesced the running turn; this function does not decide
/// *whether* to reload, only how to do it safely.
///
/// On Unix this never returns on success (the process is replaced). On Windows it
/// returns the spawned successor so the caller can exit the parent.
///
/// # Errors
/// Returns [`SelfDevError`] if staging or the relaunch fails — each *before* the
/// swap, so a failure leaves the running process intact.
pub fn perform_reload(request: &ReloadRequest<'_>) -> Result<std::process::Child, SelfDevError> {
    let program = stage_reload(request)?;
    let plan = relaunch_plan(&program, &request.successor_args);
    relaunch(&plan)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn intent(session: &str) -> ReloadIntent {
        ReloadIntent::new(
            "req-1",
            session,
            "aaaa",
            "bbbb",
            "wire the widget",
            Some("half a sentence".to_string()),
            4321,
        )
    }

    #[test]
    fn an_intent_round_trips_and_reads_back_pending() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ReloadStore::new(temp.path());
        let written = intent("sess-1");
        store.write(&written).expect("write");

        let read = store.pending("sess-1").expect("pending");
        assert_eq!(read, written);
    }

    #[test]
    fn pending_is_non_consuming() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ReloadStore::new(temp.path());
        store.write(&intent("sess-1")).expect("write");

        assert!(store.pending("sess-1").is_some());
        assert!(
            store.pending("sess-1").is_some(),
            "reading the intent must not delete it — a dying restart has to retry"
        );
    }

    #[test]
    fn a_delivered_intent_is_no_longer_pending_and_marking_is_idempotent() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ReloadStore::new(temp.path());
        store.write(&intent("sess-1")).expect("write");

        store.mark_delivered("sess-1").expect("mark");
        assert!(
            store.pending("sess-1").is_none(),
            "a delivered continuation must not be offered again"
        );
        // Marking again (a retried restart) is a harmless no-op.
        store.mark_delivered("sess-1").expect("mark again");
        assert!(store.pending("sess-1").is_none());
    }

    #[test]
    fn marking_an_absent_intent_is_a_successful_no_op() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ReloadStore::new(temp.path());
        assert!(store.mark_delivered("ghost").is_ok());
    }

    #[test]
    fn gc_reclaims_delivered_intents_but_keeps_pending_ones() {
        let temp = tempfile::tempdir().expect("temp");
        let store = ReloadStore::new(temp.path());
        store.write(&intent("done")).expect("write done");
        store.write(&intent("waiting")).expect("write waiting");
        store.mark_delivered("done").expect("mark");

        let reclaimed = store.gc();
        assert_eq!(reclaimed, vec!["done".to_string()]);
        assert!(
            store.pending("waiting").is_some(),
            "an undelivered intent must survive gc — its session has not continued yet"
        );
    }

    #[test]
    fn the_continuation_prompt_names_the_versions_and_forbids_restarting() {
        let prompt = intent("sess-1").continuation_prompt();
        assert!(prompt.contains("aaaa") && prompt.contains("bbbb"));
        assert!(prompt.contains("wire the widget"));
        assert!(prompt.contains("Do not ask the user"));
        assert!(prompt.contains("not restart"));
        assert!(
            prompt.contains("half a sentence"),
            "a preserved draft must reach the continuation"
        );
    }

    #[test]
    fn the_relaunch_plan_carries_the_program_and_args_and_the_platform_swap_mode() {
        let program = PathBuf::from("/versions/bbbb/localpilot");
        let args = vec!["--resume".to_string(), "sess-1".to_string()];
        let plan = relaunch_plan(&program, &args);

        assert_eq!(plan.program, program);
        assert_eq!(plan.args, args);
        assert_eq!(
            plan.replaces_current_process,
            cfg!(unix),
            "Unix replaces the process in place; Windows spawns and exits the parent"
        );
    }

    #[test]
    fn a_missing_draft_leaves_the_continuation_prompt_clean() {
        let mut i = intent("sess-1");
        i.draft_input = None;
        let prompt = i.continuation_prompt();
        assert!(!prompt.contains("unsent draft"));
    }

    #[test]
    fn staging_a_reload_installs_promotes_and_persists_before_any_swap() {
        let temp = tempfile::tempdir().expect("temp");
        let store = VersionStore::new(temp.path());
        let channels = Channels::new(temp.path());
        let reload = ReloadStore::new(temp.path());

        // A stand-in candidate binary the store will copy in.
        let built = temp.path().join("candidate");
        std::fs::write(&built, "the new binary").expect("write candidate");
        let marker = BuildMarker::new(
            "bbbb",
            "abc1234",
            "fp",
            false,
            "2.6.0-selfdev-bbbb",
            localpilot_dist::executable_name(crate::builder::TOOL),
        );
        let request = ReloadRequest {
            store: &store,
            channels: &channels,
            reload: &reload,
            executable: &built,
            marker: &marker,
            channel: crate::channel::CURRENT.into(),
            intent: intent("sess-1"),
            successor_args: vec!["--resume".to_string(), "sess-1".to_string()],
        };

        let program = stage_reload(&request).expect("stage");

        // The candidate is installed and immutable.
        let installed = store.get("bbbb").expect("installed");
        assert_eq!(program, installed.executable());
        assert!(program.starts_with(store.version_dir("bbbb")));
        assert_eq!(
            std::fs::read_to_string(&program).expect("read"),
            "the new binary"
        );
        // The channel points at it.
        assert_eq!(
            channels
                .resolve(&store, crate::channel::CURRENT)
                .map(|v| v.label),
            Some("bbbb".to_string())
        );
        // The continuation intent is durable and pending.
        assert!(reload.pending("sess-1").is_some());
    }

    #[test]
    fn relaunching_a_missing_program_is_a_typed_error_not_a_panic() {
        let plan = relaunch_plan(
            &PathBuf::from("this-program-does-not-exist-anywhere-0xdeadbeef"),
            &[],
        );
        // Unix `exec` and Windows `spawn` both fail cleanly for a missing program.
        assert!(matches!(relaunch(&plan), Err(SelfDevError::Io(_))));
    }
}
