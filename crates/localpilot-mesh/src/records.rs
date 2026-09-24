//! Typed records that keep what they do not know (spec V-1, V-1a).
//!
//! Every record carries the keys this crate reads as fields, and everything
//! else in `extra`, so a record another implementation (or a newer version)
//! wrote survives being read and rewritten here unchanged.

use serde::{Deserialize, Serialize};
use serde_json::{Map, Value};

/// A pointer (`active.json` in schema 1, `active.v2.json` in schema 2).
/// Pointers are rebuilt from named fields on every write (spec V-1a), so an
/// unknown pointer key is kept here only for reading.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Pointer {
    pub session_id: String,
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub status: Option<String>,
    #[serde(default)]
    pub updated_at: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participants: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<u32>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

/// A session record, as far as a participant needs to understand it. The
/// rest (authority, waiting, pauses, units, handoff, ...) stays in `extra`
/// until a later slice gives it a typed view, and is written back as read.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Session {
    pub session_id: String,
    pub status: String,
    #[serde(default)]
    pub work_unit: Option<String>,
    #[serde(default)]
    pub unit_id: Option<String>,
    #[serde(default)]
    pub driver: Option<String>,
    #[serde(default)]
    pub owner: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub schema: Option<u32>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub participants: Option<Vec<String>>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub protocol: Option<String>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub requires: Option<Vec<String>>,
    #[serde(flatten)]
    pub extra: Map<String, Value>,
}

impl Session {
    /// The protocol schema: 2 when recorded, else the historic 1.
    #[must_use]
    pub fn schema(&self) -> u32 {
        self.schema.unwrap_or(1)
    }

    /// The participants, fixed at start. Schema 1 is the historic pair.
    #[must_use]
    pub fn participants(&self) -> Vec<String> {
        self.participants
            .clone()
            .unwrap_or_else(|| vec!["claude".to_owned(), "codex".to_owned()])
    }

    /// Terminal: completed or abandoned (spec S-5).
    #[must_use]
    pub fn is_terminal(&self) -> bool {
        matches!(self.status.as_str(), "completed" | "abandoned")
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn unknown_keys_survive_a_read_and_rewrite() {
        let raw = json!({
            "session_id": "S", "status": "active", "work_unit": "w", "unit_id": "1-abcdef12",
            "driver": "claude", "owner": "claude", "authority": {"required_reviewers": ["codex"]},
            "x-test.note": "kept", "protocol": "1.0"
        });
        let s: Session = serde_json::from_value(raw.clone()).unwrap();
        assert_eq!(s.extra["x-test.note"], "kept");
        assert_eq!(serde_json::to_value(&s).unwrap(), raw);
    }

    #[test]
    fn a_schema_one_session_is_the_historic_pair() {
        let s: Session =
            serde_json::from_value(json!({"session_id": "S", "status": "active"})).unwrap();
        assert_eq!(s.schema(), 1);
        assert_eq!(s.participants(), ["claude", "codex"]);
        assert!(!s.is_terminal());
    }
}
