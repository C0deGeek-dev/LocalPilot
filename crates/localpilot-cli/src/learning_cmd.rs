//! `localpilot learning` — the LocalMind learning subsystem.
//!
//! Session closeout, the review queue, memory promotion, search, and the audit
//! log. All state is project-local under `.localmind/`. Requires the `learning`
//! build feature.

use std::io::Write;
use std::str::FromStr;

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

/// List the review queue.
///
/// # Errors
/// Returns an error if the queue cannot be read.
pub fn review_list(cwd: &std::path::Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let items = learning::review_list(cwd)?;
    if items.is_empty() {
        writeln!(out, "review queue is empty")?;
        return Ok(());
    }
    for item in items {
        writeln!(
            out,
            "{}\t{}\t{:.2}\t{}\t{}",
            item.id, item.state, item.confidence, item.category, item.summary
        )?;
    }
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

/// Search accepted memory.
///
/// # Errors
/// Returns an error if the search fails.
pub fn search(cwd: &std::path::Path, query: &str, out: &mut dyn Write) -> anyhow::Result<()> {
    let hits = learning::search(cwd, query)?;
    if hits.is_empty() {
        writeln!(out, "no matches")?;
        return Ok(());
    }
    for hit in hits {
        writeln!(out, "{}\t{}\t{}", hit.memory_id, hit.score, hit.path)?;
        writeln!(out, "  {}", hit.snippet)?;
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
