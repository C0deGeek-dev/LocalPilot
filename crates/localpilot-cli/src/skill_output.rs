//! Semantic styling for human-facing skill catalog reports.
//!
//! Layout remains the primary readability cue. ANSI styling is emitted only to
//! a terminal, follows the same named palette policy as full-screen chat, and
//! disappears completely for pipes, captured slash-command output, or
//! `NO_COLOR`.

use anstyle::{AnsiColor, RgbColor, Style};
use localpilot_skills::{MatchState, SkillCatalogStyle};

#[derive(Debug, Clone, Copy, Default)]
enum CatalogTheme {
    #[default]
    Default,
    Terminal,
    Dim,
    HighContrast,
    Colorblind,
}

impl CatalogTheme {
    fn parse(value: Option<&str>) -> Self {
        match value.map(str::trim).map(str::to_ascii_lowercase).as_deref() {
            Some("terminal" | "base16" | "base-16") => Self::Terminal,
            Some("dim") => Self::Dim,
            Some("high-contrast" | "high_contrast") => Self::HighContrast,
            Some("colorblind" | "color-blind") => Self::Colorblind,
            _ => Self::Default,
        }
    }
}

/// ANSI styles for the two identity fields readers scan in a catalog entry.
#[derive(Debug, Clone, Copy, Default)]
pub(crate) struct CliSkillCatalogStyle {
    name: Style,
    installed: Style,
    available: Style,
}

impl CliSkillCatalogStyle {
    pub(crate) fn resolve(output_is_terminal: bool, no_color: bool, theme: Option<&str>) -> Self {
        if !output_is_terminal || no_color {
            return Self::default();
        }
        let (name, installed, available) = match CatalogTheme::parse(theme) {
            CatalogTheme::Default => (
                rgb(0x44, 0x93, 0xf8),
                rgb(0x3f, 0xb9, 0x50),
                rgb(0xd2, 0x99, 0x22),
            ),
            CatalogTheme::Terminal => (
                ansi(AnsiColor::Blue).bold(),
                ansi(AnsiColor::Green).bold(),
                ansi(AnsiColor::Yellow).bold(),
            ),
            CatalogTheme::Dim => (
                ansi(AnsiColor::Cyan),
                ansi(AnsiColor::Cyan),
                ansi(AnsiColor::BrightBlack).underline(),
            ),
            CatalogTheme::HighContrast => (
                ansi(AnsiColor::BrightYellow).bold(),
                ansi(AnsiColor::BrightGreen).bold(),
                ansi(AnsiColor::BrightYellow).bold(),
            ),
            CatalogTheme::Colorblind => (
                ansi(AnsiColor::Blue).bold(),
                ansi(AnsiColor::Blue).bold(),
                ansi(AnsiColor::BrightYellow).bold(),
            ),
        };
        Self {
            name,
            installed,
            available,
        }
    }

    pub(crate) fn from_environment(output_is_terminal: bool) -> Self {
        let theme = std::env::var("LOCALPILOT_CHAT_THEME").ok();
        Self::resolve(
            output_is_terminal,
            std::env::var_os("NO_COLOR").is_some(),
            theme.as_deref(),
        )
    }

    fn paint(style: Style, value: &str) -> String {
        format!("{style}{value}{style:#}")
    }
}

impl SkillCatalogStyle for CliSkillCatalogStyle {
    fn name(&self, value: &str) -> String {
        Self::paint(self.name, value)
    }

    fn state(&self, state: MatchState) -> String {
        let style = match state {
            MatchState::Installed => self.installed,
            MatchState::Available | MatchState::Discovered => self.available,
        };
        Self::paint(style, state.label())
    }
}

fn ansi(color: AnsiColor) -> Style {
    Style::new().fg_color(Some(color.into()))
}

fn rgb(red: u8, green: u8, blue: u8) -> Style {
    Style::new().fg_color(Some(RgbColor(red, green, blue).into()))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn pipes_and_no_color_emit_no_escape_sequences() {
        for style in [
            CliSkillCatalogStyle::resolve(false, false, Some("default")),
            CliSkillCatalogStyle::resolve(true, true, Some("high-contrast")),
        ] {
            assert_eq!(style.name("alpha"), "alpha");
            assert_eq!(style.state(MatchState::Installed), "installed");
        }
    }

    #[test]
    fn every_named_color_theme_preserves_the_identity_text() {
        for theme in ["default", "terminal", "dim", "high-contrast", "colorblind"] {
            let style = CliSkillCatalogStyle::resolve(true, false, Some(theme));
            let name = style.name("alpha");
            let state = style.state(MatchState::Available);
            assert!(
                name.contains("alpha") && name.contains("\u{1b}["),
                "{theme}: {name:?}"
            );
            assert!(
                state.contains("available") && state.contains("\u{1b}["),
                "{theme}: {state:?}"
            );
        }
    }
}
