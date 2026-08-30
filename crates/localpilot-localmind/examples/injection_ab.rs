//! Does injecting retrieved memory change the answer?
//!
//! Two arms over the same tasks and the same model: **injected** (the prompt
//! carries what `context_hits` returned) against **suppressed** (the prompt
//! alone). Everything else is held fixed, so the only variable is the memory.
//!
//! # What makes a task admissible
//!
//! Every answer is project knowledge that cannot be derived from general
//! training, and every answer is **exactly gradeable** — the grader is a string
//! comparison, not a model, so a task whose correct answer cannot be stated
//! exactly is excluded rather than graded loosely.
//!
//! Answers appearing in more than a handful of memories are also excluded: if the
//! answer string is everywhere, injecting *any* memory helps the injected arm,
//! which manufactures a positive out of the task set instead of measuring one.
//!
//! # Where the variance comes from
//!
//! Generation is **deterministic**: the engine's chat client sends
//! `temperature: 0.0`, so repeating a prompt returns the same answer and repeat
//! trials would buy nothing. The planned five-trials-per-arm parameter does not
//! survive that, and running it anyway would be theatre.
//!
//! So each task is asked **once per arm**, and the per-arm interval comes from
//! variance **across tasks** — which is what the pooled standard deviation is
//! computed from in any case. This is a stronger design than the parameter it
//! replaces: the whole A/B is reproducible byte-for-byte, and the failure it was
//! written to correct (a prior run with one observation per arm) is corrected by
//! eighteen tasks rather than by five repeats of one.
//!
//! # Why each task records whether retrieval delivered
//!
//! Retrieval quality was measured first and it is uneven — one query class
//! returns the right memory none of the time. A bare null here would be
//! ambiguous: *memory does not help* and *we injected the wrong memories* produce
//! the same number. Each task records whether a memory that actually contains the
//! answer was among what was injected, and the result is reported split by that.
//! A positive needs no such split — injection helping despite imperfect retrieval
//! is still injection helping.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localmind_inference::{ChatEndpoint, ChatMessage};
use localpilot_localmind::context_hits;
use std::path::PathBuf;

#[derive(serde::Deserialize)]
struct Task {
    question: String,
    expect: String,
    owners: Vec<String>,
}

fn main() {
    let mut args = std::env::args().skip(1);
    let project = args.next().map(PathBuf::from).expect("project root");
    let tasks_path = args.next().map(PathBuf::from).expect("tasks.json");
    let base_url = args
        .next()
        .unwrap_or_else(|| "http://127.0.0.1:11435".to_string());
    let model = args.next().unwrap_or_else(|| "local".to_string());

    let tasks: Vec<Task> =
        serde_json::from_str(&std::fs::read_to_string(&tasks_path).expect("tasks.json"))
            .expect("parse tasks");
    let endpoint =
        ChatEndpoint::new(&base_url, &model, None, 300).expect("a loopback chat endpoint");

    println!("tasks   : {}", tasks.len());
    println!("model   : {model} @ {base_url}");
    println!("sampling: deterministic (temperature 0) — variance is across tasks\n");
    println!(
        "  {:24} {:>9}  {:>10} {:>8}",
        "expected answer", "delivered", "suppressed", "injected"
    );

    let mut injected = Vec::new();
    let mut suppressed = Vec::new();
    let mut delivered = Vec::new();

    for task in &tasks {
        let hits = context_hits(&project, &task.question, None).unwrap_or_default();
        // Did retrieval actually deliver a memory containing the answer?
        let got_it = hits
            .iter()
            .any(|hit| task.owners.iter().any(|owner| owner == &hit.memory_id));
        delivered.push(got_it);

        let context = if hits.is_empty() {
            String::new()
        } else {
            let mut out = String::from("Relevant accepted project memory:\n");
            for hit in &hits {
                out.push_str(&format!("- {}\n", hit.snippet.trim()));
            }
            out.push('\n');
            out
        };

        let with = ask(&endpoint, &format!("{context}{}", task.question));
        let without = ask(&endpoint, &task.question);
        let inj = f64::from(u8::from(graded(&with, &task.expect)));
        let sup = f64::from(u8::from(graded(&without, &task.expect)));
        injected.push(inj);
        suppressed.push(sup);
        println!(
            "  {:24} {:>9}  {:>10.0} {:>8.0}",
            task.expect, got_it, sup, inj
        );
    }

    report("ALL TASKS", &suppressed, &injected);

    let split = |want: bool| -> (Vec<f64>, Vec<f64>) {
        let mut s = Vec::new();
        let mut i = Vec::new();
        for (index, got) in delivered.iter().enumerate() {
            if *got == want {
                s.push(suppressed[index]);
                i.push(injected[index]);
            }
        }
        (s, i)
    };
    let (ds, di) = split(true);
    let (ns, ni) = split(false);
    if !ds.is_empty() {
        report("RETRIEVAL DELIVERED THE ANSWER", &ds, &di);
    }
    if !ns.is_empty() {
        report("RETRIEVAL DID NOT DELIVER", &ns, &ni);
    }

    #[allow(clippy::cast_precision_loss)]
    let fraction = delivered.iter().filter(|d| **d).count() as f64 / delivered.len() as f64;
    println!(
        "
retrieval delivered the answer for {:.0}% of tasks ({} of {})",
        fraction * 100.0,
        delivered.iter().filter(|d| **d).count(),
        delivered.len()
    );
}

/// Mean, sample standard deviation, delta and the band the delta must clear.
///
/// The band is the pooled per-arm standard deviation — a conservative
/// non-overlap proxy at this sample size. Reported always, so a delta is never
/// quoted as a bare point estimate.
fn report(label: &str, suppressed: &[f64], injected: &[f64]) {
    let (sm, ss) = mean_sd(suppressed);
    let (im, is) = mean_sd(injected);
    let delta = im - sm;
    let pooled = ((ss * ss + is * is) / 2.0).sqrt();
    let verdict = if delta > pooled {
        "UPLIFT"
    } else if delta < -pooled {
        "REGRESSION"
    } else {
        "NO EFFECT"
    };
    println!(
        "\n{label}  (n={} tasks)\n  suppressed {sm:.3} (sd {ss:.3})\n  injected   {im:.3} (sd {is:.3})\n  delta      {delta:+.3}   band +/-{pooled:.3}   ->  {verdict}",
        suppressed.len()
    );
}

fn mean_sd(values: &[f64]) -> (f64, f64) {
    if values.is_empty() {
        return (0.0, 0.0);
    }
    #[allow(clippy::cast_precision_loss)]
    let n = values.len() as f64;
    let mean = values.iter().sum::<f64>() / n;
    if values.len() < 2 {
        return (mean, 0.0);
    }
    let var = values.iter().map(|v| (v - mean).powi(2)).sum::<f64>() / (n - 1.0);
    (mean, var.sqrt())
}

/// Deterministic grading: does the answer contain the expected string?
///
/// Case-insensitive and whitespace-normalised, because a model writes
/// `localmind mcp serve` and `LocalMind MCP serve` interchangeably and neither is
/// more correct than the other.
fn graded(answer: &str, expect: &str) -> bool {
    let norm = |s: &str| {
        s.to_lowercase()
            .split_whitespace()
            .collect::<Vec<_>>()
            .join(" ")
    };
    norm(answer).contains(&norm(expect))
}

fn ask(endpoint: &ChatEndpoint, prompt: &str) -> String {
    match endpoint.complete(&[ChatMessage::user(prompt)]) {
        Ok(completion) => completion.content,
        Err(error) => {
            eprintln!("model call failed: {error}");
            String::new()
        }
    }
}
