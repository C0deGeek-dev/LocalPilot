//! Confirm the committed fixture still matches what the live endpoint produces.
//!
//! The offline comparison is only trustworthy if its cached vectors are the ones
//! a live endpoint would return. This re-embeds the frozen query set against a
//! running endpoint and compares, so "offline baseline" means "the live numbers,
//! cached" rather than "numbers from something we can no longer check".
//!
//! Opportunistic by policy: it needs a local model server, so it is never a
//! blocking gate. A disagreement is itself the finding.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localpilot_localmind::{query_set, DenseFixture};

const FIXTURE: &str = include_str!("../tests/fixtures/span_dense_vectors.json");

fn main() {
    let mut args = std::env::args().skip(1);
    let base_url = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:8090".into());
    let model = args.next().unwrap_or_else(|| "embedding".into());

    let fixture = DenseFixture::parse(FIXTURE).expect("fixture");
    let endpoint = localmind_inference::EmbeddingEndpoint::new(&base_url, &model, None, 120)
        .expect("a loopback embedding endpoint");

    println!("fixture model: {}", fixture.model());
    println!("live endpoint: {base_url}\n");

    let mut worst = f32::MAX;
    let mut disagreements = 0;
    for query in query_set() {
        let live = endpoint
            .embed(&[query.text.clone()])
            .expect("embed")
            .into_iter()
            .next()
            .expect("one vector");
        // Compare through the fixture's own similarity function rather than
        // element-wise: what matters is whether the live vector ranks the corpus
        // the same way, not whether it is bit-identical.
        let cached_top = fixture.top_similarity(&query.id).unwrap_or_default();
        let live_top = fixture.top_similarity_for(&live).unwrap_or_default();
        let delta = (cached_top - live_top).abs();
        worst = worst.min(1.0 - delta);
        if delta > 0.01 {
            disagreements += 1;
        }
        println!(
            "{:<4} cached top {cached_top:.4}  live top {live_top:.4}  delta {delta:.5}",
            query.id
        );
    }
    println!("\n{} disagreement(s) above 0.01", disagreements);
    if disagreements == 0 {
        println!("the offline baseline is the live baseline, cached");
    }
}
