//! Measure the chunker against the plan's budgets on the real corpus.
//!
//! Not a test: it reads whatever transcripts exist on this machine, so its
//! numbers are a measurement to record, not an assertion to enforce.
#![allow(clippy::unwrap_used, clippy::expect_used, clippy::print_stdout)]

use localpilot_localmind::{read_transcript, SpanKind, TranscriptSchema};
use std::{collections::BTreeMap, path::PathBuf, time::Instant};

fn main() {
    let root = std::env::args()
        .nth(1)
        .map_or_else(|| PathBuf::from(".localmind/sessions"), PathBuf::from);
    let mut files: Vec<PathBuf> = std::fs::read_dir(&root)
        .expect("sessions directory")
        .filter_map(Result::ok)
        .map(|entry| entry.path().join("transcript.redacted.txt"))
        .filter(|path| path.is_file())
        .collect();
    files.sort();

    let mut spans = 0_usize;
    let mut span_bytes = 0_usize;
    let mut source_bytes = 0_usize;
    let mut by_schema: BTreeMap<&str, usize> = BTreeMap::new();
    let mut by_kind: BTreeMap<&str, (usize, usize)> = BTreeMap::new();
    let mut unparseable = 0_usize;
    let mut unrecognised = 0_usize;
    let mut control = 0_usize;
    let mut worst = (0_u128, 0_usize, 0_usize);

    let started = Instant::now();
    for path in &files {
        let text = std::fs::read_to_string(path).unwrap_or_default();
        source_bytes += text.len();
        let file_started = Instant::now();
        let read = read_transcript(&text);
        let elapsed = file_started.elapsed().as_millis();
        *by_schema
            .entry(match read.schema {
                TranscriptSchema::ClaudeJsonl => "claude",
                TranscriptSchema::CodexJsonl => "codex",
                TranscriptSchema::PlainText => "plain",
            })
            .or_default() += 1;
        for span in &read.spans {
            let entry = by_kind
                .entry(match span.kind {
                    SpanKind::UserMessage => "user",
                    SpanKind::AssistantMessage => "assistant",
                    SpanKind::Reasoning => "reasoning",
                    SpanKind::ToolCall => "tool_call",
                    SpanKind::ToolOutput => "tool_output",
                    SpanKind::System => "system",
                })
                .or_default();
            entry.0 += 1;
            entry.1 += span.text.len();
            span_bytes += span.text.len();
        }
        spans += read.spans.len();
        unparseable += read.recovery.unparseable_lines;
        unrecognised += read.recovery.unrecognised_records;
        control += read.recovery.control_records;
        if text.len() > worst.1 {
            worst = (elapsed, text.len(), read.spans.len());
        }
    }
    let total = started.elapsed();

    println!("transcripts        {}", files.len());
    println!(
        "source             {:.1} MiB",
        source_bytes as f64 / 1_048_576.0
    );
    println!("spans              {spans}");
    println!(
        "span text          {:.1} MiB ({:.2}x source)",
        span_bytes as f64 / 1_048_576.0,
        span_bytes as f64 / source_bytes.max(1) as f64
    );
    println!("whole-corpus chunk {:.2} s", total.as_secs_f64());
    println!(
        "largest transcript {:.1} MiB -> {} spans in {} ms",
        worst.1 as f64 / 1_048_576.0,
        worst.2,
        worst.0
    );
    println!("schemas            {by_schema:?}");
    println!(
        "recovery           unparseable={unparseable} unrecognised={unrecognised} control={control}"
    );
    println!("spans by kind:");
    for (kind, (count, bytes)) in &by_kind {
        println!(
            "  {kind:12} {count:8}  {:8.2} MiB",
            *bytes as f64 / 1_048_576.0
        );
    }
}
