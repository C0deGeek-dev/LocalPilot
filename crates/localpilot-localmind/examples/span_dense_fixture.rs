//! Generate the dense-arm fixture once, so the comparison can run offline forever.
//!
//! The evaluation must be deterministic and must not depend on a model server
//! being up — a measurement that needs a live endpoint is a measurement that
//! silently stops happening. So embeddings are computed **once**, here, and
//! committed as a fixture the harness reads.
//!
//! The fixture is committable because everything it embeds is synthetic: the
//! corpus is invented engineering conversation and the queries are written
//! against it. Embedding real transcript spans and committing the vectors would
//! be committing a lossy encoding of session content.
//!
//! Run against a local embedding endpoint:
//!
//! ```text
//! cargo run --release -p localpilot-localmind --example span_dense_fixture -- \
//!     http://127.0.0.1:8090 qwen3-embedding
//! ```
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localpilot_localmind::{query_set, read_transcript, synthetic_corpus, synthetic_locator};

fn main() {
    let mut args = std::env::args().skip(1);
    let base_url = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:8090".into());
    let model = args.next().unwrap_or_else(|| "embedding".into());

    // Re-derive span text through the real chunker, so a fixture key is exactly
    // the locator the index will emit. Deriving it any other way would let the
    // fixture drift from the thing it describes.
    let mut items: Vec<(String, String)> = Vec::new();
    for (session, turns) in synthetic_corpus() {
        let mut body = String::new();
        for (role, text) in turns {
            body.push_str(&format!(
                r#"{{"type":"{role}","message":{{"role":"{role}","content":{}}}}}"#,
                serde_json::to_string(text).unwrap()
            ));
            body.push('\n');
        }
        for span in read_transcript(&body).spans {
            items.push((synthetic_locator(session, span.ordinal), span.text));
        }
    }
    for query in query_set() {
        items.push((format!("query:{}", query.id), query.text));
    }

    let endpoint = localmind_inference::EmbeddingEndpoint::new(&base_url, &model, None, 120)
        .expect("a loopback embedding endpoint");
    println!("embedding {} items via {base_url}", items.len());
    let mut out = String::from("{\n  \"model\": ");
    out.push_str(&serde_json::to_string(&model).unwrap());
    out.push_str(",\n  \"vectors\": {\n");
    for (index, (key, text)) in items.iter().enumerate() {
        let vector = embed(&endpoint, text);
        let rendered: Vec<String> = vector.iter().map(|value| format!("{value:.6}")).collect();
        out.push_str(&format!(
            "    {}: [{}]{}\n",
            serde_json::to_string(key).unwrap(),
            rendered.join(","),
            if index + 1 == items.len() { "" } else { "," }
        ));
        if index % 5 == 0 {
            println!("  {}/{}", index + 1, items.len());
        }
    }
    out.push_str("  }\n}\n");

    let path = std::path::Path::new("crates/localpilot-localmind/tests/fixtures")
        .join("span_dense_vectors.json");
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, out).unwrap();
    println!("wrote {}", path.display());
}

/// Embed one string through the engine's own client.
///
/// Reused rather than reimplemented, which also means this goes through the
/// loopback-only egress guard: a fixture generator pointed at a remote endpoint
/// is refused, and the corpus is synthetic precisely so that would not matter —
/// but the guard should not have an exception carved for convenience.
fn embed(endpoint: &localmind_inference::EmbeddingEndpoint, text: &str) -> Vec<f32> {
    endpoint
        .embed(&[text.to_string()])
        .expect("embedding endpoint")
        .into_iter()
        .next()
        .expect("one vector per input")
}
