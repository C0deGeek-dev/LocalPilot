//! Output-format resolution shared by the read commands.
//!
//! The dogfood run proved that *adding* a `--json` flag is not enough if nothing
//! surfaces it: both the operator and the local model missed it and tab-parsed
//! the human table. So the format a caller gets is resolved from context — a
//! non-terminal stdout (a pipe or file) defaults to the structured form, a real
//! terminal keeps the human table — and a uniform `--format` overrides either way.

use std::io::Write;

/// The output format for a read command.
#[derive(Clone, Copy, Debug, PartialEq, Eq, clap::ValueEnum)]
pub enum OutputFormat {
    /// Human-readable table (the interactive default).
    Human,
    /// A machine-readable JSON array (the non-terminal default).
    Json,
}

/// Resolve the effective output format. Precedence, highest first:
/// 1. an explicit `--format human|json`;
/// 2. the legacy `--json` alias (≡ `--format json`);
/// 3. the stdout default — structured when stdout is *not* a terminal (a pipe or
///    file, i.e. a program is reading), human when it is.
#[must_use]
pub fn resolve_format(
    explicit: Option<OutputFormat>,
    json_alias: bool,
    stdout_is_tty: bool,
) -> OutputFormat {
    if let Some(format) = explicit {
        return format;
    }
    if json_alias {
        return OutputFormat::Json;
    }
    if stdout_is_tty {
        OutputFormat::Human
    } else {
        OutputFormat::Json
    }
}

/// Whether to print the affordance hint that points at the structured form. Only
/// when the human table is rendered *interactively* (a real terminal): never into
/// a pipe and never alongside JSON, so it can't pollute machine-read output.
#[must_use]
pub fn show_format_hint(resolved: OutputFormat, stdout_is_tty: bool) -> bool {
    resolved == OutputFormat::Human && stdout_is_tty
}

/// Write the affordance hint to `err`. A single terse stderr line.
///
/// # Errors
/// Returns any error from writing to `err`.
pub fn write_format_hint(err: &mut dyn Write) -> std::io::Result<()> {
    writeln!(
        err,
        "tip: add --format json (or --json) for machine-readable output"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn explicit_format_wins_over_everything() {
        // --format human forces the table even when piped; --format json forces
        // JSON even on a terminal.
        assert_eq!(
            resolve_format(Some(OutputFormat::Human), false, false),
            OutputFormat::Human
        );
        assert_eq!(
            resolve_format(Some(OutputFormat::Json), false, true),
            OutputFormat::Json
        );
        // An explicit --format also wins over the --json alias.
        assert_eq!(
            resolve_format(Some(OutputFormat::Human), true, false),
            OutputFormat::Human
        );
    }

    #[test]
    fn json_alias_forces_json_without_an_explicit_format() {
        assert_eq!(resolve_format(None, true, true), OutputFormat::Json);
        assert_eq!(resolve_format(None, true, false), OutputFormat::Json);
    }

    #[test]
    fn the_tty_default_is_human_and_the_pipe_default_is_json() {
        assert_eq!(resolve_format(None, false, true), OutputFormat::Human);
        assert_eq!(resolve_format(None, false, false), OutputFormat::Json);
    }

    #[test]
    fn the_hint_fires_only_on_an_interactive_human_table() {
        assert!(show_format_hint(OutputFormat::Human, true));
        assert!(!show_format_hint(OutputFormat::Human, false)); // piped human (forced)
        assert!(!show_format_hint(OutputFormat::Json, true)); // json on a tty
        assert!(!show_format_hint(OutputFormat::Json, false));
    }
}
