//! Reading a frozen judgment set, and the trivial known case the harness is
//! verified against before it is trusted on a real one.
//!
//! Every number this plan reports depends on two things being right: the set is
//! the one it claims to be (the hash), and an unjudged result is not silently
//! counted as wrong (the bounds). Both are cheap to get wrong and invisible
//! afterwards.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_localmind::{score_query, summarise, JudgmentSet, QueryClass};
use std::collections::BTreeMap;

fn meta(pairs: &[(&str, QueryClass, &str)]) -> BTreeMap<String, (QueryClass, String)> {
    pairs
        .iter()
        .map(|(id, class, text)| ((*id).to_string(), (*class, (*text).to_string())))
        .collect()
}

const QRELS: &str = "\
Q01 0 mem-a 1
Q01 0 mem-b 0
Q01 0 mem-c 0
Q02 0 mem-d 0
Q02 0 mem-e 0
";

#[test]
fn the_digest_is_the_hash_of_the_file_and_changes_with_a_single_label() {
    // A judgment set that changed under a measurement invalidates it. The hash is
    // the only thing that makes that detectable rather than assumed.
    let metadata = meta(&[
        ("Q01", QueryClass::Lexical, "find a"),
        ("Q02", QueryClass::Negative, "find nothing"),
    ]);
    let first = JudgmentSet::parse(QRELS, &metadata).unwrap();
    let same = JudgmentSet::parse(QRELS, &metadata).unwrap();
    assert_eq!(
        first.digest(),
        same.digest(),
        "the same bytes hash the same"
    );
    assert_eq!(first.digest().len(), 64);

    // Flip exactly one relevance grade.
    let flipped = QRELS.replace("Q01 0 mem-b 0", "Q01 0 mem-b 1");
    let other = JudgmentSet::parse(&flipped, &metadata).unwrap();
    assert_ne!(
        first.digest(),
        other.digest(),
        "one changed label must change the hash"
    );
}

#[test]
fn a_known_sha256_matches_a_reference_value() {
    // Verifying the hash implementation itself against a value anyone can check,
    // because a wrong-but-consistent digest would pass every other test here and
    // silently fail to detect a changed set.
    let metadata = meta(&[("Q01", QueryClass::Lexical, "x")]);
    let set = JudgmentSet::parse("Q01 0 abc 1\n", &metadata).unwrap();
    // sha256("Q01 0 abc 1\n")
    assert_eq!(
        set.digest(),
        "feaceed1fe1d060d2e667831968915ed00456167b81bb17f2698776c99f72c56",
        "sha256(\"Q01 0 abc 1\n\") — independently confirmed against a reference          implementation, so a wrong-but-self-consistent digest cannot pass here"
    );
}

#[test]
fn relevance_and_the_judged_pool_are_read_apart() {
    let metadata = meta(&[
        ("Q01", QueryClass::Lexical, "find a"),
        ("Q02", QueryClass::Negative, "find nothing"),
    ]);
    let set = JudgmentSet::parse(QRELS, &metadata).unwrap();
    let queries = set.queries();
    assert_eq!(queries.len(), 2);

    let q1 = &queries[0];
    assert_eq!(q1.relevant, vec!["mem-a".to_string()]);
    assert_eq!(
        q1.judged.as_ref().unwrap().len(),
        3,
        "all three were judged"
    );

    let q2 = &queries[1];
    assert!(q2.relevant.is_empty(), "a negative query has no positives");
    assert_eq!(q2.judged.as_ref().unwrap().len(), 2);
}

#[test]
fn the_trivial_known_case_scores_exactly_as_arithmetic_says() {
    // Verify the harness on a case whose answer can be
    // computed by hand, before trusting it on a corpus where it cannot.
    //
    // Q01 has one relevant memory of three judged. A retriever returning
    // [mem-a, mem-b] has found the one positive (recall 1.0) among two results
    // (precision 0.5), with the positive first (reciprocal rank 1.0).
    let metadata = meta(&[
        ("Q01", QueryClass::Lexical, "find a"),
        ("Q02", QueryClass::Negative, "find nothing"),
    ]);
    let set = JudgmentSet::parse(QRELS, &metadata).unwrap();
    let query = &set.queries()[0];

    let returned = vec!["mem-a".to_string(), "mem-b".to_string()];
    let outcome = score_query(query, &returned, 5);

    assert_eq!(outcome.relevant, 1);
    assert_eq!(outcome.hit, 1);
    assert_eq!(outcome.returned, 2);
    assert_eq!(outcome.recall(), Some(1.0));
    assert_eq!(outcome.precision(), Some(0.5));
    assert_eq!(outcome.first_relevant_rank, Some(1));
    assert!((outcome.reciprocal_rank() - 1.0).abs() < f64::EPSILON);
    assert_eq!(outcome.unjudged, 0, "both results were judged");
    assert_eq!(outcome.coverage(), Some(1.0));
    assert_eq!(outcome.precision_lower(), Some(0.5));
    assert_eq!(
        outcome.precision_upper(),
        Some(0.5),
        "with nothing unjudged the bounds meet"
    );
}

#[test]
fn an_unjudged_result_widens_the_bounds_rather_than_scoring_as_wrong() {
    // The rule the whole plan turns on. `mem-z` was never judged: counting it
    // irrelevant would understate precision, because a genuinely relevant memory
    // that never reached the pool would be scored as a wrong answer.
    let metadata = meta(&[
        ("Q01", QueryClass::Lexical, "find a"),
        ("Q02", QueryClass::Negative, "find nothing"),
    ]);
    let set = JudgmentSet::parse(QRELS, &metadata).unwrap();
    let query = &set.queries()[0];

    let returned = vec!["mem-a".to_string(), "mem-z".to_string()];
    let outcome = score_query(query, &returned, 5);

    assert_eq!(outcome.unjudged, 1);
    assert_eq!(outcome.coverage(), Some(0.5), "one of two was judged");
    assert_eq!(
        outcome.precision_lower(),
        Some(0.5),
        "pessimistic: the unjudged result counts as wrong"
    );
    assert_eq!(
        outcome.precision_upper(),
        Some(1.0),
        "optimistic: the unjudged result might be right"
    );
    // Recall is untouched by an unjudged result — it is measured against the
    // judged-relevant set, which is why it is known-positive recall and not
    // recall.
    assert_eq!(outcome.recall(), Some(1.0));
}

#[test]
fn a_negative_query_that_returns_something_is_visible() {
    // Over-retrieval is the one failure recall cannot see: a system returning
    // everything has perfect recall.
    let metadata = meta(&[
        ("Q01", QueryClass::Lexical, "find a"),
        ("Q02", QueryClass::Negative, "find nothing"),
    ]);
    let set = JudgmentSet::parse(QRELS, &metadata).unwrap();
    let query = set
        .queries()
        .iter()
        .find(|q| q.id == "Q02")
        .expect("Q02 present");

    let quiet = score_query(query, &[], 5);
    assert_eq!(quiet.negative_correct(), Some(true));

    let noisy = score_query(query, &["mem-d".to_string()], 5);
    assert_eq!(noisy.negative_correct(), Some(false));
    assert_eq!(noisy.recall(), None, "recall is undefined, not zero");

    let report = summarise(vec![quiet, noisy], 5);
    assert_eq!(
        report.by_class.get("negative").unwrap().negatives_correct,
        Some(1)
    );
}

#[test]
fn a_malformed_qrels_line_is_refused_rather_than_guessed() {
    let metadata = meta(&[("Q01", QueryClass::Lexical, "x")]);
    for bad in ["Q01 0 mem-a", "Q01 0 mem-a notanumber", "Q01"] {
        assert!(
            JudgmentSet::parse(bad, &metadata).is_err(),
            "{bad:?} must be refused"
        );
    }
}

#[test]
fn a_query_without_metadata_is_refused() {
    // A query with no class cannot be reported per class, and silently dropping
    // it would shrink the set without saying so.
    assert!(JudgmentSet::parse(QRELS, &BTreeMap::new()).is_err());
}
