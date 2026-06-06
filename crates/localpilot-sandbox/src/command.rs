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

/// POSIX (Linux/macOS) command classification.
#[must_use]
pub fn classify_posix(program: &str, args: &[String]) -> CommandClass {
    let stem = program_stem(program);
    let args = args_lower(args);

    if matches!(stem.as_str(), "sudo" | "doas" | "su" | "pkexec") {
        return CommandClass::Privileged;
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
    if matches!(stem.as_str(), "powershell" | "pwsh") {
        return classify_powershell(&args);
    }
    if stem == "cmd" {
        return classify_cmd(&args);
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
    let sub = args
        .iter()
        .find(|a| !a.starts_with('-'))
        .map(String::as_str)
        .unwrap_or("");
    match sub {
        "clone" | "pull" | "push" | "fetch" | "remote" | "submodule" => CommandClass::Network,
        "status" | "log" | "diff" | "show" | "branch" | "rev-parse" | "describe" | "blame"
        | "tag" | "ls-files" => CommandClass::ReadOnly,
        "add" | "commit" | "checkout" | "switch" | "restore" | "stash" | "merge" | "rebase"
        | "reset" | "cherry-pick" | "apply" | "mv" | "rm" => CommandClass::ProjectWrite,
        _ => CommandClass::Unknown,
    }
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
            | "env"
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
        assert_eq!(
            classify_windows("cmd", &argv(&["/c", "del", "x"])),
            CommandClass::Destructive
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

    proptest::proptest! {
        // Classification never panics on adversarial input.
        #[test]
        fn classification_is_total(program in ".*", args in proptest::collection::vec(".*", 0..5)) {
            let _ = classify_posix(&program, &args);
            let _ = classify_windows(&program, &args);
        }
    }
}
