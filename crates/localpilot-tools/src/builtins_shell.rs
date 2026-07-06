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

#[cfg(windows)]
use crate::builtins::CREATE_NO_WINDOW;
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
        // Prefer PowerShell 7+ (`pwsh`) when it is on PATH: it supports the
        // `&&`/`||` pipeline-chain operators that Windows PowerShell 5.1
        // (`powershell.exe`) does not, so a chained command the model emits
        // (`cargo build && cargo test`) actually runs instead of erroring — which
        // is what taught the model junk "PowerShell doesn't support `&&`" lessons.
        // Fall back to the always-present `powershell.exe` when `pwsh` is absent;
        // this is "prefer", not "require". Both take the same flags.
        let program = windows_shell_program();
        (
            program.to_string(),
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

/// The Windows shell program to wrap a command with: `pwsh.exe` (PowerShell 7+)
/// when it is on PATH, else `powershell.exe` (Windows PowerShell 5.1, always
/// present). The detection is run once and cached for the process — PATH does not
/// change mid-session and a per-command PATH scan would be wasteful.
#[cfg(windows)]
fn windows_shell_program() -> &'static str {
    use std::sync::OnceLock;
    static SHELL: OnceLock<&'static str> = OnceLock::new();
    SHELL.get_or_init(|| {
        if executable_on_path("pwsh.exe") {
            "pwsh.exe"
        } else {
            "powershell.exe"
        }
    })
}

/// Whether `exe` is found in one of the `PATH` directories. A plain filesystem
/// lookup — no process is spawned to probe for it.
#[cfg(windows)]
fn executable_on_path(exe: &str) -> bool {
    let Some(path) = std::env::var_os("PATH") else {
        return false;
    };
    std::env::split_paths(&path).any(|dir| dir.join(exe).is_file())
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
            // The de-verbatim spawn cwd: a launched shell cannot `cd` into the
            // verbatim `\\?\…` containment root, so it must run in `process_dir()`.
            .current_dir(ctx.workspace.process_dir())
            // Never inherit the interactive console's stdin: the child (and its
            // grandchildren under the shell wrapper) would share the TUI's input
            // handle, and any of them that reads stdin steals the user's
            // keystrokes — including the Ctrl+C key event raw mode relies on.
            // A command that needs input gets immediate EOF instead of hanging.
            .stdin(std::process::Stdio::null())
            .stdout(std::process::Stdio::piped())
            .stderr(std::process::Stdio::piped())
            // We reap the whole process tree explicitly on timeout (below). Do not
            // let a dropped future kill only the immediate child: on Windows that
            // kills the shell wrapper first and orphans the grandchild that is the
            // real workload (a hung test run can hold gigabytes).
            .kill_on_drop(false);
        // On Unix, lead a new process group so a timeout can signal the whole tree
        // with a single negative-pid kill, even after the group leader exits. The
        // new group is also never the terminal's foreground group, so a child
        // that opens `/dev/tty` directly gets SIGTTIN/SIGTTOU instead of the
        // TUI's keystrokes or terminal modes.
        #[cfg(unix)]
        command.process_group(0);
        // On Windows, give the child its own invisible console instead of
        // attaching it to the TUI's. A shared console lets any child (or
        // grandchild) read the console input buffer via CONIN$ — stealing
        // keystrokes even with a null stdin — or re-cook the shared console
        // mode with SetConsoleMode, breaking raw mode and the keyboard
        // protocol out from under crossterm.
        #[cfg(windows)]
        command.creation_flags(CREATE_NO_WINDOW);

        let child = command
            .spawn()
            .map_err(|e| ToolError::Failed(format!("failed to start {program}: {e}")))?;
        let pid = child.id();
        let output = match tokio::time::timeout(timeout, child.wait_with_output()).await {
            Ok(Ok(output)) => output,
            Ok(Err(e)) => {
                if let Some(pid) = pid {
                    kill_process_tree(pid).await;
                }
                return Err(ToolError::Failed(e.to_string()));
            }
            Err(_) => {
                if let Some(pid) = pid {
                    kill_process_tree(pid).await;
                }
                return Err(ToolError::Failed(format!(
                    "command timed out after {}s. If this is a long-running server or \
                     watcher, start it with the `run_background` tool instead.",
                    timeout.as_secs()
                )));
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

/// Kill a timed-out command's entire process tree. `kill_on_drop` reaps only the
/// immediate child; a shell-wrapped command (`cmd /c` / `sh -c`) leaves its real
/// workload as a grandchild that would otherwise orphan and leak — a hung test
/// run or runaway model solution can hold gigabytes for the rest of the session.
/// On Windows `taskkill /T` walks and force-kills the child tree; on Unix the
/// child leads its own process group (set at spawn), so a negative pid signals
/// the whole group even after the leader exits. Best-effort: an unreapable
/// process is the OS's to report, never surfaced as a tool error.
async fn kill_process_tree(pid: u32) {
    #[cfg(windows)]
    {
        let _ = tokio::process::Command::new("taskkill")
            .args(["/T", "/F", "/PID", &pid.to_string()])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(unix)]
    {
        let _ = tokio::process::Command::new("kill")
            .args(["-KILL", &format!("-{pid}")])
            .stdout(std::process::Stdio::null())
            .stderr(std::process::Stdio::null())
            .status()
            .await;
    }
    #[cfg(not(any(windows, unix)))]
    {
        let _ = pid;
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

    #[tokio::test]
    async fn run_shell_times_out_and_reaps_a_hung_command() {
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::Interactive,
            trusted: true,
            retention: None,
            processes: None,
        };
        // A command that sleeps well past the 1s timeout: the timeout path must
        // return a "timed out" error and exercise the tree-reap (kill_process_tree
        // runs in this branch). Not a dev-server/watcher, so it is not diverted.
        #[cfg(windows)]
        let command = "ping 127.0.0.1 -n 10";
        #[cfg(unix)]
        let command = "sleep 10";
        let input = json!({ "command": command, "timeout_secs": 1 });
        let err = RunShell.invoke(input, &ctx).await.unwrap_err();
        assert!(
            format!("{err:?}").contains("timed out"),
            "expected a timeout error, got {err:?}"
        );
    }

    #[tokio::test]
    async fn run_shell_never_hands_the_interactive_stdin_to_the_child() {
        // `sort` with no input file reads stdin until EOF. With an inherited
        // interactive stdin the child would sit consuming the console's input
        // — stealing the TUI's keystrokes — until the timeout reaped it. With
        // stdin nulled it sees immediate EOF and exits at once.
        let dir = tempfile::tempdir().unwrap();
        let ws = Workspace::new(dir.path()).unwrap();
        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::Interactive,
            trusted: true,
            retention: None,
            processes: None,
        };
        let input = json!({ "program": "sort", "timeout_secs": 10 });
        let output = RunShell
            .invoke(input, &ctx)
            .await
            .expect("sort on a null stdin must exit promptly, not hit the timeout");
        assert!(
            !output.is_error,
            "sort on a null stdin should exit cleanly: {}",
            output.text
        );
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
