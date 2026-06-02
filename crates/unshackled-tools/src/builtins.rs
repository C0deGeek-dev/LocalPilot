//! The eight builtin tools.

use std::path::{Path, PathBuf};
use std::time::Duration;

use async_trait::async_trait;
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;
use unshackled_config::redact::contains_secret;
use unshackled_sandbox::{classify, is_secret_like, CommandClass, Effect};

use crate::error::ToolError;
use crate::tool::{parse_input, schema_for, Tool, ToolContext, ToolOutput};

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
        let existing = std::fs::read_to_string(&path).ok();
        if existing.is_some() && input.overwrite == Some(false) {
            return Err(ToolError::Failed(format!(
                "{} exists and overwrite is false",
                path.display()
            )));
        }
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

// --- run_shell --------------------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct RunShellInput {
    /// The program to execute (no shell interpretation).
    program: String,
    /// Arguments passed as a list, not a shell string.
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in seconds. Defaults to 60.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const DEFAULT_TIMEOUT_SECS: u64 = 60;

pub struct RunShell;

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        "run_shell"
    }
    fn description(&self) -> &'static str {
        "Run a command as an argument list (no shell), with a timeout."
    }
    fn schema(&self) -> Value {
        schema_for::<RunShellInput>()
    }
    fn effects(&self, input: &Value, _ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: RunShellInput = parse_input(input)?;
        let class = classify(&input.program, &input.args);
        let mut effects = vec![Effect::RunCommand(class)];
        if class == CommandClass::Network {
            effects.push(Effect::Network);
        }
        Ok(effects)
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: RunShellInput = parse_input(&input)?;
        let timeout = Duration::from_secs(input.timeout_secs.unwrap_or(DEFAULT_TIMEOUT_SECS));

        let mut command = tokio::process::Command::new(&input.program);
        command
            .args(&input.args)
            .current_dir(ctx.workspace.root())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            .kill_on_drop(true);

        let child = command
            .spawn()
            .map_err(|e| ToolError::Failed(format!("failed to start {}: {e}", input.program)))?;
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
        let text = format!("exit: {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}");
        let mut result = cap(text);
        result.is_error = !output.status.success();
        Ok(result)
    }
}

// --- git_status / git_commit ------------------------------------------------

#[derive(Debug, Deserialize, JsonSchema)]
struct GitStatusInput {}

pub struct GitStatus;

#[async_trait]
impl Tool for GitStatus {
    fn name(&self) -> &'static str {
        "git_status"
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
