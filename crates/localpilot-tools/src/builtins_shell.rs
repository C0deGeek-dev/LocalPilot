//! The `run_shell` builtin tool: shell command lines and direct program
//! invocations, with classification, timeout, and captured output.
//!
//! Extracted from `builtins.rs` to keep that file focused on the file/edit/
//! search tools. Shares the output cap with the rest of the builtins; everything
//! else here is shell-specific.

use std::path::Path;
use std::time::Duration;

use async_trait::async_trait;
use localpilot_sandbox::{classify, is_secret_like, CommandClass, Effect};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::builtins::{binary_placeholder, cap, looks_binary};
use crate::contract::{
    Idempotency, Postcondition, Reversibility, SideEffectClass, ToolContract, VerificationMethod,
};
use crate::error::ToolError;
use crate::tool::{detail_preview, parse_input, schema_for, Tool, ToolContext, ToolOutput};

#[derive(Debug, Deserialize, JsonSchema)]
struct RunShellInput {
    /// Shell command line to execute through the platform shell.
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::command_string")]
    command: Option<String>,
    /// Program to execute directly when `args` is provided; otherwise treated as a shell command line.
    #[serde(default)]
    #[schemars(schema_with = "crate::schema_intent::command_string")]
    program: Option<String>,
    /// Arguments passed to `program` for direct execution.
    #[serde(default)]
    args: Vec<String>,
    /// Timeout in seconds. Defaults to 60.
    #[serde(default)]
    timeout_secs: Option<u64>,
}

const DEFAULT_TIMEOUT_SECS: u64 = 60;

pub(crate) enum RunShellExecution {
    Direct { program: String, args: Vec<String> },
    Shell { command: String },
}

struct NormalizedRunShellInput {
    execution: RunShellExecution,
    timeout_secs: Option<u64>,
}

/// Resolve the `command` / `program` + `args` fields shared by `run_shell` and
/// `run_background` into a single execution: a `command` (or a bare `program`
/// with no args) runs through the platform shell; a `program` with `args` runs
/// directly. Returns [`ToolError::InvalidInput`] when neither is usable.
pub(crate) fn normalize_execution(
    command: Option<String>,
    program: Option<String>,
    args: Vec<String>,
) -> Result<RunShellExecution, ToolError> {
    if let Some(command) = command {
        let command = command.trim().to_string();
        if command.is_empty() {
            return Err(ToolError::InvalidInput(
                "command must not be empty".to_string(),
            ));
        }
        return Ok(RunShellExecution::Shell { command });
    }

    let Some(program) = program else {
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

    if args.is_empty() {
        return Ok(RunShellExecution::Shell { command: program });
    }

    Ok(RunShellExecution::Direct { program, args })
}

fn normalize_run_shell_input(input: RunShellInput) -> Result<NormalizedRunShellInput, ToolError> {
    Ok(NormalizedRunShellInput {
        execution: normalize_execution(input.command, input.program, input.args)?,
        timeout_secs: input.timeout_secs,
    })
}

pub(crate) fn split_command_line(command: &str) -> Result<Vec<String>, ToolError> {
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

pub(crate) fn execution_text(execution: &RunShellExecution) -> String {
    match execution {
        RunShellExecution::Direct { program, args } => command_text(program, args),
        RunShellExecution::Shell { command } => command.clone(),
    }
}

pub(crate) fn execution_class(execution: &RunShellExecution) -> Result<CommandClass, ToolError> {
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

/// The candidate file-path arguments of an execution: the non-flag tokens after
/// the program. Used to gate a read-only command that reads a secret-like or
/// out-of-workspace file. `-`-prefixed flags are skipped; everything else is
/// treated as a possible path (a `/`-prefixed token is a POSIX absolute path, not
/// a flag — an inline Windows shell never reaches here, it is `unknown`).
fn command_path_args(execution: &RunShellExecution) -> Vec<String> {
    let tokens = match execution {
        RunShellExecution::Direct { args, .. } => args.clone(),
        RunShellExecution::Shell { command } => {
            let mut parts = split_command_line(command).unwrap_or_default();
            if !parts.is_empty() {
                parts.remove(0); // drop the program
            }
            parts
        }
    };
    tokens
        .into_iter()
        .filter(|token| !token.starts_with('-'))
        .collect()
}

fn command_output(code: i32, stdout: &str, stderr: &str) -> String {
    format!("exit: {code}\n--- stdout ---\n{stdout}\n--- stderr ---\n{stderr}")
}

/// Render a captured stream as text, substituting a placeholder when the bytes
/// look binary so raw control bytes never reach the model context.
pub(crate) fn render_stream(bytes: &[u8]) -> String {
    if looks_binary(bytes) {
        binary_placeholder(bytes.len())
    } else {
        String::from_utf8_lossy(bytes).into_owned()
    }
}

pub(crate) fn shell_program_and_args(command: &str) -> (String, Vec<String>) {
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

/// Program basenames that are long-running servers/watchers whenever they lead a
/// command, regardless of arguments (`nodemon app.js`, `serve ./dist`).
const ALWAYS_SERVER_PROGRAMS: &[&str] =
    &["nodemon", "caddy", "serve", "http-server", "live-server"];

/// Dev tools that are long-running only for their server subcommands — they also
/// have one-shot subcommands (`vite build`, `next build`) that must not match.
const DEV_TOOL_PROGRAMS: &[&str] = &["vite", "next", "nuxt"];

/// Package managers and runtimes whose `dev`/`start`/`serve`/`watch` script (or
/// `run`/`task` script of that name) starts a long-running process.
const PACKAGE_LAUNCHERS: &[&str] = &[
    "npm", "pnpm", "yarn", "bun", "deno", "npx", "pnpx", "bunx", "node",
];

/// Script/subcommand names that denote a long-running process.
const LONG_RUNNING_SCRIPTS: &[&str] = &["dev", "start", "serve", "watch"];

/// The basename of `program`, lowercased and stripped of a Windows launcher
/// extension, so `C:\\tools\\bun.exe` and `bun` compare equal.
fn program_stem(program: &str) -> String {
    let base = program.rsplit(['/', '\\']).next().unwrap_or(program);
    let base = base
        .strip_suffix(".exe")
        .or_else(|| base.strip_suffix(".cmd"))
        .or_else(|| base.strip_suffix(".bat"))
        .unwrap_or(base);
    base.to_lowercase()
}

/// The lowercased program-and-argument tokens of an execution.
fn execution_tokens(execution: &RunShellExecution) -> Vec<String> {
    let raw = match execution {
        RunShellExecution::Direct { program, args } => {
            let mut v = Vec::with_capacity(args.len() + 1);
            v.push(program.clone());
            v.extend(args.iter().cloned());
            v
        }
        RunShellExecution::Shell { command } => split_command_line(command).unwrap_or_default(),
    };
    raw.into_iter().map(|t| t.to_lowercase()).collect()
}

/// Whether an execution looks like a long-running server or watcher — a command
/// that does not exit on its own and so belongs in `run_background` rather than
/// `run_shell`. Deliberately conservative: it recognizes clear dev-server and
/// watch launchers and leaves ambiguous cases (e.g. `bun run index.ts`) to the
/// `run_shell` timeout path, so a short command is never wrongly diverted.
pub(crate) fn looks_long_running(execution: &RunShellExecution) -> bool {
    let tokens = execution_tokens(execution);
    let Some((first, rest)) = tokens.split_first() else {
        return false;
    };
    let stem = program_stem(first);

    // A persistent `--watch` flag anywhere keeps any of these commands alive.
    if rest.iter().any(|t| t == "--watch" || t == "-w") {
        return true;
    }

    // An always-server launcher leading the command.
    if ALWAYS_SERVER_PROGRAMS.contains(&stem.as_str()) {
        return true;
    }

    // The first non-flag argument after the launcher: the script or subcommand.
    // `run`/`task` indirection (`npm run dev`, `deno task dev`) is unwrapped to
    // the script name that follows.
    let non_flags: Vec<&str> = rest
        .iter()
        .map(String::as_str)
        .filter(|t| !t.starts_with('-'))
        .collect();
    let script = match non_flags.first() {
        Some(&("run" | "task")) => non_flags.get(1).copied(),
        other => other.copied(),
    };

    if DEV_TOOL_PROGRAMS.contains(&stem.as_str()) {
        // `vite` / `next` / `nuxt`: a server subcommand, or bare (defaults to dev).
        return script.is_none_or(|s| LONG_RUNNING_SCRIPTS.contains(&s));
    }

    if PACKAGE_LAUNCHERS.contains(&stem.as_str()) {
        return script.is_some_and(|s| LONG_RUNNING_SCRIPTS.contains(&s));
    }

    false
}

pub struct RunShell;

#[async_trait]
impl Tool for RunShell {
    fn name(&self) -> &'static str {
        "run_shell"
    }
    fn contract(&self) -> ToolContract {
        ToolContract {
            model_description: "Run a shell command line or program in the workspace.",
            // A shell command can do anything, so the contract is conservative:
            // potentially destructive, not automatically reversible, and its
            // general effects cannot be cheaply verified beyond exit status.
            side_effect: SideEffectClass::Destructive,
            reversibility: Reversibility::Irreversible,
            idempotency: Idempotency::Unknown,
            postconditions: &[Postcondition::ResultStatus],
            verification: VerificationMethod::Unverifiable,
            ..ToolContract::default()
        }
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
    fn effects(&self, input: &Value, ctx: &ToolContext<'_>) -> Result<Vec<Effect>, ToolError> {
        let input: RunShellInput = parse_input(input)?;
        let input = normalize_run_shell_input(input)?;
        let class = execution_class(&input.execution)?;
        let mut effects = vec![Effect::RunCommand(class)];
        if class == CommandClass::Network {
            effects.push(Effect::Network);
        }
        // A command carries no contained path, so a `read-only` command
        // (`cat`/`type`/`head`) reading a secret or an out-of-workspace file would
        // otherwise be auto-allowed and pull it into model context. Add a
        // `ReadPath` effect for each such argument so the permission engine gates
        // it exactly like the file tools. Best-effort and conservative: ordinary
        // in-workspace reads add nothing.
        if class == CommandClass::ReadOnly {
            for arg in command_path_args(&input.execution) {
                let path = Path::new(&arg);
                let secret = is_secret_like(path);
                let inside = ctx.workspace.contains(path);
                if secret || !inside {
                    effects.push(Effect::ReadPath {
                        inside_workspace: inside,
                        secret_like: secret,
                    });
                }
            }
        }
        Ok(effects)
    }
    async fn invoke(&self, input: Value, ctx: &ToolContext<'_>) -> Result<ToolOutput, ToolError> {
        let input: RunShellInput = parse_input(&input)?;
        let input = normalize_run_shell_input(input)?;

        // A recognized dev-server or watcher never exits, so running it here
        // would only block until the timeout and then kill it. Point the model at
        // `run_background` instead of waiting. Ambiguous commands are not diverted
        // here — the timeout path below carries the same hint if they hang.
        if looks_long_running(&input.execution) {
            let detail = execution_text(&input.execution);
            return Ok(ToolOutput::ok(format!(
                "`{detail}` looks like a long-running server or watcher, which would \
                 block this call until it timed out. Start it with the `run_background` \
                 tool instead; then use `run_background` again to read its logs or stop it."
            )));
        }

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
                    "command timed out after {}s. If this is a long-running server or \
                     watcher, start it with the `run_background` tool instead.",
                    timeout.as_secs()
                )))
            }
        };

        let stdout = render_stream(&output.stdout);
        let stderr = render_stream(&output.stderr);
        let code = output.status.code().unwrap_or(-1);
        let text = command_output(code, &stdout, &stderr);
        let mut result = cap(text);
        result.is_error = !output.status.success();
        Ok(result)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use localpilot_sandbox::{Interactivity, Workspace};
    use serde_json::json;

    fn effects_of(value: Value) -> Vec<Effect> {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::Interactive,
            trusted: true,
            retention: None,
            processes: None,
        };
        RunShell.effects(&value, &ctx).unwrap()
    }

    fn reads_a_secret(effects: &[Effect]) -> bool {
        effects.iter().any(|e| {
            matches!(
                e,
                Effect::ReadPath {
                    secret_like: true,
                    ..
                }
            )
        })
    }

    fn reads_outside_workspace(effects: &[Effect]) -> bool {
        effects.iter().any(|e| {
            matches!(
                e,
                Effect::ReadPath {
                    inside_workspace: false,
                    ..
                }
            )
        })
    }

    #[test]
    fn a_read_only_command_reading_a_secret_is_gated() {
        // `cat`/`type` of a credential file would otherwise be auto-allowed and
        // pull the secret into model context. It now carries a gated ReadPath.
        assert!(reads_a_secret(&effects_of(
            json!({ "program": "cat", "args": [".env"] })
        )));
        assert!(reads_a_secret(&effects_of(
            json!({ "program": "cat", "args": ["/home/u/.ssh/id_rsa"] })
        )));
        // The shell-string form is gated too.
        assert!(reads_a_secret(&effects_of(
            json!({ "command": "cat .env" })
        )));
    }

    #[test]
    fn a_read_only_command_reading_outside_the_workspace_is_gated() {
        let outside = if cfg!(windows) {
            "C:/Windows/System32/drivers/etc/hosts"
        } else {
            "/etc/hosts"
        };
        assert!(reads_outside_workspace(&effects_of(
            json!({ "program": "cat", "args": [outside] })
        )));
    }

    #[test]
    fn an_ordinary_in_workspace_read_is_not_gated() {
        let effects = effects_of(json!({ "program": "cat", "args": ["src/main.rs"] }));
        assert!(!reads_a_secret(&effects));
        assert!(!reads_outside_workspace(&effects));
        // Only the RunCommand effect — no added prompt for a routine read.
        assert_eq!(effects.len(), 1);
    }

    fn long_running(command: &str) -> bool {
        looks_long_running(&RunShellExecution::Shell {
            command: command.to_string(),
        })
    }

    #[test]
    fn dev_servers_and_watchers_are_recognized_as_long_running() {
        for command in [
            "npm run dev",
            "pnpm dev",
            "yarn start",
            "npm run start",
            "bun serve",
            "bun dev",
            "bun run dev",
            "deno task dev",
            "vite",
            "next dev",
            "nuxt dev",
            "nodemon app.js",
            "serve ./dist",
            "jest --watch",
        ] {
            assert!(long_running(command), "`{command}` should be long-running");
        }
    }

    #[test]
    fn one_shot_commands_are_not_recognized_as_long_running() {
        for command in [
            "npm run build",
            "npm install",
            "bun run index.ts",
            "vite build",
            "next build",
            "ls -la",
            "cargo test",
            "echo hello",
            "git status",
        ] {
            assert!(
                !long_running(command),
                "`{command}` should not be diverted to run_background"
            );
        }
    }

    #[test]
    fn recognition_handles_direct_program_form_and_windows_extensions() {
        assert!(looks_long_running(&RunShellExecution::Direct {
            program: "npm".to_string(),
            args: vec!["run".to_string(), "dev".to_string()],
        }));
        assert!(looks_long_running(&RunShellExecution::Direct {
            program: "C:\\tools\\bun.exe".to_string(),
            args: vec!["serve".to_string()],
        }));
    }
}
