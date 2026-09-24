//! Journal lines (spec J-1..J-8).
//!
//! A journal is split as bytes on `\n` only and each line is decoded on its
//! own: a crash can cut a multi-byte character in half, and Unicode line
//! separators inside a string must not split a record. A line that is blank,
//! not UTF-8, not JSON, or JSON but not an object is an invalid line, skipped
//! for every protocol purpose and reported by line number.

use std::path::Path;

use serde_json::{Map, Value};

use crate::error::MeshError;
use crate::fsio;
use crate::session::check_protocol;

/// A journal's lines, each decoded on its own: `None` for a line that is not
/// valid UTF-8. A file that ends in `\n` has no line after it.
#[must_use]
pub fn split_lines(bytes: &[u8]) -> Vec<Option<String>> {
    let mut parts: Vec<&[u8]> = bytes.split(|b| *b == b'\n').collect();
    if parts.last().is_some_and(|l| l.is_empty()) {
        parts.pop();
    }
    parts
        .into_iter()
        .map(|l| {
            std::str::from_utf8(l)
                .ok()
                .map(|s| s.trim_end_matches('\r').to_owned())
        })
        .collect()
}

/// The JSON object on a line, or `None` for an invalid line.
fn object(line: Option<&str>) -> Option<Map<String, Value>> {
    match serde_json::from_str::<Value>(line?) {
        Ok(Value::Object(m)) => Some(m),
        _ => None,
    }
}

/// Every valid record in a journal, in file order. A record from another
/// major protocol version, or requiring a feature this build lacks, is
/// refused rather than skipped (spec V-3).
///
/// # Errors
/// [`MeshError::Unsupported`] for such a record; I/O errors.
pub fn records(path: &Path) -> Result<Vec<Map<String, Value>>, MeshError> {
    let Some(bytes) = fsio::read_bytes(path)? else {
        return Ok(Vec::new());
    };
    let mut out = Vec::new();
    for line in split_lines(&bytes) {
        if let Some(m) = object(line.as_deref()) {
            check_protocol(&m, "a journal record")?;
            out.push(m);
        }
    }
    Ok(out)
}

/// The 1-based numbers of a journal's invalid lines (spec J-7).
///
/// # Errors
/// I/O errors.
pub fn invalid_lines(path: &Path) -> Result<Vec<usize>, MeshError> {
    let Some(bytes) = fsio::read_bytes(path)? else {
        return Ok(Vec::new());
    };
    Ok(split_lines(&bytes)
        .iter()
        .enumerate()
        .filter(|(_, l)| object(l.as_deref()).is_none())
        .map(|(i, _)| i + 1)
        .collect())
}

/// One record as a journal line, without its terminator: compact JSON with
/// U+2028, U+2029 and U+0085 escaped, so no reader, however it splits lines,
/// cuts the record in two (spec J-2).
///
/// # Errors
/// [`MeshError::Serde`].
pub fn encode(record: &Map<String, Value>) -> Result<String, MeshError> {
    let raw = serde_json::to_string(record)?;
    Ok(escape_line_breaks(&raw))
}

fn escape_line_breaks(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    for c in s.chars() {
        match c {
            '\u{2028}' => out.push_str("\\u2028"),
            '\u{2029}' => out.push_str("\\u2029"),
            '\u{0085}' => out.push_str("\\u0085"),
            _ => out.push(c),
        }
    }
    out
}

/// Append one record, sealing a torn tail first and flushing (spec J-1,
/// J-6). The caller holds the file's role lock.
///
/// # Errors
/// Serialization or the store's I/O error.
pub fn append(path: &Path, record: &Map<String, Value>) -> Result<(), MeshError> {
    let line = encode(record)?;
    localpilot_store::append_line(path, &line)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn obj(v: Value) -> Map<String, Value> {
        match v {
            Value::Object(m) => m,
            _ => unreachable!(),
        }
    }

    #[test]
    fn splits_on_newline_only_and_decodes_each_line() {
        let bytes = "{\"a\":1}\n{\"b\":\"x\u{2028}y\"}\r\n\n{\"c\":\n".as_bytes();
        let lines = split_lines(bytes);
        assert_eq!(lines.len(), 4);
        assert_eq!(
            lines[1].as_deref(),
            Some("{\"b\":\"x\u{2028}y\"}"),
            "a raw U+2028 does not split"
        );
        assert_eq!(lines[2].as_deref(), Some(""));
    }

    #[test]
    fn a_torn_multibyte_character_invalidates_only_its_line() {
        let mut bytes = b"{\"seq\":1}\n{\"body\":\"caf".to_vec();
        bytes.push(0xC3);
        let lines = split_lines(&bytes);
        assert_eq!(lines[0].as_deref(), Some("{\"seq\":1}"));
        assert_eq!(lines[1], None);
    }

    #[test]
    fn records_skip_invalid_lines_and_invalid_lines_are_reported() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("codex.jsonl");
        std::fs::write(
            &p,
            "{\"seq\":1}\n\nnot json\nnull\n42\n{\"seq\":2}\r\n{\"seq\":3}",
        )
        .unwrap();
        let recs = records(&p).unwrap();
        let seqs: Vec<i64> = recs.iter().filter_map(|r| r.get("seq")?.as_i64()).collect();
        assert_eq!(
            seqs,
            [1, 2, 3],
            "CRLF and an unterminated last record are records"
        );
        assert_eq!(invalid_lines(&p).unwrap(), [2, 3, 4, 5]);
    }

    #[test]
    fn a_record_from_another_major_version_is_refused() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("codex.jsonl");
        std::fs::write(&p, "{\"seq\":1,\"protocol\":\"2.0\"}\n").unwrap();
        assert!(matches!(records(&p), Err(MeshError::Unsupported(_))));
    }

    #[test]
    fn encoding_escapes_unicode_line_separators() {
        let rec = obj(json!({"body": "one\u{2028}two\u{2029}three\u{0085}four"}));
        let line = encode(&rec).unwrap();
        assert!(
            !line.contains('\u{2028}') && !line.contains('\u{2029}') && !line.contains('\u{0085}')
        );
        let back: Value = serde_json::from_str(&line).unwrap();
        assert_eq!(back["body"], "one\u{2028}two\u{2029}three\u{0085}four");
    }

    #[test]
    fn append_seals_a_torn_tail_and_the_record_lands_whole() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("codex.jsonl");
        std::fs::write(&p, "{\"seq\":1}\n{\"seq\":2,\"at\":\"2026-01").unwrap();
        append(&p, &obj(json!({"seq": 2}))).unwrap();
        let seqs: Vec<i64> = records(&p)
            .unwrap()
            .iter()
            .filter_map(|r| r.get("seq")?.as_i64())
            .collect();
        assert_eq!(seqs, [1, 2]);
        assert_eq!(invalid_lines(&p).unwrap(), [2]);
    }
}
