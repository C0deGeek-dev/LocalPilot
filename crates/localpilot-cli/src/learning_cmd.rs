//! `localpilot learning` — the LocalMind learning subsystem.
//!
//! Session closeout, the review queue, memory promotion, search, and the audit
//! log. All state is project-local under `.localmind/`. Requires the `learning`
//! build feature.

use std::io::Write;
use std::path::Path;
use std::str::FromStr;
use std::time::{SystemTime, UNIX_EPOCH};

use localpilot_core::SessionId;
use localpilot_localmind::{self as learning, ReviewVerdict};
use localpilot_store::Store;

/// Close out a session: extract candidate lessons and enqueue them for review.
///
/// # Errors
/// Returns an error if the session id is invalid or close-out fails.
pub fn closeout(cwd: &std::path::Path, session: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let session_id = SessionId::from_str(session)
        .map_err(|e| anyhow::anyhow!("invalid session id '{session}': {e}"))?;
    let store = Store::open(cwd);
    let summary = learning::closeout_session(cwd, &store, session_id)?;
    writeln!(
        out,
        "closed out {} — {} candidate(s), {} enqueued for review",
        summary.session_id, summary.candidate_count, summary.enqueued_count
    )?;
    Ok(())
}

/// Seed curated lessons from a JSON pack (`{ "lessons": [ ... ] }`) directly into
/// accepted memory. Idempotent: lessons whose body already exists are skipped.
///
/// # Errors
/// Returns an error if the file cannot be read or parsed, or if seeding fails.
pub fn seed(cwd: &Path, file: &Path, dry_run: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let text = std::fs::read_to_string(file)
        .map_err(|e| anyhow::anyhow!("read seed file '{}': {e}", file.display()))?;
    let pack: localpilot_localmind::SeedPack = serde_json::from_str(&text)
        .map_err(|e| anyhow::anyhow!("parse seed file '{}': {e}", file.display()))?;
    let report = learning::seed_memory(cwd, &pack.lessons, dry_run)?;
    let suffix = if dry_run {
        " (dry run — nothing written)"
    } else {
        ""
    };
    writeln!(
        out,
        "seeded {} lesson(s), skipped {}{}",
        report.seeded, report.skipped, suffix
    )?;
    Ok(())
}

/// List the review queue, grouped into similarity clusters so a reviewer can
/// triage near-duplicates together.
///
/// # Errors
/// Returns an error if the queue cannot be read.
pub fn review_list(cwd: &std::path::Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let items = learning::review_list(cwd)?;
    if items.is_empty() {
        writeln!(out, "review queue is empty")?;
        return Ok(());
    }
    let summaries: Vec<String> = items.iter().map(|item| item.summary.clone()).collect();
    let clusters = learning::cluster_by_similarity(&summaries);
    for (index, cluster) in clusters.iter().enumerate() {
        if cluster.len() > 1 {
            writeln!(
                out,
                "# cluster {} ({} similar — review together)",
                index + 1,
                cluster.len()
            )?;
        }
        for &item_index in cluster {
            let item = &items[item_index];
            let seen = if item.seen_count > 1 {
                format!("\t(seen {}x)", item.seen_count)
            } else {
                String::new()
            };
            writeln!(
                out,
                "{}\t{}\t{:.2}\t{}\t{}{}",
                item.id, item.state, item.confidence, item.category, item.summary, seen
            )?;
        }
    }
    Ok(())
}

/// Back up the store, then delete every pending review candidate. A one-time
/// cleanup of an un-reviewed backlog; decided items and accepted memory are
/// untouched. Requires `--yes` to actually delete.
///
/// # Errors
/// Returns an error if the store cannot be backed up or purged.
pub fn review_purge(cwd: &Path, yes: bool, out: &mut dyn Write) -> anyhow::Result<()> {
    let items = learning::review_list(cwd)?;
    let pending = items.iter().filter(|item| item.state == "Pending").count();
    if pending == 0 {
        writeln!(out, "no pending candidates to purge")?;
        return Ok(());
    }
    if !yes {
        writeln!(
            out,
            "{pending} pending candidate(s) would be purged. Re-run with --yes to back up the \
             store and delete them."
        )?;
        return Ok(());
    }

    // Back up the sqlite to a timestamped copy before the irreversible delete.
    let db = cwd.join(".localmind").join("localmind.sqlite");
    if db.exists() {
        let stamp = SystemTime::now().duration_since(UNIX_EPOCH)?.as_secs();
        let backup = cwd
            .join(".localmind")
            .join(format!("localmind.sqlite.backup-{stamp}"));
        std::fs::copy(&db, &backup)?;
        writeln!(out, "backed up store to {}", backup.display())?;
    }

    let removed = learning::review_purge(cwd)?;
    writeln!(out, "purged {removed} pending candidate(s)")?;
    Ok(())
}

/// Inspect one review item.
///
/// # Errors
/// Returns an error if the item cannot be read.
pub fn review_show(cwd: &std::path::Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    match learning::review_show(cwd, id)? {
        Some(item) => {
            writeln!(out, "id: {}", item.id)?;
            writeln!(out, "state: {}", item.state)?;
            writeln!(out, "session: {}", item.session_id)?;
            writeln!(out, "category: {}", item.category)?;
            writeln!(out, "confidence: {:.3}", item.confidence)?;
            writeln!(out, "summary: {}", item.summary)?;
            if let Some(replacement) = item.replacement {
                writeln!(out, "replacement: {replacement}")?;
            }
            if let Some(note) = item.note {
                writeln!(out, "note: {note}")?;
            }
        }
        None => writeln!(out, "review item not found")?,
    }
    Ok(())
}

/// Apply a verdict to a review item.
///
/// # Errors
/// Returns an error if the decision fails.
pub fn review_decide(
    cwd: &std::path::Path,
    id: &str,
    verdict: ReviewVerdict,
    reviewer: &str,
    note: Option<String>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    let state = learning::review_decide(cwd, id, verdict, reviewer, note)?;
    writeln!(out, "{id} -> {state}")?;
    Ok(())
}

/// Promote an accepted item into durable memory.
///
/// # Errors
/// Returns an error if promotion fails.
pub fn promote(cwd: &std::path::Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let memory_id = learning::promote(cwd, id)?;
    writeln!(out, "promoted memory {memory_id}")?;
    Ok(())
}

/// Search accepted memory in the resolved store at `root`.
///
/// `found` is whether an existing store was resolved (walked up from the cwd, or
/// pinned by `--workspace`). A read never creates a store: when no store exists
/// the search is reported as such on stderr and stdout stays script-stable (an
/// empty JSON array stays valid). The three empty outcomes — no store, empty
/// store, and a non-empty store that the query missed — get distinct stderr lines
/// so a caller can tell them apart instead of reading a bare `no matches`.
///
/// # Errors
/// Returns an error if the search fails.
pub fn search(
    root: &Path,
    found: bool,
    query: &str,
    json: bool,
    out: &mut dyn Write,
    err: &mut dyn Write,
) -> anyhow::Result<()> {
    if !found {
        // (a) No `.localmind` at or above the search start. Diagnose on stderr;
        // keep stdout script-stable so a `--json` consumer still parses an array.
        writeln!(
            err,
            "localmind: no store found at or above {} (no ancestor holds .localmind) — \
             create one with `localpilot learning seed`/`closeout`, or pass --workspace <path>",
            root.display()
        )?;
        if json {
            writeln!(out, "[]")?;
        } else {
            writeln!(out, "no matches")?;
        }
        return Ok(());
    }
    // Read-only: never initialize a store from a search.
    let hits = learning::search_readonly(root, query)?;
    if json {
        // Structured output for agents: one JSON array of hits (id, score, path,
        // snippet, category). Empty results are a valid empty array.
        writeln!(out, "{}", serde_json::to_string_pretty(&hits)?)?;
    } else if hits.is_empty() {
        writeln!(out, "no matches")?;
    } else {
        for hit in &hits {
            writeln!(out, "{}\t{}\t{}", hit.memory_id, hit.score, hit.path)?;
            writeln!(out, "  {}", hit.snippet)?;
        }
    }
    if hits.is_empty() {
        report_empty_search(root, query, err)?;
    }
    Ok(())
}

/// Tell the (b) empty-store case apart from the (c) non-empty-store-missed case on
/// stderr. Stays read-only: only counts when the store config already exists, so a
/// diagnostic never writes one.
fn report_empty_search(root: &Path, query: &str, err: &mut dyn Write) -> anyhow::Result<()> {
    let count = if root.join(".localmind.toml").is_file() {
        learning::memory_list(root).map(|m| m.len()).unwrap_or(0)
    } else {
        0
    };
    if count == 0 {
        writeln!(
            err,
            "localmind: store at {} has no accepted memory yet",
            root.display()
        )?;
    } else {
        writeln!(
            err,
            "localmind: {count} accepted {} in store at {}, none matched {query:?}",
            if count == 1 { "memory" } else { "memories" },
            root.display()
        )?;
    }
    Ok(())
}

/// Generate disabled skill drafts from accepted review items.
///
/// # Errors
/// Returns an error if generation fails.
pub fn skills_generate(cwd: &std::path::Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let drafts = learning::skills_generate(cwd)?;
    if drafts.is_empty() {
        writeln!(out, "no skill drafts generated")?;
        return Ok(());
    }
    for draft in drafts {
        writeln!(out, "{}\t{}", draft.id, draft.path)?;
    }
    Ok(())
}

/// List generated skill drafts.
///
/// # Errors
/// Returns an error if the drafts cannot be read.
pub fn skills_list(cwd: &std::path::Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let drafts = learning::skills_list(cwd)?;
    if drafts.is_empty() {
        writeln!(out, "no skill drafts")?;
        return Ok(());
    }
    for draft in drafts {
        let state = if draft.disabled {
            "disabled"
        } else {
            "enabled"
        };
        writeln!(out, "{}\t{}\t{}", draft.id, state, draft.name)?;
    }
    Ok(())
}

/// Inspect a skill draft.
///
/// # Errors
/// Returns an error if the draft cannot be read.
pub fn skill_show(cwd: &std::path::Path, id: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    match learning::skill_show(cwd, id)? {
        Some(draft) => {
            writeln!(out, "id: {}", draft.id)?;
            writeln!(out, "name: {}", draft.name)?;
            writeln!(out, "disabled: {}", draft.disabled)?;
            writeln!(out, "description: {}", draft.description)?;
            writeln!(out, "path: {}", draft.path)?;
        }
        None => writeln!(out, "skill draft not found")?,
    }
    Ok(())
}

/// Export a skill draft's Markdown body to a file or stdout.
///
/// # Errors
/// Returns an error if the draft cannot be read or written.
pub fn skill_export(
    cwd: &std::path::Path,
    id: &str,
    output: Option<std::path::PathBuf>,
    out: &mut dyn Write,
) -> anyhow::Result<()> {
    match learning::skill_body(cwd, id)? {
        Some(body) => match output {
            Some(path) => {
                std::fs::write(&path, body)?;
                writeln!(out, "{}", path.display())?;
            }
            None => writeln!(out, "{body}")?,
        },
        None => writeln!(out, "skill draft not found")?,
    }
    Ok(())
}

/// Print the memory-change audit log.
///
/// # Errors
/// Returns an error if the audit log cannot be read.
pub fn audit(cwd: &std::path::Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let records = learning::audit(cwd)?;
    if records.is_empty() {
        writeln!(out, "no audit records")?;
        return Ok(());
    }
    for record in records {
        writeln!(
            out,
            "{}\t{}\t{}\t{}\t{}",
            record.id, record.at, record.kind, record.actor, record.subject
        )?;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    #![allow(clippy::unwrap_used)]

    use super::*;
    use localpilot_core::{Message, Role};

    /// Seed at least one pending candidate by closing out a real session.
    fn seed_pending(dir: &Path) {
        let store = Store::open(dir);
        let session = SessionId::new();
        store
            .append_message(
                session,
                &Message::text(Role::User, "Lesson: redact secrets before persisting."),
            )
            .unwrap();
        learning::closeout_session(dir, &store, session).unwrap();
        assert!(!learning::review_list(dir).unwrap().is_empty());
    }

    fn backup_count(dir: &Path) -> usize {
        std::fs::read_dir(dir.join(".localmind"))
            .map(|entries| {
                entries
                    .filter_map(Result::ok)
                    .filter(|e| e.file_name().to_string_lossy().contains("backup"))
                    .count()
            })
            .unwrap_or(0)
    }

    #[test]
    fn purge_backs_up_then_clears_pending() {
        let dir = tempfile::tempdir().unwrap();
        seed_pending(dir.path());

        let mut out = Vec::new();
        review_purge(dir.path(), true, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("backed up store"), "got: {text}");
        assert!(text.contains("purged"), "got: {text}");
        assert_eq!(backup_count(dir.path()), 1, "exactly one backup is written");
        assert!(
            learning::review_list(dir.path())
                .unwrap()
                .iter()
                .all(|item| item.state != "Pending"),
            "no pending candidate survives the purge"
        );
    }

    /// Seed one accepted lesson into a store at `dir` so a search can hit it.
    fn seed_one(dir: &Path, body: &str) {
        std::fs::write(dir.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let lesson = learning::SeedLesson {
            body: body.to_string(),
            category: Some("Process".to_string()),
            confidence: Some(0.8),
            related_files: Vec::new(),
            related_entities: Vec::new(),
            evidence: None,
            tags: Vec::new(),
        };
        learning::seed_memory(dir, &[lesson], false).unwrap();
    }

    #[test]
    fn search_with_no_store_reports_state_a_and_creates_nothing() {
        // State (a): no `.localmind` at or above the search start. A read must not
        // create one, stdout stays script-stable, and stderr explains the miss.
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        search(dir.path(), false, "anything", false, &mut out, &mut err).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "no matches\n");
        let err = String::from_utf8(err).unwrap();
        assert!(err.contains("no store found at or above"), "got: {err}");
        assert!(
            !dir.path().join(".localmind").exists() && !dir.path().join(".localmind.toml").exists(),
            "a read must not create a store"
        );
    }

    #[test]
    fn search_with_no_store_keeps_json_a_valid_empty_array() {
        let dir = tempfile::tempdir().unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        search(dir.path(), false, "anything", true, &mut out, &mut err).unwrap();
        let out = String::from_utf8(out).unwrap();
        let parsed: serde_json::Value = serde_json::from_str(out.trim()).unwrap();
        assert!(
            parsed.as_array().is_some_and(|a| a.is_empty()),
            "got: {out}"
        );
    }

    #[test]
    fn search_empty_store_reports_state_b() {
        // State (b): a store exists but holds no accepted memory.
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localmind.toml"),
            "[learning]\nenabled = true\n",
        )
        .unwrap();
        let mut out = Vec::new();
        let mut err = Vec::new();
        search(dir.path(), true, "anything", false, &mut out, &mut err).unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "no matches\n");
        assert!(String::from_utf8(err)
            .unwrap()
            .contains("has no accepted memory yet"));
    }

    #[test]
    fn search_nonempty_store_no_match_reports_state_c() {
        // State (c): a non-empty store whose memory the query simply missed.
        let dir = tempfile::tempdir().unwrap();
        seed_one(
            dir.path(),
            "always redact secrets before persisting a transcript",
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        search(
            dir.path(),
            true,
            "an unrelated query about audio latency",
            false,
            &mut out,
            &mut err,
        )
        .unwrap();

        assert_eq!(String::from_utf8(out).unwrap(), "no matches\n");
        assert!(String::from_utf8(err).unwrap().contains("none matched"));
    }

    #[test]
    fn search_returns_the_resolved_stores_hits() {
        let dir = tempfile::tempdir().unwrap();
        seed_one(
            dir.path(),
            "propagate a subprocess exit code before reporting success",
        );
        let mut out = Vec::new();
        let mut err = Vec::new();
        search(
            dir.path(),
            true,
            "subprocess exit code",
            false,
            &mut out,
            &mut err,
        )
        .unwrap();
        let out = String::from_utf8(out).unwrap();
        assert!(out.contains("subprocess exit code"), "got: {out}");
    }

    #[test]
    fn purge_without_yes_is_a_dry_run() {
        let dir = tempfile::tempdir().unwrap();
        seed_pending(dir.path());
        let before = learning::review_list(dir.path()).unwrap().len();

        let mut out = Vec::new();
        review_purge(dir.path(), false, &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();

        assert!(text.contains("would be purged"), "got: {text}");
        assert_eq!(backup_count(dir.path()), 0, "a dry run writes no backup");
        assert_eq!(
            learning::review_list(dir.path()).unwrap().len(),
            before,
            "a dry run deletes nothing"
        );
    }
}
