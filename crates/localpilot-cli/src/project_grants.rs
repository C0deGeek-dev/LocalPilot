//! A project's request for Accept all, and the user's authorization of it.
//!
//! A project file may ask for broad action authority (`permissions.
//! requested_action_mode`, or the legacy `bypass`/`unrestricted` profile) but
//! never grant it to itself: the loader turns the ask into a
//! [`ProjectRequest`] and drops the project's own profile. Only the user's
//! authorization makes it effective, from one of three places that no repository
//! controls: a stored grant, an answer at a terminal, or an explicit environment
//! value for one run.
//!
//! Stored grants live in `project-grants.json` beside the user config. A grant is
//! bound to the canonical project path and to the mode it was given for, so a
//! copied config in another folder, or a project that later asks for something
//! else, is asked about again. A store that cannot be read grants nothing.

use std::io::{BufRead, Write};
use std::path::{Path, PathBuf};

use localpilot_config::{ActionMode, Config, ProjectRequest};
use serde::{Deserialize, Serialize};

use crate::trust::{canonical_key, removal_key, TrustError};

/// Authorizes the project's request for this run only, for a launch nobody is
/// there to ask. The value must name the requested mode (`accept_all`).
pub const GRANT_ENV: &str = "LOCALPILOT_ACCEPT_PROJECT_ACTIONS";

const STORE_VERSION: u32 = 1;

#[derive(Debug, Default, Serialize, Deserialize)]
struct Store {
    version: u32,
    grants: Vec<Grant>,
}

/// One stored authorization.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub(crate) struct Grant {
    pub project: String,
    pub mode: ActionMode,
    pub granted: String,
}

/// A grant operation that could not be completed.
#[derive(Debug)]
pub(crate) enum GrantError {
    /// No user config directory exists, so there is nowhere to keep a grant.
    NoStore,
    /// The project path is not an existing directory, or cannot be resolved.
    Target(String),
    /// A real read, write or permission error touching the store.
    Io(std::io::Error),
    /// The store is not valid grant JSON. It grants nothing until repaired.
    Corrupt(String),
}

impl std::fmt::Display for GrantError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::NoStore => write!(formatter, "no user config directory is available"),
            Self::Target(reason) | Self::Corrupt(reason) => write!(formatter, "{reason}"),
            Self::Io(error) => write!(formatter, "{error}"),
        }
    }
}

impl std::error::Error for GrantError {}

impl From<TrustError> for GrantError {
    fn from(error: TrustError) -> Self {
        match error {
            TrustError::ConfigBaseUnavailable => Self::NoStore,
            TrustError::InvalidTarget(reason) => Self::Target(reason),
            TrustError::Persistence(error) => Self::Io(error),
        }
    }
}

/// The grant file, beside the user config. `None` when no config base exists.
pub(crate) fn store_path() -> Option<PathBuf> {
    localpilot_config::user_config_path().map(|config| config.with_file_name("project-grants.json"))
}

fn read(store: &Path) -> Result<Store, GrantError> {
    match std::fs::read_to_string(store) {
        Ok(text) => serde_json::from_str(&text)
            .map_err(|error| GrantError::Corrupt(format!("{}: {error}", store.display()))),
        Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(Store::default()),
        Err(error) => Err(GrantError::Io(error)),
    }
}

/// Replace the store through a same-directory temp file, so the live file is only
/// ever swapped for a fully written one.
fn write(store: &Path, contents: &Store) -> Result<(), GrantError> {
    let parent = store.parent().ok_or(GrantError::NoStore)?;
    std::fs::create_dir_all(parent).map_err(GrantError::Io)?;
    let mut staged = tempfile::NamedTempFile::new_in(parent).map_err(GrantError::Io)?;
    serde_json::to_writer_pretty(
        &mut staged,
        &Store {
            version: STORE_VERSION,
            grants: contents.grants.clone(),
        },
    )
    .map_err(|error| GrantError::Io(error.into()))?;
    writeln!(staged).map_err(GrantError::Io)?;
    staged.flush().map_err(GrantError::Io)?;
    staged.as_file().sync_all().map_err(GrantError::Io)?;
    staged
        .persist(store)
        .map_err(|error| GrantError::Io(error.error))?;
    Ok(())
}

/// Two spellings of one folder differ only by a Windows verbatim prefix.
fn same_project(left: &str, right: &str) -> bool {
    let bare = |path: &str| path.strip_prefix(r"\\?\").unwrap_or(path).to_string();
    bare(left) == bare(right)
}

/// Whether `dir` holds a stored grant for exactly `mode`.
pub(crate) fn is_granted_in(
    dir: &Path,
    store: &Path,
    mode: ActionMode,
) -> Result<bool, GrantError> {
    let project = canonical_key(dir)?;
    Ok(read(store)?
        .grants
        .iter()
        .any(|grant| grant.mode == mode && same_project(&grant.project, &project)))
}

/// Record a grant. Returns `false` when the same grant was already stored.
pub(crate) fn add_in(
    dir: &Path,
    store: &Path,
    mode: ActionMode,
    granted: &str,
) -> Result<bool, GrantError> {
    let project = canonical_key(dir)?;
    let mut contents = read(store)?;
    if contents
        .grants
        .iter()
        .any(|grant| grant.mode == mode && same_project(&grant.project, &project))
    {
        return Ok(false);
    }
    contents.grants.push(Grant {
        project,
        mode,
        granted: granted.to_string(),
    });
    write(store, &contents)?;
    Ok(true)
}

/// Remove every grant for `dir`, whatever its mode. Returns how many were removed.
pub(crate) fn revoke_in(dir: &Path, store: &Path) -> Result<usize, GrantError> {
    let project = removal_key(dir)?;
    let mut contents = read(store)?;
    let before = contents.grants.len();
    contents
        .grants
        .retain(|grant| !same_project(&grant.project, &project));
    let removed = before - contents.grants.len();
    if removed > 0 {
        write(store, &contents)?;
    }
    Ok(removed)
}

/// Every stored grant, sorted by project.
pub(crate) fn list_in(store: &Path) -> Result<Vec<Grant>, GrantError> {
    let mut grants = read(store)?.grants;
    grants.sort_by(|left, right| left.project.cmp(&right.project));
    Ok(grants)
}

/// The answer at the terminal.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Answer {
    /// Allow it for this run only.
    Session,
    /// Allow it and keep the grant for this project.
    Remember,
    /// Anything else, including no answer.
    No,
}

/// Ask on `output`, read the answer from `input`. A closed input is a no.
pub(crate) fn ask_on<R: BufRead, W: Write>(
    input: &mut R,
    output: &mut W,
    request: &ProjectRequest,
    root: &Path,
) -> Answer {
    let _ = write!(
        output,
        "
  This project asks for Accept all.

    {}
    asks with: {}

  Accept all lets the agent run any command, including scripts LocalPilot cannot
  inspect, and change files outside this folder, without asking again. Programs it
  starts can reach the network with your OS account's permissions. Work limits,
  required verification, an incognito session and a read-only restriction still
  apply.

  [s] this session only    [r] remember for this project    [n] no (default)
  > ",
        root.display(),
        request.source.describe(),
    );
    let _ = output.flush();
    let mut line = String::new();
    match input.read_line(&mut line) {
        Ok(0) | Err(_) => Answer::No,
        Ok(_) => match line.trim().chars().next().map(|c| c.to_ascii_lowercase()) {
            Some('s') => Answer::Session,
            Some('r') => Answer::Remember,
            _ => Answer::No,
        },
    }
}

/// Whatever asks the user about a request, at a terminal or in a test.
pub(crate) type Asker = dyn FnMut(&ProjectRequest, &Path) -> Answer;

/// Where the authorization came from.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Basis {
    Environment,
    Stored,
    Session,
    Remembered,
}

impl Basis {
    const fn describe(self) -> &'static str {
        match self {
            Self::Environment => "by LOCALPILOT_ACCEPT_PROJECT_ACTIONS for this run",
            Self::Stored => "by a stored grant for this project",
            Self::Session => "for this session",
            Self::Remembered => "and remembered for this project",
        }
    }
}

/// The outcome of resolving a request: `basis` is set when it was authorized.
/// `note` carries what the user should know either way.
#[derive(Debug, PartialEq, Eq)]
pub(crate) struct Resolution {
    pub basis: Option<Basis>,
    pub note: Option<String>,
}

fn mode_name(mode: ActionMode) -> &'static str {
    match mode {
        ActionMode::Normal => "normal",
        ActionMode::AcceptAll => "accept_all",
    }
}

/// Decide whether `request` for the project at `root` is authorized. A stored
/// grant or the environment value needs no one to ask; otherwise `ask` is
/// consulted when there is someone to ask, and without one the request is
/// declined with the way to authorize it.
pub(crate) fn resolve(
    request: &ProjectRequest,
    root: &Path,
    store: Option<&Path>,
    env: Option<&str>,
    ask: Option<&mut Asker>,
    today: &str,
) -> Resolution {
    let wanted = mode_name(request.mode);
    match env.map(str::trim).filter(|value| !value.is_empty()) {
        Some(value) if value == wanted => {
            return Resolution {
                basis: Some(Basis::Environment),
                note: None,
            }
        }
        Some(value) => {
            return Resolution {
                basis: None,
                note: Some(format!(
                    "{GRANT_ENV} is `{value}`; it must be `{wanted}` to authorize this request"
                )),
            }
        }
        None => {}
    }
    let mut note = None;
    match store.map(|path| is_granted_in(root, path, request.mode)) {
        Some(Ok(true)) => {
            return Resolution {
                basis: Some(Basis::Stored),
                note: None,
            }
        }
        Some(Err(error)) => note = Some(format!("the stored grants cannot be read: {error}")),
        Some(Ok(false)) | None => {}
    }
    let Some(ask) = ask else {
        return Resolution {
            basis: None,
            note: Some(note.unwrap_or_else(|| {
                format!(
                    "nobody is there to ask. Authorize it once at a terminal \
                     (`localpilot chat`), or set {GRANT_ENV}={wanted} for one run"
                )
            })),
        };
    };
    match ask(request, root) {
        Answer::Session => Resolution {
            basis: Some(Basis::Session),
            note,
        },
        Answer::Remember => {
            let saved = match store {
                Some(path) => add_in(root, path, request.mode, today).map_err(|e| e.to_string()),
                None => Err("no user config directory is available".to_string()),
            };
            match saved {
                Ok(_) => Resolution {
                    basis: Some(Basis::Remembered),
                    note,
                },
                Err(error) => Resolution {
                    basis: Some(Basis::Session),
                    note: Some(format!("it could not be remembered: {error}")),
                },
            }
        }
        Answer::No => Resolution {
            basis: None,
            note: Some(match note {
                Some(note) => format!("not authorized; {note}"),
                None => "not authorized".to_string(),
            }),
        },
    }
}

/// Resolve the project's request and, when authorized, make it the user's
/// selection for this run. A request that is not authorized changes nothing, and
/// the launch says so.
pub(crate) fn apply_with(
    config: &mut Config,
    root: &Path,
    store: Option<&Path>,
    env: Option<&str>,
    ask: Option<&mut Asker>,
    today: &str,
) -> Resolution {
    let Some(request) = config.project_request else {
        return Resolution {
            basis: None,
            note: None,
        };
    };
    let resolution = resolve(&request, root, store, env, ask, today);
    match resolution.basis {
        Some(basis) => {
            config.permissions.action_mode = Some(request.mode);
            eprintln!(
                "Project request: Accept all authorized {}.",
                basis.describe()
            );
            if let Some(note) = &resolution.note {
                eprintln!("  ({note})");
            }
        }
        None => eprintln!(
            "Project request: this project asks for Accept all ({}) and it is off for this run: {}.",
            request.source.describe(),
            resolution.note.as_deref().unwrap_or("not authorized")
        ),
    }
    resolution
}

/// The launcher's entry: the real store, environment and terminal. `may_prompt`
/// is false for single-turn and evaluation runs, which never wait for an answer.
pub(crate) fn apply(config: &mut Config, root: &Path, may_prompt: bool) {
    if config.project_request.is_none() {
        return;
    }
    let store = store_path();
    let env = std::env::var(GRANT_ENV).ok();
    let mut terminal = |request: &ProjectRequest, root: &Path| {
        ask_on(
            &mut std::io::stdin().lock(),
            &mut std::io::stderr(),
            request,
            root,
        )
    };
    let ask: Option<&mut Asker> = if may_prompt && crate::egress_consent::someone_to_ask() {
        Some(&mut terminal)
    } else {
        None
    };
    apply_with(
        config,
        root,
        store.as_deref(),
        env.as_deref(),
        ask,
        &localpilot_harness::today(),
    );
}

/// `localpilot trust actions list`.
pub(crate) fn list_cmd() -> anyhow::Result<()> {
    let store = store_path().ok_or(GrantError::NoStore)?;
    let grants = list_in(&store)?;
    if grants.is_empty() {
        println!("No project has been authorized for Accept all.");
    }
    for grant in grants {
        println!(
            "{}\t{}\t{}",
            grant.project,
            mode_name(grant.mode),
            grant.granted
        );
    }
    Ok(())
}

/// `localpilot trust actions revoke [PATH]`.
pub(crate) fn revoke_cmd(path: Option<PathBuf>) -> anyhow::Result<()> {
    let store = store_path().ok_or(GrantError::NoStore)?;
    let dir = match path {
        Some(path) => path,
        None => std::env::current_dir()?,
    };
    match revoke_in(&dir, &store)? {
        0 => println!("No grant for {}.", dir.display()),
        count => println!("Revoked {count} grant(s) for {}.", dir.display()),
    }
    Ok(())
}

#[cfg(test)]
#[allow(clippy::unwrap_used)]
mod tests {
    use super::*;
    use localpilot_config::{ConfigPaths, ProjectRequestSource};
    use localpilot_core::{ToolCall, ToolUseId};
    use localpilot_sandbox::{
        Interactivity, PermissionEngine, Profile, ScriptedApprover, Workspace,
    };
    use localpilot_tools::{ToolContext, ToolRegistry};
    use serde_json::json;

    const REQUEST: ProjectRequest = ProjectRequest {
        mode: ActionMode::AcceptAll,
        source: ProjectRequestSource::RequestedActionMode,
    };

    fn never(_: &ProjectRequest, _: &Path) -> Answer {
        panic!("a stored grant or the environment must not ask");
    }

    #[test]
    fn a_grant_is_bound_to_its_project_and_its_mode_and_can_be_revoked() {
        let base = tempfile::tempdir().unwrap();
        let store = base.path().join("config").join("project-grants.json");
        let project = tempfile::tempdir().unwrap();
        let other = tempfile::tempdir().unwrap();

        assert!(!is_granted_in(project.path(), &store, ActionMode::AcceptAll).unwrap());
        assert!(add_in(project.path(), &store, ActionMode::AcceptAll, "2026-11-04").unwrap());
        assert!(!add_in(project.path(), &store, ActionMode::AcceptAll, "2026-11-04").unwrap());
        assert!(is_granted_in(project.path(), &store, ActionMode::AcceptAll).unwrap());
        // Copying a project's config into another folder does not copy consent.
        assert!(!is_granted_in(other.path(), &store, ActionMode::AcceptAll).unwrap());
        // A different mode than the one granted is a changed request.
        assert!(!is_granted_in(project.path(), &store, ActionMode::Normal).unwrap());

        let listed = list_in(&store).unwrap();
        assert_eq!(listed.len(), 1);
        assert_eq!(listed[0].granted, "2026-11-04");

        assert_eq!(revoke_in(project.path(), &store).unwrap(), 1);
        assert!(!is_granted_in(project.path(), &store, ActionMode::AcceptAll).unwrap());
        assert_eq!(revoke_in(project.path(), &store).unwrap(), 0);
    }

    #[test]
    fn a_store_that_cannot_be_read_grants_nothing_and_is_not_overwritten() {
        let base = tempfile::tempdir().unwrap();
        let store = base.path().join("project-grants.json");
        std::fs::write(&store, "{ not json").unwrap();
        let project = tempfile::tempdir().unwrap();

        assert!(matches!(
            is_granted_in(project.path(), &store, ActionMode::AcceptAll),
            Err(GrantError::Corrupt(_))
        ));
        assert!(add_in(project.path(), &store, ActionMode::AcceptAll, "2026-11-04").is_err());
        assert_eq!(std::fs::read_to_string(&store).unwrap(), "{ not json");

        let mut ask = |_: &ProjectRequest, _: &Path| Answer::No;
        let resolution = resolve(
            &REQUEST,
            project.path(),
            Some(&store),
            None,
            Some(&mut ask),
            "d",
        );
        assert_eq!(resolution.basis, None);
        assert!(resolution.note.unwrap().contains("cannot be read"));
    }

    #[test]
    fn the_terminal_answer_chooses_session_remembered_or_no() {
        let root = Path::new("project");
        let answer = |input: &str| {
            let mut output = Vec::new();
            let result = ask_on(&mut input.as_bytes(), &mut output, &REQUEST, root);
            (result, String::from_utf8(output).unwrap())
        };
        assert_eq!(answer("s\n").0, Answer::Session);
        assert_eq!(answer("R\n").0, Answer::Remember);
        for no in ["n\n", "\n", "yes\n", ""] {
            assert_eq!(answer(no).0, Answer::No, "{no:?}");
        }
        let shown = answer("n\n").1;
        assert!(shown.contains("permissions.requested_action_mode"));
        assert!(shown.contains("outside this folder"));
    }

    #[test]
    fn resolution_uses_the_environment_a_stored_grant_or_asks_and_never_hangs() {
        let base = tempfile::tempdir().unwrap();
        let store = base.path().join("project-grants.json");
        let project = tempfile::tempdir().unwrap();
        let root = project.path();
        let resolve_with = |env: Option<&str>, ask: Option<&mut Asker>| {
            resolve(&REQUEST, root, Some(&store), env, ask, "2026-11-04")
        };

        // Headless without a grant: declined, with the way to authorize it.
        let declined = resolve_with(None, None);
        assert_eq!(declined.basis, None);
        let note = declined.note.unwrap();
        assert!(
            note.contains(GRANT_ENV) && note.contains("accept_all"),
            "{note}"
        );

        // The environment value authorizes one run and must name the mode.
        let mut asked = never;
        assert_eq!(
            resolve_with(Some("accept_all"), Some(&mut asked)).basis,
            Some(Basis::Environment)
        );
        let wrong = resolve_with(Some("yes"), Some(&mut asked));
        assert_eq!(wrong.basis, None);
        assert!(wrong.note.unwrap().contains("must be `accept_all`"));

        // Session only does not remember.
        let mut session = |_: &ProjectRequest, _: &Path| Answer::Session;
        assert_eq!(
            resolve_with(None, Some(&mut session)).basis,
            Some(Basis::Session)
        );
        assert!(!is_granted_in(root, &store, ActionMode::AcceptAll).unwrap());

        // Remember stores it, and the next launch no longer asks.
        let mut remember = |_: &ProjectRequest, _: &Path| Answer::Remember;
        assert_eq!(
            resolve_with(None, Some(&mut remember)).basis,
            Some(Basis::Remembered)
        );
        assert_eq!(
            resolve_with(None, Some(&mut asked)).basis,
            Some(Basis::Stored)
        );

        // Revoked: asked again, and with nobody to ask, declined.
        revoke_in(root, &store).unwrap();
        assert_eq!(resolve_with(None, None).basis, None);
        let mut no = |_: &ProjectRequest, _: &Path| Answer::No;
        assert_eq!(resolve_with(None, Some(&mut no)).basis, None);
    }

    /// Whether a write outside the workspace is dispatched under `profile`,
    /// headless and with nobody to approve it.
    async fn external_write_runs(profile: Profile) -> bool {
        let workspace_dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        let workspace = Workspace::new(workspace_dir.path()).unwrap();
        let ctx = ToolContext {
            workspace: &workspace,
            interactivity: Interactivity::NonInteractive,
            trusted: true,
            retention: None,
            processes: None,
            agents: None,
            prompter: None,
            peers: None,
        };
        let target = outside.path().join("external.txt");
        let call = ToolCall::new(
            ToolUseId::from("write"),
            "write_file",
            json!({"path": target, "content": "x"}),
        );
        let result = ToolRegistry::with_builtins()
            .dispatch(
                &call,
                &ctx,
                &PermissionEngine::new(profile, Vec::new()),
                &ScriptedApprover::new(Vec::new()),
            )
            .await;
        !result.is_error() && target.exists()
    }

    #[tokio::test]
    async fn a_project_request_changes_actual_dispatch_only_with_the_users_grant() {
        let base = tempfile::tempdir().unwrap();
        let store = base.path().join("project-grants.json");
        let project = tempfile::tempdir().unwrap();
        std::fs::write(
            project.path().join(".localpilot.toml"),
            "[permissions]\nprofile = \"unrestricted\"\n",
        )
        .unwrap();
        let load = || {
            localpilot_config::load(
                &ConfigPaths {
                    user: None,
                    project: Some(project.path().join(".localpilot.toml")),
                },
                &localpilot_config::CliOverrides::default(),
            )
            .unwrap()
        };
        let profile_of = |config: &Config| {
            crate::session_cmd::action_profile(
                config,
                None,
                false,
                crate::session_cmd::resolve_profile_from_config(config),
            )
        };
        let root = project.path();

        // Absent grant: the project's own profile never applies.
        let mut config = load();
        apply_with(&mut config, root, Some(&store), None, None, "2026-11-04");
        assert_eq!(profile_of(&config), Profile::Default);
        assert!(!external_write_runs(profile_of(&config)).await);

        // Granted: the request is the user's selection and takes effect.
        add_in(root, &store, ActionMode::AcceptAll, "2026-11-04").unwrap();
        let mut config = load();
        let mut asked = never;
        apply_with(
            &mut config,
            root,
            Some(&store),
            None,
            Some(&mut asked),
            "2026-11-04",
        );
        assert_eq!(profile_of(&config), Profile::Unrestricted);
        assert!(external_write_runs(profile_of(&config)).await);

        // Revoked: back to the default, on the very next launch.
        revoke_in(root, &store).unwrap();
        let mut config = load();
        apply_with(&mut config, root, Some(&store), None, None, "2026-11-04");
        assert_eq!(profile_of(&config), Profile::Default);
        assert!(!external_write_runs(profile_of(&config)).await);
    }
}
