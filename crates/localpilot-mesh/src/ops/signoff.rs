//! What counts as an agreement that closes a unit (spec M-6, M-6a, U-3).
//!
//! One definition serves both reviewer paths: a pair's latest decision (U-3a)
//! and each required reviewer's standing decision (U-3b). A legacy session
//! keeps the prefix rule; a strict session counts tokens in the verdict's
//! header. Every rule here is a character rule: no Unicode notion of a line,
//! a space, a digit or a case mapping is used.

use serde_json::Value;

use super::{seq_of, str_of, Obj};
use crate::session::is_strict;

/// The control token names of a strict header (M-6a).
const CONTROL: &[&str] = &["round", "blocking", "important"];

/// What a refusal says strict sign-off needs.
pub(super) const STRICT_SIGNOFF: &str = "strict sign-off needs a VERDICT whose first line starts `AGREE round=<n>` with n above zero, \
names round=, blocking= and important= at most once each with ASCII-digit values, and gives blocking=0 if any";

/// A message's body as text; anything else reads as empty.
fn body(m: &Obj) -> &str {
    str_of(m, "body").unwrap_or_default()
}

/// A strict verdict's header: the body before its first LF, less one CR at
/// its end. No other character ends it.
pub(super) fn strict_header(body: &str) -> &str {
    let h = body.split('\n').next().unwrap_or_default();
    h.strip_suffix('\r').unwrap_or(h)
}

/// A legacy verdict's first line, ended by any of the line boundaries the
/// reference reader recognises.
fn legacy_first_line(body: &str) -> &str {
    const ENDS: &[char] = &[
        '\n', '\r', '\u{0b}', '\u{0c}', '\u{1c}', '\u{1d}', '\u{1e}', '\u{85}', '\u{2028}',
        '\u{2029}',
    ];
    body.split(ENDS).next().unwrap_or_default()
}

/// Why `header` is not a strict AGREE (M-6a), or `None` when it is one.
pub(super) fn strict_problem(header: &str) -> Option<String> {
    let toks: Vec<&str> = header
        .split([' ', '\t'])
        .filter(|t| !t.is_empty())
        .collect();
    if toks.first() != Some(&"AGREE") {
        return Some("it does not start with AGREE".into());
    }
    if !toks.get(1).is_some_and(|t| t.starts_with("round=")) {
        return Some("its second token is not round=<n>".into());
    }
    let mut seen: Vec<(&str, &str)> = Vec::new();
    for t in &toks[1..] {
        let Some((name, value)) = t.split_once('=') else {
            continue;
        };
        if !CONTROL.contains(&name.to_ascii_lowercase().as_str()) {
            continue;
        }
        if !CONTROL.contains(&name) {
            return Some(format!("{t}: control names are lowercase"));
        }
        if seen.iter().any(|(k, _)| *k == name) {
            return Some(format!("{name}= appears twice"));
        }
        if value.is_empty() || !value.bytes().all(|b| b.is_ascii_digit()) {
            return Some(format!("{t} has no ASCII-digit value"));
        }
        seen.push((name, value));
    }
    let get = |k: &str| seen.iter().find(|(n, _)| *n == k).map(|(_, v)| *v);
    let round = get("round").unwrap_or_default();
    if round.bytes().all(|b| b == b'0') {
        return Some(format!("round={round} is not above zero"));
    }
    if let Some(b) = get("blocking") {
        if !b.bytes().all(|c| c == b'0') {
            return Some(format!("blocking={b} reports a blocker"));
        }
    }
    None
}

/// Why message `m` does not sign off a unit of session `s`, or `None`.
pub(super) fn agreement_problem(s: &Obj, m: &Obj) -> Option<String> {
    let verdict = str_of(m, "kind") == Some("VERDICT");
    if !is_strict(s) {
        return if verdict && legacy_first_line(body(m)).starts_with("AGREE ") {
            None
        } else {
            Some("it is not an AGREE verdict".into())
        };
    }
    if !verdict {
        return Some("it is not a VERDICT".into());
    }
    strict_problem(strict_header(body(m)))
}

/// A decision as a refusal names it: kind, seq and header.
pub(super) fn decision_text(m: &Obj) -> String {
    format!(
        "{} #{} \"{}\"",
        m.get("kind").and_then(Value::as_str).unwrap_or("None"),
        seq_of(m),
        strict_header(body(m))
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn ok(h: &str) -> bool {
        strict_problem(strict_header(h)).is_none()
    }

    #[test]
    fn the_strict_matrix() {
        for h in [
            "AGREE round=1",
            "AGREE round=1 looks right",
            "AGREE round=1 blocking=0 important=2",
            "AGREE round=01 blocking=00",
            "AGREE round=1\r\nmore",
            "AGREE\tround=1",
            "AGREE round=1 bloc\u{212a}ing=1",
            "AGREE round=1 blocking:1",
        ] {
            assert!(ok(h), "{h:?} should agree");
        }
        for h in [
            "AGREE round=1 blocking=1 important=0",
            "AGREE but the crash is open",
            "AGREE but blocking=0",
            "AGREE",
            "AGREE round=0",
            "AGREE round=00",
            "AGREE round=1 blocking=x",
            "AGREE round=",
            "AGREE round=1 blocking=",
            "AGREE round=\u{663}",
            "AGREE round=\u{b3}",
            "AGREE round=1 round=2",
            "AGREE round=1 blocking=0 blocking=0",
            "AGREE round=1 important=1 important=2",
            "AGREE round=1 Blocking=1",
            "AGREE round=1 BLOCKING=0",
            "AGREE round=1 Round=2",
            "AGREE round=1 blocking=0 Blocking=0",
            "AGREE\u{a0}round=1",
            "AGREE round=1\u{2003}blocking=0",
            "AGREE round=1\u{2028}blocking=1",
            "AGREE round=1\rblocking=1",
            "REVISE round=1 blocking=1 important=0",
            "agree round=1",
        ] {
            assert!(!ok(h), "{h:?} should not agree");
        }
    }

    #[test]
    fn a_legacy_session_keeps_the_prefix_rule() {
        let legacy = json!({}).as_object().cloned().unwrap_or_default();
        let strict = json!({"signoff": "strict", "requires": ["strict-signoff"], "schema": 2})
            .as_object()
            .cloned()
            .unwrap_or_default();
        let v = |b: &str| {
            json!({"kind": "VERDICT", "body": b, "seq": 3})
                .as_object()
                .cloned()
                .unwrap_or_default()
        };
        assert!(agreement_problem(&legacy, &v("AGREE but the crash is open")).is_none());
        assert!(agreement_problem(&legacy, &v("AGREE")).is_some());
        assert!(agreement_problem(&legacy, &v("AGREE\u{2028}x")).is_some());
        assert!(agreement_problem(&strict, &v("AGREE but the crash is open")).is_some());
        assert!(agreement_problem(&strict, &v("AGREE round=2\nok")).is_none());
        let note = json!({"kind": "NOTE", "body": "AGREE round=1"})
            .as_object()
            .cloned()
            .unwrap_or_default();
        assert!(agreement_problem(&strict, &note).is_some());
        assert_eq!(
            decision_text(&v("AGREE round=0\nx")),
            "VERDICT #3 \"AGREE round=0\""
        );
    }
}
