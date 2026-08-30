//! One curation pass: what is here, what is worth a look, and nothing applied.
//!
//! Curation is operator-invoked by design — there is no daemon and no schedule.
//! But "run three commands sometimes" is not a workflow, so this is the single
//! entry point that runs the read-only parts in order and says what it found.
//!
//! **It presents candidates, not verdicts.** The domain expert judged the
//! shipped heuristics at 7 of 76 — **9.21% agreement** — so a flag from them is
//! not evidence that a memory is wrong. Anything here is a cue for a person to
//! look, and the pass applies nothing and deletes nothing.

use localpilot_localmind as learning;
use std::io::Write;
use std::path::Path;

/// How many candidates to show. The reviewer's attention is the scarce resource,
/// and a list nobody finishes is a list that did not help.
const SHOWN: usize = 10;

/// Run one read-only curation pass over `cwd` and report it.
///
/// # Errors
/// Returns an error if the memory index cannot be read.
pub fn curate(cwd: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let lifecycle = learning::memory_lifecycle(cwd, SHOWN)?;
    writeln!(
        out,
        "curation pass — nothing is applied, nothing is deleted\n"
    )?;

    writeln!(out, "## the store")?;
    writeln!(out, "  accepted memory: {}", lifecycle.total)?;
    writeln!(
        out,
        "  injections ever: {} (about {} retrieving turns)",
        lifecycle.total_injections,
        lifecycle.implied_turns()
    )?;

    writeln!(out, "\n## what the signals can and cannot tell you")?;
    writeln!(
        out,
        "  never retrieved: {} of {}",
        lifecycle.never_retrieved.len(),
        lifecycle.total
    )?;
    if lifecycle.unreachable_floor > 0 {
        writeln!(
            out,
            "    of which at least {} could not have been retrieved at all —",
            lifecycle.unreachable_floor
        )?;
        writeln!(
            out,
            "    every injection touches one memory, and there have not been enough."
        )?;
    }
    if !lifecycle.never_retrieved_is_informative() {
        writeln!(
            out,
            "    Treat this as unmeasured rather than unused. It is not a deletion cue."
        )?;
    }
    writeln!(
        out,
        "  flagged for review already: {}",
        lifecycle.stale.len()
    )?;
    writeln!(out, "  contradicted: {}", lifecycle.contradicted.len())?;

    // The offline heuristics, run as a preview. Never applied from here: this
    // command's whole contract is that it changes nothing.
    let params = learning::FreshnessParams::default();
    let freshness = learning::freshness_pass(cwd, &params, "both", true)?;
    writeln!(out, "\n## candidates the offline pass would raise")?;
    writeln!(
        out,
        "  {} of {} scanned (low-quality {}, version-sensitive {}, never-retrieved {}, age {})",
        freshness.total_candidates,
        freshness.scanned,
        freshness.low_quality,
        freshness.version_sensitive,
        freshness.unused,
        freshness.age
    )?;
    writeln!(
        out,
        "  These agreed with a human reviewer 7 times in 76 (9.21%). They are"
    )?;
    writeln!(
        out,
        "  candidates for a look, not findings — judge them, do not apply them."
    )?;
    for flag in freshness.flagged.iter().take(SHOWN) {
        writeln!(out, "    {} [{}]", flag.memory_id, flag.reason)?;
    }
    if freshness.flagged.len() > SHOWN {
        writeln!(
            out,
            "    … and {} more (raise the cap deliberately, not by habit)",
            freshness.flagged.len() - SHOWN
        )?;
    }

    writeln!(out, "\n## checking one against the world")?;
    writeln!(
        out,
        "  `localpilot learning revalidate --memory <id>` prepares a check and sends"
    )?;
    writeln!(
        out,
        "  nothing. You write the question; `localpilot research` asks before every"
    )?;
    writeln!(out, "  request and routes what it finds to review.")?;

    writeln!(out, "\n## acting on any of it")?;
    writeln!(
        out,
        "  `localpilot learning review` to judge, `learning keep <id>` to clear a flag."
    )?;
    writeln!(
        out,
        "  Nothing in this pass wrote to the store. Nothing ever deletes."
    )?;
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    fn project() -> tempfile::TempDir {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(".localmind.toml"),
            "[learning]\nenabled = true\nallowed_scopes = [\"project\"]\n",
        )
        .unwrap();
        dir
    }

    #[test]
    fn a_pass_over_an_empty_store_still_says_what_it_did() {
        let dir = project();
        let mut out = Vec::new();
        curate(dir.path(), &mut out).unwrap();
        let text = String::from_utf8(out).unwrap();
        assert!(text.contains("nothing is applied"), "{text}");
        assert!(text.contains("accepted memory: 0"), "{text}");
    }

    #[test]
    fn a_pass_never_writes_to_the_store() {
        // The contract that makes this safe to run on a whim. The freshness pass
        // it drives has an apply mode; this must never reach it.
        let dir = project();
        let lesson = localpilot_localmind::SeedLesson {
            body: "the --foo flag was deprecated in v1.2".to_string(),
            category: None,
            confidence: None,
            related_files: Vec::new(),
            related_entities: Vec::new(),
            evidence: None,
            tags: Vec::new(),
        };
        localpilot_localmind::seed_memory(dir.path(), std::slice::from_ref(&lesson), false)
            .unwrap();

        let before = localpilot_localmind::memory_lifecycle(dir.path(), 10).unwrap();
        let mut out = Vec::new();
        curate(dir.path(), &mut out).unwrap();
        let after = localpilot_localmind::memory_lifecycle(dir.path(), 10).unwrap();

        assert_eq!(
            before.stale.len(),
            after.stale.len(),
            "a curation pass must not flag anything itself"
        );
        let text = String::from_utf8(out).unwrap();
        assert!(
            text.contains("9.21%"),
            "the pass states how much to trust it: {text}"
        );
    }
}
