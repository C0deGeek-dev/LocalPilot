//! Per-class malformed-argument baseline against the *real* builtin schemas.
//!
//! The offline discipline benchmark only exercises the missing-required-field
//! class. This test pins the classifier's coverage of the structural malformed
//! classes (stringified array, bare string for array, object for array, scalar
//! type mismatch, missing field) against the actual schemas the builtins emit,
//! so when a live local-model class breakdown is captured (DEFERRED, D008) the
//! per-class counts are trustworthy. It is measurement, not behaviour: it asserts
//! how the shared validator classifies, nothing about dispatch.

use localpilot_tools::{tool_input_issues, MalformedClass, ToolRegistry};
use serde_json::{json, Value};

/// The JSON schema the named builtin advertises.
fn schema_of(registry: &ToolRegistry, name: &str) -> Value {
    registry
        .get(name)
        .unwrap_or_else(|| panic!("builtin {name} is registered"))
        .schema()
}

/// The class the shared validator assigns to the first issue of `input` against
/// the named builtin's schema, or `None` when the input validates.
fn first_class(registry: &ToolRegistry, name: &str, input: &Value) -> Option<MalformedClass> {
    tool_input_issues(&schema_of(registry, name), input)
        .first()
        .map(|issue| issue.class)
}

#[test]
fn each_malformed_class_is_recognized_on_a_real_builtin_schema() {
    let registry = ToolRegistry::with_builtins();

    // Class 7 — missing required field: read_file without `path`.
    assert_eq!(
        first_class(&registry, "read_file", &json!({})),
        Some(MalformedClass::MissingRequiredField)
    );

    // Class 4 — bare string where an array<string> is expected: git_diff.paths.
    assert_eq!(
        first_class(&registry, "git_diff", &json!({ "paths": "README.md" })),
        Some(MalformedClass::BareStringForArray)
    );

    // Class 2 — stringified array: git_add.paths as a JSON string.
    assert_eq!(
        first_class(
            &registry,
            "git_add",
            &json!({ "paths": "[\"a.rs\",\"b.rs\"]" })
        ),
        Some(MalformedClass::StringifiedJson)
    );

    // Class 2 — stringified array of objects: multi_edit.edits as a JSON string.
    assert_eq!(
        first_class(
            &registry,
            "multi_edit",
            &json!({ "path": "a.rs", "edits": "[{\"old_text\":\"a\",\"new_text\":\"b\"}]" })
        ),
        Some(MalformedClass::StringifiedJson)
    );

    // Class 3 — object where an array is expected: apply_patch.operations as {}.
    assert_eq!(
        first_class(&registry, "apply_patch", &json!({ "operations": {} })),
        Some(MalformedClass::ObjectForArray)
    );

    // Scalar type mismatch: write_file.content as a number.
    assert_eq!(
        first_class(
            &registry,
            "write_file",
            &json!({ "path": "a.rs", "content": 7 })
        ),
        Some(MalformedClass::TypeMismatch)
    );
}

#[test]
fn well_formed_calls_and_markdown_paths_are_not_flagged() {
    let registry = ToolRegistry::with_builtins();

    // Fully valid calls are byte-clean: no issues.
    assert!(first_class(&registry, "read_file", &json!({ "path": "a.rs" })).is_none());
    assert!(first_class(&registry, "git_diff", &json!({ "paths": ["a.rs"] })).is_none());
    assert!(first_class(
        &registry,
        "write_file",
        &json!({ "path": "a.rs", "content": "x" })
    )
    .is_none());

    // A markdown-autolink path is a *valid JSON string*, so it is type-valid and
    // not a structural issue — class 5 is intent-shaped and out of this
    // validator's scope (it lands with the schema-intent helpers, Phase 3).
    assert!(first_class(
        &registry,
        "read_file",
        &json!({ "path": "[notes.md](http://notes.md)" })
    )
    .is_none());
}

#[test]
fn the_offline_class_baseline_histogram() {
    // The offline corpus exercises exactly the structural classes; the live
    // per-model frequency breakdown is DEFERRED (D008). This prints the coverage
    // histogram so a live run can be compared against a known classifier.
    let registry = ToolRegistry::with_builtins();
    let corpus: &[(&str, Value)] = &[
        ("read_file", json!({})),
        ("git_diff", json!({ "paths": "README.md" })),
        ("git_add", json!({ "paths": "[\"a.rs\"]" })),
        ("apply_patch", json!({ "operations": {} })),
        ("write_file", json!({ "path": "a.rs", "content": 7 })),
    ];
    let mut counts = std::collections::BTreeMap::new();
    for (tool, input) in corpus {
        if let Some(class) = first_class(&registry, tool, input) {
            *counts.entry(class.label()).or_insert(0usize) += 1;
        }
    }
    eprintln!("offline malformed-class baseline (classifier coverage): {counts:?}");
    // Every corpus entry classifies into a structural class — no silent misses.
    assert_eq!(counts.values().sum::<usize>(), corpus.len());
}
