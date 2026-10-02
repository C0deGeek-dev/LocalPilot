//! Explicit, bounded local evidence for review attempts. Never mailbox traffic.

use std::fs::{File, OpenOptions};
use std::io::{self, Write};
use std::path::Path;

use localpilot_config::redact::redact;
use localpilot_mesh::ops::engine::Request;
use serde_json::{json, Value};

pub(crate) const TEXT_BYTES: usize = 8 * 1024;
const FILE_BYTES: usize = 1024 * 1024;
const LIMIT_RESERVE: usize = 512;

#[derive(Clone, Copy, serde::Serialize)]
#[serde(rename_all = "snake_case")]
pub(crate) enum Outcome {
    Accepted,
    TurnError,
    ParseError,
    ValidationError,
}

pub(crate) struct Attempt<'a> {
    pub request: &'a Request,
    pub role: &'a str,
    pub model: &'a str,
    pub number: usize,
    pub stop: Option<&'a str>,
    pub text: Option<&'a str>,
    pub outcome: Outcome,
    pub error: Option<&'a str>,
}

pub(crate) struct Capture {
    file: File,
    bytes: usize,
    exhausted: bool,
}

impl Capture {
    /// Exclusive creation refuses existing files (including final symlinks).
    /// Parent directories must already exist; the mailbox is never a target.
    pub(crate) fn create(path: &Path, anchor: &Path) -> io::Result<Self> {
        let parent = path
            .parent()
            .filter(|p| !p.as_os_str().is_empty())
            .unwrap_or(Path::new("."));
        let parent = parent.canonicalize()?;
        let mailbox = anchor.join(".pair-programming");
        let mailbox = mailbox.canonicalize().unwrap_or(mailbox);
        if parent.starts_with(mailbox)
            || parent
                .components()
                .any(|p| p.as_os_str() == ".pair-programming")
        {
            return Err(io::Error::new(
                io::ErrorKind::InvalidInput,
                "review diagnostics must be outside the mailbox",
            ));
        }
        let name = path.file_name().ok_or_else(|| {
            io::Error::new(io::ErrorKind::InvalidInput, "diagnostic file name required")
        })?;
        let mut options = OpenOptions::new();
        options.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt;
            options.mode(0o600);
        }
        Ok(Self::new(options.open(parent.join(name))?))
    }

    pub(crate) fn new(file: File) -> Self {
        Self {
            file,
            bytes: 0,
            exhausted: false,
        }
    }

    pub(crate) fn record(&mut self, attempt: Attempt<'_>) -> io::Result<()> {
        if self.exhausted {
            return Ok(());
        }
        let record = json!({
            "format_version": 1,
            "event": "review_attempt",
            "session_id": sample(&attempt.request.expect.session_id, 256),
            "request_id": sample(&attempt.request.msg_id, 256),
            "role": sample(attempt.role, 256),
            "model": sample(attempt.model, 512),
            "attempt": attempt.number,
            "attempt_kind": if attempt.number == 1 { "initial" } else { "repair" },
            "stop_reason": attempt.stop.map(|s| sample(s, 1024)),
            "response": attempt.text.map(|s| sample(s, TEXT_BYTES)),
            "response_kind": response_kind(attempt.text, attempt.outcome),
            "outcome": attempt.outcome,
            "error": attempt.error.map(|s| sample(s, 1024)),
        });
        let mut line = serde_json::to_vec(&record)?;
        line.push(b'\n');
        if self.bytes + line.len() > FILE_BYTES - LIMIT_RESERVE {
            let marker = b"{\"format_version\":1,\"event\":\"capture_limit\",\"capture_truncated\":true,\"max_bytes\":1048576}\n";
            self.file.write_all(marker)?;
            self.bytes += marker.len();
            self.exhausted = true;
        } else {
            self.file.write_all(&line)?;
            self.bytes += line.len();
        }
        self.file.flush()
    }
}

fn sample(raw: &str, cap: usize) -> Value {
    // Redact the complete value first: cutting a token in half can defeat its
    // detector. Keep only a bounded UTF-8 prefix, explicitly marked as partial.
    let redacted = redact(raw);
    let mut end = redacted.len().min(cap);
    while !redacted.is_char_boundary(end) {
        end -= 1;
    }
    json!({ "text": &redacted[..end], "original_bytes": raw.len(),
            "truncated": end < redacted.len() })
}

fn response_kind(text: Option<&str>, outcome: Outcome) -> &'static str {
    let Some(text) = text else {
        return "unavailable";
    };
    if text.trim().is_empty() {
        return "empty";
    }
    if matches!(outcome, Outcome::Accepted | Outcome::ValidationError) {
        return "answer";
    }
    // Mirror the parser's candidate boundaries for classification only. This
    // never repairs or changes the answer the production parser sees.
    for (i, _) in text.char_indices().rev().filter(|(_, c)| *c == '{') {
        if let Some(Ok(Value::Object(_))) = serde_json::Deserializer::from_str(&text[i..])
            .into_iter::<Value>()
            .next()
        {
            return "schema_invalid";
        }
    }
    if serde_json::from_str::<Value>(text.trim()).is_ok() {
        return "non_object_json";
    }
    if text.contains(['{', '}']) {
        "unparseable_json_candidate"
    } else {
        "prose"
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_mesh::ops::engine::Need;
    use localpilot_mesh::ops::Expect;

    fn request() -> Request {
        Request {
            need: Need::Review,
            msg_id: "peer:3".into(),
            kind: "REVIEW_REQUEST".into(),
            from: "peer".into(),
            body: "PRIVATE PROMPT".into(),
            task: "PRIVATE TASK".into(),
            round: 1,
            files: vec![],
            expect: Expect {
                session_id: "session".into(),
                unit_id: None,
                reviewer: true,
                owner: false,
            },
        }
    }

    #[test]
    fn samples_redact_before_a_utf8_bound_and_never_capture_the_prompt() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("review.jsonl");
        let mut capture = Capture::create(&path, dir.path()).unwrap();
        let secret = format!("sk-{}", "a".repeat(40));
        let raw = format!(
            "{} {secret} {}",
            "x".repeat(TEXT_BYTES - 18),
            "🦀".repeat(100)
        );
        capture
            .record(Attempt {
                request: &request(),
                role: "localpilot",
                model: "fixture-model",
                number: 1,
                stop: Some("Done"),
                text: Some(&raw),
                outcome: Outcome::ParseError,
                error: Some(&secret),
            })
            .unwrap();
        let bytes = std::fs::read_to_string(path).unwrap();
        assert!(!bytes.contains("sk-"));
        assert!(bytes.contains("[REDACTED]"));
        assert!(!bytes.contains("PRIVATE PROMPT") && !bytes.contains("PRIVATE TASK"));
        let row: Value = serde_json::from_str(&bytes).unwrap();
        assert!(row["response"]["text"].as_str().unwrap().len() <= TEXT_BYTES);
        assert_eq!(row["response"]["truncated"], true);
        assert_eq!(row["response"]["original_bytes"], raw.len());
        let unicode = sample(&"🦀".repeat(10), 7);
        assert_eq!(unicode["text"], "🦀");
        assert_eq!(unicode["truncated"], true);
    }

    #[test]
    fn a_capture_file_is_fresh_and_never_in_the_mailbox() {
        let dir = tempfile::tempdir().unwrap();
        let existing = dir.path().join("keep.jsonl");
        std::fs::write(&existing, "keep").unwrap();
        assert!(Capture::create(&existing, dir.path()).is_err());
        assert_eq!(std::fs::read_to_string(existing).unwrap(), "keep");
        let mailbox = dir.path().join(".pair-programming");
        std::fs::create_dir(&mailbox).unwrap();
        let path = mailbox.join("capture.jsonl");
        assert!(Capture::create(&path, dir.path()).is_err());
        assert!(!path.exists());
    }

    #[test]
    fn file_growth_stops_with_a_marker_even_for_escaped_text() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("bounded.jsonl");
        let mut capture = Capture::create(&path, dir.path()).unwrap();
        let text = "\0".repeat(TEXT_BYTES);
        for _ in 0..100 {
            capture
                .record(Attempt {
                    request: &request(),
                    role: "localpilot",
                    model: "fixture-model",
                    number: 1,
                    stop: None,
                    text: Some(&text),
                    outcome: Outcome::ParseError,
                    error: None,
                })
                .unwrap();
        }
        let bytes = std::fs::read_to_string(path).unwrap();
        assert!(bytes.len() <= FILE_BYTES);
        assert!(capture.exhausted);
        let rows: Vec<Value> = bytes
            .lines()
            .map(|l| serde_json::from_str(l).unwrap())
            .collect();
        assert_eq!(rows.last().unwrap()["event"], "capture_limit");
        assert_eq!(
            rows.iter()
                .filter(|r| r["event"] == "capture_limit")
                .count(),
            1
        );
    }

    #[test]
    fn classifications_distinguish_absent_empty_prose_syntax_and_schema() {
        for (text, kind) in [
            (None, "unavailable"),
            (Some(" \n"), "empty"),
            (Some("looks fine"), "prose"),
            (Some("{\"kind\":"), "unparseable_json_candidate"),
            (Some("{\"wrong\":1}"), "schema_invalid"),
            (Some("[]"), "non_object_json"),
        ] {
            assert_eq!(response_kind(text, Outcome::ParseError), kind);
        }
        assert_eq!(
            response_kind(Some("{}"), Outcome::ValidationError),
            "answer"
        );
    }
}
