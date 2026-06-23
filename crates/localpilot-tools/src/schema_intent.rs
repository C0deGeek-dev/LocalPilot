//! Schema-intent markers: a tool author declares what a field *means* so the
//! argument-repair stage keys off the declared intent rather than a field-name
//! guess. Each helper is a `#[schemars(schema_with = "...")]` function that
//! annotates the schemars-generated JSON Schema with a private
//! `x-localpilot-intent` extension. The field's Rust type and its deserialization
//! are unchanged — this is metadata the repair pipeline reads, nothing more.
//!
//! - `path_string` / `glob_string` — a single path / glob (the markdown-autolink
//!   repair fires only on `path`).
//! - `file_content_string` / `command_string` — a file body / shell string, marked
//!   **repair-exempt**: a rule must never parse or rewrite it, even if it happens
//!   to look JSON- or markdown-shaped.
//! - `one_or_many_string` — a path list the model may give as one or many; the
//!   wrap/parse repairs target it.
//! - `line_range` — a 1-based line endpoint. Marker only: relational repair is
//!   deferred (D006), so no rule consumes it; it documents intent for readers and
//!   future work.

use schemars::gen::SchemaGenerator;
use schemars::schema::{InstanceType, Schema, SchemaObject, SingleOrVec};
use serde_json::Value;

/// The JSON Schema extension key carrying a field's repair intent.
pub const INTENT_KEY: &str = "x-localpilot-intent";

/// Intent label: a single filesystem path.
pub const INTENT_PATH: &str = "path";
/// Intent label: a glob pattern.
pub const INTENT_GLOB: &str = "glob";
/// Intent label: file content (repair-exempt).
pub const INTENT_CONTENT: &str = "content";
/// Intent label: a shell command/program string (repair-exempt).
pub const INTENT_COMMAND: &str = "command";
/// Intent label: a one-or-many path list.
pub const INTENT_PATH_LIST: &str = "path-list";
/// Intent label: a 1-based line endpoint.
pub const INTENT_LINE: &str = "line";

/// The declared repair intent of a schema property, if any.
#[must_use]
pub fn field_intent(prop: &Value) -> Option<&str> {
    prop.get(INTENT_KEY).and_then(Value::as_str)
}

/// Whether a property's declared intent makes it repair-exempt (content/command):
/// such a field is never parsed or rewritten by any repair rule.
#[must_use]
pub fn is_repair_exempt(prop: &Value) -> bool {
    matches!(field_intent(prop), Some(INTENT_CONTENT | INTENT_COMMAND))
}

/// A `{"type": "string"}` schema carrying the given intent.
fn string_with_intent(intent: &str) -> Schema {
    let mut obj = SchemaObject {
        instance_type: Some(InstanceType::String.into()),
        ..SchemaObject::default()
    };
    obj.extensions
        .insert(INTENT_KEY.to_string(), Value::String(intent.to_string()));
    Schema::Object(obj)
}

/// A single filesystem path.
#[must_use]
pub fn path_string(_gen: &mut SchemaGenerator) -> Schema {
    string_with_intent(INTENT_PATH)
}

/// A glob pattern.
#[must_use]
pub fn glob_string(_gen: &mut SchemaGenerator) -> Schema {
    string_with_intent(INTENT_GLOB)
}

/// File content — repair-exempt.
#[must_use]
pub fn file_content_string(_gen: &mut SchemaGenerator) -> Schema {
    string_with_intent(INTENT_CONTENT)
}

/// A shell command/program — repair-exempt.
#[must_use]
pub fn command_string(_gen: &mut SchemaGenerator) -> Schema {
    string_with_intent(INTENT_COMMAND)
}

/// A list of paths the model may pass as one or many (an `array<string>` with a
/// path-list intent). Keeps the strict `Vec<String>` deserialization — the repair
/// stage is what wraps a bare string or parses a stringified array.
#[must_use]
pub fn one_or_many_string(gen: &mut SchemaGenerator) -> Schema {
    let mut obj = SchemaObject {
        instance_type: Some(InstanceType::Array.into()),
        ..SchemaObject::default()
    };
    obj.array().items = Some(SingleOrVec::Single(Box::new(gen.subschema_for::<String>())));
    obj.extensions.insert(
        INTENT_KEY.to_string(),
        Value::String(INTENT_PATH_LIST.to_string()),
    );
    Schema::Object(obj)
}

/// A 1-based line endpoint (a nullable integer carrying the `line` intent). Marker
/// only — no repair rule consumes it (relational repair is deferred, D006).
#[must_use]
pub fn line_range(_gen: &mut SchemaGenerator) -> Schema {
    let mut obj = SchemaObject {
        instance_type: Some(SingleOrVec::Vec(vec![
            InstanceType::Integer,
            InstanceType::Null,
        ])),
        ..SchemaObject::default()
    };
    obj.extensions.insert(
        INTENT_KEY.to_string(),
        Value::String(INTENT_LINE.to_string()),
    );
    Schema::Object(obj)
}

#[cfg(test)]
mod tests {
    use super::*;
    use schemars::gen::SchemaGenerator;

    fn rendered(f: fn(&mut SchemaGenerator) -> Schema) -> Value {
        let mut gen = SchemaGenerator::default();
        serde_json::to_value(f(&mut gen)).unwrap()
    }

    #[test]
    fn each_string_helper_marks_its_intent_on_a_string_schema() {
        for (f, intent) in [
            (path_string as fn(&mut SchemaGenerator) -> Schema, "path"),
            (glob_string, "glob"),
            (file_content_string, "content"),
            (command_string, "command"),
        ] {
            let schema = rendered(f);
            assert_eq!(schema["type"], "string");
            assert_eq!(schema[INTENT_KEY], intent);
            assert_eq!(field_intent(&schema), Some(intent));
        }
    }

    #[test]
    fn one_or_many_marks_a_string_array_as_a_path_list() {
        let schema = rendered(one_or_many_string);
        assert_eq!(schema["type"], "array");
        assert_eq!(schema["items"]["type"], "string");
        assert_eq!(schema[INTENT_KEY], "path-list");
    }

    #[test]
    fn line_range_is_a_nullable_integer_marked_line() {
        let schema = rendered(line_range);
        assert_eq!(schema["type"], serde_json::json!(["integer", "null"]));
        assert_eq!(schema[INTENT_KEY], "line");
    }

    #[test]
    fn content_and_command_are_repair_exempt_but_path_is_not() {
        assert!(is_repair_exempt(&rendered(file_content_string)));
        assert!(is_repair_exempt(&rendered(command_string)));
        assert!(!is_repair_exempt(&rendered(path_string)));
        assert!(!is_repair_exempt(&rendered(one_or_many_string)));
    }
}
