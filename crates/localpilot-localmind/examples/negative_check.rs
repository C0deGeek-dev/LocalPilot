//! Measure over-retrieval: queries whose subject exists nowhere in the corpus.
//!
//! Each anchor is a synthetic identifier whose every sub-token appears in zero
//! active memories, so the correct answer is nothing — by construction, with no
//! labelling pass and no judgement to be shaded.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]
use localmind_store::ProjectConfig;
use localpilot_localmind::context_hits;
use std::path::PathBuf;
fn main() {
    let mut a = std::env::args().skip(1);
    let root = a.next().map(PathBuf::from).expect("root");
    let queries = std::fs::read_to_string(a.next().expect("queries")).expect("queries");

    // Say which corpus this is measuring, before measuring it.
    //
    // `.cargo/config.toml` sets `LOCALMIND_GLOBAL_ROOT=@project` so tests are
    // hermetic, and `cargo run --example` inherits it. An eval run therefore
    // reads an empty global store unless the variable is overridden, and reports
    // a number for a corpus a third the size of the real one without saying so.
    // It cost a day of chasing a recall figure that had collapsed for that reason
    // alone. One line of provenance is the whole fix.
    let config = ProjectConfig::discover(&root).expect("config");
    eprintln!(
        "corpus: {} | global root {:?}",
        root.display(),
        config.global_memory_root()
    );
    let mut noisy = 0;
    let mut total = 0;
    let mut returned = 0;
    for line in queries.lines() {
        let Some((id, text)) = line.split_once('\t') else {
            continue;
        };
        total += 1;
        let got = context_hits(&root, text, None).unwrap_or_default();
        returned += got.len();
        if got.is_empty() {
            println!("  {id} clean");
        } else {
            noisy += 1;
            let top = got.first().map(|h| h.memory_id.as_str()).unwrap_or("");
            let cos = got
                .first()
                .and_then(|h| h.cosine)
                .map_or_else(|| "none".to_string(), |c| format!("{c:.3}"));
            println!("  {id} OVER-RETRIEVES {} (top {top} cos {cos})", got.len());
        }
    }
    #[allow(clippy::cast_precision_loss)]
    let rate = noisy as f64 / total.max(1) as f64;
    println!("\nover-retrieval: {noisy}/{total} = {rate:.3}");
    println!("memories returned in total: {returned}");
}
