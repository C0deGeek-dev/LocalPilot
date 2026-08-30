//! `localpilot handoff` (write) and `localpilot handoff resume <id>` (check).
//!
//! The writer captures the most recent session's durable state into a redacted,
//! git-ignored handoff under `.localpilot/handoffs/`. The resume check verifies a
//! handoff against the current repo deterministically before a fresh agent acts on
//! it. Neither path promotes the handoff into LocalMind memory — a handoff is
//! transient execution state.

use std::io::Write;
use std::path::Path;

use localpilot_harness::{check_handoff, write_handoff};
use localpilot_skills::discover_trusted;
use localpilot_store::Store;

/// Write a handoff for the most recent session in this workspace.
///
/// # Errors
/// Returns an error if there is no session to hand off or the artifact cannot be
/// written.
pub fn write(root: &Path, objective: Option<&str>, out: &mut dyn Write) -> anyhow::Result<()> {
    let store = Store::open(root);
    let Some(latest) = store.latest_session()? else {
        writeln!(out, "no session in this workspace to hand off")?;
        return Ok(());
    };

    // Suggest discoverable skills relevant to the objective from the effective
    // merged catalog — the user-global baseline plus, only when the workspace is
    // trusted, the project overlay (best-effort, LocalHub#39).
    let suggested = objective_suggestions(root, objective, crate::trust::is_trusted(root));

    let summary = write_handoff(root, &store, latest.id, objective, suggested)?;
    writeln!(
        out,
        "wrote handoff {} to {}",
        summary.id,
        summary.path.display()
    )?;
    writeln!(
        out,
        "resume it with: localpilot handoff resume {}",
        summary.id
    )?;
    Ok(())
}

/// The up-to-three discoverable skills most relevant to `objective`, from the
/// effective catalog. `trusted` gates the project overlay: an untrusted project's
/// skills must not enter a handoff suggestion, so it is passed in explicitly
/// rather than read from the store, keeping the choice testable.
fn objective_suggestions(root: &Path, objective: Option<&str>, trusted: bool) -> Vec<String> {
    objective
        .map(|obj| {
            discover_trusted(root, trusted)
                .map(|set| {
                    set.relevant(obj)
                        .into_iter()
                        .map(|s| s.manifest.name.clone())
                        .take(3)
                        .collect::<Vec<_>>()
                })
                .unwrap_or_default()
        })
        .unwrap_or_default()
}

/// Load a handoff by id and run the deterministic resume check against the repo.
///
/// # Errors
/// Returns an error if the handoff is missing or malformed.
pub fn resume(root: &Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let store = Store::open(root);
    let (handoff, report) = check_handoff(root, &store, id)?;
    writeln!(
        out,
        "handoff {} — {}",
        handoff.header.id, handoff.header.objective
    )?;
    writeln!(out, "next action: {}\n", handoff.header.next_action)?;
    write!(out, "{}", report.render())?;
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_core::{Message, Role, SessionId};

    /// The LocalMind boundary: a handoff present at session close-out is never
    /// promoted into accepted memory — close-out reads the transcript, never the
    /// handoff file, so the handoff body cannot appear in review candidates or
    /// accepted memory.
    #[test]
    fn an_untrusted_project_skill_is_never_suggested() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        // A discoverable project skill relevant to a unique objective keyword, so
        // real user-global skills cannot affect the assertion.
        let skill = root
            .join(".localpilot")
            .join("skills")
            .join("handoff-fixture");
        std::fs::create_dir_all(&skill).unwrap();
        std::fs::write(
            skill.join("SKILL.md"),
            "---\nname: handoff-fixture\ndescription: helps with zzqhandoffkeyword tasks\n---\n\nBody.\n",
        )
        .unwrap();

        let trusted = objective_suggestions(root, Some("zzqhandoffkeyword"), true);
        assert!(
            trusted.iter().any(|name| name == "handoff-fixture"),
            "a trusted project skill should be suggested: {trusted:?}"
        );

        let untrusted = objective_suggestions(root, Some("zzqhandoffkeyword"), false);
        assert!(
            !untrusted.iter().any(|name| name == "handoff-fixture"),
            "an untrusted project skill must not be suggested: {untrusted:?}"
        );
    }

    #[test]
    fn closeout_never_promotes_a_handoff_into_memory() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let store = Store::open(root);
        let session = SessionId::new();
        // A real transcript so close-out has something to learn from.
        store
            .append_message(
                session,
                &Message::text(Role::User, "Lesson: redact secrets before persisting."),
            )
            .unwrap();

        // Write a handoff whose objective carries a unique marker.
        let marker = "ZZHANDOFFBODYMARKERZZ";
        let summary = write_handoff(root, &store, session, Some(marker), Vec::new()).unwrap();
        assert!(summary.path.exists());

        // Close the session out into LocalMind.
        let _ = localpilot_localmind::closeout_session(root, &store, session);

        // The handoff body marker must not appear in any review candidate.
        let items = localpilot_localmind::review_list(root).unwrap_or_default();
        for item in &items {
            assert!(
                !item.summary.contains(marker),
                "handoff content leaked into a review candidate: {}",
                item.summary
            );
        }
        // The handoff stays under .localpilot/ (execution record), not promoted to
        // the .localmind/ memory store.
        assert!(summary
            .path
            .starts_with(root.join(".localpilot").join("handoffs")));
    }
}
