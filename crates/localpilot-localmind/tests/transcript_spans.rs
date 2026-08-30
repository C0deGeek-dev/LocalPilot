//! Reading real transcript shapes into spans.
//!
//! The corpus is three record schemas — Claude Code JSONL (76% of bytes), Codex
//! JSONL (23%), and LocalPilot's line-oriented rendering (0.4%) — and the two
//! that matter are both JSONL. Most of these tests exist because a reader that
//! understands only one shape *looks correct* on the rest, which is the failure
//! mode worth pinning.
#![allow(clippy::unwrap_used, clippy::expect_used)]

use localpilot_localmind::{
    detect_schema, read_transcript, SpanKind, TranscriptSchema, MAX_SPAN_BYTES,
    SPAN_CHUNKING_VERSION,
};

const CLAUDE: &str = concat!(
    r#"{"type":"mode","mode":"normal"}"#,
    "\n",
    r#"{"type":"user","message":{"role":"user","content":"why is the index empty"}}"#,
    "\n",
    r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"thinking","thinking":"the vectors were never written"},{"type":"text","text":"the embedding endpoint was down"}]}}"#,
    "\n",
    r#"{"type":"assistant","message":{"role":"assistant","content":[{"type":"tool_use","name":"run_shell","input":{"command":"localmind status"}}]}}"#,
    "\n",
    r#"{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"vectors: 0"}]}}"#,
    "\n",
    r#"{"type":"ai-title","title":"debugging"}"#,
    "\n",
);

const CODEX: &str = concat!(
    r#"{"timestamp":"t","type":"session_meta","payload":{"type":"session_meta"}}"#,
    "\n",
    r#"{"timestamp":"t","type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"check the release train"}]}}"#,
    "\n",
    r#"{"timestamp":"t","type":"response_item","payload":{"type":"reasoning","summary":[{"type":"summary_text","text":"parity across five repos"}]}}"#,
    "\n",
    r#"{"timestamp":"t","type":"response_item","payload":{"type":"custom_tool_call","name":"shell","arguments":"git status"}}"#,
    "\n",
    r#"{"timestamp":"t","type":"response_item","payload":{"type":"custom_tool_call_output","output":"nothing to commit"}}"#,
    "\n",
    // The duplicate carrier: the same reasoning again, in the other shape.
    r#"{"timestamp":"t","type":"event_msg","payload":{"type":"item_completed","item":{"type":"Reasoning","text":"parity across five repos"}}}"#,
    "\n",
    r#"{"timestamp":"t","type":"event_msg","payload":{"type":"token_count","info":{}}}"#,
    "\n",
);

const PLAIN: &str = "\
user: what does localbox stop do
assistant (reasoning): recalling the command surface
assistant calls run_shell: {\"command\":\"localbox stop\"}
tool result: tool: run_shell
status: success
output:
exit: 0
--- stdout ---
stopped every server and proxy
assistant: it stops every server and proxy
";

#[test]
fn each_schema_is_recognised_by_its_records_not_its_filename() {
    assert_eq!(detect_schema(CLAUDE), TranscriptSchema::ClaudeJsonl);
    assert_eq!(detect_schema(CODEX), TranscriptSchema::CodexJsonl);
    assert_eq!(detect_schema(PLAIN), TranscriptSchema::PlainText);
}

#[test]
fn a_quoted_string_is_valid_json_and_is_not_a_record() {
    // This is how a phantom "mixed format" was manufactured: a plain-text
    // transcript containing pasted Rust source has lines like the one below,
    // and `serde_json` accepts every one of them as a JSON *string*.
    let pasted = "user: paste this\n\"    configured model: {configured}\"\n\" (active)\"\n";
    assert_eq!(
        detect_schema(pasted),
        TranscriptSchema::PlainText,
        "a line must start with '{{' and parse as an object to count as a record"
    );
}

#[test]
fn detection_reads_the_whole_input_not_a_prefix() {
    // In the real file that caused this, the misleading lines begin past line
    // 450. A prefix probe classifies it with no evidence either way.
    let mut text = String::new();
    for index in 0..500 {
        text.push_str(&format!("user: message {index}\n"));
    }
    text.push_str("\"a quoted line that parses as JSON\"\n");
    assert_eq!(detect_schema(&text), TranscriptSchema::PlainText);
}

#[test]
fn claude_conversation_is_indexed_and_control_records_are_not() {
    let read = read_transcript(CLAUDE);
    let kinds: Vec<SpanKind> = read.spans.iter().map(|span| span.kind).collect();
    assert_eq!(
        kinds,
        vec![
            SpanKind::UserMessage,
            SpanKind::Reasoning,
            SpanKind::AssistantMessage,
            SpanKind::ToolCall,
            SpanKind::ToolOutput,
        ]
    );
    // `mode` and `ai-title` are understood and deliberately excluded — reported,
    // never silently dropped.
    assert_eq!(read.recovery.control_records, 2);
    assert_eq!(read.recovery.unparseable_lines, 0);
    assert_eq!(read.recovery.unrecognised_records, 0);
}

#[test]
fn codex_turns_are_read_once_from_one_carrier() {
    // Codex writes each turn twice. Reading both carriers would return two spans
    // of the same moment for every hit — which reads as a ranking failure and is
    // actually an ingestion one.
    let read = read_transcript(CODEX);
    let reasoning = read
        .spans
        .iter()
        .filter(|span| span.kind == SpanKind::Reasoning)
        .count();
    assert_eq!(
        reasoning, 1,
        "the same reasoning appears in both carriers and must be indexed once"
    );
    assert!(read
        .spans
        .iter()
        .any(|span| span.kind == SpanKind::ToolOutput && span.text == "nothing to commit"));
    // The duplicate carrier and the token counter are both control here.
    assert_eq!(read.recovery.control_records, 3);
}

#[test]
fn a_plain_text_record_is_not_a_line() {
    // `status:`, `output:` and `exit:` all look like speaker headers and are all
    // tool-result body. A line-per-record reader shreds this format.
    let read = read_transcript(PLAIN);
    assert_eq!(read.spans.len(), 5, "five records, none of them split");
    let tool_output = read
        .spans
        .iter()
        .find(|span| span.kind == SpanKind::ToolOutput)
        .expect("the tool result is one record");
    for inner in [
        "status: success",
        "exit: 0",
        "stopped every server and proxy",
    ] {
        assert!(
            tool_output.text.contains(inner),
            "{inner:?} belongs to the tool result, not to a record of its own"
        );
    }
}

#[test]
fn an_indented_speaker_label_is_body() {
    let text = "user: paste\n    assistant: this is quoted output\ntool result: done\n";
    let read = read_transcript(text);
    assert_eq!(read.spans.len(), 2);
    assert!(read.spans[0]
        .text
        .contains("    assistant: this is quoted output"));
}

#[test]
fn chunking_is_deterministic_across_runs() {
    for source in [CLAUDE, CODEX, PLAIN] {
        let first = read_transcript(source);
        let second = read_transcript(source);
        assert_eq!(first.spans, second.spans);
        assert_eq!(first.recovery, second.recovery);
    }
}

#[test]
fn multi_byte_characters_are_never_split_in_half() {
    // One enormous line with no break opportunities, made of 3-byte characters
    // so no split offset lands on a boundary by luck.
    let line = "日".repeat(MAX_SPAN_BYTES);
    let text = format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":{}}}}}\n",
        serde_json::to_string(&line).unwrap()
    );
    let read = read_transcript(&text);
    assert!(read.spans.len() > 1, "an over-long record must be split");
    let mut rebuilt = String::new();
    for span in &read.spans {
        assert!(span.text.len() <= MAX_SPAN_BYTES);
        rebuilt.push_str(&span.text);
    }
    assert_eq!(
        rebuilt, line,
        "splitting must lose nothing and corrupt nothing"
    );
}

#[test]
fn long_single_line_tool_output_is_split_and_parts_are_numbered() {
    // Command output is the corpus's largest content type, and it routinely
    // arrives as one line far past the bound.
    let output = "x".repeat(MAX_SPAN_BYTES * 3 + 17);
    let text = format!(
        "{{\"type\":\"user\",\"message\":{{\"role\":\"user\",\"content\":[{{\"type\":\"tool_result\",\"content\":{}}}]}}}}\n",
        serde_json::to_string(&output).unwrap()
    );
    let read = read_transcript(&text);
    assert_eq!(read.spans.len(), 4);
    for (index, span) in read.spans.iter().enumerate() {
        assert_eq!(span.kind, SpanKind::ToolOutput);
        assert_eq!(
            span.part, index,
            "parts number from zero within their record"
        );
        assert_eq!(span.ordinal, index);
        assert_eq!(span.chunking_version, SPAN_CHUNKING_VERSION);
    }
}

#[test]
fn a_malformed_line_does_not_abort_the_transcript() {
    // Genuine corruption is rare (~0.02% of the real corpus) and not confined to
    // one export, so one bad record must cost one record and nothing more.
    let text = format!(
        "{}{}{}",
        r#"{"type":"user","message":{"role":"user","content":"before"}}"#,
        "\n{\"type\":\"user\",\"message\":{ TRUNCATED\n",
        "{\"type\":\"user\",\"message\":{\"role\":\"user\",\"content\":\"after\"}}\n"
    );
    let read = read_transcript(&text);
    assert_eq!(read.recovery.unparseable_lines, 1);
    assert_eq!(read.recovery.indexed_records, 2);
    let texts: Vec<&str> = read.spans.iter().map(|span| span.text.as_str()).collect();
    assert_eq!(texts, vec!["before", "after"]);
}

#[test]
fn corruption_is_reported_separately_from_an_unknown_record_type() {
    // Conflating these is how a reader that ignores a quarter of the corpus
    // still looks healthy. Only the first is a defect; the second is news.
    let text = concat!(
        r#"{"type":"user","message":{"role":"user","content":"kept"}}"#,
        "\n",
        r#"{"type":"a-record-type-invented-next-year","detail":"who knows"}"#,
        "\n",
        "{ not json at all\n",
    );
    let read = read_transcript(text);
    assert_eq!(read.recovery.unparseable_lines, 1);
    assert_eq!(read.recovery.unrecognised_records, 1);
    assert_eq!(read.recovery.indexed_records, 1);
}

#[test]
fn an_empty_or_contentless_transcript_yields_no_spans() {
    for text in ["", "\n\n\n", r#"{"type":"mode","mode":"normal"}"#] {
        let read = read_transcript(text);
        assert!(read.spans.is_empty());
        assert!(read.recovery.is_empty());
    }
}
