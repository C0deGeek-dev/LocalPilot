//! Per-file declaration extraction.
//!
//! Turns one source file's text into the declarations it contains — kind, name,
//! symbol path, signature, and spans — by parsing it with the code-intelligence
//! provider. Nothing here opens a store, reads an index, or writes anything: a
//! call parses the text it is handed and returns. That statelessness is the
//! point. The indexed code graph answers questions about a *project* and pays
//! for it with an ingest step and a staleness surface; this answers questions
//! about a *file* and is always current because it has nothing to be stale.
//!
//! The host keeps ownership of walking the filesystem, honouring ignore rules,
//! and enforcing workspace scope. This module only ever sees text a caller has
//! already decided it may read.

use std::path::Path;

use localmind_codegraph::{
    AdmittedFile, CodeIntelligenceProvider, Language, NativeProvider, ParsedFile,
};
use localmind_core::{GraphNode, NodeKind};

/// Largest file this module will parse. Beyond it, parsing costs more than the
/// answer is worth and a single pathological file could stall a whole search;
/// callers get [`ParseOutcome::TooLarge`] and can fall back to a text search.
pub const MAX_PARSE_BYTES: usize = 1024 * 1024;

/// What kind of declaration a [`Definition`] is.
///
/// Deliberately narrower than the graph's node kinds: this module reports things
/// a reader would call a definition, so file, repository, and dependency nodes
/// are not represented at all.
#[derive(Clone, Copy, Debug, Eq, Hash, PartialEq)]
pub enum DefKind {
    /// A module, namespace, or other named container of declarations.
    Module,
    /// A type: struct, class, enum, interface, trait, record.
    Type,
    /// A function, method, or procedure.
    Function,
    /// A test function.
    Test,
}

impl DefKind {
    /// A stable lowercase identifier, used in output and as the filter spelling.
    #[must_use]
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Module => "module",
            Self::Type => "type",
            Self::Function => "function",
            Self::Test => "test",
        }
    }

    /// Every kind, in the order they are listed to a caller.
    pub const ALL: &'static [DefKind] = &[
        DefKind::Module,
        DefKind::Type,
        DefKind::Function,
        DefKind::Test,
    ];

    /// Parses a filter spelling, accepting the plural a caller may reasonably
    /// write (`functions` for `function`).
    #[must_use]
    pub fn parse(text: &str) -> Option<Self> {
        let normalized = text.trim().to_ascii_lowercase();
        let singular = normalized.strip_suffix('s').unwrap_or(&normalized);
        Self::ALL.iter().copied().find(|k| k.as_str() == singular)
    }

    /// The graph node kinds that are declarations; anything else is structural.
    fn from_node(kind: NodeKind) -> Option<Self> {
        match kind {
            NodeKind::Module => Some(Self::Module),
            NodeKind::Type => Some(Self::Type),
            NodeKind::Function => Some(Self::Function),
            NodeKind::Test => Some(Self::Test),
            NodeKind::Repository | NodeKind::File | NodeKind::Dependency => None,
        }
    }
}

/// One declaration found in a file.
#[derive(Clone, Debug, Eq, PartialEq)]
pub struct Definition {
    pub kind: DefKind,
    /// Unqualified name, as written (`norm`).
    pub name: String,
    /// Path-qualified name (`geometry::Point::norm`); equals `name` when there is
    /// no enclosing scope.
    pub symbol_path: String,
    /// The declaration with its body elided, when the language's extractor
    /// produces one. Absent is normal, not an error.
    pub signature: Option<String>,
    pub byte_start: usize,
    pub byte_end: usize,
    /// 1-based, inclusive.
    pub line_start: usize,
    /// 1-based, inclusive.
    pub line_end: usize,
}

impl Definition {
    /// Does this declaration's byte span contain `offset`?
    #[must_use]
    pub fn contains(&self, offset: usize) -> bool {
        offset >= self.byte_start && offset < self.byte_end
    }

    /// How many bytes this declaration spans. Used to pick the innermost of
    /// several containing declarations.
    #[must_use]
    fn width(&self) -> usize {
        self.byte_end.saturating_sub(self.byte_start)
    }

    fn from_node(node: &GraphNode) -> Option<Self> {
        let kind = DefKind::from_node(node.kind)?;
        let location = node.location.as_ref()?;
        Some(Self {
            kind,
            name: node.name.clone(),
            symbol_path: node.qualified_name.clone(),
            signature: node.skeleton.clone(),
            byte_start: usize::try_from(location.byte_start).unwrap_or(usize::MAX),
            byte_end: usize::try_from(location.byte_end).unwrap_or(usize::MAX),
            line_start: usize::try_from(location.line_start).unwrap_or(usize::MAX),
            line_end: usize::try_from(location.line_end).unwrap_or(usize::MAX),
        })
    }
}

/// Why a file yielded no declarations. Every case is reported rather than
/// returning an empty list, so a caller can tell "nothing matched" from "this
/// file was never parsed" — a silent empty result is how a search tool grows a
/// blind spot.
#[derive(Clone, Debug, Eq, PartialEq)]
pub enum ParseOutcome {
    /// The file parsed; `Vec` may still be empty if it declares nothing.
    Parsed(Vec<Definition>),
    /// No grammar claims this file's extension.
    UnsupportedLanguage,
    /// Larger than [`MAX_PARSE_BYTES`].
    TooLarge { bytes: usize },
    /// The parser rejected the text.
    ParseFailed { reason: String },
}

impl ParseOutcome {
    /// The declarations, or an empty slice for any non-parsed outcome.
    #[must_use]
    pub fn definitions(&self) -> &[Definition] {
        match self {
            Self::Parsed(defs) => defs,
            _ => &[],
        }
    }

    /// A short reason a caller can show, or `None` when the file parsed.
    #[must_use]
    pub fn skip_reason(&self) -> Option<String> {
        match self {
            Self::Parsed(_) => None,
            Self::UnsupportedLanguage => Some("no grammar for this file type".to_string()),
            Self::TooLarge { bytes } => Some(format!("file is {bytes} bytes; too large to parse")),
            Self::ParseFailed { reason } => Some(format!("parse failed: {reason}")),
        }
    }
}

/// Parses files into declarations. Holds one provider so a caller that parses
/// many files pays the grammar setup cost once.
pub struct DefinitionParser {
    provider: NativeProvider,
}

impl DefinitionParser {
    /// Builds a parser.
    ///
    /// # Errors
    /// Returns a message when the underlying provider cannot be built (a grammar
    /// failed to load).
    pub fn new() -> Result<Self, String> {
        NativeProvider::new()
            .map(|provider| Self { provider })
            .map_err(|error| error.to_string())
    }

    /// Is there a grammar for this path's extension?
    ///
    /// Extension-based, deliberately: a `.txt` file containing Rust is not Rust,
    /// and guessing from content would make results depend on a heuristic the
    /// caller cannot see.
    #[must_use]
    pub fn language_for(path: &Path) -> Option<Language> {
        let extension = path.extension()?.to_str()?.to_ascii_lowercase();
        Language::ALL
            .iter()
            .copied()
            .find(|language| language.extensions().contains(&extension.as_str()))
    }

    /// Extracts the declarations in `text`, which is the content of `relative`.
    ///
    /// `relative` is used for language selection and for the symbol paths the
    /// extractor builds; nothing is read from disk here.
    pub fn definitions(&mut self, relative: &Path, text: &str) -> ParseOutcome {
        if Self::language_for(relative).is_none() {
            return ParseOutcome::UnsupportedLanguage;
        }
        if text.len() > MAX_PARSE_BYTES {
            return ParseOutcome::TooLarge { bytes: text.len() };
        }
        let file = AdmittedFile {
            absolute: relative.to_path_buf(),
            relative: relative.to_string_lossy().replace('\\', "/"),
        };
        match self.provider.parse_file(&file, text) {
            Ok(parsed) => ParseOutcome::Parsed(collect(&parsed)),
            Err(error) => ParseOutcome::ParseFailed {
                reason: error.to_string(),
            },
        }
    }
}

/// The innermost declaration whose span contains `offset`.
///
/// Nested declarations overlap, so "innermost" is the narrowest containing span
/// rather than the first match.
#[must_use]
pub fn enclosing(definitions: &[Definition], offset: usize) -> Option<&Definition> {
    definitions
        .iter()
        .filter(|definition| definition.contains(offset))
        .min_by_key(|definition| definition.width())
}

fn collect(parsed: &ParsedFile) -> Vec<Definition> {
    let mut definitions: Vec<Definition> = parsed
        .items
        .iter()
        .filter_map(Definition::from_node)
        .collect();
    definitions.sort_by(|a, b| {
        a.byte_start
            .cmp(&b.byte_start)
            .then_with(|| b.byte_end.cmp(&a.byte_end))
            .then_with(|| a.symbol_path.cmp(&b.symbol_path))
    });
    definitions
}

#[cfg(test)]
mod tests {
    use super::*;

    const RUST: &str = "pub struct Point { x: f64 }\n\
                        impl Point {\n\
                        \x20   pub fn norm(&self) -> f64 { self.x }\n\
                        }\n";

    fn parse(name: &str, text: &str) -> ParseOutcome {
        let mut parser = DefinitionParser::new().expect("provider builds");
        parser.definitions(Path::new(name), text)
    }

    #[test]
    fn every_known_extension_selects_a_language() {
        for language in Language::ALL {
            for extension in language.extensions() {
                let path = format!("sample.{extension}");
                assert!(
                    DefinitionParser::language_for(Path::new(&path)).is_some(),
                    "no language for .{extension}"
                );
            }
        }
    }

    #[test]
    fn an_unknown_extension_is_reported_not_guessed() {
        assert_eq!(
            parse("notes.txt", RUST),
            ParseOutcome::UnsupportedLanguage,
            "a .txt file holding Rust must not be parsed as Rust"
        );
        assert_eq!(
            parse("noextension", RUST),
            ParseOutcome::UnsupportedLanguage
        );
    }

    #[test]
    fn rust_declarations_carry_kind_name_and_spans() {
        let outcome = parse("src/geometry.rs", RUST);
        let definitions = outcome.definitions();
        assert!(!definitions.is_empty(), "expected declarations, got none");

        let point = definitions
            .iter()
            .find(|d| d.name == "Point" && d.kind == DefKind::Type)
            .expect("the struct is reported as a type");
        assert!(point.byte_end > point.byte_start);
        assert!(point.line_start >= 1);

        let norm = definitions
            .iter()
            .find(|d| d.name == "norm")
            .expect("the method is reported");
        assert_eq!(norm.kind, DefKind::Function);
        assert!(
            norm.symbol_path.contains("norm"),
            "symbol path {} should name the method",
            norm.symbol_path
        );
    }

    #[test]
    fn python_and_go_and_typescript_parse() {
        let python = parse(
            "app.py",
            "class Shape:\n    def area(self):\n        return 1\n",
        );
        assert!(
            python.definitions().iter().any(|d| d.name == "area"),
            "python method missing: {:?}",
            python.definitions()
        );

        let go = parse(
            "main.go",
            "package main\n\nfunc Answer() int { return 42 }\n",
        );
        assert!(
            go.definitions().iter().any(|d| d.name == "Answer"),
            "go function missing: {:?}",
            go.definitions()
        );

        let ts = parse(
            "app.ts",
            "export class Widget {\n  render(): string { return \"\"; }\n}\n",
        );
        assert!(
            ts.definitions().iter().any(|d| d.name == "Widget"),
            "typescript class missing: {:?}",
            ts.definitions()
        );
    }

    #[test]
    fn nesting_is_visible_in_the_symbol_path() {
        let outcome = parse("src/geometry.rs", RUST);
        let norm = outcome
            .definitions()
            .iter()
            .find(|d| d.name == "norm")
            .expect("the method is reported")
            .clone();
        assert!(
            norm.symbol_path.len() >= norm.name.len(),
            "a nested item's symbol path should be at least its own name"
        );
    }

    #[test]
    fn enclosing_picks_the_innermost_declaration() {
        let outcome = parse("src/geometry.rs", RUST);
        let definitions = outcome.definitions();
        let norm = definitions
            .iter()
            .find(|d| d.name == "norm")
            .expect("the method is reported");
        let inside = norm.byte_start + 1;

        let found = enclosing(definitions, inside).expect("an offset inside a method is enclosed");
        assert_eq!(
            found.name, "norm",
            "expected the method, not the enclosing impl or type"
        );
    }

    #[test]
    fn enclosing_is_none_outside_every_declaration() {
        let definitions = vec![Definition {
            kind: DefKind::Function,
            name: "f".to_string(),
            symbol_path: "f".to_string(),
            signature: None,
            byte_start: 10,
            byte_end: 20,
            line_start: 2,
            line_end: 3,
        }];
        assert!(enclosing(&definitions, 5).is_none(), "before the span");
        assert!(enclosing(&definitions, 20).is_none(), "end is exclusive");
        assert!(enclosing(&definitions, 10).is_some(), "start is inclusive");
    }

    #[test]
    fn an_oversized_file_is_reported_not_parsed() {
        let huge = "// pad\n".repeat(MAX_PARSE_BYTES / 7 + 2);
        let outcome = parse("src/huge.rs", &huge);
        assert!(
            matches!(outcome, ParseOutcome::TooLarge { .. }),
            "expected TooLarge, got {outcome:?}"
        );
        assert!(outcome.skip_reason().is_some());
    }

    #[test]
    fn a_skipped_file_always_explains_itself() {
        assert!(ParseOutcome::UnsupportedLanguage.skip_reason().is_some());
        assert!(ParseOutcome::TooLarge { bytes: 1 }.skip_reason().is_some());
        assert!(ParseOutcome::ParseFailed {
            reason: "bad".to_string()
        }
        .skip_reason()
        .is_some());
        assert!(ParseOutcome::Parsed(Vec::new()).skip_reason().is_none());
    }

    #[test]
    fn kind_filters_accept_singular_and_plural() {
        assert_eq!(DefKind::parse("function"), Some(DefKind::Function));
        assert_eq!(DefKind::parse("Functions"), Some(DefKind::Function));
        assert_eq!(DefKind::parse(" type "), Some(DefKind::Type));
        assert_eq!(DefKind::parse("banana"), None);
    }

    #[test]
    fn declarations_come_back_in_source_order() {
        let outcome = parse("src/geometry.rs", RUST);
        let definitions = outcome.definitions();
        let starts: Vec<usize> = definitions.iter().map(|d| d.byte_start).collect();
        let mut sorted = starts.clone();
        sorted.sort_unstable();
        assert_eq!(starts, sorted, "definitions must be in source order");
    }
}
