//! The builtin tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use localpilot_config::redact::contains_secret;
use localpilot_sandbox::{classify, is_secret_like, CommandClass, Effect};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

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

fn cap(text: String) -> ToolOutput {
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

// --- read_file --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct ReadFileInput {
    /// Workspace-relative or absolute path to read.
    path: String,
    /// First line to include (1-based, inclusive).
    #[serde(default)]
    start_line: Option<usize>,
    /// Last line to include (1-based, inclusive).
    #[serde(default)]
    end_line: Option<usize>,
}

pub struct ReadFile;

#[async_trait]
impl Tool for ReadFile {
    fn name(&self) -> &'static str {
        "read_file"
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
        let text = std::fs::read_to_string(&path)
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
    path: String,
    /// File contents.
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

// --- edit_file --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct EditFileInput {
    /// Path to edit within the workspace.
    path: String,
    /// Exact text to replace; must match exactly once.
    old_text: String,
    /// Replacement text.
    new_text: String,
}

pub struct EditFile;

#[async_trait]
impl Tool for EditFile {
    fn name(&self) -> &'static str {
        "edit_file"
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
        let matches = content.matches(&input.old_text).count();
        match matches {
            0 => Err(ToolError::Failed("old_text was not found".to_string())),
            1 => {
                let updated = content.replacen(&input.old_text, &input.new_text, 1);
                let newline = detect_newline(&content);
                atomic_write(&path, apply_newline(&updated, newline).as_bytes())?;
                Ok(ToolOutput::ok(format!("edited {}", path.display())))
            }
            n => Err(ToolError::Failed(format!(
                "ambiguous edit: old_text matches {n} times; provide a unique snippet"
            ))),
        }
    }
}

// --- multi_edit -------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct MultiEditInput {
    /// Path to edit within the workspace.
    path: String,
    /// Ordered exact-text replacements. Each `old_text` must match exactly once
    /// at the point that edit is applied.
    edits: Vec<TextEditInput>,
}

#[derive(Debug, Deserialize, JsonSchema)]
struct TextEditInput {
    /// Exact text to replace.
    old_text: String,
    /// Replacement text.
    new_text: String,
}

pub struct MultiEdit;

#[async_trait]
impl Tool for MultiEdit {
    fn name(&self) -> &'static str {
        "multi_edit"
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
        let mut updated = original.clone();
        for (index, edit) in input.edits.iter().enumerate() {
            let matches = updated.matches(&edit.old_text).count();
            match matches {
                0 => {
                    return Err(ToolError::Failed(format!(
                        "edit {} failed: old_text was not found",
                        index + 1
                    )))
                }
                1 => updated = updated.replacen(&edit.old_text, &edit.new_text, 1),
                n => {
                    return Err(ToolError::Failed(format!(
                        "edit {} failed: old_text matches {n} times",
                        index + 1
                    )))
                }
            }
        }
        let newline = detect_newline(&original);
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
    pattern: String,
    /// Directory to search, relative to the workspace. Defaults to the root.
    #[serde(default)]
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
                    let mut updated = original.clone();
                    for (hunk_index, hunk) in hunks.iter().enumerate() {
                        match updated.matches(&hunk.old_text).count() {
                            0 => {
                                return Err(ToolError::Failed(format!(
                                    "{label}: hunk {} old_text was not found; \
                                     re-read the file and resend the patch",
                                    hunk_index + 1
                                )))
                            }
                            1 => {
                                updated = updated.replacen(&hunk.old_text, &hunk.new_text, 1);
                            }
                            n => {
                                return Err(ToolError::Failed(format!(
                                    "{label}: hunk {} old_text matches {n} times; \
                                     provide a unique snippet",
                                    hunk_index + 1
                                )))
                            }
                        }
                    }
                    let newline = detect_newline(&original);
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
    start_line: Option<usize>,
    /// Last line to include (1-based, inclusive).
    #[serde(default)]
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

// --- run_shell --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct RunShellInput {
    /// Shell command line to execute through the platform shell.
    #[serde(default)]
    command: Option<String>,
    /// Program to execute directly when `args` is provided; otherwise treated as a shell command line.
    #[serde(default)]
    program: Option<String>,
    /// Arguments passed to `program` for direct execution.
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in seconds. Defaults to 60.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const DEFAULT_TIMEOUT_SECS: u64 = 60;

enum RunShellExecution {
    Direct { program: String, args: Vec<String> },
    Shell { command: String },
}

struct NormalizedRunShellInput {
    execution: RunShellExecution,
    timeout_secs: Option<u64>,
}

fn normalize_run_shell_input(input: RunShellInput) -> Result<NormalizedRunShellInput, ToolError> {
    if let Some(command) = input.command {
        let command = command.trim().to_string();
        if command.is_empty() {
            return Err(ToolError::InvalidInput(
                "command must not be empty".to_string(),
            ));
        }
        return Ok(NormalizedRunShellInput {
            execution: RunShellExecution::Shell { command },
            timeout_secs: input.timeout_secs,
        });
    }

    let Some(program) = input.program else {
        return Err(ToolError::InvalidInput(
            "expected `command` or `program`".to_string(),
        ));
    };
    let program = program.trim().to_string();
    if program.is_empty() {
        return Err(ToolError::InvalidInput(
            "program must not be empty".to_string(),
        ));
    }

    if input.args.is_empty() {
        return Ok(NormalizedRunShellInput {
            execution: RunShellExecution::Shell { command: program },
            timeout_secs: input.timeout_secs,
        });
    }

    Ok(NormalizedRunShellInput {
        execution: RunShellExecution::Direct {
            program,
            args: input.args,
        },
        timeout_secs: input.timeout_secs,
    })
}

fn split_command_line(command: &str) -> Result<Vec<String>, ToolError> {
    let mut parts = Vec::new();
    let mut current = String::new();
    let mut quote = None;

    for ch in command.trim().chars() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (None, '"' | '\'') => quote = Some(ch),
            (None, c) if c.is_whitespace() => {
                if !current.is_empty() {
                    parts.push(std::mem::take(&mut current));
                }
            }
            _ => current.push(ch),
        }
    }

    if quote.is_some() {
        return Err(ToolError::Failed(
            "malformed command line: unterminated quote".to_string(),
        ));
    }
    if !current.is_empty() {
        parts.push(current);
    }
    Ok(parts)
}

fn command_text(program: &str, args: &[String]) -> String {
    if args.is_empty() {
        program.to_string()
    } else {
        format!("{program} {}", args.join(" "))
    }
}

fn execution_text(execution: &RunShellExecution) -> String {
    match execution {
        RunShellExecution::Direct { program, args } => command_text(program, args),
        RunShellExecution::Shell { command } => command.clone(),
    }
}

fn execution_class(execution: &RunShellExecution) -> Result<CommandClass, ToolError> {
    match execution {
        RunShellExecution::Direct { program, args } => Ok(classify(program, args)),
        RunShellExecution::Shell { command } => classify_shell_command(command),
    }
}

fn classify_shell_command(command: &str) -> Result<CommandClass, ToolError> {
    let parts = split_command_line(command)?;
    let Some((program, args)) = parts.split_first() else {
        return Err(ToolError::InvalidInput(
            "command must not be empty".to_string(),
        ));
    };
    if has_shell_metachar(command) {
        return Ok(CommandClass::Unknown);
    }
    Ok(classify(program, args))
}

fn has_shell_metachar(command: &str) -> bool {
    command.chars().any(|ch| {
        matches!(
            ch,
            '\n' | '\r'
                | '|'
                | '&'
                | ';'
                | '<'
                | '>'
                | '('
                | ')'
                | '`'
                | '$'
                | '*'
                | '?'
                | '{'
                | '}'
                | '['
                | ']'
                | '%'
                | '!'
        )
    })
}

fn command_output(code: i32, stdout: &str, stderr: &str) -> String {
    format!("exit: {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
}

fn shell_program_and_args(command: &str) -> (String, Vec<String>) {
    #[cfg(windows)]
    {
        (
            "powershell.exe".to_string(),
            vec![
                "-NoProfile".to_string(),
                "-NonInteractive".to_string(),
                "-Command".to_string(),
                command.to_string(),
            ],
        )
    }
    #[cfg(not(windows))]
    {
        let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".to_string());
        (shell, vec!["-lc".to_string(), command.to_string()])
    }
}

pub struct RunShell;

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        "run_shell"
    }
    fn approval_detail(&self, input: &Value) -> String {
        // The user must see the full command line they are approving.
        parse_input(input)
            .and_then(normalize_run_shell_input)
            .map(|input| detail_preview(&execution_text(&input.execution)))
            .unwrap_or_default()
    }
    fn description(&self) -> &'static str {
        "Run a shell command or direct program invocation with a timeout."
    }
    fn schema(&self) -> Value {
        schema_for::<RunShellInput>()
    }
    fn effects(&self, input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: RunShellInput = parse_input(input)?;
        let input = normalize_run_shell_input(input)?;
        let class = execution_class(&input.execution)?;
        let mut effects = vec![Effect::RunCommand(class)];
        if class == CommandClass::Network {
            effects.push(Effect::Network);
        }
        Ok(effects)
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: RunShellInput = parse_input(&input)?;
        let input = normalize_run_shell_input(input)?;
        let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let (program, args) = match input.execution {
            RunShellExecution::Direct { program, args } => (program, args),
            RunShellExecution::Shell { command } => shell_program_and_args(&command),
        };

        let mut command = tokio::process::Command::new(&program);
        command
            .args(&args)
            .current_dir(ctx.workspace.root())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|e| ToolError::Failed(format!("failed to start {program}: {e}")))?;
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => return Err(ToolError::Failed(e.to_string())),
            Err(_) => {
                return Err(ToolError::Failed(format!(
                    "command timed out after {}s",
                    timeout.as_secs()
                )))
            }
        };

        let stdout = String::from_utf8_lossy(&output.stdout);
        let stderr = String::from_utf8_lossy(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        let text = command_output(code, &stdout, &stderr);
        let mut result = cap(text);
        result.is_error = !output.status.success();
        Ok(result)
    }
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
    message: String,
    /// Specific paths to stage and commit. Empty commits already-staged changes.
    #[serde(default)]
    paths: Vec<String>,
}

pub struct GitCommit;

#[async_trait]
impl Tool for GitCommit {
    fn name(&self) -> &'static str {
        "git_commit"
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
        .current_dir(ctx.workspace.root())
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
