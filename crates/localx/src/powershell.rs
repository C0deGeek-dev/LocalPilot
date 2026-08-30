//! Optional PowerShell compatibility shortcuts for the LocalX stack.
//!
//! The shortcut payload ships inside `localx`, so release and source installs
//! use exactly the same definitions. Profile integration is deliberately
//! conservative: an empty setup is wired automatically, Chris Titus Tech's
//! managed profile uses its documented `profile.ps1` customization seam, and
//! any other non-empty profile is left for its owner to edit.

use std::ffi::OsString;
use std::fs;
use std::io::Write;
use std::path::{Path, PathBuf};
use std::process::Command;

use anyhow::{Context, Result};

const SHORTCUTS: &str = include_str!("../assets/LocalX.Shortcuts.ps1");
const BLOCK_START: &str = "# >>> LocalX PowerShell shortcuts >>>";
const BLOCK_END: &str = "# <<< LocalX PowerShell shortcuts <<<";
const POWERSHELL_HOST_ENV: &str = "LOCALX_POWERSHELL_HOST";

#[derive(Debug, Clone)]
struct ProfilePaths {
    all_hosts: PathBuf,
    current_host: PathBuf,
}

#[derive(Debug, PartialEq, Eq)]
enum Integration {
    Linked {
        profile: PathBuf,
        ctt: bool,
    },
    AlreadyLinked {
        profile: PathBuf,
    },
    Manual {
        profiles: Vec<PathBuf>,
        line: String,
    },
}

/// Install the managed shortcut payload and connect it to the current user's
/// PowerShell profile when doing so is safe.
pub fn install(out: &mut dyn Write) -> Result<()> {
    let shortcut_path = shortcut_path()?;
    write_shortcuts(&shortcut_path)?;
    writeln!(
        out,
        "PowerShell shortcuts: installed {}",
        shortcut_path.display()
    )?;

    let profiles = discover_profile_paths()?;
    match integrate_profile(&shortcut_path, &profiles)? {
        Integration::Linked { profile, ctt } => {
            if ctt {
                writeln!(
                    out,
                    "PowerShell profile: Chris Titus Tech profile detected; added the LocalX load line to its custom profile at {}.",
                    profile.display()
                )?;
            } else {
                writeln!(
                    out,
                    "PowerShell profile: added the LocalX load line to {}.",
                    profile.display()
                )?;
            }
            writeln!(out, "Open a new PowerShell session, then run `llm`.")?;
        }
        Integration::AlreadyLinked { profile } => {
            writeln!(
                out,
                "PowerShell profile: LocalX shortcuts are already loaded by {}.",
                profile.display()
            )?;
        }
        Integration::Manual { profiles, line } => {
            writeln!(
                out,
                "PowerShell profile: a non-standard profile was detected, so LocalX left it unchanged."
            )?;
            for profile in profiles {
                writeln!(out, "  detected: {}", profile.display())?;
            }
            writeln!(
                out,
                "Add this line to the profile where you keep custom code:"
            )?;
            writeln!(out, "  {line}")?;
        }
    }
    Ok(())
}

/// Keep an already-enabled payload current during ordinary LocalX updates.
/// No script is created and no profile is inspected unless the user opted in
/// previously.
pub fn refresh_if_installed(out: &mut dyn Write) -> Result<()> {
    let Some(path) = resolved_shortcut_path() else {
        return Ok(());
    };
    if !path.is_file() {
        return Ok(());
    }
    let changed = fs::read_to_string(&path).ok().as_deref() != Some(SHORTCUTS);
    if changed {
        write_shortcuts(&path)?;
        writeln!(out, "PowerShell shortcuts: refreshed {}", path.display())?;
    }
    Ok(())
}

fn shortcut_path() -> Result<PathBuf> {
    resolved_shortcut_path()
        .context("could not resolve the LocalX data directory for PowerShell shortcuts")
}

fn resolved_shortcut_path() -> Option<PathBuf> {
    let bin = localpilot_stack::shared_bin_dir()?;
    Some(
        bin.parent()?
            .join("powershell")
            .join("LocalX.Shortcuts.ps1"),
    )
}

fn write_shortcuts(path: &Path) -> Result<()> {
    if let Some(parent) = path.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create the PowerShell shortcut directory {}",
                parent.display()
            )
        })?;
    }
    if fs::read_to_string(path).ok().as_deref() != Some(SHORTCUTS) {
        fs::write(path, SHORTCUTS).with_context(|| {
            format!(
                "could not write the PowerShell shortcuts to {}",
                path.display()
            )
        })?;
    }
    Ok(())
}

fn discover_profile_paths() -> Result<ProfilePaths> {
    const QUERY: &str = "$utf8 = New-Object System.Text.UTF8Encoding($false); \
        [Console]::OutputEncoding = $utf8; \
        [Console]::Out.Write([string]$PROFILE.CurrentUserAllHosts); \
        [Console]::Out.Write([char]0); \
        [Console]::Out.Write([string]$PROFILE.CurrentUserCurrentHost)";

    let explicit = std::env::var_os(POWERSHELL_HOST_ENV).filter(|value| !value.is_empty());
    let candidates: Vec<OsString> = if let Some(host) = explicit.clone() {
        vec![host]
    } else if cfg!(windows) {
        vec![OsString::from("pwsh.exe"), OsString::from("powershell.exe")]
    } else {
        vec![OsString::from("pwsh")]
    };

    let mut failures = Vec::new();
    for candidate in candidates {
        match Command::new(&candidate)
            .args([
                "-NoLogo",
                "-NoProfile",
                "-NonInteractive",
                "-Command",
                QUERY,
            ])
            .output()
        {
            Ok(output) if output.status.success() => match parse_profile_paths(&output.stdout) {
                Ok(paths) => return Ok(paths),
                Err(error) => failures.push(format!(
                    "{} returned invalid profile paths: {error:#}",
                    Path::new(&candidate).display()
                )),
            },
            Ok(output) => failures.push(format!(
                "{} exited with {}: {}",
                Path::new(&candidate).display(),
                output.status,
                String::from_utf8_lossy(&output.stderr).trim()
            )),
            Err(error) => failures.push(format!("{}: {error}", Path::new(&candidate).display())),
        }
    }

    let hint = if explicit.is_some() {
        format!(" (from {POWERSHELL_HOST_ENV})")
    } else {
        String::new()
    };
    anyhow::bail!(
        "could not query a PowerShell host{hint}; install PowerShell or set \
         {POWERSHELL_HOST_ENV} to its executable. Attempts: {}",
        failures.join("; ")
    )
}

fn parse_profile_paths(stdout: &[u8]) -> Result<ProfilePaths> {
    let text = std::str::from_utf8(stdout).context("PowerShell profile output was not UTF-8")?;
    let mut fields = text.split('\0');
    let all_hosts = fields.next().unwrap_or_default().trim();
    let current_host = fields.next().unwrap_or_default().trim();
    if all_hosts.is_empty() || current_host.is_empty() || fields.next().is_some() {
        anyhow::bail!("expected exactly two non-empty NUL-separated paths");
    }
    Ok(ProfilePaths {
        all_hosts: PathBuf::from(all_hosts),
        current_host: PathBuf::from(current_host),
    })
}

fn integrate_profile(shortcut_path: &Path, profiles: &ProfilePaths) -> Result<Integration> {
    let profile_paths = unique_profile_paths(profiles);
    let contents = profile_paths
        .iter()
        .map(|path| Ok((path.clone(), read_profile(path)?)))
        .collect::<Result<Vec<_>>>()?;

    let load_line = dot_source_line(shortcut_path);
    if let Some((path, _)) = contents.iter().find(|(_, content)| {
        content.contains(BLOCK_START) || content.lines().any(|line| line.trim() == load_line)
    }) {
        return Ok(Integration::AlreadyLinked {
            profile: path.clone(),
        });
    }

    let ctt = contents
        .iter()
        .any(|(_, content)| is_chris_titus_profile(content));
    let non_empty: Vec<PathBuf> = contents
        .iter()
        .filter(|(_, content)| !content.trim().is_empty())
        .map(|(path, _)| path.clone())
        .collect();

    // Chris Titus Tech's managed Microsoft.PowerShell_profile.ps1 explicitly
    // reserves CurrentUserAllHosts/profile.ps1 for user customizations. An
    // otherwise empty PowerShell setup also safely uses that all-hosts profile.
    if ctt || non_empty.is_empty() {
        append_managed_block(&profiles.all_hosts, shortcut_path)?;
        return Ok(Integration::Linked {
            profile: profiles.all_hosts.clone(),
            ctt,
        });
    }

    Ok(Integration::Manual {
        profiles: non_empty,
        line: load_line,
    })
}

fn unique_profile_paths(profiles: &ProfilePaths) -> Vec<PathBuf> {
    if profiles.all_hosts == profiles.current_host {
        vec![profiles.all_hosts.clone()]
    } else {
        vec![profiles.all_hosts.clone(), profiles.current_host.clone()]
    }
}

fn read_profile(path: &Path) -> Result<String> {
    match fs::read(path) {
        Ok(content) => Ok(decode_profile(&content)),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(String::new()),
        Err(error) => Err(error)
            .with_context(|| format!("could not read PowerShell profile {}", path.display())),
    }
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ProfileEncoding {
    Utf8,
    Utf16Le,
    Utf16Be,
}

fn profile_encoding(bytes: &[u8]) -> ProfileEncoding {
    if bytes.starts_with(&[0xff, 0xfe]) {
        ProfileEncoding::Utf16Le
    } else if bytes.starts_with(&[0xfe, 0xff]) {
        ProfileEncoding::Utf16Be
    } else {
        ProfileEncoding::Utf8
    }
}

fn decode_profile(bytes: &[u8]) -> String {
    match profile_encoding(bytes) {
        ProfileEncoding::Utf8 => {
            let bytes = bytes.strip_prefix(&[0xef, 0xbb, 0xbf]).unwrap_or(bytes);
            String::from_utf8_lossy(bytes).into_owned()
        }
        encoding @ (ProfileEncoding::Utf16Le | ProfileEncoding::Utf16Be) => {
            let words = bytes[2..].chunks_exact(2).map(|pair| match encoding {
                ProfileEncoding::Utf16Le => u16::from_le_bytes([pair[0], pair[1]]),
                ProfileEncoding::Utf16Be => u16::from_be_bytes([pair[0], pair[1]]),
                ProfileEncoding::Utf8 => unreachable!(),
            });
            char::decode_utf16(words)
                .map(|result| result.unwrap_or(char::REPLACEMENT_CHARACTER))
                .collect()
        }
    }
}

fn encode_profile_append(text: &str, encoding: ProfileEncoding) -> Vec<u8> {
    match encoding {
        ProfileEncoding::Utf8 => text.as_bytes().to_vec(),
        ProfileEncoding::Utf16Le => text.encode_utf16().flat_map(u16::to_le_bytes).collect(),
        ProfileEncoding::Utf16Be => text.encode_utf16().flat_map(u16::to_be_bytes).collect(),
    }
}

fn is_chris_titus_profile(content: &str) -> bool {
    let lower = content.to_ascii_lowercase();
    lower.contains("christitustech/powershell-profile")
        || lower.contains("chris titus tech's powershell profile")
}

fn dot_source_line(shortcut_path: &Path) -> String {
    let escaped = shortcut_path.to_string_lossy().replace('\'', "''");
    format!(". '{escaped}'")
}

fn append_managed_block(profile: &Path, shortcut_path: &Path) -> Result<()> {
    if let Some(parent) = profile.parent() {
        fs::create_dir_all(parent).with_context(|| {
            format!(
                "could not create the PowerShell profile directory {}",
                parent.display()
            )
        })?;
    }
    let existing_bytes = match fs::read(profile) {
        Ok(bytes) => bytes,
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Vec::new(),
        Err(error) => {
            return Err(error).with_context(|| {
                format!("could not read PowerShell profile {}", profile.display())
            });
        }
    };
    let encoding = profile_encoding(&existing_bytes);
    let existing = decode_profile(&existing_bytes);
    let newline = if existing.contains("\r\n") {
        "\r\n"
    } else {
        "\n"
    };
    let separator = if existing.is_empty() || existing.ends_with('\n') || existing.ends_with('\r') {
        ""
    } else {
        newline
    };
    let block = format!(
        "{separator}{BLOCK_START}{newline}{}{newline}{BLOCK_END}{newline}",
        dot_source_line(shortcut_path)
    );
    let mut options = fs::OpenOptions::new();
    options.create(true).append(true);
    options
        .open(profile)
        .and_then(|mut file| file.write_all(&encode_profile_append(&block, encoding)))
        .with_context(|| format!("could not update PowerShell profile {}", profile.display()))?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use tempfile::tempdir;

    fn profiles(root: &Path) -> ProfilePaths {
        ProfilePaths {
            all_hosts: root.join("profile.ps1"),
            current_host: root.join("Microsoft.PowerShell_profile.ps1"),
        }
    }

    #[test]
    fn powershell_profile_query_accepts_unicode_paths() {
        let parsed = parse_profile_paths(
            "C:\\Users\\Dávid\\Documents\\PowerShell\\profile.ps1\0C:\\Users\\Dávid\\Documents\\PowerShell\\Microsoft.PowerShell_profile.ps1"
                .as_bytes(),
        )
        .unwrap();
        assert!(parsed.all_hosts.to_string_lossy().contains("Dávid"));
        assert!(parsed
            .current_host
            .to_string_lossy()
            .ends_with("Microsoft.PowerShell_profile.ps1"));
    }

    #[test]
    fn empty_setup_gets_an_all_hosts_profile_and_is_idempotent() {
        let root = tempdir().unwrap();
        let paths = profiles(root.path());
        let shortcuts = root.path().join("LocalX.Shortcuts.ps1");

        let first = integrate_profile(&shortcuts, &paths).unwrap();
        assert_eq!(
            first,
            Integration::Linked {
                profile: paths.all_hosts.clone(),
                ctt: false
            }
        );
        let profile = fs::read_to_string(&paths.all_hosts).unwrap();
        assert!(profile.contains(BLOCK_START));
        assert!(profile.contains(&dot_source_line(&shortcuts)));

        assert_eq!(
            integrate_profile(&shortcuts, &paths).unwrap(),
            Integration::AlreadyLinked {
                profile: paths.all_hosts
            }
        );
    }

    #[test]
    fn chris_titus_profile_uses_its_custom_profile_convention() {
        let root = tempdir().unwrap();
        let paths = profiles(root.path());
        fs::write(
            &paths.current_host,
            "# Chris Titus Tech's PowerShell profile\nfunction Update-Profile {}\n",
        )
        .unwrap();
        fs::write(&paths.all_hosts, "function mine {}\n").unwrap();
        let shortcuts = root.path().join("LocalX.Shortcuts.ps1");

        assert_eq!(
            integrate_profile(&shortcuts, &paths).unwrap(),
            Integration::Linked {
                profile: paths.all_hosts.clone(),
                ctt: true
            }
        );
        assert!(!fs::read_to_string(&paths.current_host)
            .unwrap()
            .contains(BLOCK_START));
        assert!(fs::read_to_string(&paths.all_hosts)
            .unwrap()
            .contains(BLOCK_START));
    }

    #[test]
    fn unrelated_custom_profile_is_reported_but_not_changed() {
        let root = tempdir().unwrap();
        let paths = profiles(root.path());
        let original = "function prompt { 'mine' }\n";
        fs::write(&paths.current_host, original).unwrap();
        let shortcuts = root.path().join("LocalX.Shortcuts.ps1");

        let result = integrate_profile(&shortcuts, &paths).unwrap();
        assert_eq!(
            result,
            Integration::Manual {
                profiles: vec![paths.current_host.clone()],
                line: dot_source_line(&shortcuts)
            }
        );
        assert_eq!(fs::read_to_string(&paths.current_host).unwrap(), original);
        assert!(!paths.all_hosts.exists());
    }

    #[test]
    fn a_manual_load_line_is_recognized_without_adding_a_managed_block() {
        let root = tempdir().unwrap();
        let paths = profiles(root.path());
        let shortcuts = root.path().join("LocalX.Shortcuts.ps1");
        fs::write(
            &paths.all_hosts,
            format!("{}\n", dot_source_line(&shortcuts)),
        )
        .unwrap();

        assert_eq!(
            integrate_profile(&shortcuts, &paths).unwrap(),
            Integration::AlreadyLinked {
                profile: paths.all_hosts
            }
        );
    }

    #[test]
    fn chris_titus_custom_profile_keeps_its_utf16_encoding() {
        let root = tempdir().unwrap();
        let paths = profiles(root.path());
        fs::write(
            &paths.current_host,
            "# https://github.com/ChrisTitusTech/powershell-profile\n",
        )
        .unwrap();
        let original = "function mine {}\r\n";
        let mut utf16 = vec![0xff, 0xfe];
        utf16.extend(encode_profile_append(original, ProfileEncoding::Utf16Le));
        fs::write(&paths.all_hosts, utf16).unwrap();
        let shortcuts = root.path().join("LocalX.Shortcuts.ps1");

        integrate_profile(&shortcuts, &paths).unwrap();
        let bytes = fs::read(&paths.all_hosts).unwrap();
        assert!(bytes.starts_with(&[0xff, 0xfe]));
        let decoded = decode_profile(&bytes);
        assert!(decoded.starts_with(original));
        assert!(decoded.contains(BLOCK_START));
    }

    #[test]
    fn quoted_profile_line_escapes_an_apostrophe_in_the_path() {
        let path = Path::new("C:\\Users\\D'Angelo\\LocalX.Shortcuts.ps1");
        assert_eq!(
            dot_source_line(path),
            ". 'C:\\Users\\D''Angelo\\LocalX.Shortcuts.ps1'"
        );
    }

    #[test]
    fn shipped_shortcuts_cover_the_legacy_commands_and_model_add_flow() {
        for name in [
            "llm",
            "llm-add",
            "llm-update",
            "llmlaunch",
            "llmserve",
            "llmstop",
            "llmstatus",
            "llminfo",
            "llmlog",
            "llmtune",
        ] {
            assert!(
                SHORTCUTS.contains(&format!("function {name}")),
                "missing {name}"
            );
        }
        assert!(SHORTCUTS.contains("localbox download @args"));
    }
}
