//! The first honest retrieval-quality measurement for session spans.
//!
//! Everything measured before this was *coverage* — how many spans exist, how
//! fast they index, how much disk they take. None of it says whether a query
//! returns the spans it should.
//!
//! # Why the corpus is synthetic
//!
//! A lexical query built from a distinctive string in a real transcript **is**
//! transcript content, so committing such a query set would put corpus content
//! in a repository — which the privacy posture forbids for exactly this class of
//! artefact. The committed set therefore runs against a corpus authored for the
//! purpose. That is not a compromise: it also makes the measurement
//! deterministic, runnable in CI, and stable across machines, none of which a
//! real-corpus set could be.
//!
//! Real-corpus runs happen locally and record only their *numbers*.
//!
//! # The set is frozen before it is run
//!
//! A query set adjusted after seeing results measures nothing. These queries and
//! their relevant spans were written against the corpus below before any of them
//! was scored, and the negative class was included from the start — without it a
//! system that returns something for every query scores perfectly.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_localmind::{
    query_set, score_query, span_locator, summarise, synthetic_corpus, QueryClass, SpanStore,
};
use std::path::Path;

fn build(root: &Path) -> SpanStore {
    for (id, turns) in synthetic_corpus() {
        let directory = root.join(".localmind").join("sessions").join(id);
        std::fs::create_dir_all(&directory).unwrap();
        let mut body = String::new();
        for (role, text) in turns {
            body.push_str(&format!(
                r#"{{"type":"{role}","message":{{"role":"{role}","content":{}}}}}"#,
                serde_json::to_string(text).unwrap()
            ));
            body.push('\n');
        }
        std::fs::write(directory.join("transcript.redacted.txt"), body).unwrap();
    }
    let store = SpanStore::open(root).unwrap();
    store
        .index_sessions(&root.join(".localmind").join("sessions"))
        .unwrap();
    store
}

fn run(root: &Path, cut: usize) -> localpilot_localmind::QualityReport {
    let store = build(root);
    let outcomes = query_set()
        .iter()
        .map(|query| {
            let returned: Vec<String> = store
                .search(&query.text, cut)
                .unwrap()
                .iter()
                .map(span_locator)
                .collect();
            score_query(query, &returned, cut)
        })
        .collect();
    summarise(outcomes, cut)
}

// -------------------------------------------------------------- the tests

#[test]
fn the_measurement_is_deterministic() {
    // A benchmark that moves between runs cannot detect a regression.
    let first = tempfile::tempdir().unwrap();
    let second = tempfile::tempdir().unwrap();
    let a = run(first.path(), 5);
    let b = run(second.path(), 5);
    for (left, right) in a.outcomes.iter().zip(b.outcomes.iter()) {
        assert_eq!(left.id, right.id);
        assert_eq!(left.hit, right.hit);
        assert_eq!(left.returned, right.returned);
        assert_eq!(left.first_relevant_rank, right.first_relevant_rank);
    }
}

#[test]
fn lexical_queries_are_answered_at_rank_one() {
    // The floor. A keyword index that cannot put an exact identifier first has
    // no business being the baseline every other arm is compared against.
    let dir = tempfile::tempdir().unwrap();
    let report = run(dir.path(), 5);
    for outcome in report
        .outcomes
        .iter()
        .filter(|outcome| outcome.class == QueryClass::Lexical)
    {
        assert_eq!(
            outcome.first_relevant_rank,
            Some(1),
            "{} should rank its exact match first",
            outcome.id
        );
        assert_eq!(outcome.recall(), Some(1.0), "{}", outcome.id);
    }
}

#[test]
fn cross_session_queries_reach_more_than_one_session() {
    // The capability that motivated the work: the existing session source reads
    // one session's summary.
    let dir = tempfile::tempdir().unwrap();
    let store = build(dir.path());
    let hits = store.search("release version tagging failure", 10).unwrap();
    let sessions: std::collections::BTreeSet<&str> =
        hits.iter().map(|hit| hit.session_id.as_str()).collect();
    assert!(
        sessions.len() >= 2,
        "a cross-session query must reach more than one session, got {sessions:?}"
    );
}

#[test]
fn negative_queries_expose_over_retrieval() {
    // The class that makes the measurement honest. Recall cannot see
    // over-retrieval: a system that answers everything scores perfectly on it.
    let dir = tempfile::tempdir().unwrap();
    let report = run(dir.path(), 5);
    let negatives: Vec<_> = report
        .outcomes
        .iter()
        .filter(|outcome| outcome.class == QueryClass::Negative)
        .collect();
    assert_eq!(negatives.len(), 2);
    for outcome in negatives {
        assert_eq!(
            outcome.negative_correct(),
            Some(true),
            "{} returned {} results for a query with no relevant span",
            outcome.id,
            outcome.returned
        );
    }
}

#[test]
fn a_report_never_contains_span_text() {
    // Privacy clause, pinned structurally. Evaluation artefacts are
    // corpus-derived, and a report quoting what it found is a transcript
    // excerpt wearing the name of a test result.
    let dir = tempfile::tempdir().unwrap();
    let rendered = run(dir.path(), 5).render();
    for phrase in [
        "vector_scan_kind helper returns nothing",
        "subject kind before the cosine gate",
        "submodule pointer",
        "exited non-zero",
        "[REDACTED]",
    ] {
        assert!(
            !rendered.contains(phrase),
            "the report leaked span text: {phrase:?}"
        );
    }
    // It does carry locators, which are addresses rather than content.
    assert!(rendered.contains("lexical"));
    assert!(rendered.contains("L1"));
}

#[test]
fn the_baseline_is_recorded_so_a_regression_is_visible() {
    // The numbers this plan cites. Written as a floor rather than an equality:
    // an improvement should not fail the suite, and a drop should.
    let dir = tempfile::tempdir().unwrap();
    let report = run(dir.path(), 5);
    println!("{}", report.render());

    let lexical = report.by_class.get("lexical").unwrap();
    assert_eq!(lexical.queries, 2);
    assert_eq!(lexical.recall, Some(1.0), "lexical recall is the floor");
    assert!((lexical.mrr - 1.0).abs() < f64::EPSILON);

    let cross = report.by_class.get("cross-session").unwrap();
    assert!(
        cross.recall.unwrap_or_default() >= 0.66,
        "cross-session recall regressed: {:?}",
        cross.recall
    );

    let negative = report.by_class.get("negative").unwrap();
    assert_eq!(negative.negatives_correct, Some(2));

    // **The number a dense arm has to beat: paraphrase recall is zero.**
    //
    // A keyword index cannot match words the span does not contain, and that is
    // the whole of it. Pinned as an equality rather than a floor precisely
    // because it is *not* a target to defend — if a change makes this non-zero,
    // this test should fail and demand an explanation, since the only honest
    // ways to move it are a different retriever or a query set that got easier.
    //
    // It read 1.00 before stopwords were filtered out of queries. That was an
    // artefact: the queries matched on `one`, `the` and `of`, and the metric
    // reported perfect recall for retrieval that was doing nothing.
    let paraphrase = report.by_class.get("paraphrase").unwrap();
    assert_eq!(paraphrase.queries, 2);
    assert_eq!(
        paraphrase.recall,
        Some(0.0),
        "lexical retrieval scores zero on paraphrase; a non-zero value here is          either a new retriever or an easier query set, and both need saying out loud"
    );

    // Redaction's price, measured rather than assumed: the surviving words still
    // locate the span, so indexing the redacted form costs nothing *here*. On a
    // large corpus the same query would compete with far more noise, so this is
    // a floor on a small corpus, not a general claim.
    let redaction = report.by_class.get("redaction-affected").unwrap();
    assert_eq!(redaction.recall, Some(1.0));
}

// ------------------------------------------------------- the dense arm

/// The precomputed vectors, committed so this runs offline and deterministically.
const DENSE_FIXTURE: &str = include_str!("fixtures/span_dense_vectors.json");

#[test]
fn dense_and_lexical_are_compared_on_the_same_frozen_query_set() {
    // This is the plan's headline question, answered with numbers rather than
    // with what everyone expects to be true.
    //
    // The dense arm is **evaluation scaffolding, not a shipped retriever**. It
    // reads precomputed vectors, so it needs no model server and cannot drift
    // between runs — a comparison that needs a live endpoint is one that
    // silently stops happening.
    let fixture = localpilot_localmind::DenseFixture::parse(DENSE_FIXTURE).unwrap();
    let dir = tempfile::tempdir().unwrap();
    let store = build(dir.path());
    let cut = 5;

    let mut lexical_outcomes = Vec::new();
    let mut dense_outcomes = Vec::new();
    for query in query_set() {
        let returned: Vec<String> = store
            .search(&query.text, cut)
            .unwrap()
            .iter()
            .map(span_locator)
            .collect();
        lexical_outcomes.push(score_query(&query, &returned, cut));

        // A negative query's correct dense answer is also "nothing", but cosine
        // always returns *something* — every vector has some similarity to every
        // other. Scoring it against a similarity floor is the only fair
        // comparison; without one the dense arm fails every negative by
        // construction, which would flatter the lexical arm rather than measure
        // it.
        let ranked = fixture.rank(&query.id, cut).unwrap_or_default();
        dense_outcomes.push(score_query(&query, &ranked, cut));
    }

    let lexical = summarise(lexical_outcomes, cut);
    let dense = summarise(dense_outcomes, cut);
    println!("--- lexical (shipped) ---\n{}", lexical.render());
    println!(
        "--- dense (fixture, {}) ---\n{}",
        fixture.model(),
        dense.render()
    );

    // The finding: lexical cannot answer a paraphrase query at all, and dense
    // can. That is the whole case for a dense arm, and it is now a number.
    let lexical_paraphrase = lexical.by_class.get("paraphrase").unwrap();
    let dense_paraphrase = dense.by_class.get("paraphrase").unwrap();
    assert_eq!(lexical_paraphrase.recall, Some(0.0));
    assert!(
        dense_paraphrase.recall.unwrap_or_default() > 0.0,
        "if dense cannot beat zero on paraphrase, the case for it is not made: {:?}",
        dense_paraphrase.recall
    );

    // And the other half, which is the reason this is a comparison and not a
    // replacement: lexical must not lose on the queries it exists to win.
    let lexical_exact = lexical.by_class.get("lexical").unwrap();
    assert_eq!(lexical_exact.recall, Some(1.0));
}

#[test]
fn the_dense_fixture_covers_every_span_and_query() {
    // A fixture missing an entry scores as a retrieval miss, which would read as
    // a finding about dense retrieval rather than a gap in the fixture.
    let fixture = localpilot_localmind::DenseFixture::parse(DENSE_FIXTURE).unwrap();
    for query in query_set() {
        assert!(
            fixture.rank(&query.id, 1).is_some(),
            "no fixture vector for query {}",
            query.id
        );
    }
    let dir = tempfile::tempdir().unwrap();
    let store = build(dir.path());
    let indexed = store.span_count().unwrap();
    let ranked = fixture.rank("L1", usize::MAX).unwrap_or_default();
    assert_eq!(
        ranked.len(),
        indexed,
        "the fixture must cover exactly the spans the index holds"
    );
}

#[test]
fn whether_a_similarity_floor_could_separate_negatives_is_measured_not_assumed() {
    // The dense arm answers every query with a full result set, because cosine
    // always returns *something*. The obvious remedy is a similarity floor — but
    // choosing a threshold after seeing which queries it rescues is tuning, not
    // measurement.
    //
    // So this measures the thing a floor would depend on and stops there:
    // whether the best similarity for a query with an answer separates from the
    // best similarity for a query without one. If they overlap, no floor exists
    // that works, and that is a finding about dense retrieval on this corpus
    // rather than a knob left unturned.
    let fixture = localpilot_localmind::DenseFixture::parse(DENSE_FIXTURE).unwrap();
    let mut answerable: Vec<f32> = Vec::new();
    let mut unanswerable: Vec<f32> = Vec::new();
    for query in query_set() {
        let Some(top) = fixture.top_similarity(&query.id) else {
            continue;
        };
        if query.class == QueryClass::Negative {
            unanswerable.push(top);
        } else {
            answerable.push(top);
        }
        println!(
            "{:<4} {:<18} top cosine {top:.4}",
            query.id,
            query.class.name()
        );
    }
    let worst_answerable = answerable.iter().copied().fold(f32::MAX, f32::min);
    let best_unanswerable = unanswerable.iter().copied().fold(f32::MIN, f32::max);
    println!(
        "\nworst answerable {worst_answerable:.4}  best unanswerable {best_unanswerable:.4}  \
         separation {:.4}",
        worst_answerable - best_unanswerable
    );
    assert!(!answerable.is_empty() && !unanswerable.is_empty());
}
