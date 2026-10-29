//! `powershell -Command "<one read-only cmdlet>"` can be inspected, because the
//! cmdlet and its parameters are known. Anything this module cannot account for
//! returns `None`, and the caller keeps treating the command as opaque code.
//!
//! Two layers parse the text: the shell that runs the tool call, then the
//! PowerShell child that receives the command string. The lexer below accepts
//! only a subset both layers read the same way: literal words, and quoted
//! strings without expansion. Substitution, pipelines, redirection, escapes,
//! quote doubling and wildcards in a path never match.

use std::path::Path;

use crate::builtins_shell::RunShellExecution;

/// Switches that may precede `-Command` without changing what runs.
const HOST_FLAGS: &[&str] = &["-noprofile", "-noninteractive", "-nologo"];

struct Cmdlet {
    names: &'static [&'static str],
    /// Parameters taking a value.
    values: &'static [&'static str],
    /// Parameters without a value.
    switches: &'static [&'static str],
    /// Positional parameters, in order.
    positional: &'static [&'static str],
    /// Parameters whose value is a file path that gets the read gate.
    paths: &'static [&'static str],
    /// A path is required, or the cmdlet would read its pipeline input.
    path_required: bool,
}

const CMDLETS: &[Cmdlet] = &[
    Cmdlet {
        names: &["select-string", "sls"],
        values: &[
            "pattern",
            "path",
            "literalpath",
            "include",
            "exclude",
            "context",
            "encoding",
        ],
        switches: &[
            "simplematch",
            "casesensitive",
            "list",
            "quiet",
            "notmatch",
            "allmatches",
        ],
        positional: &["pattern", "path"],
        paths: &["path", "literalpath"],
        path_required: true,
    },
    Cmdlet {
        names: &["get-content", "gc"],
        values: &["path", "literalpath", "totalcount", "tail", "encoding"],
        switches: &["raw"],
        positional: &["path"],
        paths: &["path", "literalpath"],
        path_required: true,
    },
    Cmdlet {
        names: &["get-childitem", "gci"],
        values: &["path", "literalpath", "filter", "depth"],
        switches: &["recurse", "name", "file", "directory", "force"],
        positional: &["path"],
        paths: &["path", "literalpath"],
        path_required: false,
    },
    Cmdlet {
        names: &["get-item", "gi"],
        values: &["path", "literalpath"],
        switches: &["force"],
        positional: &["path"],
        paths: &["path", "literalpath"],
        path_required: true,
    },
    Cmdlet {
        names: &["test-path"],
        values: &["path", "literalpath", "pathtype"],
        switches: &[],
        positional: &["path"],
        paths: &["path", "literalpath"],
        path_required: true,
    },
    Cmdlet {
        names: &["resolve-path"],
        values: &["path", "literalpath"],
        switches: &["relative"],
        positional: &["path"],
        paths: &["path", "literalpath"],
        path_required: true,
    },
];

/// The file paths a read-only inline cmdlet reads, or `None` when the command
/// is anything else (or not provably this).
pub(crate) fn read_only_paths(execution: &RunShellExecution) -> Option<Vec<String>> {
    let outer = match execution {
        RunShellExecution::Direct { program, args } => {
            let mut words = vec![program.clone()];
            words.extend(args.iter().cloned());
            words
        }
        RunShellExecution::Shell { command } => {
            let words: Vec<String> = lex(command)?.into_iter().map(|(word, _)| word).collect();
            // A quote that survives into an argument is re-split by the child,
            // and a trailing backslash escapes the quote that closes it.
            if words
                .iter()
                .any(|word| word.contains('"') || word.ends_with('\\'))
            {
                return None;
            }
            words
        }
    };
    let (program, rest) = outer.split_first()?;
    let stem = Path::new(program)
        .file_stem()
        .and_then(|stem| stem.to_str())?
        .to_ascii_lowercase();
    if !matches!(stem.as_str(), "powershell" | "pwsh") {
        return None;
    }
    let mut command_at = None;
    for (index, flag) in rest.iter().enumerate() {
        let flag = flag.to_ascii_lowercase();
        if flag == "-command" || flag == "-c" {
            command_at = Some(index + 1);
            break;
        }
        if !HOST_FLAGS.contains(&flag.as_str()) {
            return None;
        }
    }
    // The child joins what follows with single spaces.
    let inner = rest.get(command_at?..)?.join(" ");
    cmdlet_paths(&lex(&inner)?)
}

/// Split into words, keeping whether each was quoted. Only literal text is
/// accepted: a character with a meaning to either shell outside quotes, an
/// expansion inside double quotes, a doubled quote or a line break gives `None`.
fn lex(text: &str) -> Option<Vec<(String, bool)>> {
    let mut words = Vec::new();
    let mut word = String::new();
    let mut open = false;
    let mut quoted = false;
    let mut chars = text.chars().peekable();
    while let Some(ch) = chars.next() {
        match ch {
            '\'' | '"' => {
                open = true;
                quoted = true;
                let mut closed = false;
                for inner in chars.by_ref() {
                    if inner == ch {
                        closed = true;
                        break;
                    }
                    if matches!(inner, '\n' | '\r') || (ch == '"' && matches!(inner, '$' | '`')) {
                        return None;
                    }
                    word.push(inner);
                }
                if !closed || chars.peek() == Some(&ch) {
                    return None;
                }
            }
            c if c.is_whitespace() => {
                if matches!(c, '\n' | '\r') {
                    return None;
                }
                if open {
                    words.push((std::mem::take(&mut word), quoted));
                    open = false;
                    quoted = false;
                }
            }
            '|' | '&' | ';' | '<' | '>' | '(' | ')' | '{' | '}' | '[' | ']' | '@' | '#' | '$'
            | '`' | '%' | '!' | ',' => return None,
            c => {
                word.push(c);
                open = true;
            }
        }
    }
    if open {
        words.push((word, quoted));
    }
    Some(words)
}

fn cmdlet_paths(words: &[(String, bool)]) -> Option<Vec<String>> {
    let ((name, name_quoted), arguments) = words.split_first()?;
    if *name_quoted {
        return None;
    }
    let name = name.to_ascii_lowercase();
    let spec = CMDLETS
        .iter()
        .find(|spec| spec.names.contains(&name.as_str()))?;
    let mut named: Vec<(String, &str)> = Vec::new();
    let mut next_positional = 0;
    let mut index = 0;
    while let Some((word, quoted)) = arguments.get(index) {
        index += 1;
        let parameter = (!quoted && word.len() > 1)
            .then(|| word.strip_prefix('-'))
            .flatten();
        let Some(parameter) = parameter else {
            let slot = spec.positional.get(next_positional)?;
            next_positional += 1;
            named.push(((*slot).to_string(), word));
            continue;
        };
        // `-Name:value` and abbreviations are not matched.
        let parameter = parameter.to_ascii_lowercase();
        if spec.switches.contains(&parameter.as_str()) {
            continue;
        }
        if !spec.values.contains(&parameter.as_str()) {
            return None;
        }
        let (value, value_quoted) = arguments.get(index)?;
        index += 1;
        if !value_quoted && value.starts_with('-') {
            return None;
        }
        named.push((parameter, value));
    }
    if spec.names[0] == "select-string" && !named.iter().any(|(slot, _)| slot == "pattern") {
        return None;
    }
    let mut paths = Vec::new();
    for (slot, value) in &named {
        if spec.paths.contains(&slot.as_str()) {
            if !crate::command_paths::literal_target(value) {
                return None;
            }
            paths.push((*value).to_string());
        }
    }
    if spec.path_required && paths.is_empty() {
        return None;
    }
    Some(paths)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn shell(command: &str) -> RunShellExecution {
        RunShellExecution::Shell {
            command: command.to_string(),
        }
    }

    fn paths(command: &str) -> Option<Vec<String>> {
        read_only_paths(&shell(command))
    }

    #[test]
    fn a_single_read_only_cmdlet_is_inspected_with_its_paths() {
        for (command, expected) in [
            (
                "powershell -NoProfile -Command \"Select-String -Path index.html -Pattern cdnjs\"",
                vec!["index.html"],
            ),
            (
                "powershell -NoProfile -Command \"Select-String -Path index.html -Pattern 'a|b|type=module'\"",
                vec!["index.html"],
            ),
            (
                "pwsh -c \"sls 'three' src\\main.js\"",
                vec!["src\\main.js"],
            ),
            ("powershell -Command Get-Content notes.txt -TotalCount 5", vec!["notes.txt"]),
            ("powershell -NoLogo -Command \"Test-Path -LiteralPath 'a b.txt'\"", vec!["a b.txt"]),
            ("powershell -Command \"Get-ChildItem -Recurse -Name\"", vec![]),
        ] {
            assert_eq!(paths(command), Some(expected.into_iter().map(String::from).collect()), "{command}");
        }
    }

    #[test]
    fn a_direct_invocation_is_inspected_the_same_way() {
        let direct = RunShellExecution::Direct {
            program: "powershell".into(),
            args: vec![
                "-NoProfile".into(),
                "-Command".into(),
                "Select-String -Path index.html -Pattern 'x'".into(),
            ],
        };
        assert_eq!(
            read_only_paths(&direct),
            Some(vec!["index.html".to_string()])
        );
    }

    #[test]
    fn anything_beyond_one_read_only_cmdlet_stays_opaque() {
        for command in [
            // Pipelines, chains, redirection, substitution, escapes.
            "powershell -Command \"Select-String -Path a -Pattern x | Out-File b\"",
            "powershell -Command \"Get-Content a; Remove-Item a\"",
            "powershell -Command \"Get-Content a > b\"",
            "powershell -Command \"Get-Content $(Remove-Item a)\"",
            "powershell -Command \"Get-Content `a\"",
            "powershell -Command \"Select-String -Path a -Pattern \\\"x\\\"\"",
            "powershell -Command 'Select-String -Path a -Pattern \"x\"'",
            "powershell -Command \"Get-Content 'it''s'\"",
            // Cmdlets and parameters that are not on the list.
            "powershell -Command \"Remove-Item a\"",
            "powershell -Command \"Set-Content a x\"",
            "powershell -Command \"Invoke-Expression 'x'\"",
            "powershell -Command \"Get-Content a -Wait\"",
            "powershell -Command \"Get-Content -Pa a\"",
            "powershell -Command \"Get-Content -Path:a\"",
            "powershell -Command \"'Get-Content' a\"",
            // A path that is not one literal path, or none at all.
            "powershell -Command \"Get-Content *.txt\"",
            "powershell -Command \"Get-Content a,b\"",
            "powershell -Command \"Select-String -Pattern x\"",
            "powershell -Command \"Get-Content\"",
            "powershell -Command \"Select-String -Path a\"",
            // Host flags that change what runs, other programs, other forms.
            "powershell -ExecutionPolicy Bypass -Command \"Get-Content a\"",
            "powershell -File script.ps1",
            "powershell -EncodedCommand AAAA",
            "cmd /c type a",
            "powershell -Command \"Get-Content 'a\"",
            "powershell -Command \"Get-Content a\nGet-Content b\"",
        ] {
            assert_eq!(paths(command), None, "{command}");
        }
    }
}
