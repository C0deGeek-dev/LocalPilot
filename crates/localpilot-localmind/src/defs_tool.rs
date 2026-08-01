//! `search_definitions`: find declarations, not lines.
//!
//! `search_text` answers "where does this string appear"; this answers "which
//! function, type, or module is this string in". A hit is one declaration with
//! its symbol path, signature, and location — enough to decide whether to read
//! the file, instead of a line fragment that always needs a follow-up read.
//!
//! Two properties keep it honest. It is **stateless**: no index, no cache, no
//! store — a call parses what it needs and forgets it, so results are never
//! stale. And it is **text-first**: the cheap literal/regex scan runs before any
//! parsing, and only files that already contain a match are parsed. Cost then
//! scales with the number of hits rather than the size of the repository, which
//! matters because parsing runs at roughly 2 MB/s in a debug build — parsing an
//! entire tree per search would be seconds of work to answer "not found".

use std::collections::BTreeMap;
use std::path::Path;

use async_trait::async_trait;
use localpilot_sandbox::{is_secret_like, Effect};
use localpilot_tools::{Tool, ToolContext, ToolError, ToolOutput};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::defs::{enclosing, DefKind, Definition, DefinitionParser, ParseOutcome};

/// Default hit ceiling when the caller does not ask for one. Deliberately small:
/// a definition-scoped hit is far denser than a line, so twenty of them is
/// already a substantial read.
const DEFAULT_MAX_HITS: usize = 20;
/// Hard ceiling a caller cannot exceed, so one call cannot flood the context.
const MAX_HITS: usize = 100;
/// Files scanned per call. A search that would sweep more than this stops and
/// says so, rather than spending an unbounded amount of time.
const MAX_FILES_SCANNED: usize = 20_000;

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchDefinitionsInput {
    /// Text or regular expression to look for.
    query: String,
    /// Directory to search, relative to the workspace. Defaults to the root.
    #[serde(default)]
    path: Option<String>,
    /// Treat `query` as a regular expression. Defaults to false (literal).
    #[serde(default)]
    is_regex: bool,
    /// Only search this language, e.g. `rust`, `python`, `typescript`.
    #[serde(default)]
    language: Option<String>,
    /// Only return this kind of declaration: `module`, `type`, `function`, or
    /// `test`.
    #[serde(default)]
    kind: Option<String>,
    /// Maximum number of declarations to return (default 20, capped at 100).
    #[serde(default)]
    max_hits: Option<usize>,
}

/// One declaration that matched, with how many times it matched inside.
#[derive(Clone, Debug)]
pub(crate) struct Hit {
    pub(crate) file: String,
    pub(crate) definition: Definition,
    pub(crate) matches: usize,
    /// The match landed on the declaration's own name or signature line, rather
    /// than somewhere in its body.
    pub(crate) on_name: bool,
}

/// Everything one search produced, before rendering.
#[derive(Clone, Debug, Default)]
pub(crate) struct SearchOutcome {
    pub(crate) hits: Vec<Hit>,
    /// Files that matched the text but could not be parsed, with the reason,
    /// deduplicated so one unsupported extension does not produce a hundred
    /// lines. Reported so a caller never reads an empty result as "not found".
    pub(crate) skipped: BTreeMap<String, usize>,
    /// Text matches that landed outside every declaration (file-level code,
    /// imports, comments between items).
    pub(crate) top_level: usize,
    /// The file-scan ceiling was reached and the walk stopped early.
    pub(crate) scan_truncated: bool,
}

/// Searches declarations across the workspace. Read-only, stateless.
pub struct SearchDefinitions;

#[async_trait]
impl Tool for SearchDefinitions {
    fn name(&self) -> &str {
        "search_definitions"
    }

    fn description(&self) -> &str {
        "Search workspace code and return the enclosing declarations — function, type, module, or \
         test — with their symbol path, signature, and location, respecting ignore files. Use it \
         for \"where is X defined\", \"which function handles Y\", or \"what implements Z\". Use \
         `search_text` instead for prose, configuration, non-code files, or when you want every \
         matching line; use `find_files` to locate files by name. Read-only."
    }

    fn schema(&self) -> Value {
        serde_json::to_value(schemars::schema_for!(SearchDefinitionsInput)).unwrap_or(Value::Null)
    }

    fn approval_detail(&self, input: &Value) -> String {
        input
            .get("query")
            .and_then(Value::as_str)
            .unwrap_or_default()
            .chars()
            .take(160)
            .collect()
    }

    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: SearchDefinitionsInput = serde_json::from_value(input.clone())
            .map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let dir = input.path.unwrap_or_else(|| ".".to_string());
        let path = Path::new(&dir);
        Ok(vec![Effect::ReadPath {
            inside_workspace: ctx.workspace.read_scoped(path),
            secret_like: is_secret_like(path),
        }])
    }

    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: SearchDefinitionsInput =
            serde_json::from_value(input).map_err(|e| ToolError::InvalidInput(e.to_string()))?;
        let query = Query::build(&input)?;
        let dir = ctx
            .workspace
            .normalize(Path::new(input.path.as_deref().unwrap_or(".")))?;
        let root = ctx.workspace.root().to_path_buf();

        let outcome = search(&dir, &root, &query);
        Ok(render(&outcome, &query))
    }
}

/// A validated search request: the pattern plus the filters, resolved once.
#[derive(Debug)]
pub(crate) struct Query {
    pub(crate) needle: String,
    pub(crate) regex: Option<regex::Regex>,
    pub(crate) language: Option<String>,
    pub(crate) kind: Option<DefKind>,
    pub(crate) limit: usize,
}

impl Query {
    fn build(input: &SearchDefinitionsInput) -> Result<Self, ToolError> {
        let needle = input.query.trim();
        if needle.is_empty() {
            return Err(ToolError::InvalidInput(
                "query must not be empty".to_string(),
            ));
        }
        let regex = if input.is_regex {
            Some(regex::Regex::new(needle).map_err(|e| ToolError::InvalidInput(e.to_string()))?)
        } else {
            None
        };
        let kind = match input.kind.as_deref() {
            None => None,
            Some(text) => Some(DefKind::parse(text).ok_or_else(|| {
                ToolError::InvalidInput(format!(
                    "unknown kind {text:?}; supported kinds are {}",
                    DefKind::ALL
                        .iter()
                        .map(|k| k.as_str())
                        .collect::<Vec<_>>()
                        .join(", ")
                ))
            })?),
        };
        let language = match input.language.as_deref() {
            None => None,
            Some(text) => {
                let wanted = text.trim().to_ascii_lowercase();
                let known = localmind_codegraph::Language::ALL
                    .iter()
                    .any(|l| l.as_str() == wanted);
                if !known {
                    return Err(ToolError::InvalidInput(format!(
                        "unknown language {text:?}; supported languages are {}",
                        localmind_codegraph::Language::ALL
                            .iter()
                            .map(|l| l.as_str())
                            .collect::<Vec<_>>()
                            .join(", ")
                    )));
                }
                Some(wanted)
            }
        };
        Ok(Self {
            needle: needle.to_string(),
            regex,
            language,
            kind,
            limit: input
                .max_hits
                .unwrap_or(DEFAULT_MAX_HITS)
                .clamp(1, MAX_HITS),
        })
    }

    /// Byte offsets of every match in `text`.
    fn match_offsets(&self, text: &str) -> Vec<usize> {
        match &self.regex {
            Some(regex) => regex.find_iter(text).map(|m| m.start()).collect(),
            None => text.match_indices(&self.needle).map(|(at, _)| at).collect(),
        }
    }

    fn wants(&self, definition: &Definition) -> bool {
        self.kind.is_none_or(|kind| definition.kind == kind)
    }
}

/// Walks `dir`, matching text first and parsing only the files that matched.
pub(crate) fn search(dir: &Path, root: &Path, query: &Query) -> SearchOutcome {
    let mut outcome = SearchOutcome::default();
    let mut parser = match DefinitionParser::new() {
        Ok(parser) => parser,
        Err(reason) => {
            outcome
                .skipped
                .insert(format!("parser unavailable: {reason}"), 1);
            return outcome;
        }
    };
    let mut scanned = 0usize;
    // Collected unbounded, then ranked and cut in `render`: cutting during the
    // walk would make the result depend on filesystem order rather than on
    // relevance.
    let mut hits: Vec<Hit> = Vec::new();

    for entry in ignore::WalkBuilder::new(dir)
        .hidden(true)
        .require_git(false)
        .build()
        .flatten()
    {
        if !entry.file_type().is_some_and(|t| t.is_file()) {
            continue;
        }
        if scanned >= MAX_FILES_SCANNED {
            outcome.scan_truncated = true;
            break;
        }
        scanned += 1;

        let path = entry.path();
        if let Some(wanted) = &query.language {
            match DefinitionParser::language_for(path) {
                Some(language) if language.as_str() == wanted => {}
                _ => continue,
            }
        }
        let Ok(text) = std::fs::read_to_string(path) else {
            continue; // binary or unreadable: not a search result, not an error
        };
        let offsets = query.match_offsets(&text);
        if offsets.is_empty() {
            continue; // the whole point: no match, no parse
        }

        let relative = path.strip_prefix(root).unwrap_or(path);
        let display = relative.display().to_string().replace('\\', "/");
        let parsed = parser.definitions(relative, &text);
        let definitions = match &parsed {
            ParseOutcome::Parsed(definitions) => definitions,
            other => {
                if let Some(reason) = other.skip_reason() {
                    *outcome.skipped.entry(reason).or_insert(0) += 1;
                }
                continue;
            }
        };

        // Group this file's matches by the declaration that encloses them, so a
        // helper mentioned twelve times is one hit with a count — not twelve
        // hits crowding out twelve other declarations.
        let mut by_definition: BTreeMap<usize, (usize, bool)> = BTreeMap::new();
        for offset in offsets {
            let Some(found) = enclosing(definitions, offset) else {
                outcome.top_level += 1;
                continue;
            };
            let Some(index) = definitions.iter().position(|d| d == found) else {
                continue;
            };
            let entry = by_definition.entry(index).or_insert((0, false));
            entry.0 += 1;
            if is_on_name(found, &text, offset) {
                entry.1 = true;
            }
        }

        for (index, (matches, on_name)) in by_definition {
            let definition = definitions[index].clone();
            if !query.wants(&definition) {
                continue;
            }
            hits.push(Hit {
                file: display.clone(),
                definition,
                matches,
                on_name,
            });
        }
    }

    outcome.hits = hits;
    outcome
}

/// Did the match land on the declaration's own name or signature line, rather
/// than deeper in its body? A hit on the name is what the caller usually wants
/// first, so this drives ranking in [`render`].
fn is_on_name(definition: &Definition, text: &str, offset: usize) -> bool {
    let header_end = text[definition.byte_start..definition.byte_end.min(text.len())]
        .find('\n')
        .map_or(definition.byte_end, |at| definition.byte_start + at);
    offset <= header_end
}

/// Ranks, cuts to the limit, and renders. Ordering is a total order so the same
/// inputs always produce the same bytes, on every platform.
pub(crate) fn render(outcome: &SearchOutcome, query: &Query) -> ToolOutput {
    let mut hits = outcome.hits.clone();
    hits.sort_by(|a, b| {
        b.on_name
            .cmp(&a.on_name)
            .then_with(|| depth(&a.definition.symbol_path).cmp(&depth(&b.definition.symbol_path)))
            .then_with(|| b.matches.cmp(&a.matches))
            .then_with(|| a.file.cmp(&b.file))
            .then_with(|| a.definition.byte_start.cmp(&b.definition.byte_start))
    });
    let total = hits.len();
    let dropped = total.saturating_sub(query.limit);
    hits.truncate(query.limit);

    let mut lines = Vec::new();
    for hit in &hits {
        let definition = &hit.definition;
        let mut header = format!(
            "{} {} — {}:{}",
            definition.kind.as_str(),
            definition.symbol_path,
            hit.file,
            definition.line_start
        );
        if hit.matches > 1 {
            header.push_str(&format!(" ({} matches)", hit.matches));
        }
        lines.push(header);
        if let Some(signature) = &definition.signature {
            lines.push(format!("    {}", signature.trim()));
        }
    }

    if lines.is_empty() {
        lines.push(format!("No declaration contains {:?}.", query.needle));
    }

    let mut notes = Vec::new();
    if dropped > 0 {
        notes.push(format!(
            "{dropped} more declaration(s) matched; narrow the query, or raise max_hits (cap {MAX_HITS})"
        ));
    }
    if outcome.top_level > 0 {
        notes.push(format!(
            "{} match(es) were outside any declaration (imports, file-level code, comments) — use search_text to see them",
            outcome.top_level
        ));
    }
    for (reason, count) in &outcome.skipped {
        notes.push(format!("{count} matching file(s) not parsed: {reason}"));
    }
    if outcome.scan_truncated {
        notes.push(format!(
            "stopped after scanning {MAX_FILES_SCANNED} files; search a subdirectory with `path`"
        ));
    }
    if !notes.is_empty() {
        lines.push(String::new());
        for note in &notes {
            lines.push(format!("note: {note}"));
        }
    }

    let text = lines.join("\n");
    if dropped > 0 || outcome.scan_truncated {
        ToolOutput::truncated(text)
    } else {
        ToolOutput::ok(text)
    }
}

/// Nesting depth of a symbol path. Shallower declarations rank first: a
/// top-level item is usually the answer, a deeply nested helper usually is not.
fn depth(symbol_path: &str) -> usize {
    symbol_path.matches("::").count() + symbol_path.matches('.').count()
}

/// Shared test constructors, so the render tests build the same `Query` the
/// contract tests do.
#[cfg(test)]
pub(crate) mod tests_support {
    use super::{Query, DEFAULT_MAX_HITS};

    /// A literal (non-regex) query with no filters and the default ceiling.
    pub(crate) fn plain(needle: &str) -> Query {
        Query {
            needle: needle.to_string(),
            regex: None,
            language: None,
            kind: None,
            limit: DEFAULT_MAX_HITS,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::tests_support::plain as query;
    use super::*;
    use std::fs;

    fn tree() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        let src = dir.path().join("src");
        fs::create_dir_all(&src).expect("src dir");
        fs::write(
            src.join("geometry.rs"),
            "pub struct Point { x: f64 }\n\
             impl Point {\n\
             \x20   pub fn norm(&self) -> f64 { self.x }\n\
             \x20   pub fn scaled(&self) -> f64 { self.norm() * 2.0 }\n\
             }\n",
        )
        .expect("write rust");
        fs::write(
            src.join("app.py"),
            "def norm(v):\n    return v\n\ndef other():\n    return norm(1)\n",
        )
        .expect("write python");
        fs::write(dir.path().join("notes.txt"), "norm appears here too\n").expect("write text");
        dir
    }

    fn search_tree(dir: &tempfile::TempDir, query: &Query) -> SearchOutcome {
        search(dir.path(), dir.path(), query)
    }

    #[test]
    fn a_hit_is_a_declaration_not_a_line() {
        let dir = tree();
        let outcome = search_tree(&dir, &query("norm"));
        assert!(
            outcome.hits.iter().any(|h| h.definition.name == "norm"),
            "expected the declaration itself: {:?}",
            outcome.hits
        );
        assert!(
            outcome
                .hits
                .iter()
                .all(|h| h.definition.byte_end > h.definition.byte_start),
            "every hit carries a real span"
        );
    }

    #[test]
    fn matches_inside_one_declaration_collapse_to_one_hit() {
        let dir = tree();
        let outcome = search_tree(&dir, &query("self"));
        for hit in &outcome.hits {
            assert!(hit.matches >= 1);
        }
        let norm_hits = outcome
            .hits
            .iter()
            .filter(|h| h.definition.name == "norm")
            .count();
        assert!(norm_hits <= 1, "one declaration must not produce two hits");
    }

    #[test]
    fn a_file_with_no_grammar_is_reported_not_silently_dropped() {
        let dir = tree();
        let outcome = search_tree(&dir, &query("norm"));
        assert!(
            outcome.skipped.keys().any(|r| r.contains("no grammar")),
            "the .txt match must be reported, got {:?}",
            outcome.skipped
        );
    }

    #[test]
    fn the_language_filter_narrows_the_walk() {
        let dir = tree();
        let mut q = query("norm");
        q.language = Some("python".to_string());
        let outcome = search_tree(&dir, &q);
        assert!(!outcome.hits.is_empty(), "python has a match");
        assert!(
            outcome.hits.iter().all(|h| h.file.ends_with(".py")),
            "only python files: {:?}",
            outcome.hits.iter().map(|h| &h.file).collect::<Vec<_>>()
        );
    }

    #[test]
    fn the_kind_filter_narrows_the_hits() {
        let dir = tree();
        let mut q = query("Point");
        q.kind = Some(DefKind::Type);
        let outcome = search_tree(&dir, &q);
        assert!(
            outcome
                .hits
                .iter()
                .all(|h| h.definition.kind == DefKind::Type),
            "only types: {:?}",
            outcome.hits
        );
    }

    #[test]
    fn an_unknown_kind_is_an_argument_error_not_an_empty_result() {
        let input = SearchDefinitionsInput {
            query: "x".to_string(),
            path: None,
            is_regex: false,
            language: None,
            kind: Some("banana".to_string()),
            max_hits: None,
        };
        let error = Query::build(&input).expect_err("unknown kind must be refused");
        let message = error.to_string();
        assert!(message.contains("banana"), "names the bad value: {message}");
        assert!(
            message.contains("function"),
            "lists what is valid: {message}"
        );
    }

    #[test]
    fn an_unknown_language_is_an_argument_error() {
        let input = SearchDefinitionsInput {
            query: "x".to_string(),
            path: None,
            is_regex: false,
            language: Some("cobol".to_string()),
            kind: None,
            max_hits: None,
        };
        let error = Query::build(&input).expect_err("unknown language must be refused");
        assert!(error.to_string().contains("cobol"));
    }

    #[test]
    fn an_empty_query_is_refused() {
        let input = SearchDefinitionsInput {
            query: "   ".to_string(),
            path: None,
            is_regex: false,
            language: None,
            kind: None,
            max_hits: None,
        };
        assert!(Query::build(&input).is_err());
    }

    #[test]
    fn a_bad_regex_is_an_argument_error() {
        let input = SearchDefinitionsInput {
            query: "(".to_string(),
            path: None,
            is_regex: true,
            language: None,
            kind: None,
            max_hits: None,
        };
        assert!(Query::build(&input).is_err());
    }

    #[test]
    fn the_hit_ceiling_is_signalled_with_a_count() {
        let dir = tree();
        let mut q = query("norm");
        q.limit = 1;
        let outcome = search_tree(&dir, &q);
        let rendered = render(&outcome, &q);
        assert!(rendered.truncated, "a cut result must be marked truncated");
        assert!(
            rendered.text.contains("more declaration"),
            "the caller must be told how many were dropped: {}",
            rendered.text
        );
    }

    #[test]
    fn nothing_found_says_so_rather_than_returning_empty() {
        let dir = tree();
        let q = query("zzz_no_such_symbol");
        let outcome = search_tree(&dir, &q);
        let rendered = render(&outcome, &q);
        assert!(!rendered.truncated);
        assert!(
            rendered.text.contains("No declaration"),
            "got {:?}",
            rendered.text
        );
    }

    #[test]
    fn output_is_byte_identical_across_runs() {
        let dir = tree();
        let q = query("norm");
        let first = render(&search_tree(&dir, &q), &q).text;
        let second = render(&search_tree(&dir, &q), &q).text;
        assert_eq!(
            first, second,
            "identical inputs must produce identical bytes"
        );
    }

    #[test]
    fn a_declaration_match_outranks_a_body_match() {
        let dir = tree();
        let q = query("norm");
        let outcome = search_tree(&dir, &q);
        let rendered = render(&outcome, &q).text;
        let declaration_line = rendered
            .lines()
            .position(|l| l.contains("norm") && !l.contains("scaled"));
        let body_line = rendered.lines().position(|l| l.contains("scaled"));
        if let (Some(declaration), Some(body)) = (declaration_line, body_line) {
            assert!(
                declaration < body,
                "the declaration named `norm` should rank above the one that only calls it:\n{rendered}"
            );
        }
    }

    #[test]
    fn regex_queries_match_the_same_way_search_text_does() {
        let dir = tree();
        let mut q = query("n.rm");
        q.regex = Some(regex::Regex::new("n.rm").expect("valid"));
        let outcome = search_tree(&dir, &q);
        assert!(!outcome.hits.is_empty(), "regex must find the declarations");
    }
}

#[cfg(test)]
mod render_tests {
    use super::tests_support::*;
    use super::*;
    use std::fs;

    /// A fixture with one decorated Python function, one plain function that
    /// only *calls* it, and a Rust type — enough to pin ranking, the match
    /// count, the signature line, and the notes block in one string.
    fn fixture() -> tempfile::TempDir {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            dir.path().join("service.py"),
            "@cached\ndef resolve(name):\n    return name\n\ndef caller():\n    return resolve(resolve(\"x\"))\n",
        )
        .expect("write python");
        fs::write(dir.path().join("readme.md"), "resolve is documented here\n")
            .expect("write markdown");
        dir
    }

    #[test]
    fn rendered_output_is_pinned() {
        let dir = fixture();
        let q = plain("resolve");
        let rendered = render(&search(dir.path(), dir.path(), &q), &q);
        assert_eq!(
            rendered.text,
            concat!(
                "function service.py::resolve — service.py:2\n",
                "    def resolve(name):\n",
                "function service.py::caller — service.py:5 (2 matches)\n",
                "    def caller():\n",
                "\n",
                "note: 1 matching file(s) not parsed: no grammar for this file type",
            ),
            "rendered output changed:\n{}",
            rendered.text
        );
        assert!(!rendered.truncated);
    }

    #[test]
    fn a_decorated_declaration_still_ranks_above_its_callers() {
        let dir = fixture();
        let q = plain("resolve");
        let rendered = render(&search(dir.path(), dir.path(), &q), &q).text;
        let declaration = rendered
            .lines()
            .position(|l| l.starts_with("function service.py::resolve"))
            .expect("the declaration is present");
        let caller = rendered
            .lines()
            .position(|l| l.starts_with("function service.py::caller"))
            .expect("the caller is present");
        assert!(
            declaration < caller,
            "a decorator above the signature must not demote the declaration:\n{rendered}"
        );
    }

    #[test]
    fn a_truncated_result_pins_its_note() {
        let dir = fixture();
        let mut q = plain("resolve");
        q.limit = 1;
        let rendered = render(&search(dir.path(), dir.path(), &q), &q);
        assert!(rendered.truncated);
        assert!(
            rendered.text.contains(
                "note: 1 more declaration(s) matched; narrow the query, or raise max_hits (cap 100)"
            ),
            "truncation note changed:\n{}",
            rendered.text
        );
    }

    #[test]
    fn matches_outside_a_declaration_point_at_the_other_tool() {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::write(
            dir.path().join("top.py"),
            "import resolve_helper\n\ndef unrelated():\n    return 1\n",
        )
        .expect("write python");
        let q = plain("resolve_helper");
        let rendered = render(&search(dir.path(), dir.path(), &q), &q).text;
        assert!(
            rendered.contains("outside any declaration") && rendered.contains("search_text"),
            "a top-level-only match must name the tool that can show it:\n{rendered}"
        );
    }
}

/// The safety tests the repository requires of every tool: an allow path, a deny
/// path, a containment test that refuses an escape outside the workspace, and a
/// malformed-input test. The tool never decides its own permission — it declares
/// effects and the engine rules on them — so the allow/deny pair asserts the
/// *declared effect*, which is the thing the engine acts on.
#[cfg(test)]
mod safety_tests {
    use super::tests_support::plain;
    use super::*;
    use localpilot_sandbox::{Interactivity, Workspace};
    use serde_json::json;
    use std::fs;

    fn context(workspace: &Workspace) -> ToolContext<'_> {
        ToolContext {
            workspace,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
        }
    }

    fn workspace() -> (tempfile::TempDir, Workspace) {
        let dir = tempfile::tempdir().expect("temp dir");
        fs::create_dir_all(dir.path().join("src")).expect("src");
        fs::write(
            dir.path().join("src/lib.rs"),
            "pub fn inside_workspace() -> u8 { 1 }\n",
        )
        .expect("write");
        let workspace = Workspace::new(dir.path()).expect("workspace");
        (dir, workspace)
    }

    #[tokio::test]
    async fn allow_path_a_workspace_search_declares_an_in_workspace_read() {
        let (_dir, workspace) = workspace();
        let ctx = context(&workspace);
        let effects = SearchDefinitions
            .effects(&json!({ "query": "inside_workspace", "path": "src" }), &ctx)
            .expect("effects resolve");
        assert_eq!(effects.len(), 1);
        assert!(
            matches!(
                effects[0],
                Effect::ReadPath {
                    inside_workspace: true,
                    ..
                }
            ),
            "a path under the root must declare an in-workspace read: {effects:?}"
        );

        let output = SearchDefinitions
            .invoke(json!({ "query": "inside_workspace" }), &ctx)
            .await
            .expect("the search runs");
        assert!(
            output.text.contains("inside_workspace"),
            "the declaration should be found: {}",
            output.text
        );
    }

    #[tokio::test]
    async fn deny_path_an_escape_declares_an_out_of_workspace_read() {
        let (_dir, workspace) = workspace();
        let ctx = context(&workspace);
        let effects = SearchDefinitions
            .effects(&json!({ "query": "x", "path": "../../elsewhere" }), &ctx)
            .expect("effects resolve");
        assert!(
            matches!(
                effects[0],
                Effect::ReadPath {
                    inside_workspace: false,
                    ..
                }
            ),
            "an escaping path must be declared out-of-workspace so the engine can refuse it: {effects:?}"
        );
    }

    #[test]
    fn containment_is_the_engine_s_decision_and_the_tool_reports_it_faithfully() {
        let (dir, workspace) = workspace();
        // `Workspace::normalize` deliberately does not enforce containment —
        // ADR-0070 lets the engine approve a read outside the root — so a tool
        // must not invent its own check, or it would diverge from `search_text`
        // and quietly refuse reads the user granted. What it must do is report
        // containment truthfully, which is what the engine rules on.
        assert!(workspace.contains(&dir.path().join("src/lib.rs")));
        assert!(!workspace.contains(Path::new("..")));

        let ctx = context(&workspace);
        for (path, expected) in [("src", true), ("..", false), ("../..", false)] {
            let effects = SearchDefinitions
                .effects(&json!({ "query": "x", "path": path }), &ctx)
                .expect("effects resolve");
            assert!(
                matches!(
                    effects[0],
                    Effect::ReadPath { inside_workspace, .. } if inside_workspace == expected
                ),
                "path {path:?} should report inside_workspace={expected}: {effects:?}"
            );
        }
    }

    #[tokio::test]
    async fn malformed_input_is_refused_not_coerced() {
        let (_dir, workspace) = workspace();
        let ctx = context(&workspace);

        // Wrong type for a typed field.
        assert!(SearchDefinitions
            .invoke(json!({ "query": 42 }), &ctx)
            .await
            .is_err());
        // Missing the required field.
        assert!(SearchDefinitions
            .invoke(json!({ "path": "src" }), &ctx)
            .await
            .is_err());
        // Not an object at all.
        assert!(SearchDefinitions
            .invoke(json!("query"), &ctx)
            .await
            .is_err());
    }

    #[test]
    fn the_tool_declares_itself_read_only_and_schema_generated() {
        let schema = SearchDefinitions.schema();
        assert!(
            schema.get("properties").is_some(),
            "the schema must be generated from the typed input, not hand-written"
        );
        // A read-only tool never declares a write or network effect.
        let (_dir, workspace) = workspace();
        let ctx = context(&workspace);
        let effects = SearchDefinitions
            .effects(&json!({ "query": "x" }), &ctx)
            .expect("effects resolve");
        assert!(
            effects.iter().all(|e| matches!(e, Effect::ReadPath { .. })),
            "read-only means read effects only: {effects:?}"
        );
    }

    #[tokio::test]
    async fn the_limit_is_clamped_to_the_hard_ceiling() {
        let (_dir, workspace) = workspace();
        let ctx = context(&workspace);
        let input = json!({ "query": "inside_workspace", "max_hits": 100_000 });
        let parsed: SearchDefinitionsInput = serde_json::from_value(input.clone()).expect("parses");
        let query = Query::build(&parsed).expect("builds");
        assert_eq!(
            query.limit, MAX_HITS,
            "a caller must not be able to exceed the hard ceiling"
        );
        assert!(SearchDefinitions.invoke(input, &ctx).await.is_ok());
    }

    #[test]
    fn a_zero_limit_is_clamped_up_rather_than_returning_nothing() {
        let parsed: SearchDefinitionsInput =
            serde_json::from_value(json!({ "query": "x", "max_hits": 0 })).expect("parses");
        let query = Query::build(&parsed).expect("builds");
        assert_eq!(query.limit, 1, "zero must not silently mean 'no results'");
    }

    #[test]
    fn unused_helper_is_referenced() {
        // Keeps the shared constructor honest: the safety tests use the same
        // `Query` shape the contract tests do.
        assert_eq!(plain("x").limit, DEFAULT_MAX_HITS);
    }
}
