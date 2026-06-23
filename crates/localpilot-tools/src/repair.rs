//! The validator-first, schema-guided tool-input repair stage.
//!
//! This is the **cure** sibling of the model-readable error (the *re-prompt*) and
//! the grammar constraint (the *prevent*): a single, pure, deterministic pass that
//! runs once pre-dispatch. It validates the model's raw arguments; on a failure it
//! repairs **only** the validator-reported issue paths, with the small set of
//! conservative, schema-typed rules that genuinely occur, then re-validates and
//! either yields a repaired input or falls back to the readable error.
//!
//! Safety contract (see the plan §12 / ADR): validate-first (a valid input is
//! byte-unchanged); issue-path-localized (only failed paths are touched);
//! schema-guided (a rule fires only when the schema proves the target type);
//! conservative (a destructive / external-write / irreversible / MCP tool is
//! **never** repaired — readable error only; content/command fields are never
//! parsed or rewritten); auditable (every repair is a named rule recorded in the
//! returned [`ToolInputValidationResult`]). Repair changes arguments, never
//! authority: the permission engine still runs on the repaired input downstream.

use serde_json::Value;

use crate::contract::{Reversibility, SideEffectClass, ToolExample};
use crate::validate::{readable_input_error, tool_input_issues, MalformedClass, SchemaIssue};

/// The outcome of validating (and possibly repairing) a tool call's arguments.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum RepairOutcome {
    /// The raw input was already valid; it is used byte-unchanged.
    Valid,
    /// The input was invalid but repaired to a valid shape.
    Repaired,
    /// The input was invalid and not repaired (a refused tool, no matching rule,
    /// or a re-validation that still failed): the caller surfaces the readable
    /// error and the model retries.
    Invalid,
}

/// The auditable record of one validate-or-repair pass. Pure data — it drives the
/// caller's dispatch choice, the model-visible note, and the redacted telemetry.
#[derive(Debug, Clone)]
pub struct ToolInputValidationResult {
    /// The tool the call targeted.
    pub tool: String,
    /// What the pass decided.
    pub outcome: RepairOutcome,
    /// The structural / intent issues found (empty when `Valid`).
    pub issues: Vec<SchemaIssue>,
    /// The ids of the repair rules applied, in order (empty unless `Repaired`).
    pub repairs_applied: Vec<&'static str>,
    /// The repaired input to dispatch, present iff `Repaired`.
    pub repaired_input: Option<Value>,
    /// A one-line, model-visible note describing the repair, present iff `Repaired`.
    pub model_note: Option<String>,
    /// A concise, schema-aware, redacted error message, present iff `Invalid`. The
    /// caller delivers it (when readable errors are on) so the model self-corrects.
    pub readable_message: Option<String>,
    /// The tool's side-effect class, for redacted telemetry.
    pub risk: SideEffectClass,
    /// `true` iff the input was `Invalid` because the safety gate refused to repair
    /// a destructive/external/irreversible/MCP tool (vs. simply having no matching
    /// rule). Distinguishes the `tool_repair_rejected_high_risk` telemetry.
    pub rejected_high_risk: bool,
}

/// Everything the pass needs about the target tool. Borrowed, so the caller owns
/// the schema and the contract.
pub struct RepairRequest<'a> {
    /// The tool name (for the result and the readable message).
    pub tool: &'a str,
    /// The tool's schemars-generated JSON schema.
    pub schema: &'a Value,
    /// The tool contract's side-effect class (the safety gate).
    pub side_effect: SideEffectClass,
    /// The tool contract's reversibility (the safety gate).
    pub reversibility: Reversibility,
    /// Whether the tool comes from an MCP server (no typed schema → never repair).
    pub is_mcp: bool,
    /// Whether to attempt repair (`[tools] repair` is `warn`/`on`). When `false`,
    /// the pass only validates: it never rewrites arguments.
    pub attempt_repair: bool,
    /// The tool's curated examples, used to build the readable error.
    pub examples: &'a [ToolExample],
}

/// Whether a tool may have its arguments repaired. Static and conservative: an
/// MCP tool (no typed schema), or any tool the contract marks as destructive,
/// external-write, network, or irreversible, is **never** repaired. Only a
/// read-only or in-workspace project-write tool that is not irreversible is
/// eligible — so repair can never turn an invalid destructive call into a valid
/// one.
#[must_use]
pub fn is_repair_eligible(
    side_effect: SideEffectClass,
    reversibility: Reversibility,
    is_mcp: bool,
) -> bool {
    !is_mcp
        && matches!(
            side_effect,
            SideEffectClass::ReadOnly | SideEffectClass::ProjectWrite
        )
        && !matches!(reversibility, Reversibility::Irreversible)
}

/// Whether a field name denotes a single path/URL the markdown-autolink rule may
/// clean. The Phase-2 heuristic; Phase 3 replaces it with a declared schema
/// intent marker so a content/command field can never be mistaken for a path.
fn is_path_like_field(name: &str) -> bool {
    matches!(name, "path" | "url")
}

/// The single complete degenerate markdown autolink `[text](target)`, returning
/// the link text — the path/URL the model meant — or `None` when the whole string
/// is not exactly one autolink. Conservative: a string with any surrounding text
/// (a prose field that merely contains a link) does not match.
#[must_use]
pub fn unwrap_markdown_autolink(value: &str) -> Option<String> {
    let value = value.trim();
    let inner = value.strip_prefix('[')?;
    let (text, rest) = inner.split_once("](")?;
    let target = rest.strip_suffix(')')?;
    if text.is_empty()
        || target.is_empty()
        || text.contains(['[', ']'])
        || target.contains(['(', ')'])
    {
        return None;
    }
    Some(text.to_string())
}

/// Wrap a bare string as a one-element array — only when the schema declares the
/// path is `array<string>`. Returns `None` for any other shape, so it can never
/// produce an array of the wrong item type.
#[must_use]
pub fn wrap_bare_string_as_array(prop: &Value, value: &Value) -> Option<Value> {
    let s = value.as_str()?;
    if array_item_type(prop)? != "string" {
        return None;
    }
    Some(Value::Array(vec![Value::String(s.to_string())]))
}

/// Parse a string that double-encodes the schema's expected array/object, **only**
/// when the parsed value matches the expected structural type and (for an array)
/// every item matches the expected item type. Returns `None` otherwise, so a
/// stringified value of the wrong shape is never accepted.
#[must_use]
pub fn parse_stringified_json(prop: &Value, value: &Value) -> Option<Value> {
    let s = value.as_str()?;
    let parsed: Value = serde_json::from_str(s).ok()?;
    let expected = expected_type(prop)?;
    match expected {
        "array" => {
            let items = parsed.as_array()?;
            if let Some(item_type) = array_item_type(prop) {
                if !items.iter().all(|item| json_type_matches(item_type, item)) {
                    return None;
                }
            }
            Some(parsed)
        }
        "object" => parsed.is_object().then_some(parsed),
        _ => None,
    }
}

/// The declared item type of an `array` property, when the schema states it.
fn array_item_type(prop: &Value) -> Option<&str> {
    prop.get("items")
        .and_then(Value::as_object)
        .and_then(|items| items.get("type"))
        .and_then(Value::as_str)
}

/// The single expected JSON type a property declares, or `None` when ambiguous.
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

/// Whether a JSON value matches a declared scalar/structural type name.
fn json_type_matches(expected: &str, actual: &Value) -> bool {
    match expected {
        "string" => actual.is_string(),
        "array" => actual.is_array(),
        "object" => actual.is_object(),
        "boolean" => actual.is_boolean(),
        "integer" => actual.is_i64() || actual.is_u64(),
        "number" => actual.is_number(),
        _ => true,
    }
}

/// The schema for a top-level property by name.
fn property<'a>(schema: &'a Value, name: &str) -> Option<&'a Value> {
    schema.get("properties").and_then(|p| p.get(name))
}

/// Intent-shaped issues the structural validator cannot see from the JSON type
/// alone: a path/URL field whose value is a degenerate markdown autolink (a valid
/// JSON string, so structurally fine, but not a usable path).
fn intent_issues(schema: &Value, input: &Value) -> Vec<SchemaIssue> {
    let mut issues = Vec::new();
    let (Some(_props), Some(obj)) = (
        schema.get("properties").and_then(Value::as_object),
        input.as_object(),
    ) else {
        return issues;
    };
    for (field, value) in obj {
        if !is_path_like_field(field) {
            continue;
        }
        if let Some(text) = value.as_str() {
            if unwrap_markdown_autolink(text).is_some() {
                issues.push(SchemaIssue {
                    path: field.clone(),
                    expected: "path".to_string(),
                    actual: "string".to_string(),
                    class: MalformedClass::MarkdownAutolink,
                });
            }
        }
    }
    issues
}

/// Apply the one repair rule that matches an issue, returning the repaired value
/// at that path, the rule id, and a model-visible note — or `None` when no rule
/// applies (the issue cannot be safely repaired).
fn apply_rule(
    schema: &Value,
    issue: &SchemaIssue,
    value: &Value,
) -> Option<(Value, &'static str, String)> {
    let prop = property(schema, &issue.path);
    match issue.class {
        MalformedClass::BareStringForArray => {
            let repaired = wrap_bare_string_as_array(prop?, value)?;
            Some((
                repaired,
                "wrap_bare_string_as_array",
                format!("interpreted `{}` as a one-element array", issue.path),
            ))
        }
        MalformedClass::StringifiedJson => {
            let repaired = parse_stringified_json(prop?, value)?;
            Some((
                repaired,
                "parse_stringified_json",
                format!("parsed `{}` from a JSON string into its value", issue.path),
            ))
        }
        MalformedClass::MarkdownAutolink => {
            let text = value.as_str()?;
            let unwrapped = unwrap_markdown_autolink(text)?;
            Some((
                Value::String(unwrapped),
                "unwrap_markdown_autolink",
                format!("unwrapped a markdown link in `{}`", issue.path),
            ))
        }
        // Not repaired: a missing field cannot be invented; `{}`→`[]` only
        // relocates the error (prefer the readable error); a generic mismatch has
        // no safe coercion.
        MalformedClass::MissingRequiredField
        | MalformedClass::ObjectForArray
        | MalformedClass::TypeMismatch => None,
    }
}

/// Validate, then (when enabled and eligible) repair, a tool call's arguments.
/// One flat pass: validate → on failure gate then repair the reported issue paths
/// → re-validate → `Valid` | `Repaired` | `Invalid`.
#[must_use]
pub fn evaluate(req: &RepairRequest<'_>, input: &Value) -> ToolInputValidationResult {
    let mut issues = tool_input_issues(req.schema, input);
    issues.extend(intent_issues(req.schema, input));

    if issues.is_empty() {
        return ToolInputValidationResult {
            tool: req.tool.to_string(),
            outcome: RepairOutcome::Valid,
            issues,
            repairs_applied: Vec::new(),
            repaired_input: None,
            model_note: None,
            readable_message: None,
            risk: req.side_effect,
            rejected_high_risk: false,
        };
    }

    let eligible = is_repair_eligible(req.side_effect, req.reversibility, req.is_mcp);
    let make_invalid = |rejected_high_risk: bool, applied: Vec<&'static str>| {
        let message = readable_input_error(req.tool, &issues, req.examples);
        ToolInputValidationResult {
            tool: req.tool.to_string(),
            outcome: RepairOutcome::Invalid,
            issues: issues.clone(),
            repairs_applied: applied,
            repaired_input: None,
            model_note: None,
            readable_message: Some(localpilot_config::redact::redact(&message)),
            risk: req.side_effect,
            rejected_high_risk,
        }
    };

    // A refused tool, or repair disabled, never has its arguments rewritten.
    if !req.attempt_repair || !eligible {
        // `rejected_high_risk` marks the case where repair *would* have run but the
        // tool is too dangerous — the auditable "we refused to reshape this" signal.
        let rejected = req.attempt_repair && !eligible;
        return make_invalid(rejected, Vec::new());
    }

    // Repair only the reported issue paths, on a clone.
    let mut candidate = input.clone();
    let mut applied = Vec::new();
    let mut notes = Vec::new();
    for issue in &issues {
        let current = candidate.get(&issue.path).cloned().unwrap_or(Value::Null);
        if let Some((repaired_value, rule_id, note)) = apply_rule(req.schema, issue, &current) {
            if let Some(obj) = candidate.as_object_mut() {
                obj.insert(issue.path.clone(), repaired_value);
                applied.push(rule_id);
                notes.push(note);
            }
        }
    }

    if applied.is_empty() {
        return make_invalid(false, Vec::new());
    }

    // Re-validate: a repair must produce a fully valid input, or it is discarded
    // and the model gets the readable error instead.
    let mut remaining = tool_input_issues(req.schema, &candidate);
    remaining.extend(intent_issues(req.schema, &candidate));
    if !remaining.is_empty() {
        return make_invalid(false, applied);
    }

    ToolInputValidationResult {
        tool: req.tool.to_string(),
        outcome: RepairOutcome::Repaired,
        issues,
        repairs_applied: applied,
        repaired_input: Some(candidate),
        model_note: Some(notes.join("; ")),
        readable_message: None,
        risk: req.side_effect,
        rejected_high_risk: false,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn array_string_schema() -> Value {
        json!({
            "type": "object",
            "required": ["paths"],
            "properties": {
                "paths": { "type": "array", "items": { "type": "string" } },
                "path": { "type": "string" },
            }
        })
    }

    fn request(
        schema: &Value,
        side_effect: SideEffectClass,
        reversibility: Reversibility,
        is_mcp: bool,
        attempt_repair: bool,
    ) -> RepairRequest<'_> {
        RepairRequest {
            tool: "t",
            schema,
            side_effect,
            reversibility,
            is_mcp,
            attempt_repair,
            examples: &[],
        }
    }

    // --- the gate -----------------------------------------------------------

    #[test]
    fn the_gate_allows_read_only_and_project_write_but_refuses_the_rest() {
        assert!(is_repair_eligible(
            SideEffectClass::ReadOnly,
            Reversibility::Reversible,
            false
        ));
        assert!(is_repair_eligible(
            SideEffectClass::ProjectWrite,
            Reversibility::ReversibleWithArtifact,
            false
        ));
        // Destructive / external-write / network → never.
        for class in [
            SideEffectClass::Destructive,
            SideEffectClass::ExternalWrite,
            SideEffectClass::Network,
        ] {
            assert!(!is_repair_eligible(class, Reversibility::Reversible, false));
        }
        // Irreversible → never, even when project-write.
        assert!(!is_repair_eligible(
            SideEffectClass::ProjectWrite,
            Reversibility::Irreversible,
            false
        ));
        // MCP → never.
        assert!(!is_repair_eligible(
            SideEffectClass::ReadOnly,
            Reversibility::Reversible,
            true
        ));
    }

    // --- rule: wrap_bare_string_as_array -----------------------------------

    #[test]
    fn wrap_rule_wraps_a_bare_string_only_for_an_array_of_strings() {
        let array_string = json!({ "type": "array", "items": { "type": "string" } });
        assert_eq!(
            wrap_bare_string_as_array(&array_string, &json!("README.md")),
            Some(json!(["README.md"]))
        );
        // An array of objects is not wrapped from a bare string.
        let array_object = json!({ "type": "array", "items": { "type": "object" } });
        assert_eq!(
            wrap_bare_string_as_array(&array_object, &json!("README.md")),
            None
        );
    }

    #[test]
    fn the_pipeline_wraps_a_bare_string_path_list() {
        let schema = array_string_schema();
        let req = request(
            &schema,
            SideEffectClass::ReadOnly,
            Reversibility::Reversible,
            false,
            true,
        );
        let result = evaluate(&req, &json!({ "paths": "README.md" }));
        assert_eq!(result.outcome, RepairOutcome::Repaired);
        assert_eq!(result.repairs_applied, vec!["wrap_bare_string_as_array"]);
        assert_eq!(
            result.repaired_input,
            Some(json!({ "paths": ["README.md"] }))
        );
        assert!(result.model_note.is_some());
    }

    #[test]
    fn a_plain_string_field_that_fails_otherwise_is_not_wrapped() {
        // `path` is a string; a missing required `paths` is the issue, and a string
        // field is never wrapped into an array.
        let schema = array_string_schema();
        let req = request(
            &schema,
            SideEffectClass::ReadOnly,
            Reversibility::Reversible,
            false,
            true,
        );
        let result = evaluate(&req, &json!({ "path": "a.rs" }));
        // `paths` is missing (cannot be invented) → Invalid, not repaired.
        assert_eq!(result.outcome, RepairOutcome::Invalid);
    }

    // --- rule: parse_stringified_json --------------------------------------

    #[test]
    fn parse_rule_unwraps_a_stringified_array_of_the_right_item_type() {
        let prop = json!({ "type": "array", "items": { "type": "string" } });
        assert_eq!(
            parse_stringified_json(&prop, &json!("[\"a.rs\",\"b.rs\"]")),
            Some(json!(["a.rs", "b.rs"]))
        );
        // Item type mismatch (numbers, not strings) is rejected.
        assert_eq!(parse_stringified_json(&prop, &json!("[1,2]")), None);
        // A string field is never parsed.
        let string_prop = json!({ "type": "string" });
        assert_eq!(
            parse_stringified_json(&string_prop, &json!("{\"a\":1}")),
            None
        );
    }

    #[test]
    fn the_pipeline_parses_a_stringified_vec() {
        let schema = array_string_schema();
        let req = request(
            &schema,
            SideEffectClass::ReadOnly,
            Reversibility::Reversible,
            false,
            true,
        );
        let result = evaluate(&req, &json!({ "paths": "[\"a.rs\"]" }));
        assert_eq!(result.outcome, RepairOutcome::Repaired);
        assert_eq!(result.repairs_applied, vec!["parse_stringified_json"]);
        assert_eq!(result.repaired_input, Some(json!({ "paths": ["a.rs"] })));
    }

    // --- rule: unwrap_markdown_autolink ------------------------------------

    #[test]
    fn unwrap_rule_unwraps_only_a_complete_single_autolink() {
        assert_eq!(
            unwrap_markdown_autolink("[notes.md](http://notes.md)"),
            Some("notes.md".to_string())
        );
        // Prose that merely contains a link is left alone.
        assert_eq!(
            unwrap_markdown_autolink("see [docs](http://x) for more"),
            None
        );
        // A plain path is not an autolink.
        assert_eq!(unwrap_markdown_autolink("src/main.rs"), None);
    }

    #[test]
    fn the_pipeline_unwraps_a_markdown_autolink_on_a_path_field() {
        // A markdown autolink in a path field is a *valid JSON string* (structurally
        // fine) but not a usable path — the intent rule catches it.
        let schema = json!({
            "type": "object",
            "required": ["path"],
            "properties": { "path": { "type": "string" } }
        });
        let req = request(
            &schema,
            SideEffectClass::ProjectWrite,
            Reversibility::ReversibleWithArtifact,
            false,
            true,
        );
        let result = evaluate(&req, &json!({ "path": "[a.rs](a.rs)" }));
        assert_eq!(result.outcome, RepairOutcome::Repaired);
        assert_eq!(result.repairs_applied, vec!["unwrap_markdown_autolink"]);
        assert_eq!(result.repaired_input, Some(json!({ "path": "a.rs" })));
    }

    #[test]
    fn a_bracketed_content_field_is_left_untouched() {
        // `content` is not a path-like field, so a bracketed/markdown value is never
        // unwrapped — and a valid string is not an issue at all.
        let schema = json!({
            "type": "object",
            "required": ["content"],
            "properties": { "content": { "type": "string" } }
        });
        let req = request(
            &schema,
            SideEffectClass::ProjectWrite,
            Reversibility::ReversibleWithArtifact,
            false,
            true,
        );
        let result = evaluate(&req, &json!({ "content": "[a.rs](a.rs)" }));
        assert_eq!(result.outcome, RepairOutcome::Valid);
        assert!(result.repaired_input.is_none());
    }

    // --- the gate in the pipeline ------------------------------------------

    #[test]
    fn a_refused_tool_is_never_repaired_and_is_flagged_high_risk() {
        let schema = array_string_schema();
        // A destructive tool with a repairable-looking issue is refused, not fixed.
        let req = request(
            &schema,
            SideEffectClass::Destructive,
            Reversibility::Reversible,
            false,
            true,
        );
        let result = evaluate(&req, &json!({ "paths": "README.md" }));
        assert_eq!(result.outcome, RepairOutcome::Invalid);
        assert!(result.repaired_input.is_none());
        assert!(result.rejected_high_risk, "a refused repair is auditable");
        assert!(result.readable_message.is_some());
    }

    #[test]
    fn repair_off_validates_but_never_rewrites() {
        let schema = array_string_schema();
        let req = request(
            &schema,
            SideEffectClass::ReadOnly,
            Reversibility::Reversible,
            false,
            false, // repair disabled
        );
        let result = evaluate(&req, &json!({ "paths": "README.md" }));
        assert_eq!(result.outcome, RepairOutcome::Invalid);
        assert!(result.repaired_input.is_none());
        assert!(
            !result.rejected_high_risk,
            "off is not a high-risk refusal, just disabled"
        );
    }

    #[test]
    fn a_valid_input_is_byte_unchanged() {
        let schema = array_string_schema();
        let req = request(
            &schema,
            SideEffectClass::ReadOnly,
            Reversibility::Reversible,
            false,
            true,
        );
        let result = evaluate(&req, &json!({ "paths": ["a.rs"] }));
        assert_eq!(result.outcome, RepairOutcome::Valid);
        assert!(result.repaired_input.is_none());
        assert!(result.issues.is_empty());
    }
}
