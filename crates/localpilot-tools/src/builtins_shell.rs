//! The `run_shell` builtin tool: shell command lines and direct program
//! invocations, with classification, timeout, and captured output.
//!
//! Extracted from `builtins.rs` to keep that file focused on the file/edit/
//! search tools. Shares the output cap with the rest of the builtins; everything
//! else here is shell-specific.

use std::time::Duration;

use async_trait::async_trait;
use localpilot_sandbox::{classify, CommandClass, Effect};
use schemars::JsonSchema;
use serde::Deserialize;
use serde_json::Value;

use crate::builtins::cap;
use crate::error::ToolError;
use crate::tool::{detail_preview, parse_input, schema_for, Tool, ToolContext, ToolOutput};

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
