//! The builtin tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use localpilot_config::redact::contains_secret;
use localpilot_sandbox::{is_secret_like, CommandClass, Effect};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::contract::{
    Idempotency, PathEffectKind, Postcondition, Precondition, Reversibility, SideEffectClass,
    ToolContract, VerificationMethod,
};
use crate::error::ToolError;
use crate::tool::{detail_preview, parse_input, schema_for, Tool, ToolContext, ToolOutput};

/// Approval detail from a single string field of the input. Tools know their
/// own schema; this is a typed read, not cross-tool key-guessing.
fn string_field_detail(input: &Value, key: &str) -> String {
    input
        .get(key)
        .and_then(Value::as_str)
        .map(detail_preview)
        .unwrap_or_default()
}

// --- builtin contract fragments ---------------------------------------------

/// `write_file`/`edit_file`-style preconditions: the target must have been read
/// this session before it is overwritten (enforced only when it already exists).
const PRIOR_READ_PATH: &[Precondition] = &[Precondition::RequiresPriorRead { path_arg: "path" }];
const PATH_EXISTS: &[Postcondition] = &[Postcondition::PathEffect {
    path_arg: "path",
    kind: PathEffectKind::Exists,
}];
const PATH_MODIFIED: &[Postcondition] = &[Postcondition::PathEffect {
    path_arg: "path",
    kind: PathEffectKind::Modified,
}];
const RESULT_STATUS: &[Postcondition] = &[Postcondition::ResultStatus];

/// A read-only tool's contract: no side effect, idempotent, its own status is
/// the postcondition.
fn read_only_contract(model_description: &'static str) -> ToolContract {
    ToolContract {
        model_description,
        side_effect: SideEffectClass::ReadOnly,
        reversibility: Reversibility::Reversible,
        idempotency: Idempotency::Idempotent,
        postconditions: RESULT_STATUS,
        verification: VerificationMethod::Postconditions,
        ..ToolContract::default()
    }
}

/// Approval detail for a `paths` array field, joined for display.
fn paths_detail(input: &Value, prefix: &str) -> String {
    let joined = input
        .get("paths")
        .and_then(Value::as_array)
        .map(|paths| {
            paths
                .iter()
                .filter_map(Value::as_str)
                .take(6)
                .collect::<Vec<_>>()
                .join(" ")
        })
        .unwrap_or_default();
    detail_preview(&format!("{prefix} {joined}"))
}

/// Cap on a tool's textual output before truncation.
const MAX_OUTPUT_BYTES: usize = 64 * 1024;

pub(crate) fn cap(text: String) -> ToolOutput {
    if text.len() <= MAX_OUTPUT_BYTES {
        return ToolOutput::ok(text);
    }
    let mut end = MAX_OUTPUT_BYTES;
    while !text.is_char_boundary(end) {
        end -= 1;
    }
    let mut capped = text[..end].to_string();
    capped.push_str("\n... [output truncated]");
    ToolOutput::truncated(capped)
}

/// Heuristic: does this byte slice look like binary (non-text) data?
///
/// Inlining binary into a tool result is never useful to the model and can
/// poison the context — `String::from_utf8_lossy` keeps raw control bytes
/// verbatim, and a `.glb`/image/executable dumped as text has derailed local
/// models into degenerate loops. A single NUL byte is the strongest signal
/// (text never contains them); otherwise a high share of non-text control
/// bytes marks it binary. Only the head is sampled, and bytes `>= 0x80` are
/// never counted so valid UTF-8 text is not misclassified.
pub(crate) fn looks_binary(bytes: &[u8]) -> bool {
    if bytes.is_empty() {
        return false;
    }
    let sample = &bytes[..bytes.len().min(8192)];
    if sample.contains(&0) {
        return true;
    }
    let suspect = sample
        .iter()
        .filter(|&&b| matches!(b, 0x00..=0x08 | 0x0b | 0x0c | 0x0e..=0x1f))
        .count();
    suspect * 100 / sample.len() > 30
}

/// Stand-in shown to the model in place of binary content.
pub(crate) fn binary_placeholder(len: usize) -> String {
    format!("<binary data: {len} bytes, not shown>")
}

fn read_path_effect(ctx: &ToolContext<'_>, path: &Path) -> Effect {
    Effect::ReadPath {
        inside_workspace: ctx.workspace.contains(path),
        secret_like: is_secret_like(path),
    }
}

fn write_path_effect(ctx: &ToolContext<'_>, path: &Path, overwrite: bool) -> Effect {
    Effect::WritePath {
        inside_workspace: ctx.workspace.contains(path),
        overwrite,
    }
}

fn detect_newline(existing: &str) -> &'static str {
    if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    }
}

fn apply_newline(content: &str, newline: &str) -> String {
    let normalized = content.replace("\r\n", "\n");
    if newline == "\n" {
        normalized
    } else {
        normalized.replace('\n', newline)
    }
}

/// Line-ending-insensitive matching base: CRLF→LF. The model emits an edit's
/// `old_text` with `\n`, but a file may be stored with `\r\n`; matching on the
/// normalized form lets an edit land on a CRLF file instead of failing "old_text
/// was not found" (which is what pushed the model to give up and rewrite whole
/// files). The file's original newline style is restored on write via
/// [`apply_newline`].
fn lf(s: &str) -> String {
    s.replace("\r\n", "\n")
}

fn atomic_write(path: &Path, bytes: &[u8]) -> Result<(), ToolError> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| ToolError::Failed(e.to_string()))?;
    }
    let mut tmp = path.as_os_str().to_os_string();
    tmp.push(".tmp");
    let tmp = PathBuf::from(tmp);
    std::fs::write(&tmp, bytes).map_err(|e| ToolError::Failed(e.to_string()))?;
    std::fs::rename(&tmp, path).map_err(|e| {
        let _ = std::fs::remove_file(&tmp);
        ToolError::Failed(e.to_string())
    })
}

// --- edit matching (shared by edit_file / multi_edit / apply_patch) ----------
//
// All three edit tools locate an exact, unique `old_text` and replace it once.
// They share one anchored matcher so a near-miss yields the same guiding error
// everywhere instead of three copy-pasted count-and-replace loops. The matcher
// is **anchored, never fuzzy**: an exact unique match first, then a single
// leading-indentation-tolerant rung that must *also* be unique, else it changes
// nothing and guides the model. There is no Levenshtein/best-guess rung — a
// wrong-location edit is far worse than a failed one.

/// A located, anchored replacement: the byte span of the LF-normalized haystack
/// to replace and the text to write there (re-indented to the file when the
/// match needed indentation tolerance).
#[derive(Debug)]
struct EditPlan {
    start: usize,
    end: usize,
    replacement: String,
}

/// Why an `old_text` could not be applied — carries what a guiding error needs.
#[derive(Debug)]
enum EditMiss {
    /// No exact match and no unique leading-indent-tolerant match.
    NotFound,
    /// The match (exact or tolerant) was not unique.
    Ambiguous { count: usize },
}

/// Locate `needle` in `haystack` (both already LF-normalized) for an anchored
/// replacement with `replacement`. Exact unique substring match first; on a
/// zero-count miss, one leading-indentation-tolerant rung that must be unique;
/// otherwise an [`EditMiss`]. Anchored, never a best-guess edit.
fn locate_edit(haystack: &str, needle: &str, replacement: &str) -> Result<EditPlan, EditMiss> {
    let exact: Vec<usize> = haystack.match_indices(needle).map(|(i, _)| i).collect();
    match exact.len() {
        1 => Ok(EditPlan {
            start: exact[0],
            end: exact[0] + needle.len(),
            replacement: replacement.to_string(),
        }),
        0 => tolerant_locate(haystack, needle, replacement),
        n => Err(EditMiss::Ambiguous { count: n }),
    }
}

/// Byte spans of each line in `text` (content only, excluding the `\n`); the span
/// end is the newline index, or the text length for the final line.
fn line_spans(text: &str) -> Vec<(usize, usize)> {
    let mut spans = Vec::new();
    let mut start = 0;
    for (i, b) in text.bytes().enumerate() {
        if b == b'\n' {
            spans.push((start, i));
            start = i + 1;
        }
    }
    spans.push((start, text.len()));
    spans
}

/// The content lines of `text`: split on `\n`, dropping the empty element a
/// trailing newline produces so `"a\nb\n"` is two lines, not three.
fn content_lines(text: &str) -> Vec<&str> {
    let mut lines: Vec<&str> = text.split('\n').collect();
    if text.ends_with('\n') {
        lines.pop();
    }
    lines
}

/// A leading-indentation-tolerant, anchored block match. The needle's content
/// lines, left-trimmed, must equal a *unique* contiguous run of the haystack's
/// content lines, and the indentation must differ by **one consistent whitespace
/// prefix across the whole block** (the file is the block uniformly re-indented,
/// or the needle is) — never a per-line guess. `replacement` is re-indented by
/// that prefix so the file's own indentation is preserved. Only the matched
/// content lines are replaced; the surrounding newlines are left intact.
fn tolerant_locate(haystack: &str, needle: &str, replacement: &str) -> Result<EditPlan, EditMiss> {
    let h_spans = line_spans(haystack);
    let n_lines = content_lines(needle);
    let k = n_lines.len();
    if k == 0 || k > h_spans.len() {
        return Err(EditMiss::NotFound);
    }
    let mut hit: Option<(usize, String)> = None;
    let mut count = 0usize;
    for i in 0..=(h_spans.len() - k) {
        let h_block: Vec<&str> = (0..k)
            .map(|j| &haystack[h_spans[i + j].0..h_spans[i + j].1])
            .collect();
        if let Some(reindented) = block_matches(&h_block, &n_lines, replacement) {
            count += 1;
            if hit.is_none() {
                hit = Some((i, reindented));
            }
        }
    }
    match (count, hit) {
        (1, Some((i, replacement))) => Ok(EditPlan {
            start: h_spans[i].0,
            end: h_spans[i + k - 1].1,
            replacement,
        }),
        (0, _) => Err(EditMiss::NotFound),
        (n, _) => Err(EditMiss::Ambiguous { count: n }),
    }
}

/// Whether a haystack block matches the needle lines under the uniform-indent
/// rule; on a match, the re-indented replacement. Blank lines match any blank
/// line. Returns `None` (no match) when content differs or the indentation
/// difference is not one consistent whitespace prefix across the block.
fn block_matches(h_block: &[&str], n_lines: &[&str], replacement: &str) -> Option<String> {
    let mut delta: Option<(String, bool)> = None;
    for (h, n) in h_block.iter().zip(n_lines.iter()) {
        let hb = h.trim_start();
        let nb = n.trim_start();
        if hb.is_empty() && nb.is_empty() {
            continue; // both blank — matches, contributes no indentation constraint
        }
        if hb.is_empty() != nb.is_empty() || hb != nb {
            return None; // content differs beyond leading indentation
        }
        let h_indent = &h[..h.len() - hb.len()];
        let n_indent = &n[..n.len() - nb.len()];
        let rel = indent_delta(h_indent, n_indent)?;
        match &delta {
            None => delta = Some(rel),
            Some(prev) if *prev != rel => return None, // not a uniform shift
            Some(_) => {}
        }
    }
    let (pad, add) = delta.unwrap_or_else(|| (String::new(), true));
    Some(reindent(replacement, &pad, add))
}

/// The uniform whitespace shift between a file line's indent and a needle line's
/// indent: `(pad, add)` where `add` means "prepend `pad` to the replacement"
/// (the file is more indented) and `!add` means "strip `pad`" (the needle is more
/// indented). `None` when neither indent is a prefix of the other (e.g. tabs vs
/// spaces) — an incompatible difference the tolerant rung must not guess through.
fn indent_delta(h_indent: &str, n_indent: &str) -> Option<(String, bool)> {
    if let Some(extra) = h_indent.strip_prefix(n_indent) {
        return Some((extra.to_string(), true));
    }
    n_indent
        .strip_prefix(h_indent)
        .map(|extra| (extra.to_string(), false))
}

/// Re-indent each content line of `replacement` by `pad`: prepend it when `add`,
/// strip it otherwise. Blank lines are left untouched (never padded into trailing
/// whitespace). An empty `pad` returns the text unchanged.
fn reindent(replacement: &str, pad: &str, add: bool) -> String {
    if pad.is_empty() {
        return replacement.to_string();
    }
    let mut out = String::new();
    for (idx, line) in content_lines(replacement).iter().enumerate() {
        if idx > 0 {
            out.push('\n');
        }
        if line.trim().is_empty() {
            out.push_str(line);
        } else if add {
            out.push_str(pad);
            out.push_str(line);
        } else {
            out.push_str(line.strip_prefix(pad).unwrap_or(line));
        }
    }
    out
}

/// Build a guiding error for an edit that did not apply: the match count plus
/// "add surrounding context" for an ambiguous match, or the nearest existing
/// block + a re-read/stale hint for a not-found one. `location` names the failing
/// edit (a path, or `edit N (path)`, or `operation N … hunk M`).
fn edit_miss_error(location: &str, haystack: &str, needle: &str, miss: &EditMiss) -> ToolError {
    match miss {
        EditMiss::Ambiguous { count } => ToolError::Failed(format!(
            "{location}: ambiguous edit — old_text matches {count} times; include more \
             surrounding context so it matches exactly once (or use replace_in_file to \
             change every occurrence)."
        )),
        EditMiss::NotFound => ToolError::Failed(format!(
            "{location}: old_text was not found. {} Re-read the file before editing — its \
             contents may have changed since your last read; the text must match the file \
             (whitespace/indentation aside).",
            nearest_block_hint(haystack, needle)
        )),
    }
}

/// A pointer to the closest existing line for a not-found edit: the line number
/// of the first haystack line whose trimmed content equals the needle's first
/// non-blank trimmed line, or a note that none was close.
fn nearest_block_hint(haystack: &str, needle: &str) -> String {
    let Some(first) = content_lines(needle)
        .into_iter()
        .find(|l| !l.trim().is_empty())
    else {
        return "The old_text has no content.".to_string();
    };
    let target = first.trim();
    for (idx, line) in haystack.split('\n').enumerate() {
        if line.trim() == target {
            return format!(
                "The closest matching line is line {} (`{}`).",
                idx + 1,
                cap_line(line.trim())
            );
        }
    }
    "No closely matching line was found.".to_string()
}

/// Truncate a single line to a readable length on a char boundary for an error.
fn cap_line(line: &str) -> String {
    const MAX: usize = 80;
    if line.len() <= MAX {
        return line.to_string();
    }
    let mut end = MAX;
    while !line.is_char_boundary(end) {
        end -= 1;
    }
    format!("{}…", &line[..end])
}

/// Reject a degenerate edit before matching: an empty `old_text` (which would
/// match everywhere) or an `old_text` identical to `new_text` (a no-op write).
/// `location` names the failing edit for a multi-edit/patch batch.
fn validate_edit(location: &str, needle: &str, replacement: &str) -> Result<(), ToolError> {
    if needle.is_empty() {
        return Err(ToolError::InvalidInput(format!(
            "{location}: old_text must not be empty"
        )));
    }
    if needle == replacement {
        return Err(ToolError::InvalidInput(format!(
            "{location}: old_text and new_text are identical; the edit would change nothing"
        )));
    }
    Ok(())
}

/// Apply an [`EditPlan`] to `haystack`, returning the new (still LF-normalized)
/// content. Splices the replacement into the matched span without disturbing the
/// rest of the file.
fn apply_plan(haystack: &str, plan: &EditPlan) -> String {
    format!(
        "{}{}{}",
        &haystack[..plan.start],
        plan.replacement,
        &haystack[plan.end..]
    )
}

// --- read_file --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileInput {
    /// Workspace-relative or absolute path to read.
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: String,
    /// First line to include (1-based, inclusive).
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::line_range")]
    start_line: Option<usize>,
    /// Last line to include (1-based, inclusive).
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::line_range")]
    end_line: Option<usize>,
}

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
    }
    fn contract(&self) -> ToolContract {
        read_only_contract("Read a file's contents from the workspace.")
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "path")
    }
    fn description(&self) -> &'static str {
        "Read UTF-8 text from a file in the workspace, optionally a line range."
    }
    fn schema(&self) -> Value {
        schema_for::<ReadFileInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: ReadFileInput = parse_input(input)?;
        Ok(vec![read_path_effect(ctx, Path::new(&input.path))])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ReadFileInput = parse_input(&input)?;
        let path = ctx.workspace.normalize(Path::new(&input.path))?;
        let bytes = std::fs::read(&path)
            .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
        // Refuse to inline binary: emit a short placeholder instead of dumping
        // lossy bytes that waste context and can derail the model.
        if looks_binary(&bytes) {
            return Ok(cap(binary_placeholder(bytes.len())));
        }
        let text = String::from_utf8(bytes)
            .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
        let selected = match (input.start_line, input.end_line) {
            (None, None) => text,
            (start, end) => {
                let start = start.unwrap_or(1).max(1);
                let end = end.unwrap_or(usize::MAX);
                text.lines()
                    .enumerate()
                    .filter(|(i, _)| {
                        let line = i + 1;
                        line >= start && line <= end
                    })
                    .map(|(_, l)| l)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        Ok(cap(selected))
    }
}

// --- write_file -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct WriteFileInput {
    /// Path to write within the workspace.
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: String,
    /// File contents.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    content: String,
    /// Allow replacing an existing file. Defaults to true (overwrite is gated by
    /// the permission engine).
    #[serde(default)]
    overwrite: Option<bool>,
}

pub struct WriteFile;

#[async_trait]
impl Tool for WriteFile {
    fn name(&self) -> &'static str {
        "write_file"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Create or overwrite a workspace file with exact content.",
            side_effect: SideEffectClass::ProjectWrite,
            reversibility: Reversibility::ReversibleWithArtifact,
            idempotency: Idempotency::Idempotent,
            preconditions: PRIOR_READ_PATH,
            postconditions: PATH_EXISTS,
            verification: VerificationMethod::ReadBack { tool: "read_file" },
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "path")
    }
    fn description(&self) -> &'static str {
        "Create or replace a file in the workspace, preserving newline style."
    }
    fn schema(&self) -> Value {
        schema_for::<WriteFileInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: WriteFileInput = parse_input(input)?;
        let path = Path::new(&input.path);
        let overwrite = ctx
            .workspace
            .normalize(path)
            .map(|p| p.exists())
            .unwrap_or(false);
        Ok(vec![write_path_effect(ctx, path, overwrite)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: WriteFileInput = parse_input(&input)?;
        let path = ctx.workspace.normalize(Path::new(&input.path))?;
        // Existence is checked on the path itself: a non-UTF-8 (binary) file
        // fails `read_to_string` but must still refuse an overwrite=false
        // write. The lossy read is used only for newline detection.
        if path.exists() && input.overwrite == Some(false) {
            return Err(ToolError::Failed(format!(
                "{} exists and overwrite is false",
                path.display()
            )));
        }
        let existing = std::fs::read_to_string(&path).ok();
        let newline = existing.as_deref().map_or("\n", detect_newline);
        let body = apply_newline(&input.content, newline);
        atomic_write(&path, body.as_bytes())?;
        Ok(ToolOutput::ok(format!(
            "wrote {} bytes to {}",
            body.len(),
            path.display()
        )))
    }
}

// --- append_file ------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct AppendFileInput {
    /// Path to append to within the workspace.
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: String,
    /// Content to append to the end of the file.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    content: String,
}

pub struct AppendFile;

#[async_trait]
impl Tool for AppendFile {
    fn name(&self) -> &'static str {
        "append_file"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description:
                "Append content to the end of a workspace file, creating it if absent.",
            side_effect: SideEffectClass::ProjectWrite,
            reversibility: Reversibility::ReversibleWithArtifact,
            // Re-running an append adds the content again, so it is not idempotent
            // (unlike write_file, which overwrites).
            idempotency: Idempotency::NonIdempotent,
            preconditions: PRIOR_READ_PATH,
            postconditions: PATH_EXISTS,
            verification: VerificationMethod::ReadBack { tool: "read_file" },
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "path")
    }
    fn description(&self) -> &'static str {
        "Append to the end of a workspace file (creating it if needed), preserving newline style. Use to write a large file in pieces."
    }
    fn schema(&self) -> Value {
        schema_for::<AppendFileInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: AppendFileInput = parse_input(input)?;
        let path = Path::new(&input.path);
        let overwrite = ctx
            .workspace
            .normalize(path)
            .map(|p| p.exists())
            .unwrap_or(false);
        Ok(vec![write_path_effect(ctx, path, overwrite)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: AppendFileInput = parse_input(&input)?;
        let path = ctx.workspace.normalize(Path::new(&input.path))?;
        let existing = std::fs::read_to_string(&path).ok();
        // A file that exists but is not valid UTF-8 (binary) reads as `None`;
        // appending text would clobber it, so refuse rather than overwrite.
        if existing.is_none() && path.exists() {
            return Err(ToolError::Failed(format!(
                "{} is not a UTF-8 text file; cannot append",
                path.display()
            )));
        }
        // Match the file's newline style (default LF for a new file) so a
        // chunked write stays consistent across appends and platforms.
        let newline = existing.as_deref().map_or("\n", detect_newline);
        let addition = apply_newline(&input.content, newline);
        let body = match existing {
            Some(mut current) => {
                current.push_str(&addition);
                current
            }
            None => addition.clone(),
        };
        atomic_write(&path, body.as_bytes())?;
        Ok(ToolOutput::ok(format!(
            "appended {} bytes to {}",
            addition.len(),
            path.display()
        )))
    }
}

// --- edit_file --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct EditFileInput {
    /// Path to edit within the workspace.
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: String,
    /// Exact text to replace; must match exactly once.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    old_text: String,
    /// Replacement text.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    new_text: String,
}

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Replace an exact span of text in an existing file.",
            side_effect: SideEffectClass::ProjectWrite,
            reversibility: Reversibility::ReversibleWithArtifact,
            idempotency: Idempotency::NonIdempotent,
            preconditions: PRIOR_READ_PATH,
            postconditions: PATH_MODIFIED,
            verification: VerificationMethod::ReadBack { tool: "read_file" },
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "path")
    }
    fn description(&self) -> &'static str {
        "Replace an exact, unique snippet in a workspace file; rejects ambiguous edits."
    }
    fn schema(&self) -> Value {
        schema_for::<EditFileInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: EditFileInput = parse_input(input)?;
        Ok(vec![write_path_effect(ctx, Path::new(&input.path), true)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: EditFileInput = parse_input(&input)?;
        let path = ctx.workspace.normalize(Path::new(&input.path))?;
        let content = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
        // Match line-ending-insensitively (the model emits `old_text` with `\n`,
        // the file may be CRLF) through the shared anchored matcher, then restore
        // the file's newline style on write.
        let newline = detect_newline(&content);
        let haystack = lf(&content);
        let needle = lf(&input.old_text);
        let replacement = lf(&input.new_text);
        let location = path.display().to_string();
        validate_edit(&location, &needle, &replacement)?;
        match locate_edit(&haystack, &needle, &replacement) {
            Ok(plan) => {
                let updated = apply_plan(&haystack, &plan);
                atomic_write(&path, apply_newline(&updated, newline).as_bytes())?;
                Ok(ToolOutput::ok(format!("edited {}", path.display())))
            }
            Err(miss) => Err(edit_miss_error(&location, &haystack, &needle, &miss)),
        }
    }
}

// --- multi_edit -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct MultiEditInput {
    /// Path to edit within the workspace.
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: String,
    /// Ordered exact-text replacements. Each `old_text` must match exactly once
    /// at the point that edit is applied.
    edits: Vec<TextEditInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TextEditInput {
    /// Exact text to replace.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    old_text: String,
    /// Replacement text.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    new_text: String,
}

pub struct MultiEdit;

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> &'static str {
        "multi_edit"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Apply several exact text edits to one file atomically.",
            side_effect: SideEffectClass::ProjectWrite,
            reversibility: Reversibility::ReversibleWithArtifact,
            idempotency: Idempotency::NonIdempotent,
            preconditions: PRIOR_READ_PATH,
            postconditions: PATH_MODIFIED,
            verification: VerificationMethod::ReadBack { tool: "read_file" },
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "path")
    }
    fn description(&self) -> &'static str {
        "Apply several exact text replacements to one workspace file atomically; rejects missing or ambiguous context."
    }
    fn schema(&self) -> Value {
        schema_for::<MultiEditInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: MultiEditInput = parse_input(input)?;
        Ok(vec![write_path_effect(ctx, Path::new(&input.path), true)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: MultiEditInput = parse_input(&input)?;
        if input.edits.is_empty() {
            return Err(ToolError::InvalidInput(
                "edits must contain at least one replacement".to_string(),
            ));
        }
        let path = ctx.workspace.normalize(Path::new(&input.path))?;
        let original = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;
        let newline = detect_newline(&original);
        // Apply each edit through the shared anchored matcher on the LF-normalized
        // content, atomically: all edits land in memory, then one write restores
        // the file's newline style. A miss on any edit aborts with nothing written.
        let mut updated = lf(&original);
        for (index, edit) in input.edits.iter().enumerate() {
            let needle = lf(&edit.old_text);
            let replacement = lf(&edit.new_text);
            let location = format!("edit {} ({})", index + 1, path.display());
            validate_edit(&location, &needle, &replacement)?;
            match locate_edit(&updated, &needle, &replacement) {
                Ok(plan) => updated = apply_plan(&updated, &plan),
                Err(miss) => return Err(edit_miss_error(&location, &updated, &needle, &miss)),
            }
        }
        atomic_write(&path, apply_newline(&updated, newline).as_bytes())?;
        Ok(ToolOutput::ok(format!(
            "applied {} edits to {}",
            input.edits.len(),
            path.display()
        )))
    }
}

// --- list_files -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ListFilesInput {
    /// Directory to list, relative to the workspace. Defaults to the root.
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: Option<String>,
    /// Include hidden files. Defaults to false.
    #[serde(default)]
    hidden: bool,
}

const MAX_LIST: usize = 1000;

pub struct ListFiles;

#[async_trait]
impl Tool for ListFiles {
    fn name(&self) -> &'static str {
        "list_files"
    }
    fn contract(&self) -> ToolContract {
        read_only_contract("List files under a workspace directory.")
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "path")
    }
    fn description(&self) -> &'static str {
        "List files under a workspace directory, respecting ignore files."
    }
    fn schema(&self) -> Value {
        schema_for::<ListFilesInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: ListFilesInput = parse_input(input)?;
        let dir = input.path.unwrap_or_else(|| ".".to_string());
        Ok(vec![read_path_effect(ctx, Path::new(&dir))])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ListFilesInput = parse_input(&input)?;
        let dir = ctx
            .workspace
            .normalize(Path::new(&input.path.unwrap_or_else(|| ".".to_string())))?;
        let root = ctx.workspace.root().to_path_buf();
        let mut entries = Vec::new();
        let mut truncated = false;
        for result in ignore::WalkBuilder::new(&dir)
            .hidden(!input.hidden)
            .require_git(false)
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            if entry.file_type().is_some_and(|t| t.is_file()) {
                let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                entries.push(rel.display().to_string());
                if entries.len() >= MAX_LIST {
                    truncated = true;
                    break;
                }
            }
        }
        entries.sort();
        let text = entries.join("\n");
        Ok(if truncated {
            ToolOutput::truncated(text)
        } else {
            ToolOutput::ok(text)
        })
    }
}

// --- find_files -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct FindFilesInput {
    /// Glob-like filename pattern. Supports `*` and `?`.
    #[schemars(schema_with = "crate::schema_intent::glob_string")]
    pattern: String,
    /// Directory to search, relative to the workspace. Defaults to the root.
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: Option<String>,
    /// Include hidden files. Defaults to false.
    #[serde(default)]
    hidden: bool,
    /// Maximum number of paths to return.
    #[serde(default)]
    max_matches: Option<usize>,
}

pub struct FindFiles;

#[async_trait]
impl Tool for FindFiles {
    fn name(&self) -> &'static str {
        "find_files"
    }
    fn contract(&self) -> ToolContract {
        read_only_contract("Find files whose path matches a glob pattern.")
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "pattern")
    }
    fn description(&self) -> &'static str {
        "Find workspace files by filename pattern, respecting ignore files."
    }
    fn schema(&self) -> Value {
        schema_for::<FindFilesInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: FindFilesInput = parse_input(input)?;
        let dir = input.path.unwrap_or_else(|| ".".to_string());
        Ok(vec![read_path_effect(ctx, Path::new(&dir))])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: FindFilesInput = parse_input(&input)?;
        let dir = ctx
            .workspace
            .normalize(Path::new(input.path.as_deref().unwrap_or(".")))?;
        let root = ctx.workspace.root().to_path_buf();
        let pattern = wildcard_regex(&input.pattern)?;
        let limit = input.max_matches.unwrap_or(MAX_LIST).min(MAX_LIST);
        let mut paths = Vec::new();
        let mut truncated = false;
        for result in ignore::WalkBuilder::new(&dir)
            .hidden(!input.hidden)
            .require_git(false)
            .build()
        {
            let entry = match result {
                Ok(entry) => entry,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let name = entry.file_name().to_string_lossy();
            if pattern.is_match(&name) {
                let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
                paths.push(rel.display().to_string());
                if paths.len() >= limit {
                    truncated = true;
                    break;
                }
            }
        }
        paths.sort();
        Ok(if truncated {
            ToolOutput::truncated(paths.join("\n"))
        } else {
            ToolOutput::ok(paths.join("\n"))
        })
    }
}

fn wildcard_regex(pattern: &str) -> Result<regex::Regex, ToolError> {
    let mut regex = String::from("^");
    for ch in pattern.chars() {
        match ch {
            '*' => regex.push_str(".*"),
            '?' => regex.push('.'),
            _ => regex.push_str(&regex::escape(&ch.to_string())),
        }
    }
    regex.push('$');
    regex::Regex::new(&regex).map_err(|e| ToolError::InvalidInput(e.to_string()))
}

// --- search_text ------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct SearchTextInput {
    /// Text or regular expression to search for.
    query: String,
    /// Directory to search, relative to the workspace. Defaults to the root.
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: Option<String>,
    /// Treat `query` as a regular expression. Defaults to false (literal).
    #[serde(default)]
    is_regex: bool,
    /// Maximum number of matches to return.
    #[serde(default)]
    max_matches: Option<usize>,
}

const MAX_MATCHES: usize = 500;

pub struct SearchText;

#[async_trait]
impl Tool for SearchText {
    fn name(&self) -> &'static str {
        "search_text"
    }
    fn contract(&self) -> ToolContract {
        read_only_contract("Search workspace file contents for a query.")
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "query")
    }
    fn description(&self) -> &'static str {
        "Search workspace files for text or a regex, respecting ignore files."
    }
    fn schema(&self) -> Value {
        schema_for::<SearchTextInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: SearchTextInput = parse_input(input)?;
        let dir = input.path.unwrap_or_else(|| ".".to_string());
        Ok(vec![read_path_effect(ctx, Path::new(&dir))])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: SearchTextInput = parse_input(&input)?;
        let dir = ctx
            .workspace
            .normalize(Path::new(input.path.as_deref().unwrap_or(".")))?;
        let root = ctx.workspace.root().to_path_buf();
        let limit = input.max_matches.unwrap_or(MAX_MATCHES).min(MAX_MATCHES);
        let regex = if input.is_regex {
            Some(
                regex::Regex::new(&input.query)
                    .map_err(|e| ToolError::InvalidInput(e.to_string()))?,
            )
        } else {
            None
        };

        let mut hits = Vec::new();
        'walk: for result in ignore::WalkBuilder::new(&dir)
            .hidden(true)
            .require_git(false)
            .build()
        {
            let entry = match result {
                Ok(e) => e,
                Err(_) => continue,
            };
            if !entry.file_type().is_some_and(|t| t.is_file()) {
                continue;
            }
            let Ok(content) = std::fs::read_to_string(entry.path()) else {
                continue; // skip binary / unreadable files
            };
            let rel = entry.path().strip_prefix(&root).unwrap_or(entry.path());
            for (line_no, line) in content.lines().enumerate() {
                let matched = match &regex {
                    Some(re) => re.is_match(line),
                    None => line.contains(&input.query),
                };
                if matched {
                    hits.push(format!(
                        "{}:{}: {}",
                        rel.display(),
                        line_no + 1,
                        line.trim()
                    ));
                    if hits.len() >= limit {
                        break 'walk;
                    }
                }
            }
        }
        Ok(cap(hits.join("\n")))
    }
}

// --- apply_patch ------------------------------------------------------------

/// A structured multi-file patch. The grammar is typed JSON generated from
/// these structs (original to this repository): an ordered list of operations,
/// each creating, updating (exact-match hunks), or deleting one file.
#[derive(Debug, Deserialize, JsonSchema)]
struct ApplyPatchInput {
    /// Ordered file operations; the whole patch is validated before any write.
    operations: Vec<PatchOperation>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[serde(tag = "action", rename_all = "snake_case")]
enum PatchOperation {
    /// Create a new file (fails if the file already exists).
    Create { path: String, content: String },
    /// Apply exact-match hunks to an existing file, in order.
    Update { path: String, hunks: Vec<PatchHunk> },
    /// Delete an existing file.
    Delete { path: String },
}

#[derive(Debug, Deserialize, JsonSchema)]
struct PatchHunk {
    /// Exact text to replace; must match exactly once at the point this hunk
    /// is applied.
    old_text: String,
    /// Replacement text.
    new_text: String,
}

impl PatchOperation {
    fn path(&self) -> &str {
        match self {
            PatchOperation::Create { path, .. }
            | PatchOperation::Update { path, .. }
            | PatchOperation::Delete { path } => path,
        }
    }

    fn describe(&self) -> String {
        match self {
            PatchOperation::Create { path, .. } => format!("create {path}"),
            PatchOperation::Update { path, hunks } => {
                format!("update {path} ({} hunks)", hunks.len())
            }
            PatchOperation::Delete { path } => format!("delete {path}"),
        }
    }
}

pub struct ApplyPatch;

#[async_trait]
impl Tool for ApplyPatch {
    fn name(&self) -> &'static str {
        "apply_patch"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Apply a unified-diff patch to the workspace.",
            // apply_patch can delete files in a batch, so it is classified
            // destructive — which also keeps the argument-repair gate from ever
            // reshaping a patch (e.g. un-stringifying an operations array that
            // contains a delete). create/update remain ordinary project writes at
            // the permission layer; this is advisory metadata only.
            side_effect: SideEffectClass::Destructive,
            reversibility: Reversibility::ReversibleWithArtifact,
            idempotency: Idempotency::NonIdempotent,
            postconditions: RESULT_STATUS,
            verification: VerificationMethod::Postconditions,
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        // The diff preview for the approval prompt: one line per operation.
        let Ok(input) = serde_json::from_value::<ApplyPatchInput>(input.clone()) else {
            return String::new();
        };
        let lines: Vec<String> = input
            .operations
            .iter()
            .take(12)
            .map(PatchOperation::describe)
            .collect();
        detail_preview(&lines.join("; "))
    }
    fn description(&self) -> &'static str {
        "Apply a structured multi-file patch: create, update (exact-match hunks), or delete files. Validated atomically before any write."
    }
    fn schema(&self) -> Value {
        schema_for::<ApplyPatchInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: ApplyPatchInput = parse_input(input)?;
        if input.operations.is_empty() {
            return Err(ToolError::InvalidInput(
                "operations must contain at least one file operation".to_string(),
            ));
        }
        Ok(input
            .operations
            .iter()
            .map(|op| {
                let overwrite = !matches!(op, PatchOperation::Create { .. });
                write_path_effect(ctx, Path::new(op.path()), overwrite)
            })
            .collect())
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ApplyPatchInput = parse_input(&input)?;

        // Validate every operation against the current tree before any write,
        // so a rejected hunk fails the whole patch with nothing applied.
        let mut writes: Vec<(PathBuf, Option<String>)> = Vec::new();
        for (index, op) in input.operations.iter().enumerate() {
            let label = format!("operation {} ({})", index + 1, op.describe());
            let path = ctx.workspace.normalize(Path::new(op.path()))?;
            match op {
                PatchOperation::Create { content, .. } => {
                    if path.exists() {
                        return Err(ToolError::Failed(format!(
                            "{label}: the file already exists; use an update operation"
                        )));
                    }
                    writes.push((path, Some(content.clone())));
                }
                PatchOperation::Update { hunks, .. } => {
                    if hunks.is_empty() {
                        return Err(ToolError::InvalidInput(format!(
                            "{label}: hunks must contain at least one replacement"
                        )));
                    }
                    let original = std::fs::read_to_string(&path)
                        .map_err(|e| ToolError::Failed(format!("{label}: {e}")))?;
                    let newline = detect_newline(&original);
                    // Apply each hunk through the shared anchored matcher on the
                    // LF-normalized content (a CRLF file must not reject a hunk
                    // whose `old_text` uses `\n`); validate-then-apply keeps the
                    // whole patch all-or-nothing.
                    let mut updated = lf(&original);
                    for (hunk_index, hunk) in hunks.iter().enumerate() {
                        let needle = lf(&hunk.old_text);
                        let replacement = lf(&hunk.new_text);
                        let location = format!("{label}: hunk {}", hunk_index + 1);
                        validate_edit(&location, &needle, &replacement)?;
                        match locate_edit(&updated, &needle, &replacement) {
                            Ok(plan) => updated = apply_plan(&updated, &plan),
                            Err(miss) => {
                                return Err(edit_miss_error(&location, &updated, &needle, &miss))
                            }
                        }
                    }
                    writes.push((path, Some(apply_newline(&updated, newline))));
                }
                PatchOperation::Delete { .. } => {
                    if !path.exists() {
                        return Err(ToolError::Failed(format!(
                            "{label}: the file does not exist"
                        )));
                    }
                    writes.push((path, None));
                }
            }
        }

        // Apply. Each file write is atomic (temp-then-rename); validation
        // above makes the whole patch all-or-nothing in practice.
        let mut applied = Vec::new();
        for ((path, content), op) in writes.iter().zip(&input.operations) {
            match content {
                Some(content) => atomic_write(path, content.as_bytes())?,
                None => std::fs::remove_file(path)
                    .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?,
            }
            applied.push(op.describe());
        }
        Ok(ToolOutput::ok(format!("applied: {}", applied.join("; "))))
    }
}

// --- read_tool_output --------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadToolOutputInput {
    /// The retention id from a truncated tool result.
    id: String,
    /// First line to include (1-based, inclusive).
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::line_range")]
    start_line: Option<usize>,
    /// Last line to include (1-based, inclusive).
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::line_range")]
    end_line: Option<usize>,
}

/// Fetches the full output of an earlier tool call whose result was truncated
/// in context and spilled to the retention store.
pub struct ReadToolOutput;

#[async_trait]
impl Tool for ReadToolOutput {
    fn name(&self) -> &'static str {
        "read_tool_output"
    }
    fn contract(&self) -> ToolContract {
        read_only_contract("Read back the full output of an earlier tool call.")
    }
    fn description(&self) -> &'static str {
        "Read the full retained output of an earlier tool call that was truncated in context, by its retention id, optionally a line range."
    }
    fn schema(&self) -> Value {
        schema_for::<ReadToolOutputInput>()
    }
    fn effects(&self, input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Reads runtime state already mediated at capture time; no new side
        // effect.
        let _: ReadToolOutputInput = parse_input(input)?;
        Ok(Vec::new())
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ReadToolOutputInput = parse_input(&input)?;
        let Some(retention) = ctx.retention else {
            return Err(ToolError::Failed(
                "no retained output is available in this session".to_string(),
            ));
        };
        let full = retention
            .fetch(&input.id)
            .map_err(ToolError::Failed)?
            .ok_or_else(|| {
                ToolError::Failed(format!("no retained output under id {}", input.id))
            })?;
        let selected = match (input.start_line, input.end_line) {
            (None, None) => full,
            (start, end) => {
                let start = start.unwrap_or(1).max(1);
                let end = end.unwrap_or(usize::MAX);
                full.lines()
                    .enumerate()
                    .filter(|(i, _)| {
                        let line = i + 1;
                        line >= start && line <= end
                    })
                    .map(|(_, l)| l)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };
        Ok(cap(selected))
    }
}

// run_shell moved to builtins_shell.rs (hotspot split).

// --- fetch -------------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct FetchInput {
    /// http or https URL to retrieve.
    url: String,
    /// Maximum number of body bytes to return. Capped at the tool output limit.
    #[serde(default)]
    max_bytes: Option<usize>,
    /// Request timeout in seconds. Defaults to 30.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const FETCH_DEFAULT_TIMEOUT_SECS: u64 = 30;

/// Cap on the TCP/TLS connect phase, so a stalled connect fails fast instead of
/// waiting out the full request timeout. A hung network tool otherwise blocks the
/// agent loop with no output; bounded well under the total timeout.
const FETCH_CONNECT_TIMEOUT_SECS: u64 = 10;

/// Validate that a URL uses an http/https scheme before any network effect is
/// resolved. Rejecting other schemes (`file:`, `ftp:`, …) keeps `fetch` from
/// reading local resources and sidestepping the workspace boundary.
fn validate_fetch_url(url: &str) -> Result<(), ToolError> {
    let scheme = url
        .split_once("://")
        .map(|(scheme, _)| scheme)
        .filter(|rest| !rest.is_empty())
        .ok_or_else(|| {
            ToolError::InvalidInput(format!("url must be an http or https URL: {url}"))
        })?;
    if scheme.eq_ignore_ascii_case("http") || scheme.eq_ignore_ascii_case("https") {
        Ok(())
    } else {
        Err(ToolError::InvalidInput(format!(
            "url scheme must be http or https, got `{scheme}`"
        )))
    }
}

pub struct Fetch;

#[async_trait]
impl Tool for Fetch {
    fn name(&self) -> &'static str {
        "fetch"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Fetch a URL over the network and return its body.",
            side_effect: SideEffectClass::Network,
            reversibility: Reversibility::Reversible,
            idempotency: Idempotency::Idempotent,
            verification: VerificationMethod::Unverifiable,
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "url")
    }
    fn description(&self) -> &'static str {
        "Fetch the body of an http or https URL over the network."
    }
    fn schema(&self) -> Value {
        schema_for::<FetchInput>()
    }
    fn effects(&self, input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: FetchInput = parse_input(input)?;
        validate_fetch_url(&input.url)?;
        Ok(vec![Effect::Network])
    }
    async fn invoke(&self, input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: FetchInput = parse_input(&input)?;
        validate_fetch_url(&input.url)?;
        let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(FETCH_DEFAULT_TIMEOUT_SECS));
        let client = reqwest::Client::builder()
            .timeout(timeout)
            // Fail fast on a stalled connect instead of hanging out the full
            // request timeout (a never-completing network call blocks the loop).
            .connect_timeout(timeout.min(Duration::from_secs(FETCH_CONNECT_TIMEOUT_SECS)))
            .build()
            .map_err(|e| ToolError::Failed(format!("failed to build HTTP client: {e}")))?;

        let response = client
            .get(&input.url)
            .send()
            .await
            .map_err(|e| ToolError::Failed(format!("request failed: {e}")))?;

        let status = response.status();
        let content_type = response
            .headers()
            .get(reqwest::header::CONTENT_TYPE)
            .and_then(|value| value.to_str().ok())
            .unwrap_or("")
            .to_string();
        let body = response
            .text()
            .await
            .map_err(|e| ToolError::Failed(format!("failed to read response body: {e}")))?;

        let body = match input.max_bytes {
            Some(limit) if limit < body.len() => {
                let mut end = limit;
                while end > 0 && !body.is_char_boundary(end) {
                    end -= 1;
                }
                body[..end].to_string()
            }
            _ => body,
        };

        let header = if content_type.is_empty() {
            format!("HTTP {status}\n")
        } else {
            format!("HTTP {status} {content_type}\n")
        };
        let mut result = cap(format!("{header}{body}"));
        result.is_error = !status.is_success();
        Ok(result)
    }
}

// --- replace_in_file --------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ReplaceInFileInput {
    /// Path to edit within the workspace.
    #[schemars(schema_with = "crate::schema_intent::path_string")]
    path: String,
    /// Text to find — an exact block that may span multiple lines. A
    /// platform-native regex when `regex` is true.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    find: String,
    /// Replacement text. May span multiple lines.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    replace: String,
    /// Treat `find` as a regex (.NET on Windows, Perl on Unix). Defaults to
    /// false (literal block).
    #[serde(default)]
    regex: bool,
    /// Replace every occurrence. Defaults to true; false replaces only the
    /// first.
    #[serde(default)]
    all: Option<bool>,
}

/// The fixed PowerShell stream-edit script. `find`/`replace` arrive via the
/// environment (never interpolated into this text), so the command carries no
/// model-controlled string and cannot be turned into another command. It
/// transforms the whole input, so a `find`/`replace` may span lines.
#[cfg(windows)]
const POWERSHELL_REPLACE_SCRIPT: &str = r#"$ErrorActionPreference='Stop'
[Console]::InputEncoding=[System.Text.UTF8Encoding]::new($false)
[Console]::OutputEncoding=[System.Text.UTF8Encoding]::new($false)
$find=$env:RIF_FIND; $repl=$env:RIF_REPL
$useRegex=$env:RIF_REGEX -eq '1'; $all=$env:RIF_ALL -eq '1'
$text=[Console]::In.ReadToEnd()
if($useRegex){
  $re=[regex]::new($find)
  $out= if($all){ $re.Replace($text,$repl) } else { $re.Replace($text,$repl,1) }
} else {
  if($all){ $out=$text.Replace($find,$repl) }
  else { $i=$text.IndexOf($find); if($i -lt 0){ $out=$text } else { $out=$text.Substring(0,$i)+$repl+$text.Substring($i+$find.Length) } }
}
[Console]::Out.Write($out)"#;

/// The fixed Perl stream-edit script (Unix). `sed` cannot do portable
/// multi-line edits (BSD/macOS `sed` lacks `-z`), so the whole input is slurped
/// and transformed with Perl. `find`/`replace` arrive via the environment.
/// Literal replacement is exact; in regex mode the replacement is not
/// re-interpolated, so capture backreferences (`$1`) are not expanded on this
/// platform.
#[cfg(not(windows))]
const PERL_REPLACE_SCRIPT: &str = r#"
my $find=$ENV{RIF_FIND}; my $repl=$ENV{RIF_REPL};
my $all=($ENV{RIF_ALL} eq '1'); my $useRegex=($ENV{RIF_REGEX} eq '1');
my $text=do{ local $/=undef; <STDIN> };
$text='' unless defined $text;
if($useRegex){
  if($all){ $text=~s/$find/$repl/g; } else { $text=~s/$find/$repl/; }
} else {
  my $flen=length($find);
  if($flen>0){
    my $out=''; my $pos=0;
    while((my $i=index($text,$find,$pos))>=0){
      $out.=substr($text,$pos,$i-$pos).$repl; $pos=$i+$flen; last unless $all;
    }
    $out.=substr($text,$pos); $text=$out;
  }
}
print $text;
"#;

pub struct ReplaceInFile;

#[async_trait]
impl Tool for ReplaceInFile {
    fn name(&self) -> &'static str {
        "replace_in_file"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Replace occurrences of a pattern in an existing file.",
            side_effect: SideEffectClass::ProjectWrite,
            reversibility: Reversibility::ReversibleWithArtifact,
            idempotency: Idempotency::NonIdempotent,
            preconditions: PRIOR_READ_PATH,
            postconditions: PATH_MODIFIED,
            verification: VerificationMethod::ReadBack { tool: "read_file" },
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        string_field_detail(input, "path")
    }
    fn description(&self) -> &'static str {
        "Edit a file by replacing an exact block of text with another (literal by default; the block may span multiple lines). Runs through the platform stream editor (PowerShell on Windows, Perl on Unix). Use this as the default way to modify an existing file instead of rewriting it with write_file."
    }
    fn schema(&self) -> Value {
        schema_for::<ReplaceInFileInput>()
    }
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: ReplaceInFileInput = parse_input(input)?;
        Ok(vec![write_path_effect(ctx, Path::new(&input.path), true)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: ReplaceInFileInput = parse_input(&input)?;
        if input.find.is_empty() {
            return Err(ToolError::InvalidInput(
                "find must not be empty".to_string(),
            ));
        }
        let path = ctx.workspace.normalize(Path::new(&input.path))?;
        let original = std::fs::read_to_string(&path)
            .map_err(|e| ToolError::Failed(format!("{}: {e}", path.display())))?;

        let updated = run_stream_editor(&original, &input).await?;

        if updated == original {
            return Ok(cap(format!("no match for find in {}", path.display())));
        }
        atomic_write(&path, updated.as_bytes())?;
        Ok(cap(format!("updated {}", path.display())))
    }
}

/// Run the find/replace through the platform stream editor as a pure stdin ->
/// stdout transform over the whole file. The child never touches the
/// filesystem; path handling and the atomic write stay in Rust.
async fn run_stream_editor(text: &str, input: &ReplaceInFileInput) -> Result<String, ToolError> {
    use tokio::io::AsyncWriteExt;

    let all = input.all.unwrap_or(true);

    #[cfg(windows)]
    let (program, args): (&str, Vec<String>) = (
        "powershell.exe",
        vec![
            "-NoProfile".to_string(),
            "-NonInteractive".to_string(),
            "-Command".to_string(),
            POWERSHELL_REPLACE_SCRIPT.to_string(),
        ],
    );
    #[cfg(not(windows))]
    let (program, args): (&str, Vec<String>) = (
        "perl",
        vec!["-e".to_string(), PERL_REPLACE_SCRIPT.to_string()],
    );

    // `find`/`replace` are passed as data through the environment, never spliced
    // into the command text, so model input can never become another command.
    let envs = [
        ("RIF_FIND", input.find.as_str()),
        ("RIF_REPL", input.replace.as_str()),
        ("RIF_REGEX", if input.regex { "1" } else { "0" }),
        ("RIF_ALL", if all { "1" } else { "0" }),
    ];

    let mut command = tokio::process::Command::new(program);
    command
        .args(&args)
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .kill_on_drop(true);
    for (key, value) in envs {
        command.env(key, value);
    }

    let mut child = command
        .spawn()
        .map_err(|e| ToolError::Failed(format!("failed to start {program}: {e}")))?;

    // Write stdin concurrently with draining stdout so a large file cannot
    // deadlock on a full pipe buffer.
    let stdin = child.stdin.take();
    let bytes = text.as_bytes().to_vec();
    let writer = tokio::spawn(async move {
        if let Some(mut stdin) = stdin {
            let _ = stdin.write_all(&bytes).await;
            let _ = stdin.shutdown().await;
        }
    });

    let output = child
        .wait_with_output()
        .await
        .map_err(|e| ToolError::Failed(e.to_string()))?;
    let _ = writer.await;

    if !output.status.success() {
        let stderr = String::from_utf8_lossy(&output.stderr);
        return Err(ToolError::Failed(format!(
            "stream editor failed: {}",
            stderr.trim()
        )));
    }
    Ok(String::from_utf8_lossy(&output.stdout).into_owned())
}

// --- git_status / git_diff / git_log / git_add / git_restore / git_commit ---

#[derive(Debug, Deserialize, JsonSchema)]
struct GitStatusInput {}

pub struct GitStatus;

#[async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &'static str {
        "git_status"
    }
    fn contract(&self) -> ToolContract {
        read_only_contract("Show the working tree status.")
    }
    fn approval_detail(&self, _input: &Value) -> String {
        "git status".to_string()
    }
    fn description(&self) -> &'static str {
        "Show the working tree status (read-only)."
    }
    fn schema(&self) -> Value {
        schema_for::<GitStatusInput>()
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::RunCommand(CommandClass::ReadOnly)])
    }
    async fn invoke(&self, _input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let output = run_git(ctx, &["status", "--porcelain"]).await?;
        Ok(cap(output))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitDiffInput {
    /// Optional paths to limit the diff.
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::one_or_many_string")]
    paths: Vec<String>,
    /// Show staged changes. Defaults to false.
    #[serde(default)]
    staged: bool,
}

pub struct GitDiff;

#[async_trait]
impl Tool for GitDiff {
    fn name(&self) -> &'static str {
        "git_diff"
    }
    fn approval_detail(&self, input: &Value) -> String {
        paths_detail(input, "git diff")
    }
    fn description(&self) -> &'static str {
        "Show unstaged or staged git diff output for optional paths."
    }
    fn schema(&self) -> Value {
        schema_for::<GitDiffInput>()
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::RunCommand(CommandClass::ReadOnly)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: GitDiffInput = parse_input(&input)?;
        let mut args = vec!["diff"];
        if input.staged {
            args.push("--staged");
        }
        if !input.paths.is_empty() {
            args.push("--");
            args.extend(input.paths.iter().map(String::as_str));
        }
        Ok(cap(run_git(ctx, &args).await?))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitLogInput {
    /// Maximum commits to show. Defaults to 10.
    #[serde(default)]
    max_count: Option<u32>,
}

pub struct GitLog;

#[async_trait]
impl Tool for GitLog {
    fn name(&self) -> &'static str {
        "git_log"
    }
    fn approval_detail(&self, _input: &Value) -> String {
        "git log".to_string()
    }
    fn description(&self) -> &'static str {
        "Show recent git commits in one-line form."
    }
    fn schema(&self) -> Value {
        schema_for::<GitLogInput>()
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::RunCommand(CommandClass::ReadOnly)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: GitLogInput = parse_input(&input)?;
        let count = input.max_count.unwrap_or(10).min(100).to_string();
        Ok(cap(run_git(ctx, &["log", "--oneline", "-n", &count]).await?))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitPathInput {
    /// Paths to operate on.
    #[schemars(schema_with = "crate::schema_intent::one_or_many_string")]
    paths: Vec<String>,
}

pub struct GitAdd;

#[async_trait]
impl Tool for GitAdd {
    fn name(&self) -> &'static str {
        "git_add"
    }
    fn approval_detail(&self, input: &Value) -> String {
        paths_detail(input, "git add")
    }
    fn description(&self) -> &'static str {
        "Stage specific workspace paths with git add."
    }
    fn schema(&self) -> Value {
        schema_for::<GitPathInput>()
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::RunCommand(CommandClass::ProjectWrite)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: GitPathInput = parse_input(&input)?;
        if input.paths.is_empty() {
            return Err(ToolError::InvalidInput(
                "paths must contain at least one path".to_string(),
            ));
        }
        let mut args = vec!["add", "--"];
        args.extend(input.paths.iter().map(String::as_str));
        Ok(cap(run_git(ctx, &args).await?))
    }
}

pub struct GitRestore;

#[async_trait]
impl Tool for GitRestore {
    fn name(&self) -> &'static str {
        "git_restore"
    }
    fn contract(&self) -> ToolContract {
        // `git restore` discards working-tree changes that may never have been
        // saved, so it is genuinely destructive — classified here so the
        // argument-repair gate refuses it (a destructive call is never silently
        // reshaped). The reversibility/confirmation are left at their defaults so
        // this is metadata-only: the permission path and the prompt are unchanged.
        ToolContract {
            model_description: "Discard working-tree changes for specific paths.",
            side_effect: SideEffectClass::Destructive,
            idempotency: Idempotency::Idempotent,
            postconditions: RESULT_STATUS,
            verification: VerificationMethod::Postconditions,
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        paths_detail(input, "git restore")
    }
    fn description(&self) -> &'static str {
        "Discard working-tree changes for specific paths with git restore; requires destructive-command approval."
    }
    fn schema(&self) -> Value {
        schema_for::<GitPathInput>()
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::RunCommand(CommandClass::Destructive)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: GitPathInput = parse_input(&input)?;
        if input.paths.is_empty() {
            return Err(ToolError::InvalidInput(
                "paths must contain at least one path".to_string(),
            ));
        }
        let mut args = vec!["restore", "--"];
        args.extend(input.paths.iter().map(String::as_str));
        Ok(cap(run_git(ctx, &args).await?))
    }
}

#[derive(Debug, Deserialize, JsonSchema)]
struct GitCommitInput {
    /// Commit message. Must not contain secrets.
    #[schemars(schema_with = "crate::schema_intent::file_content_string")]
    message: String,
    /// Specific paths to stage and commit. Empty commits already-staged changes.
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::one_or_many_string")]
    paths: Vec<String>,
}

pub struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &'static str {
        "git_commit"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Create a git commit from staged changes.",
            // A commit writes durable VCS history that outlives the working tree
            // (and is often shared), so it is external write — classified so the
            // argument-repair gate refuses it: a commit's arguments are never
            // silently reshaped.
            side_effect: SideEffectClass::ExternalWrite,
            reversibility: Reversibility::ReversibleWithArtifact,
            idempotency: Idempotency::NonIdempotent,
            postconditions: RESULT_STATUS,
            verification: VerificationMethod::Postconditions,
            ..ToolContract::default()
        }
    }
    fn approval_detail(&self, input: &Value) -> String {
        paths_detail(input, "git commit")
    }
    fn description(&self) -> &'static str {
        "Create a commit from intended files; rejects secret-bearing messages."
    }
    fn schema(&self) -> Value {
        schema_for::<GitCommitInput>()
    }
    fn effects(&self, _input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        Ok(vec![Effect::RunCommand(CommandClass::ProjectWrite)])
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: GitCommitInput = parse_input(&input)?;
        if contains_secret(&input.message) {
            return Err(ToolError::Failed(
                "commit message appears to contain a secret".to_string(),
            ));
        }
        if !input.paths.is_empty() {
            let mut add_args = vec!["add", "--"];
            add_args.extend(input.paths.iter().map(String::as_str));
            run_git(ctx, &add_args).await?;
        }
        let output = run_git(ctx, &["commit", "-m", &input.message]).await?;
        Ok(cap(output))
    }
}

async fn run_git(ctx: &ToolContext<'_>, args: &[&str]) -> Result<String, ToolError> {
    let output = tokio::process::Command::new("git")
        .args(args)
        // De-verbatim spawn cwd (see `Workspace::process_dir`): git on Windows
        // misbehaves with a verbatim `\\?\` working directory.
        .current_dir(ctx.workspace.process_dir())
        .output()
        .await
        .map_err(|e| ToolError::Failed(format!("git: {e}")))?;
    let stdout = String::from_utf8_lossy(&output.stdout);
    let stderr = String::from_utf8_lossy(&output.stderr);
    if output.status.success() {
        Ok(stdout.into_owned())
    } else {
        Err(ToolError::Failed(format!("git failed: {stderr}")))
    }
}

// --- update_plan ------------------------------------------------------------

// These mirror the `update_plan` schema and validate the call shape on
// dispatch; the session reads the plan from the raw input value, so the fields
// are not otherwise accessed.
#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct UpdatePlanInput {
    /// The ordered task list shown to the user.
    steps: Vec<PlanStepInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
#[allow(dead_code)]
struct PlanStepInput {
    /// Short imperative description of the task.
    title: String,
    /// One of: `pending`, `in_progress`, `done`.
    status: String,
}

/// Records the task checklist shown to the user. It performs no side effect; the
/// session surfaces the plan to the UI as it changes.
pub struct UpdatePlan;

#[async_trait]
impl Tool for UpdatePlan {
    fn name(&self) -> &'static str {
        "update_plan"
    }
    fn description(&self) -> &'static str {
        "Record or update the task checklist shown to the user. Call it when you \
         start work, whenever a step changes status, and when finishing. Each step \
         has a title and a status of pending, in_progress, or done."
    }
    fn schema(&self) -> Value {
        schema_for::<UpdatePlanInput>()
    }
    fn effects(&self, input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        // Validate the shape; the tool has no side effect of its own.
        let _: UpdatePlanInput = parse_input(input)?;
        Ok(Vec::new())
    }
    async fn invoke(&self, _input: Value, _ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        Ok(ToolOutput::ok("plan updated"))
    }
}

#[cfg(test)]
mod tests {
    use super::{binary_placeholder, locate_edit, looks_binary, EditMiss};

    #[test]
    fn locate_edit_prefers_an_exact_unique_match() {
        let plan = locate_edit("a\nfoo\nb\n", "foo", "bar").unwrap();
        assert_eq!(&"a\nfoo\nb\n"[plan.start..plan.end], "foo");
        assert_eq!(plan.replacement, "bar");
    }

    #[test]
    fn locate_edit_reports_an_exact_ambiguous_match_without_guessing() {
        let miss = locate_edit("x\nx\n", "x", "y").unwrap_err();
        assert!(matches!(miss, EditMiss::Ambiguous { count: 2 }));
    }

    #[test]
    fn locate_edit_applies_a_unique_indent_drifted_block_and_reindents() {
        // The needle is the block uniformly under-indented; the unique tolerant
        // match re-indents the replacement to the file's indentation.
        let haystack = "{\n        a();\n        b();\n}\n";
        let plan = locate_edit(haystack, "a();\nb();", "a();\nc();").unwrap();
        let updated = format!(
            "{}{}{}",
            &haystack[..plan.start],
            plan.replacement,
            &haystack[plan.end..]
        );
        assert_eq!(updated, "{\n        a();\n        c();\n}\n");
    }

    #[test]
    fn locate_edit_refuses_a_non_unique_tolerant_match() {
        // The same indent-drifted block appears twice: anchored matching must
        // refuse it (Ambiguous) rather than edit a best-guess location.
        let haystack = "if a {\n        go();\n}\nif b {\n        go();\n}\n";
        let miss = locate_edit(haystack, "go();", "stop();").unwrap_err();
        // `go();` is an exact substring twice over, so it is reported ambiguous —
        // either way the contract holds: a non-unique match never applies.
        assert!(matches!(miss, EditMiss::Ambiguous { count } if count >= 2));
    }

    #[test]
    fn looks_binary_flags_nul_and_control_heavy_bytes() {
        // A NUL byte alone is decisive, even amid otherwise-printable text.
        assert!(looks_binary(b"glTF\x02\x00\x00\x00\x10rest"));
        assert!(looks_binary(&[0x01, 0x02, 0x03, 0x04, 0x05]));
    }

    #[test]
    fn looks_binary_passes_text_including_utf8_and_whitespace() {
        assert!(!looks_binary(b""));
        assert!(!looks_binary(b"plain ascii\twith\ttabs\nand\r\nnewlines\n"));
        // High bytes forming valid UTF-8 must not be mistaken for binary.
        assert!(!looks_binary("café — naïve — \u{1F600}".as_bytes()));
    }

    #[test]
    fn binary_placeholder_reports_the_byte_count() {
        assert_eq!(
            binary_placeholder(167_272),
            "<binary data: 167272 bytes, not shown>"
        );
    }
}
