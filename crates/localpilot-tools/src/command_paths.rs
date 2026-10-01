//! Inspectable command targets share the file tools' effects. This is an audit
//! boundary, not an OS sandbox: executed build tools and approved code still
//! have the user's process authority. Dynamic syntax never proves a safe target.

use std::path::Path;

use localpilot_sandbox::{CommandClass, Effect};

use crate::builtins::{read_path_effect, write_path_effect};
use crate::builtins_shell::{execution_class, RunShellExecution};
use crate::{ToolContext, ToolError};

pub(crate) fn effects(
    execution: &RunShellExecution,
    ctx: &ToolContext<'_>,
) -> Result<Vec<Effect>, ToolError> {
    let class = execution_class(execution)?;
    let mut effects = vec![Effect::RunCommand(class)];
    if class == CommandClass::Network {
        effects.push(Effect::Network);
    }
    let (tokens, shell, mut opaque) = match execution {
        RunShellExecution::Direct { program, args } => {
            let mut tokens = vec![program.clone()];
            tokens.extend(args.iter().cloned());
            (tokens, false, false)
        }
        RunShellExecution::Shell { command } => {
            let (tokens, opaque) = literal_tokens(command);
            (tokens, true, opaque)
        }
    };
    let mut args = Vec::new();
    let mut tokens = tokens.into_iter();
    let program = tokens.next().unwrap_or_default();
    while let Some(token) = tokens.next() {
        if shell && matches!(token.as_str(), ">" | ">>" | "<") {
            match tokens.next() {
                Some(target) if literal_target(&target) => {
                    add_path(&mut effects, &target, token != "<", ctx);
                }
                _ => opaque = true,
            }
        } else {
            args.push(token);
        }
    }
    let stem = Path::new(&program)
        .file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(&program)
        .to_ascii_lowercase();
    opaque |= opaque_arguments(&stem, &args);
    opaque |= !inspect_output_options(&stem, &args, &mut effects, ctx);
    if !opaque {
        opaque = !inspect_args(&stem, &args, class, &mut effects, ctx);
    }
    if opaque {
        visible_path_args(&stem, &args, class, &mut effects, ctx);
    }
    if opaque && !effects.contains(&Effect::UnscopedCommand) {
        effects.push(Effect::UnscopedCommand);
    }
    Ok(effects)
}

fn add_path(effects: &mut Vec<Effect>, target: &str, write: bool, ctx: &ToolContext<'_>) {
    let path = Path::new(target);
    // Normalize first, as file tools do: resolved secret names cannot hide
    // behind an innocuous symlink spelling.
    let normalized = ctx
        .workspace
        .normalize(path)
        .unwrap_or_else(|_| path.to_path_buf());
    effects.push(if write {
        write_path_effect(ctx, &normalized, normalized.exists())
    } else {
        read_path_effect(ctx, &normalized)
    });
}

fn inspect_args(
    stem: &str,
    args: &[String],
    class: CommandClass,
    effects: &mut Vec<Effect>,
    ctx: &ToolContext<'_>,
) -> bool {
    if matches!(stem, "echo" | "printf" | "write-output") {
        return true; // data, with redirects already inspected
    }
    if matches!(
        stem,
        "set-content" | "add-content" | "out-file" | "new-item"
    ) {
        return inspect_powershell_writer(stem, args, effects, ctx);
    }
    if matches!(
        stem,
        "touch" | "mkdir" | "rmdir" | "rm" | "tee" | "del" | "erase" | "md" | "rd"
    ) {
        if !simple_flags(args) {
            return false;
        }
        let paths: Vec<_> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
        if paths.is_empty() {
            return false;
        }
        for path in paths {
            if !literal_target(path) {
                return false;
            }
            add_path(effects, path, true, ctx);
        }
        return true;
    }
    if matches!(stem, "cp" | "mv" | "copy" | "move") {
        if !simple_flags(args) {
            return false;
        }
        let paths: Vec<_> = args.iter().filter(|arg| !arg.starts_with('-')).collect();
        if paths.len() < 2 || paths.iter().any(|path| !literal_target(path)) {
            return false;
        }
        for (i, path) in paths.iter().enumerate() {
            let Ok(source) = ctx.workspace.normalize(Path::new(path)) else {
                return false;
            };
            if i < paths.len() - 1 && source.is_dir() {
                return false;
            }
            add_path(
                effects,
                path,
                i == paths.len() - 1 || matches!(stem, "mv" | "move"),
                ctx,
            );
            if i < paths.len() - 1 {
                let destination = Path::new(paths[paths.len() - 1]);
                let Ok(real_destination) = ctx.workspace.normalize(destination) else {
                    return false;
                };
                if real_destination.is_dir() {
                    let Some(name) = Path::new(path).file_name() else {
                        return false;
                    };
                    add_path(
                        effects,
                        &real_destination.join(name).display().to_string(),
                        true,
                        ctx,
                    );
                }
            }
        }
        return true;
    }
    if class == CommandClass::ReadOnly
        || matches!(
            stem,
            "get-content" | "get-childitem" | "test-path" | "resolve-path"
        )
    {
        for arg in args.iter().filter(|arg| !arg.starts_with('-')) {
            if !literal_target(arg) {
                return false;
            }
            add_path(effects, arg, false, ctx);
        }
        return true;
    }
    // Builds/network operations keep their existing command-class gate. Explicit
    // path arguments add the path gate too, even when the executable was vetted.
    // Arbitrary programs/scripts, privileged commands and unknown syntax cannot
    // make the same promise about their targets.
    visible_path_args(stem, args, class, effects, ctx);
    matches!(class, CommandClass::ProjectWrite | CommandClass::Network)
}

fn visible_path_args(
    stem: &str,
    args: &[String],
    class: CommandClass,
    effects: &mut Vec<Effect>,
    ctx: &ToolContext<'_>,
) {
    for (index, arg) in args.iter().enumerate() {
        // An interpreter's script source is executable authority, covered by
        // the opaque-code gate and an exact user grant. It is not an output
        // target. Other explicit path arguments retain the path decision.
        if script_source(stem, args, index) {
            continue;
        }
        let target = arg.split_once('=').map_or(arg.as_str(), |(_, value)| value);
        if target.contains("://") || (arg.starts_with('-') && !arg.contains('=')) {
            continue;
        }
        if class == CommandClass::ProjectWrite
            || Path::new(target).is_absolute()
            || !ctx.workspace.contains(Path::new(target))
            || target.starts_with("../")
            || target.starts_with("..\\")
        {
            add_path(effects, target, true, ctx);
        }
    }
}

fn opaque_arguments(stem: &str, args: &[String]) -> bool {
    matches!(stem, "awk" | "sed")
        || args.iter().any(|arg| {
            matches!(arg.as_str(), "--pre" | "-exec" | "-execdir" | "-ok")
                || arg.starts_with("--pre=")
        })
        || (stem == "git"
            && (args.iter().any(|arg| {
                arg == "-c" || arg.starts_with("-c") || arg.starts_with("--config-env")
            }) || (args
                .iter()
                .any(|arg| matches!(arg.as_str(), "branch" | "tag"))
                && args.iter().filter(|arg| !arg.starts_with('-')).count() > 1)))
}

fn inspect_output_options(
    stem: &str,
    args: &[String],
    effects: &mut Vec<Effect>,
    ctx: &ToolContext<'_>,
) -> bool {
    for (index, arg) in args.iter().enumerate() {
        let target = if matches!(arg.as_str(), "-o" | "--output" | "--output-file") {
            let Some(target) = args.get(index + 1) else {
                return false;
            };
            Some(target.as_str())
        } else if let Some(target) = arg
            .strip_prefix("--output=")
            .or_else(|| arg.strip_prefix("--output-file="))
        {
            Some(target)
        } else if matches!(stem, "sort" | "uniq" | "rustc" | "curl" | "wget")
            && arg.starts_with("-o")
            && arg.len() > 2
        {
            Some(&arg[2..])
        } else {
            None
        };
        if let Some(target) = target {
            if !literal_target(target) {
                return false;
            }
            add_path(effects, target, true, ctx);
        }
    }
    if stem == "uniq" {
        if let Some(target) = args.iter().filter(|arg| !arg.starts_with('-')).nth(1) {
            if !literal_target(target) {
                return false;
            }
            add_path(effects, target, true, ctx);
        }
    }
    true
}

fn script_source(stem: &str, args: &[String], index: usize) -> bool {
    if !matches!(
        stem,
        "python"
            | "python3"
            | "node"
            | "ruby"
            | "perl"
            | "php"
            | "powershell"
            | "pwsh"
            | "sh"
            | "bash"
    ) {
        return false;
    }
    let first = args.iter().position(|arg| !arg.starts_with('-'));
    first == Some(index)
        && Path::new(&args[index])
            .extension()
            .and_then(|e| e.to_str())
            .is_some_and(|e| matches!(e, "py" | "js" | "rb" | "pl" | "php" | "ps1" | "sh"))
}

fn inspect_powershell_writer(
    stem: &str,
    args: &[String],
    effects: &mut Vec<Effect>,
    ctx: &ToolContext<'_>,
) -> bool {
    let mut target = None;
    let mut index = 0;
    while index < args.len() {
        let arg = &args[index];
        match arg.to_ascii_lowercase().as_str() {
            "-literalpath" | "-path" | "-filepath" => {
                index += 1;
                if target.is_some() {
                    return false;
                }
                target = args.get(index);
            }
            "-value" | "-encoding" | "-itemtype" => {
                index += 1;
                if index >= args.len() {
                    return false;
                }
            }
            "-force" | "-nonewline" => {}
            _ if arg.starts_with('-') => return false,
            _ if target.is_none() => target = Some(arg),
            _ if matches!(stem, "set-content" | "add-content") => {}
            _ => return false,
        }
        index += 1;
    }
    let Some(target) = target.filter(|target| literal_target(target)) else {
        return false;
    };
    add_path(effects, target, true, ctx);
    true
}

fn literal_target(target: &str) -> bool {
    !target.is_empty()
        && !target.starts_with('~')
        && (!target.contains(':') || Path::new(target).is_absolute())
        && !target.chars().any(|c| {
            matches!(
                c,
                '$' | '`'
                    | '*'
                    | '?'
                    | '%'
                    | '!'
                    | '['
                    | ']'
                    | '{'
                    | '}'
                    | '|'
                    | '&'
                    | ';'
                    | ','
                    | '\n'
                    | '\r'
                    | '<'
                    | '>'
                    | '('
                    | ')'
            )
        })
}

fn simple_flags(args: &[String]) -> bool {
    args.iter().filter(|arg| arg.starts_with('-')).all(|arg| {
        matches!(
            arg.as_str(),
            "--" | "-p"
                | "-r"
                | "-R"
                | "-f"
                | "-rf"
                | "-fr"
                | "-a"
                | "-v"
                | "-i"
                | "-n"
                | "--parents"
                | "--recursive"
                | "--force"
        )
    })
}

/// Small literal-only lexer. Preserve quoted redirections as data; split real
/// operators even without whitespace. Unsupported escapes, substitutions,
/// command chains and multiline syntax mark the whole execution opaque.
fn literal_tokens(command: &str) -> (Vec<String>, bool) {
    let mut result = Vec::new();
    let mut word = String::new();
    let mut quote = None;
    let mut opaque = false;
    let mut chars = command.chars().peekable();
    while let Some(ch) = chars.next() {
        match (quote, ch) {
            (Some(q), c) if c == q => quote = None,
            (None, '\'' | '"') => quote = Some(ch),
            (None, '>' | '<') => {
                if !word.is_empty() {
                    result.push(std::mem::take(&mut word));
                }
                let mut operator = ch.to_string();
                if chars.peek() == Some(&ch) {
                    operator.push(chars.next().unwrap_or(ch));
                }
                result.push(operator);
            }
            (None, c) if c.is_whitespace() => {
                if matches!(c, '\n' | '\r') {
                    opaque = true;
                }
                if !word.is_empty() {
                    result.push(std::mem::take(&mut word));
                }
            }
            (_, c) => {
                if matches!(c, '|' | '&' | ';' | '`' | '(' | ')' | '\n' | '\r')
                    || (quote.is_none() && c == '~' && word.is_empty())
                    || (quote != Some('\'') && matches!(c, '$' | '%' | '!'))
                    || (quote.is_none() && matches!(c, '*' | '?' | '[' | ']' | '{' | '}'))
                    || (cfg!(unix) && c == '\\')
                {
                    opaque = true;
                }
                word.push(c);
            }
        }
    }
    if !word.is_empty() {
        result.push(word);
    }
    (result, opaque || quote.is_some())
}

/// Child-only environment; inherited values never become permission grants.
pub(crate) fn scratch_environment(command: &mut tokio::process::Command, scratch: Option<&Path>) {
    command.env_remove("LOCALPILOT_SCRATCH_DIR");
    if let Some(root) = scratch {
        command
            .env("LOCALPILOT_SCRATCH_DIR", root)
            .env("TEMP", root)
            .env("TMP", root)
            .env("TMPDIR", root);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    #[allow(clippy::unwrap_used)]
    fn output_options_and_hidden_directory_leaves_keep_their_write_gate() {
        use localpilot_sandbox::{Interactivity, Workspace};
        let root = tempfile::tempdir().unwrap();
        // Windows runners spell their profile with an embedded 8.3 tilde.
        // A tilde inside a path component is literal, unlike leading ~.
        let outside = tempfile::Builder::new()
            .prefix("literal~1-")
            .tempdir()
            .unwrap();
        let ws = Workspace::new(root.path()).unwrap();
        let ctx = ToolContext {
            workspace: &ws,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
        };
        let target = outside.path().join("output").display().to_string();
        for (program, args) in [
            ("sort", vec![format!("-o{target}")]),
            ("rustc", vec!["-o".into(), target.clone()]),
            ("uniq", vec!["input".into(), target]),
        ] {
            let actual = effects(
                &RunShellExecution::Direct {
                    program: program.into(),
                    args,
                },
                &ctx,
            )
            .unwrap();
            assert!(
                actual.iter().any(|effect| matches!(
                    effect,
                    Effect::WritePath {
                        inside_workspace: false,
                        ..
                    }
                )),
                "{program}: {actual:?}"
            );
        }
        std::fs::create_dir(root.path().join("source")).unwrap();
        for (program, args) in [
            (
                "cp",
                vec!["-r".into(), "source".into(), "destination".into()],
            ),
            ("sed", vec!["w hidden-output".into()]),
            ("rg", vec!["--pre=hidden-writer".into()]),
            ("sort", vec!["--output=$DYNAMIC".into()]),
        ] {
            let actual = effects(
                &RunShellExecution::Direct {
                    program: program.into(),
                    args,
                },
                &ctx,
            )
            .unwrap();
            assert!(
                actual.contains(&Effect::UnscopedCommand),
                "{program}: {actual:?}"
            );
        }
    }

    #[test]
    fn expansions_and_provider_paths_never_prove_a_literal_write_target() {
        for command in [
            "touch ~/fixture",
            "touch fixture*",
            "echo x > $HOME/fixture",
            "echo x > %USERPROFILE%/fixture",
            "Set-Content -LiteralPath (Join-Path '/outside' 'fixture') -Value x",
            "echo x > a; echo y > b",
            "python - <<'PY'\ncode\nPY",
        ] {
            assert!(literal_tokens(command).1, "{command}");
        }
        assert!(!literal_target("HKCU:Software/fixture"));
        assert!(!literal_target("C:relative"));
        assert!(!literal_target("~/fixture"));
        assert!(literal_target("literal~1/fixture"));
        assert!(!literal_tokens("echo x > 'literal~1/fixture'").1);
        assert!(!literal_tokens("touch literal~1/fixture").1);
        assert!(!literal_target("(Join-Path"));
        let (tokens, opaque) =
            literal_tokens("echo 'data > remains data' > 'fixture with spaces.txt'");
        assert!(!opaque);
        assert_eq!(
            tokens,
            [
                "echo",
                "data > remains data",
                ">",
                "fixture with spaces.txt"
            ]
        );
    }
}
