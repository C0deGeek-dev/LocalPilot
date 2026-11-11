//! Stack processes still running from the managed `bin` directory.
//!
//! An install replaces executables that may be executing right now: a
//! long-lived `localpilot` server, or `localpilot`/`localmind` MCP servers an
//! editor keeps alive. The swap itself copes (rename-then-copy), but those
//! processes keep running the old build until they restart, and on Windows the
//! displaced copy they hold cannot be removed. So before an install, list them
//! and — when someone is at the terminal to answer — offer to stop them.
//!
//! Listing uses the platform's own process table tool (`Get-CimInstance` on
//! Windows, `ps` elsewhere) rather than a new dependency; a failure to list is
//! never fatal, the install simply proceeds as before.

use std::io::{BufRead, IsTerminal, Write};
use std::path::Path;
use std::process::{Command, Stdio};
use std::time::{Duration, Instant};

use crate::StackTool;

/// One process from the system process table.
#[derive(Debug, Clone, PartialEq, Eq)]
struct Proc {
    pid: u32,
    ppid: u32,
    /// Full executable path on Windows; the command name (or path) elsewhere.
    image: String,
    /// The command line, when the platform reports one.
    command: String,
}

/// How long to wait for stopped processes to exit before reporting stragglers.
const STOP_WAIT: Duration = Duration::from_secs(5);

/// List stack processes running from `bin` (other than this process and its
/// ancestors) and, when stdin is a terminal, offer to stop them before the
/// install replaces their executables.
///
/// # Errors
/// Returns an error only if output cannot be written.
pub fn offer_to_stop(tools: &[&StackTool], bin: &Path, out: &mut dyn Write) -> anyhow::Result<()> {
    let interactive = std::io::stdin().is_terminal();
    offer_to_stop_with(
        tools,
        bin,
        out,
        &mut || list_processes(),
        &mut |prompt, out| {
            if !interactive {
                write!(out, "no terminal to ask at; ")?;
                return Ok(false);
            }
            write!(out, "{prompt}")?;
            out.flush()?;
            let mut answer = String::new();
            std::io::stdin().lock().read_line(&mut answer)?;
            Ok(matches!(
                answer.trim().to_ascii_lowercase().as_str(),
                "y" | "yes"
            ))
        },
    )
}

type Lister<'a> = dyn FnMut() -> Option<Vec<Proc>> + 'a;
type Confirm<'a> = dyn FnMut(&str, &mut dyn Write) -> anyhow::Result<bool> + 'a;

fn offer_to_stop_with(
    tools: &[&StackTool],
    bin: &Path,
    out: &mut dyn Write,
    list: &mut Lister<'_>,
    confirm: &mut Confirm<'_>,
) -> anyhow::Result<()> {
    let Some(table) = list() else {
        return Ok(());
    };
    let found = stack_processes(&table, tools, bin, std::process::id());
    if found.is_empty() {
        return Ok(());
    }

    writeln!(
        out,
        "{} stack process(es) running from {}:",
        found.len(),
        bin.display()
    )?;
    for p in &found {
        writeln!(out, "  pid {:<7} {}", p.pid, describe(p))?;
    }
    writeln!(
        out,
        "  the update installs anyway, but these keep running the old build until they restart.\n  \
         stopping an MCP server ends that integration until its client restarts it."
    )?;

    if !confirm("stop them now? [y/N] ", out)? {
        writeln!(out, "leaving them running.\n")?;
        return Ok(());
    }

    for p in &found {
        if let Err(error) = stop(p.pid) {
            writeln!(out, "  could not stop pid {}: {error}", p.pid)?;
        }
    }
    let deadline = Instant::now() + STOP_WAIT;
    let mut remaining: Vec<u32> = found.iter().map(|p| p.pid).collect();
    while !remaining.is_empty() && Instant::now() < deadline {
        std::thread::sleep(Duration::from_millis(200));
        let Some(table) = list() else { break };
        remaining.retain(|pid| table.iter().any(|p| p.pid == *pid));
    }
    if remaining.is_empty() {
        writeln!(out, "stopped {} process(es).\n", found.len())?;
    } else {
        writeln!(
            out,
            "still running after {}s: pid {}\n",
            STOP_WAIT.as_secs(),
            remaining
                .iter()
                .map(u32::to_string)
                .collect::<Vec<_>>()
                .join(", ")
        )?;
    }
    Ok(())
}

/// The processes in `table` that run a train tool from `bin` — including a
/// displaced copy left by an earlier swap — excluding `self_pid` and its
/// ancestors (stopping the process doing the install, or the shell or tool that
/// launched it, would cut the run short).
fn stack_processes(table: &[Proc], tools: &[&StackTool], bin: &Path, self_pid: u32) -> Vec<Proc> {
    let mut protected = vec![self_pid];
    let mut cursor = self_pid;
    // Bounded walk: a malformed table must not loop forever.
    for _ in 0..table.len() {
        let Some(parent) = table.iter().find(|p| p.pid == cursor).map(|p| p.ppid) else {
            break;
        };
        if parent == 0 || protected.contains(&parent) {
            break;
        }
        protected.push(parent);
        cursor = parent;
    }

    table
        .iter()
        .filter(|p| !protected.contains(&p.pid))
        .filter(|p| runs_train_tool(&p.image, tools, bin))
        .cloned()
        .collect()
}

/// Whether `image` is a train tool's executable. On Windows the full path is
/// known, so only copies inside `bin` count (a test binary under `target/` does
/// not hold the managed file). Elsewhere `ps` may report a bare command name, so
/// the name alone decides.
fn runs_train_tool(image: &str, tools: &[&StackTool], bin: &Path) -> bool {
    let path = Path::new(image);
    let Some(name) = path.file_name().and_then(|n| n.to_str()) else {
        return false;
    };
    let name = name.to_ascii_lowercase();
    if cfg!(windows) {
        let in_bin = path.parent().is_some_and(|dir| {
            dir.to_string_lossy()
                .eq_ignore_ascii_case(&bin.to_string_lossy())
        });
        in_bin
            && tools.iter().any(|t| {
                let exe = localpilot_dist::executable_name(t.tool).to_ascii_lowercase();
                name == exe || name.starts_with(&format!("{exe}."))
            })
    } else {
        tools.iter().any(|t| name == t.tool)
    }
}

fn describe(p: &Proc) -> String {
    let name = Path::new(&p.image)
        .file_name()
        .map_or_else(|| p.image.clone(), |n| n.to_string_lossy().into_owned());
    // Drop the quoted or bare executable path from the command line; the
    // arguments are what tell two copies of the same tool apart.
    let args = p
        .command
        .trim()
        .strip_prefix('"')
        .and_then(|rest| rest.split_once('"').map(|(_, a)| a))
        .or_else(|| p.command.trim().split_once(' ').map(|(_, a)| a))
        .unwrap_or("")
        .trim();
    if args.is_empty() {
        name
    } else {
        format!("{name} {args}")
    }
}

/// The system process table, or `None` when it cannot be read.
fn list_processes() -> Option<Vec<Proc>> {
    let output = if cfg!(windows) {
        Command::new("powershell")
            .args([
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                "Get-CimInstance Win32_Process | ForEach-Object { \"{0}`t{1}`t{2}`t{3}\" -f \
                 $_.ProcessId,$_.ParentProcessId,$_.ExecutablePath,$_.CommandLine }",
            ])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?
    } else {
        Command::new("ps")
            .args(["-axo", "pid=,ppid=,comm="])
            .stdin(Stdio::null())
            .stderr(Stdio::null())
            .output()
            .ok()?
    };
    if !output.status.success() {
        return None;
    }
    let text = String::from_utf8_lossy(&output.stdout);
    Some(if cfg!(windows) {
        parse_windows(&text)
    } else {
        parse_ps(&text)
    })
}

/// Parse `pid<TAB>ppid<TAB>path<TAB>command line` rows.
fn parse_windows(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let mut fields = line.splitn(4, '\t');
            let pid = fields.next()?.trim().parse().ok()?;
            let ppid = fields.next()?.trim().parse().ok()?;
            let image = fields.next()?.trim().to_string();
            let command = fields.next().unwrap_or("").trim().to_string();
            Some(Proc {
                pid,
                ppid,
                image,
                command,
            })
        })
        .collect()
}

/// Parse `ps -o pid=,ppid=,comm=` rows; the command (last) may contain spaces.
fn parse_ps(text: &str) -> Vec<Proc> {
    text.lines()
        .filter_map(|line| {
            let line = line.trim_start();
            let (pid, rest) = line.split_once(char::is_whitespace)?;
            let rest = rest.trim_start();
            let (ppid, comm) = rest.split_once(char::is_whitespace)?;
            Some(Proc {
                pid: pid.parse().ok()?,
                ppid: ppid.parse().ok()?,
                image: comm.trim().to_string(),
                command: String::new(),
            })
        })
        .collect()
}

fn stop(pid: u32) -> anyhow::Result<()> {
    let pid = pid.to_string();
    let status = if cfg!(windows) {
        // No `/T`: a tool's children (a shell an agent started, a model server)
        // are not ours to end.
        Command::new("taskkill")
            .args(["/PID", &pid, "/F"])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
    } else {
        Command::new("kill")
            .args(["-s", "TERM", &pid])
            .stdout(Stdio::null())
            .stderr(Stdio::null())
            .status()?
    };
    anyhow::ensure!(status.success(), "the stop command exited with {status}");
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::TRAIN;
    use std::path::PathBuf;

    fn proc(pid: u32, ppid: u32, image: &str, command: &str) -> Proc {
        Proc {
            pid,
            ppid,
            image: image.to_string(),
            command: command.to_string(),
        }
    }

    fn train() -> Vec<&'static StackTool> {
        TRAIN.iter().collect()
    }

    fn bin() -> PathBuf {
        if cfg!(windows) {
            PathBuf::from(r"C:\Users\u\AppData\Local\localx\bin")
        } else {
            PathBuf::from("/home/u/.local/share/localx/bin")
        }
    }

    fn image(name: &str) -> String {
        let name = if cfg!(windows) {
            format!("{name}.exe")
        } else {
            name.to_string()
        };
        bin().join(name).display().to_string()
    }

    #[test]
    fn windows_rows_parse_with_a_command_line_containing_tabs_and_spaces() {
        let rows = parse_windows(
            "4\t0\t\t\n37220\t14568\tC:\\bin\\localpilot.exe\tC:\\bin\\localpilot.exe mcp serve\n\
             garbage\n",
        );
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].pid, 37220);
        assert_eq!(rows[1].ppid, 14568);
        assert_eq!(rows[1].image, "C:\\bin\\localpilot.exe");
        assert_eq!(rows[1].command, "C:\\bin\\localpilot.exe mcp serve");
    }

    #[test]
    fn ps_rows_parse_with_padding_and_spaces_in_the_command() {
        let rows = parse_ps("    1     0 /sbin/launchd\n  812   1 /Applications/My App/bin\n");
        assert_eq!(rows.len(), 2);
        assert_eq!(rows[1].pid, 812);
        assert_eq!(rows[1].image, "/Applications/My App/bin");
    }

    #[test]
    fn finds_train_tools_but_spares_itself_and_its_ancestors() {
        let table = vec![
            proc(1, 0, "init", ""),
            proc(10, 1, "shell", ""),
            proc(20, 10, &image("localx"), ""), // this process
            proc(30, 1, &image("localpilot"), "localpilot mcp serve"),
            proc(40, 1, &image("localmind"), "localmind mcp serve"),
            proc(50, 1, "unrelated", ""),
        ];
        let found: Vec<u32> = stack_processes(&table, &train(), &bin(), 20)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, vec![30, 40]);
    }

    #[test]
    fn a_tool_that_launched_the_install_is_never_stopped() {
        // `localpilot update --all` from a shell inside a localpilot session.
        let table = vec![
            proc(5, 1, &image("localpilot"), ""),
            proc(6, 5, &image("localpilot"), "localpilot update --all"),
        ];
        assert!(stack_processes(&table, &train(), &bin(), 6).is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn on_windows_only_copies_in_the_managed_directory_count() {
        let table = vec![
            proc(30, 1, r"D:\repo\target\debug\localpilot.exe", ""),
            proc(
                31,
                1,
                &image("localpilot").replace("localpilot.exe", "localpilot.exe.displaced"),
                "",
            ),
            proc(32, 1, &image("localpilot").to_uppercase(), ""),
        ];
        let found: Vec<u32> = stack_processes(&table, &train(), &bin(), 99)
            .iter()
            .map(|p| p.pid)
            .collect();
        assert_eq!(found, vec![31, 32]);
    }

    #[test]
    fn declining_leaves_everything_running_and_says_so() {
        let table = vec![proc(30, 1, &image("localpilot"), "")];
        let mut out = Vec::new();
        let mut asked = false;
        offer_to_stop_with(
            &train(),
            &bin(),
            &mut out,
            &mut || Some(table.clone()),
            &mut |_, _| {
                asked = true;
                Ok(false)
            },
        )
        .expect("offer");
        let text = String::from_utf8(out).expect("utf8");
        assert!(asked);
        assert!(text.contains("1 stack process(es) running"), "{text}");
        assert!(text.contains("leaving them running"), "{text}");
    }

    #[test]
    fn nothing_running_asks_nothing() {
        let mut out = Vec::new();
        offer_to_stop_with(
            &train(),
            &bin(),
            &mut out,
            &mut || Some(vec![proc(1, 0, "init", "")]),
            &mut |_, _| panic!("must not ask"),
        )
        .expect("offer");
        assert!(out.is_empty());
    }

    #[cfg(windows)]
    #[test]
    fn describe_keeps_the_arguments_that_tell_copies_apart() {
        let p = proc(
            1,
            0,
            r"C:\b\localpilot.exe",
            "\"C:\\b\\localpilot.exe\" __server-serve",
        );
        assert_eq!(describe(&p), "localpilot.exe __server-serve");
        let p = proc(1, 0, r"C:\b\localmind.exe", r"C:\b\localmind.exe mcp serve");
        assert_eq!(describe(&p), "localmind.exe mcp serve");
    }
}
