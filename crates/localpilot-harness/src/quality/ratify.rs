//! Quality-gate ratification.
//!
//! Discovery (see [`super::discovery`]) proposes; the user ratifies. Ratification
//! is the trust boundary (ADR-0009, docs/07): a proposed check is untrusted and
//! never runs until it is written into committed `.localpilot.toml`. This module
//! renders a proposal into `[[harness.checks]]` config and merges it into the
//! existing file, adding only checks not already ratified — a re-probe proposes
//! additions, it never silently adopts them or rewrites the user's settings.

use localpilot_config::{AutoFix, Cadence, CheckConfig, RuleSeverity};
use localpilot_sandbox::CommandClass;

use super::discovery::ProposedCheck;

/// The result of merging a proposal into the existing config.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct GateRatification {
    /// Checks newly written (not previously ratified).
    pub added: Vec<CheckConfig>,
    /// Proposed checks already present in config, skipped.
    pub already_present: Vec<String>,
    /// The full new `.localpilot.toml` text. Equals the input when nothing is
    /// added, so writing it back is a no-op.
    pub config_text: String,
}

/// Merge `proposed` into `existing_config`, skipping any check whose name is in
/// `ratified_names`. Existing config text is preserved verbatim; new checks are
/// appended as `[[harness.checks]]` tables.
#[must_use]
pub fn ratify_gate(
    existing_config: &str,
    ratified_names: &[String],
    proposed: &[ProposedCheck],
) -> GateRatification {
    let mut added = Vec::new();
    let mut already_present = Vec::new();
    for proposal in proposed {
        if ratified_names
            .iter()
            .any(|name| name == &proposal.check.name)
        {
            already_present.push(proposal.check.name.clone());
        } else {
            added.push(proposal.check.clone());
        }
    }
    let config_text = if added.is_empty() {
        existing_config.to_string()
    } else {
        append_checks(existing_config, &added)
    };
    GateRatification {
        added,
        already_present,
        config_text,
    }
}

/// Append rendered `[[harness.checks]]` tables after the existing config,
/// separated by a blank line so the result stays readable.
fn append_checks(existing: &str, checks: &[CheckConfig]) -> String {
    let mut out = existing.to_string();
    if !out.is_empty() && !out.ends_with('\n') {
        out.push('\n');
    }
    for check in checks {
        out.push('\n');
        out.push_str(&render_check(check));
    }
    out
}

/// Render one check as a `[[harness.checks]]` table. The output round-trips
/// through the config loader.
#[must_use]
pub fn render_check(check: &CheckConfig) -> String {
    use std::fmt::Write as _;
    let mut s = String::from("[[harness.checks]]\n");
    let _ = writeln!(s, "name = {}", toml_str(&check.name));
    let _ = writeln!(s, "program = {}", toml_str(&check.program));
    let _ = writeln!(s, "args = {}", toml_array(&check.args));
    let _ = writeln!(s, "cadence = {}", toml_str(cadence_str(check.cadence)));
    let _ = writeln!(s, "auto_fix = {}", auto_fix_lit(check.auto_fix));
    if let Some(fix_program) = &check.fix_program {
        let _ = writeln!(s, "fix_program = {}", toml_str(fix_program));
        if !check.fix_args.is_empty() {
            let _ = writeln!(s, "fix_args = {}", toml_array(&check.fix_args));
        }
    }
    if let Some(severity) = check.severity {
        let _ = writeln!(s, "severity = {}", toml_str(severity_str(severity)));
    }
    s
}

/// A human-readable preview of a proposal: each check's name, cadence, command,
/// and risk class, with an explicit warning for a destructive/privileged/network
/// command. Read-only — this is what `gate propose` prints before any write.
#[must_use]
pub fn summarize_proposal(proposed: &[ProposedCheck]) -> String {
    if proposed.is_empty() {
        return "No quality-gate checks proposed: no known stack or no tools found on PATH.\n"
            .to_string();
    }
    use std::fmt::Write as _;
    let mut s = String::new();
    for proposal in proposed {
        let command = command_line(&proposal.check.program, &proposal.check.args);
        let _ = writeln!(
            s,
            "- {} [{}] — {}",
            proposal.check.name,
            cadence_str(proposal.check.cadence),
            command
        );
        let _ = writeln!(s, "    risk class: {}", class_label(proposal.class));
        if proposal.needs_explicit_warning() {
            let _ = writeln!(
                s,
                "    WARNING: a {} command — review carefully before ratifying",
                class_label(proposal.class)
            );
        }
    }
    s
}

fn command_line(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

fn cadence_str(cadence: Cadence) -> &'static str {
    match cadence {
        Cadence::Step => "step",
        Cadence::Phase => "phase",
    }
}

fn severity_str(severity: RuleSeverity) -> &'static str {
    match severity {
        RuleSeverity::Off => "off",
        RuleSeverity::Warn => "warn",
        RuleSeverity::Block => "block",
        RuleSeverity::Discard => "discard",
    }
}

fn auto_fix_lit(auto_fix: AutoFix) -> String {
    match auto_fix {
        AutoFix::No => "false".to_string(),
        AutoFix::Full => "true".to_string(),
        AutoFix::Safe => "\"safe\"".to_string(),
    }
}

fn class_label(class: CommandClass) -> &'static str {
    match class {
        CommandClass::ReadOnly => "read-only",
        CommandClass::ProjectWrite => "project-write",
        CommandClass::ExternalWrite => "external-write",
        CommandClass::Network => "network",
        CommandClass::Destructive => "destructive",
        CommandClass::Privileged => "privileged",
        CommandClass::Unknown => "unknown",
        _ => "unknown",
    }
}

/// Render a string as a TOML basic string, escaping the characters TOML requires.
fn toml_str(value: &str) -> String {
    let mut s = String::with_capacity(value.len() + 2);
    s.push('"');
    for ch in value.chars() {
        match ch {
            '"' => s.push_str("\\\""),
            '\\' => s.push_str("\\\\"),
            '\n' => s.push_str("\\n"),
            '\t' => s.push_str("\\t"),
            '\r' => s.push_str("\\r"),
            other => s.push(other),
        }
    }
    s.push('"');
    s
}

fn toml_array(items: &[String]) -> String {
    let rendered: Vec<String> = items.iter().map(|item| toml_str(item)).collect();
    format!("[{}]", rendered.join(", "))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::quality::ProposedCheck;

    fn proposed(name: &str, class: CommandClass) -> ProposedCheck {
        ProposedCheck {
            check: CheckConfig {
                name: name.to_string(),
                program: "cargo".to_string(),
                args: vec![name.to_string()],
                fix_program: None,
                fix_args: Vec::new(),
                cadence: Cadence::Step,
                auto_fix: AutoFix::No,
                severity: None,
            },
            class,
        }
    }

    #[test]
    fn renders_all_check_fields() {
        let check = CheckConfig {
            name: "fmt".to_string(),
            program: "cargo".to_string(),
            args: vec!["fmt".to_string(), "--check".to_string()],
            fix_program: Some("cargo".to_string()),
            fix_args: vec!["fmt".to_string()],
            cadence: Cadence::Step,
            auto_fix: AutoFix::Full,
            severity: Some(RuleSeverity::Block),
        };
        let rendered = render_check(&check);
        assert!(rendered.contains("[[harness.checks]]"));
        assert!(rendered.contains("name = \"fmt\""));
        assert!(rendered.contains("args = [\"fmt\", \"--check\"]"));
        assert!(rendered.contains("cadence = \"step\""));
        assert!(rendered.contains("auto_fix = true"));
        assert!(rendered.contains("fix_program = \"cargo\""));
        assert!(rendered.contains("severity = \"block\""));
    }

    #[test]
    fn safe_auto_fix_renders_as_a_string() {
        let mut check = proposed("clippy", CommandClass::ProjectWrite).check;
        check.auto_fix = AutoFix::Safe;
        assert!(render_check(&check).contains("auto_fix = \"safe\""));
    }

    #[test]
    fn ratify_appends_new_checks_and_preserves_existing_config() {
        let existing = "[harness]\nmode = \"agent\"\n";
        let proposal = vec![proposed("fmt", CommandClass::ProjectWrite)];
        let result = ratify_gate(existing, &[], &proposal);
        assert_eq!(result.added.len(), 1);
        assert!(result
            .config_text
            .starts_with("[harness]\nmode = \"agent\"\n"));
        assert!(result.config_text.contains("[[harness.checks]]"));
        assert!(result.config_text.contains("name = \"fmt\""));
    }

    #[test]
    fn reprobe_adds_only_checks_not_already_ratified() {
        let existing = "[harness]\n";
        let proposal = vec![
            proposed("fmt", CommandClass::ProjectWrite),
            proposed("clippy", CommandClass::ProjectWrite),
        ];
        let result = ratify_gate(existing, &["fmt".to_string()], &proposal);
        assert_eq!(
            result
                .added
                .iter()
                .map(|c| c.name.as_str())
                .collect::<Vec<_>>(),
            ["clippy"]
        );
        assert_eq!(result.already_present, ["fmt"]);
        assert!(!result.config_text.contains("name = \"fmt\""));
        assert!(result.config_text.contains("name = \"clippy\""));
    }

    #[test]
    fn ratifying_an_empty_or_fully_present_proposal_is_a_no_op() {
        let existing = "[harness]\n";
        let result = ratify_gate(existing, &[], &[]);
        assert!(result.added.is_empty());
        assert_eq!(result.config_text, existing);
    }

    #[test]
    fn summary_flags_a_warning_class() {
        let summary = summarize_proposal(&[proposed("danger", CommandClass::Destructive)]);
        assert!(summary.contains("danger"));
        assert!(summary.contains("destructive"));
        assert!(summary.contains("WARNING"));
    }

    #[test]
    fn summary_of_a_safe_class_has_no_warning() {
        let summary = summarize_proposal(&[proposed("fmt", CommandClass::ProjectWrite)]);
        assert!(summary.contains("project-write"));
        assert!(!summary.contains("WARNING"));
    }

    #[test]
    fn empty_proposal_summary_explains_why() {
        assert!(summarize_proposal(&[]).contains("No quality-gate checks proposed"));
    }
}
