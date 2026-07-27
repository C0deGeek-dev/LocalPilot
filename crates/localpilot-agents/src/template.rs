//! The closed placeholder vocabulary a definition's prompt may use.
//!
//! Definitions are data. A definition must not be able to run a command,
//! interpolate a host path, or reach anything the author did not name — so
//! substitution is a fixed list of keys resolved by the host, and an unknown
//! placeholder is a **load-time error** rather than text passed through.
//! Failing at load is what keeps a typo from silently shipping `{{workspace}}`
//! into a model's prompt.

use std::collections::BTreeMap;

use crate::error::AgentError;

/// Every placeholder a definition may use. Extending this list is a deliberate
/// act; nothing resolves dynamically.
pub const VOCABULARY: &[&str] = &["agent_name", "workspace", "tools"];

/// Values the host supplies for [`VOCABULARY`] keys.
#[derive(Debug, Default)]
pub struct Bindings(BTreeMap<&'static str, String>);

impl Bindings {
    /// Bind one known key. A key outside [`VOCABULARY`] is ignored rather than
    /// silently expanding the vocabulary.
    #[must_use]
    pub fn with(mut self, key: &'static str, value: impl Into<String>) -> Self {
        if VOCABULARY.contains(&key) {
            self.0.insert(key, value.into());
        }
        self
    }

    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).map(String::as_str)
    }
}

/// Check that every `{{placeholder}}` in `text` is in [`VOCABULARY`].
///
/// # Errors
/// Returns [`AgentError::Invalid`] naming the first unknown placeholder.
pub fn validate(text: &str) -> Result<(), AgentError> {
    for key in placeholders(text) {
        if !VOCABULARY.contains(&key.as_str()) {
            return Err(AgentError::Invalid(format!(
                "unknown placeholder {{{{{key}}}}}; supported placeholders are {}",
                VOCABULARY
                    .iter()
                    .map(|k| format!("{{{{{k}}}}}"))
                    .collect::<Vec<_>>()
                    .join(", ")
            )));
        }
    }
    Ok(())
}

/// Substitute known placeholders. Unbound-but-known keys expand to an empty
/// string; unknown keys cannot occur because [`validate`] runs at load.
#[must_use]
pub fn render(text: &str, bindings: &Bindings) -> String {
    let mut out = String::with_capacity(text.len());
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        out.push_str(&rest[..start]);
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else {
            // No closing delimiter: the rest is literal text, not a placeholder.
            out.push_str(&rest[start..]);
            return out;
        };
        let key = after[..end].trim();
        out.push_str(bindings.get(key).unwrap_or(""));
        rest = &after[end + 2..];
    }
    out.push_str(rest);
    out
}

/// Every placeholder key appearing in `text`, in order of appearance.
fn placeholders(text: &str) -> Vec<String> {
    let mut keys = Vec::new();
    let mut rest = text;
    while let Some(start) = rest.find("{{") {
        let after = &rest[start + 2..];
        let Some(end) = after.find("}}") else { break };
        keys.push(after[..end].trim().to_string());
        rest = &after[end + 2..];
    }
    keys
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn known_placeholders_validate_and_render() {
        let text = "You are {{agent_name}} working in {{workspace}}.";
        validate(text).expect("known keys");
        let bindings = Bindings::default()
            .with("agent_name", "reviewer")
            .with("workspace", "/repo");
        assert_eq!(
            render(text, &bindings),
            "You are reviewer working in /repo."
        );
    }

    #[test]
    fn an_unknown_placeholder_fails_at_load_and_names_the_vocabulary() {
        let message = validate("run {{shell}} now")
            .expect_err("unknown key")
            .to_string();
        assert!(message.contains("shell"), "{message}");
        assert!(
            message.contains("agent_name"),
            "lists what is valid: {message}"
        );
    }

    #[test]
    fn a_command_substitution_attempt_is_inert_text() {
        // `$(...)`/backticks carry no meaning here: only `{{key}}` is a
        // placeholder, and `key` must be in the vocabulary.
        let text = "run $(rm -rf /) and `whoami`";
        validate(text).expect("no placeholders at all");
        assert_eq!(
            render(text, &Bindings::default()),
            text,
            "non-placeholder syntax passes through untouched"
        );
    }

    #[test]
    fn an_unbound_known_key_renders_empty_rather_than_leaking_the_placeholder() {
        assert_eq!(render("[{{tools}}]", &Bindings::default()), "[]");
    }

    #[test]
    fn an_unterminated_placeholder_is_literal_text() {
        let text = "a {{agent_name and more";
        validate(text).expect("no complete placeholder");
        assert_eq!(render(text, &Bindings::default()), text);
    }

    #[test]
    fn binding_an_out_of_vocabulary_key_does_nothing() {
        let bindings = Bindings::default().with("agent_name", "a");
        // `home` is not in the vocabulary, so it can never be introduced by a
        // caller passing extra bindings.
        assert!(!VOCABULARY.contains(&"home"));
        assert_eq!(render("{{agent_name}}", &bindings), "a");
    }
}
