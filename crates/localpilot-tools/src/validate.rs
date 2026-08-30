//! Schema-aware validation of a tool call's arguments.
//!
//! This is the shared, pure, dependency-free substrate three things read: the
//! validity metric (`schema_valid`), the per-class malformed-argument baseline,
//! and — later — the validator-first argument-repair pipeline. It reads a tool's
//! schemars-generated JSON Schema (a [`Value`]) and the model-supplied input (a
//! [`Value`]) and reports the *structural* issues, each tagged with the failure
//! class it falls into.
//!
//! It performs no I/O and mutates nothing, so it can measure the current loop
//! without changing any behaviour. It is a deliberately conservative proxy for a
//! full JSON-Schema validator (matching the spirit of the existing
//! `required_fields_present` test helper): it flags a missing required field and
//! a confident top-level type mismatch, and it stays silent whenever the schema's
//! expected type is ambiguous (a `$ref`, an `anyOf`, an enum), so it never
//! reports a false invalid.

use serde_json::Value;

use crate::contract::ToolExample;

/// The malformed-argument class of one schema issue, mapped to the failure-mode
/// taxonomy in the tool-input research (classes 2/3/4/7). Markdown-autolink and
/// relational classes are intent/cross-field shaped and are not detectable from
/// the JSON type alone, so they are out of this structural validator's scope.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum MalformedClass {
    /// A field the schema marks as required is absent.
    MissingRequiredField,
    /// A JSON string that itself parses to the schema's expected array/object
    /// (the model double-encoded the value).
    StringifiedJson,
    /// A bare scalar string where the schema expects an array (the model passed
    /// one element unwrapped).
    BareStringForArray,
    /// An object (often `{}`) where the schema expects an array.
    ObjectForArray,
    /// A degenerate markdown autolink `[text](target)` supplied for a path/URL
    /// field. Structurally a valid JSON string, so it is an *intent* issue
    /// detected by the repair stage (with a field-intent marker), not a type
    /// failure the structural validator reports.
    MarkdownAutolink,
    /// A confident type mismatch that fits none of the more specific classes.
    TypeMismatch,
}

impl MalformedClass {
    /// A stable, redaction-safe label (no raw values) for telemetry and reports.
    #[must_use]
    pub fn label(self) -> &'static str {
        match self {
            MalformedClass::MissingRequiredField => "missing_required_field",
            MalformedClass::StringifiedJson => "stringified_json",
            MalformedClass::BareStringForArray => "bare_string_for_array",
            MalformedClass::ObjectForArray => "object_for_array",
            MalformedClass::MarkdownAutolink => "markdown_autolink",
            MalformedClass::TypeMismatch => "type_mismatch",
        }
    }
}

/// One structural issue found in a tool call's input. Carries only the field
/// path and JSON type names — never the raw value — so it is safe to log.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct SchemaIssue {
    /// The offending field name (a top-level property path).
    pub path: String,
    /// The JSON type the schema expects at that path (e.g. `array`, `string`).
    pub expected: String,
    /// The JSON type actually supplied (e.g. `string`, `object`, `absent`).
    pub actual: String,
    /// The failure class this issue falls into.
    pub class: MalformedClass,
}

/// The JSON type name of a value, for the redaction-safe `actual` field.
fn type_name(value: &Value) -> &'static str {
    match value {
        Value::Null => "null",
        Value::Bool(_) => "boolean",
        Value::Number(n) => {
            if n.is_i64() || n.is_u64() {
                "integer"
            } else {
                "number"
            }
        }
        Value::String(_) => "string",
        Value::Array(_) => "array",
        Value::Object(_) => "object",
    }
}

/// The single expected JSON type the schema declares for a property, or `None`
/// when it is ambiguous (a `$ref`/`allOf`/`anyOf`/`oneOf`/`enum`) and must not be
/// type-checked. A `["string","null"]` union resolves to its non-null member, so
/// an `Option<T>` field is checked against its inner type.
fn expected_type(prop: &Value) -> Option<&str> {
    let prop = prop.as_object()?;
    if prop.contains_key("$ref")
        || prop.contains_key("allOf")
        || prop.contains_key("anyOf")
        || prop.contains_key("oneOf")
        || prop.contains_key("enum")
    {
        return None;
    }
    match prop.get("type")? {
        Value::String(t) => Some(t.as_str()),
        Value::Array(types) => types
            .iter()
            .filter_map(Value::as_str)
            .find(|t| *t != "null"),
        _ => None,
    }
}

/// Whether an actual JSON value satisfies an expected scalar/structural type
/// name. `integer` is accepted for an expected `number`, since JSON has one
/// number type and schemars distinguishes them only by `format`.
fn type_matches(expected: &str, actual: &Value) -> bool {
    match expected {
        "string" => actual.is_string(),
        "array" => actual.is_array(),
        "object" => actual.is_object(),
        "boolean" => actual.is_boolean(),
        "integer" => actual.is_i64() || actual.is_u64(),
        "number" => actual.is_number(),
        "null" => actual.is_null(),
        // An unknown/unsupported type name is not type-checked.
        _ => true,
    }
}

/// Classify a value supplied where the schema expects an `array`.
fn classify_array_mismatch(actual: &Value) -> MalformedClass {
    match actual {
        Value::String(s) => {
            if serde_json::from_str::<Value>(s).is_ok_and(|v| v.is_array()) {
                MalformedClass::StringifiedJson
            } else {
                MalformedClass::BareStringForArray
            }
        }
        Value::Object(_) => MalformedClass::ObjectForArray,
        _ => MalformedClass::TypeMismatch,
    }
}

/// Classify a value supplied where the schema expects a structural type that is
/// not an `array` (e.g. `object`). A string that parses to the expected type is a
/// stringified value; anything else is a plain type mismatch.
fn classify_scalar_mismatch(expected: &str, actual: &Value) -> MalformedClass {
    if let Value::String(s) = actual {
        if let Ok(parsed) = serde_json::from_str::<Value>(s) {
            if type_matches(expected, &parsed) {
                return MalformedClass::StringifiedJson;
            }
        }
    }
    MalformedClass::TypeMismatch
}

/// Find the structural issues in a tool call's `input` against its JSON `schema`.
///
/// Returns an empty vec when the input is structurally valid (the call would
/// deserialize), or when the schema is not an object schema (nothing to check).
/// Conservative: a property whose expected type is ambiguous is never flagged.
#[must_use]
pub fn tool_input_issues(schema: &Value, input: &Value) -> Vec<SchemaIssue> {
    let Some(schema_obj) = schema.as_object() else {
        return Vec::new();
    };
    // Only object schemas describe a set of named properties. A non-object input
    // for an object schema is itself the one issue.
    let Some(input_obj) = input.as_object() else {
        // An object schema with required fields cannot be satisfied by a non-object.
        let required = schema_obj
            .get("required")
            .and_then(Value::as_array)
            .map(|r| !r.is_empty())
            .unwrap_or(false);
        if required && schema_obj.get("type").and_then(Value::as_str) == Some("object") {
            return vec![SchemaIssue {
                path: String::new(),
                expected: "object".to_string(),
                actual: type_name(input).to_string(),
                class: MalformedClass::TypeMismatch,
            }];
        }
        return Vec::new();
    };

    let mut issues = Vec::new();

    // Missing required fields.
    if let Some(required) = schema_obj.get("required").and_then(Value::as_array) {
        for field in required.iter().filter_map(Value::as_str) {
            if !input_obj.contains_key(field) {
                let expected = schema_obj
                    .get("properties")
                    .and_then(Value::as_object)
                    .and_then(|p| p.get(field))
                    .and_then(expected_type)
                    .unwrap_or("value")
                    .to_string();
                issues.push(SchemaIssue {
                    path: field.to_string(),
                    expected,
                    actual: "absent".to_string(),
                    class: MalformedClass::MissingRequiredField,
                });
            }
        }
    }

    // Confident top-level type mismatches on present fields.
    if let Some(properties) = schema_obj.get("properties").and_then(Value::as_object) {
        for (field, value) in input_obj {
            // A `null` for an optional field is serde's `None`; never a mismatch.
            if value.is_null() {
                continue;
            }
            let Some(prop) = properties.get(field) else {
                continue; // extra fields are ignored by serde
            };
            let Some(expected) = expected_type(prop) else {
                continue; // ambiguous expected type: do not type-check
            };
            if type_matches(expected, value) {
                continue;
            }
            let class = if expected == "array" {
                classify_array_mismatch(value)
            } else {
                classify_scalar_mismatch(expected, value)
            };
            issues.push(SchemaIssue {
                path: field.clone(),
                expected: expected.to_string(),
                actual: type_name(value).to_string(),
                class,
            });
        }
    }

    issues
}

/// Whether a tool call's input is structurally valid against its schema (no
/// issues). The boolean the `schema_valid` validity metric records.
#[must_use]
pub fn is_input_valid(schema: &Value, input: &Value) -> bool {
    tool_input_issues(schema, input).is_empty()
}

/// A concise, schema-aware, model-readable message describing why a tool call's
/// arguments were rejected, built from the structural `issues` and the tool's
/// curated `examples` — never from the raw input, so it cannot echo a secret.
/// This replaces the raw deserializer string handed to the model, so the model
/// can self-correct on the next turn. Each line names the offending field, what
/// is wrong, and the exact shape to use; a valid example is appended when the
/// tool contract supplies one.
#[must_use]
pub fn readable_input_error(
    tool: &str,
    issues: &[SchemaIssue],
    examples: &[ToolExample],
) -> String {
    let mut out = format!("the `{tool}` call's arguments did not match its schema:");
    for issue in issues {
        out.push('\n');
        out.push_str("  - ");
        out.push_str(&describe_issue(issue));
    }
    if let Some(example) = examples.first() {
        out.push_str(&format!(
            "\na valid `{tool}` call looks like: {}",
            example.input
        ));
    }
    out
}

/// A one-line, value-free description of a single schema issue and how to fix it.
fn describe_issue(issue: &SchemaIssue) -> String {
    let field = &issue.path;
    if field.is_empty() {
        return format!(
            "the arguments must be a JSON object, but a {} was sent",
            issue.actual
        );
    }
    match issue.class {
        MalformedClass::MissingRequiredField => format!(
            "`{field}` is required (expected {}) but was not provided",
            issue.expected
        ),
        MalformedClass::StringifiedJson => format!(
            "`{field}` must be a JSON {0}, but was sent as a quoted string; pass it as an \
             actual {0}, not a string (remove the surrounding quotes)",
            issue.expected
        ),
        MalformedClass::BareStringForArray => format!(
            "`{field}` must be an array, but a single string was sent; wrap the value in an \
             array, e.g. [\"value\"]"
        ),
        MalformedClass::ObjectForArray => {
            format!("`{field}` must be an array, but an object was sent")
        }
        MalformedClass::MarkdownAutolink => format!(
            "`{field}` looks like a markdown link `[text](target)`; pass just the path or URL, \
             not the markdown form"
        ),
        MalformedClass::TypeMismatch => format!(
            "`{field}` must be {}, but {} was sent",
            issue.expected, issue.actual
        ),
    }
}

/// Whether `input` supplies every field the schema marks as required — the
/// missing-field component of validity, kept as a named, reusable check.
#[must_use]
pub fn required_fields_present(schema: &Value, input: &Value) -> bool {
    let Some(required) = schema.get("required").and_then(Value::as_array) else {
        return true;
    };
    let Some(obj) = input.as_object() else {
        return required.is_empty();
    };
    required
        .iter()
        .filter_map(Value::as_str)
        .all(|field| obj.contains_key(field))
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    /// A representative object schema: one required `array<string>`, one required
    /// `string`, one optional `integer`.
    fn schema() -> Value {
        json!({
            "type": "object",
            "required": ["paths", "name"],
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" } },
                "name": { "type": "string" },
                "count": { "type": ["integer", "null"] },
            }
        })
    }

    #[test]
    fn a_valid_input_has_no_issues() {
        let input = json!({ "paths": ["a.rs"], "name": "x", "count": 3 });
        assert!(tool_input_issues(&schema(), &input).is_empty());
        assert!(is_input_valid(&schema(), &input));
    }

    #[test]
    fn an_explicit_null_for_an_optional_field_is_valid() {
        let input = json!({ "paths": ["a.rs"], "name": "x", "count": null });
        assert!(is_input_valid(&schema(), &input));
    }

    #[test]
    fn a_missing_required_field_is_flagged() {
        let input = json!({ "paths": ["a.rs"] });
        let issues = tool_input_issues(&schema(), &input);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "name");
        assert_eq!(issues[0].class, MalformedClass::MissingRequiredField);
        assert_eq!(issues[0].actual, "absent");
    }

    #[test]
    fn a_stringified_array_is_classified() {
        let input = json!({ "paths": "[\"a.rs\",\"b.rs\"]", "name": "x" });
        let issues = tool_input_issues(&schema(), &input);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].path, "paths");
        assert_eq!(issues[0].class, MalformedClass::StringifiedJson);
        assert_eq!(issues[0].expected, "array");
    }

    #[test]
    fn a_bare_string_for_an_array_is_classified() {
        let input = json!({ "paths": "README.md", "name": "x" });
        let issues = tool_input_issues(&schema(), &input);
        assert_eq!(issues.len(), 1);
        assert_eq!(issues[0].class, MalformedClass::BareStringForArray);
    }

    #[test]
    fn an_object_for_an_array_is_classified() {
        let input = json!({ "paths": {}, "name": "x" });
        let issues = tool_input_issues(&schema(), &input);
        assert_eq!(issues[0].class, MalformedClass::ObjectForArray);
    }

    #[test]
    fn a_scalar_type_mismatch_is_flagged() {
        // `name` expects a string; a number is a confident mismatch.
        let input = json!({ "paths": ["a.rs"], "name": 7 });
        let issues = tool_input_issues(&schema(), &input);
        assert_eq!(issues[0].path, "name");
        assert_eq!(issues[0].class, MalformedClass::TypeMismatch);
    }

    #[test]
    fn an_ambiguous_ref_property_is_not_type_checked() {
        let schema = json!({
            "type": "object",
            "required": ["op"],
            "properties": { "op": { "$ref": "#/definitions/Op" } }
        });
        // A `$ref` expected type is ambiguous, so any present value is left alone.
        let input = json!({ "op": "anything" });
        assert!(tool_input_issues(&schema, &input).is_empty());
    }

    #[test]
    fn extra_unknown_fields_are_ignored() {
        let input = json!({ "paths": ["a.rs"], "name": "x", "extra": true });
        assert!(is_input_valid(&schema(), &input));
    }

    #[test]
    fn the_readable_error_names_the_field_the_fix_and_an_example() {
        let issues = tool_input_issues(&schema(), &json!({ "paths": "README.md" }));
        let examples = [ToolExample {
            input: r#"{"paths": ["src/lib.rs"]}"#,
            note: "list specific paths",
        }];
        let message = readable_input_error("git_diff", &issues, &examples);
        assert!(message.contains("git_diff"));
        assert!(message.contains("`paths`"));
        assert!(message.contains("array"), "states the expected shape");
        assert!(
            message.contains(r#"["src/lib.rs"]"#),
            "shows a valid example"
        );
    }

    #[test]
    fn the_readable_error_for_a_missing_field_and_a_stringified_array() {
        let missing = readable_input_error(
            "read_file",
            &tool_input_issues(
                &json!({"type":"object","required":["path"],"properties":{"path":{"type":"string"}}}),
                &json!({}),
            ),
            &[],
        );
        assert!(missing.contains("`path` is required"));

        let stringified = readable_input_error(
            "git_add",
            &tool_input_issues(&schema(), &json!({ "paths": "[\"a.rs\"]", "name": "x" })),
            &[],
        );
        assert!(stringified.contains("quoted string"));
    }

    #[test]
    fn the_readable_error_never_echoes_the_raw_value() {
        // A secret-bearing argument must not appear in the model-visible message:
        // the formatter builds from field names, types, and curated examples only.
        let secret = "sk-abcdefghijklmnopqrstuvwxyz0123";
        let input = json!({ "paths": secret, "name": "x" });
        let message = readable_input_error("git_diff", &tool_input_issues(&schema(), &input), &[]);
        assert!(
            !message.contains(secret),
            "the readable error must not echo the raw argument value"
        );
    }

    #[test]
    fn required_fields_present_matches_the_legacy_proxy() {
        assert!(required_fields_present(
            &schema(),
            &json!({ "paths": [], "name": "x" })
        ));
        assert!(!required_fields_present(&schema(), &json!({ "paths": [] })));
        // No `required` key ⇒ vacuously present.
        assert!(required_fields_present(
            &json!({ "type": "object" }),
            &json!({})
        ));
    }
}
