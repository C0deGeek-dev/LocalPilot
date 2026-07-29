use std::error::Error;
use std::fmt;
use std::str::FromStr;

use ratatui::style::{Color, Modifier, Style};

use crate::{ColorSupport, SemanticRole, TextStyle};

#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum Theme {
    #[default]
    Default,
    Dim,
    HighContrast,
    Colorblind,
}

impl Theme {
    #[must_use]
    pub const fn name(self) -> &'static str {
        match self {
            Self::Default => "default",
            Self::Dim => "dim",
            Self::HighContrast => "high-contrast",
            Self::Colorblind => "colorblind",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ThemeParseError {
    value: String,
}

impl fmt::Display for ThemeParseError {
    fn fmt(&self, formatter: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(
            formatter,
            "unknown terminal chat theme {:?}; expected default, dim, high-contrast, or colorblind",
            self.value
        )
    }
}

impl Error for ThemeParseError {}

impl FromStr for Theme {
    type Err = ThemeParseError;

    fn from_str(value: &str) -> Result<Self, Self::Err> {
        match value.trim().to_ascii_lowercase().as_str() {
            "default" => Ok(Self::Default),
            "dim" => Ok(Self::Dim),
            "high-contrast" | "high_contrast" => Ok(Self::HighContrast),
            "colorblind" | "color-blind" => Ok(Self::Colorblind),
            _ => Err(ThemeParseError {
                value: value.to_string(),
            }),
        }
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum UiRole {
    Foreground,
    Muted,
    Accent,
    Border,
    Focus,
    TabActive,
    TabInactive,
    Selection,
    Warning,
    Success,
    Error,
    Code,
}

#[derive(Debug, Clone, Copy)]
pub struct ThemeResolver {
    theme: Theme,
    color_support: ColorSupport,
}

impl ThemeResolver {
    #[must_use]
    pub const fn new(theme: Theme, color_support: ColorSupport) -> Self {
        Self {
            theme,
            color_support,
        }
    }

    #[must_use]
    pub fn text(self, text_style: TextStyle) -> Style {
        let role = match text_style.role {
            SemanticRole::User | SemanticRole::Accent | SemanticRole::Heading => UiRole::Accent,
            SemanticRole::Assistant => UiRole::Foreground,
            SemanticRole::Reasoning | SemanticRole::Muted => UiRole::Muted,
            SemanticRole::Tool | SemanticRole::Code => UiRole::Code,
            SemanticRole::Notice => UiRole::Warning,
            SemanticRole::Link => UiRole::Accent,
            SemanticRole::Success => UiRole::Success,
            SemanticRole::Error => UiRole::Error,
        };
        let mut style = self.ui(role);
        if text_style.bold || text_style.role == SemanticRole::Heading {
            style = style.add_modifier(Modifier::BOLD);
        }
        if text_style.italic || text_style.role == SemanticRole::Reasoning {
            style = style.add_modifier(Modifier::ITALIC);
        }
        if text_style.underlined || text_style.role == SemanticRole::Link {
            style = style.add_modifier(Modifier::UNDERLINED);
        }
        style
    }

    #[must_use]
    pub fn ui(self, role: UiRole) -> Style {
        if self.color_support == ColorSupport::NoColor {
            return no_color_style(role);
        }
        color_style(self.theme, role)
    }

    #[must_use]
    pub fn selected(self, base: Style) -> Style {
        let selection = self.ui(UiRole::Selection);
        base.patch(selection)
    }
}

fn no_color_style(role: UiRole) -> Style {
    match role {
        UiRole::TabActive | UiRole::Selection | UiRole::Code => {
            Style::default().add_modifier(Modifier::REVERSED)
        }
        UiRole::Accent | UiRole::Focus | UiRole::Success => {
            Style::default().add_modifier(Modifier::BOLD)
        }
        UiRole::Warning | UiRole::Error => Style::default()
            .add_modifier(Modifier::BOLD)
            .add_modifier(Modifier::UNDERLINED),
        UiRole::Muted | UiRole::TabInactive | UiRole::Border => {
            Style::default().add_modifier(Modifier::DIM)
        }
        UiRole::Foreground => Style::default(),
    }
}

fn color_style(theme: Theme, role: UiRole) -> Style {
    let (foreground, background, modifier) = match theme {
        Theme::Default => default_colors(role),
        Theme::Dim => dim_colors(role),
        Theme::HighContrast => high_contrast_colors(role),
        Theme::Colorblind => colorblind_colors(role),
    };
    let mut style = Style::default().fg(foreground);
    if let Some(background) = background {
        style = style.bg(background);
    }
    if let Some(modifier) = modifier {
        style = style.add_modifier(modifier);
    }
    style
}

fn default_colors(role: UiRole) -> (Color, Option<Color>, Option<Modifier>) {
    match role {
        UiRole::Foreground => (Color::Reset, None, None),
        UiRole::Muted | UiRole::TabInactive | UiRole::Border => (Color::DarkGray, None, None),
        UiRole::Accent | UiRole::Focus => (Color::Blue, None, Some(Modifier::BOLD)),
        UiRole::TabActive => (Color::White, Some(Color::Blue), Some(Modifier::BOLD)),
        UiRole::Selection => (Color::Black, Some(Color::LightBlue), None),
        UiRole::Warning => (Color::Yellow, None, None),
        UiRole::Success => (Color::Green, None, None),
        UiRole::Error => (Color::Red, None, Some(Modifier::BOLD)),
        UiRole::Code => (Color::LightCyan, Some(Color::Black), None),
    }
}

fn dim_colors(role: UiRole) -> (Color, Option<Color>, Option<Modifier>) {
    match role {
        UiRole::Foreground => (Color::Gray, None, None),
        UiRole::Muted | UiRole::TabInactive | UiRole::Border => {
            (Color::DarkGray, None, Some(Modifier::DIM))
        }
        UiRole::Accent | UiRole::Focus => (Color::Cyan, None, None),
        UiRole::TabActive => (Color::Gray, Some(Color::Blue), Some(Modifier::BOLD)),
        UiRole::Selection => (Color::Black, Some(Color::Cyan), None),
        UiRole::Warning => (Color::DarkGray, None, Some(Modifier::UNDERLINED)),
        UiRole::Success => (Color::Cyan, None, None),
        UiRole::Error => (Color::LightRed, None, None),
        UiRole::Code => (Color::Gray, Some(Color::Black), None),
    }
}

fn high_contrast_colors(role: UiRole) -> (Color, Option<Color>, Option<Modifier>) {
    match role {
        UiRole::Foreground => (Color::White, None, None),
        UiRole::Muted | UiRole::TabInactive | UiRole::Border => (Color::Gray, None, None),
        UiRole::Accent | UiRole::Focus => (Color::Yellow, None, Some(Modifier::BOLD)),
        UiRole::TabActive => (Color::Black, Some(Color::Yellow), Some(Modifier::BOLD)),
        UiRole::Selection => (Color::Black, Some(Color::White), Some(Modifier::BOLD)),
        UiRole::Warning => (Color::Yellow, None, Some(Modifier::BOLD)),
        UiRole::Success => (Color::LightGreen, None, Some(Modifier::BOLD)),
        UiRole::Error => (Color::LightRed, None, Some(Modifier::BOLD)),
        UiRole::Code => (Color::Black, Some(Color::White), None),
    }
}

fn colorblind_colors(role: UiRole) -> (Color, Option<Color>, Option<Modifier>) {
    match role {
        UiRole::Foreground => (Color::Reset, None, None),
        UiRole::Muted | UiRole::TabInactive | UiRole::Border => (Color::DarkGray, None, None),
        UiRole::Accent | UiRole::Focus | UiRole::Success => {
            (Color::Blue, None, Some(Modifier::BOLD))
        }
        UiRole::TabActive => (Color::White, Some(Color::Blue), Some(Modifier::BOLD)),
        UiRole::Selection => (Color::Black, Some(Color::LightBlue), None),
        UiRole::Warning | UiRole::Error => (Color::LightYellow, None, Some(Modifier::BOLD)),
        UiRole::Code => (Color::LightYellow, Some(Color::Black), None),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parser_accepts_the_four_public_theme_names() {
        assert_eq!("default".parse::<Theme>(), Ok(Theme::Default));
        assert_eq!("dim".parse::<Theme>(), Ok(Theme::Dim));
        assert_eq!("high-contrast".parse::<Theme>(), Ok(Theme::HighContrast));
        assert_eq!("colorblind".parse::<Theme>(), Ok(Theme::Colorblind));
        assert!("brand-theme".parse::<Theme>().is_err());
    }

    #[test]
    fn no_color_keeps_non_color_state_cues() {
        let resolver = ThemeResolver::new(Theme::Default, ColorSupport::NoColor);
        assert!(resolver
            .ui(UiRole::TabActive)
            .add_modifier
            .contains(Modifier::REVERSED));
        assert!(resolver
            .ui(UiRole::Error)
            .add_modifier
            .contains(Modifier::UNDERLINED));
        assert!(resolver
            .text(TextStyle::new(SemanticRole::Link))
            .add_modifier
            .contains(Modifier::UNDERLINED));
    }

    #[test]
    fn colorblind_success_and_error_do_not_use_red_or_green() {
        let resolver = ThemeResolver::new(Theme::Colorblind, ColorSupport::Color);
        let success = resolver.ui(UiRole::Success).fg;
        let error = resolver.ui(UiRole::Error).fg;
        assert!(!matches!(success, Some(Color::Red | Color::Green)));
        assert!(!matches!(error, Some(Color::Red | Color::Green)));
    }
}
