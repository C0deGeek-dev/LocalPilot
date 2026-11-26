//! Reaching the LocalBox model the user adopted when a session starts.
//!
//! `localpilot localbox adopt --user` records the LocalBox model behind the
//! user's `local` provider. That record is the standing consent to start the
//! model again: when a chat, `print` or `ask` uses exactly that provider and
//! its loopback endpoint does not answer (after a reboot, say), LocalPilot asks
//! LocalBox to start its saved launch for that port and waits for it. LocalBox
//! keeps owning the server and its teardown. Anything else — another
//! provider, a project or environment override of the address, a different
//! model on the port — is left alone.

use std::path::{Path, PathBuf};
use std::process::Stdio;

use localpilot_config::Config;

/// The adoption the user config file records.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Adopted {
    pub(crate) model: String,
    kind: String,
    base_url: String,
}

/// Read the adoption from the user config file's text: `[localbox] model`
/// and the `[providers.local]` address it was recorded for.
pub(crate) fn adopted_from_user_file(text: &str) -> Option<Adopted> {
    let doc: toml_edit::DocumentMut = text.parse().ok()?;
    let model = doc
        .get("localbox")?
        .get("model")?
        .as_str()?
        .trim()
        .to_string();
    let local = doc.get("providers")?.get("local")?;
    let kind = local.get("kind")?.as_str()?.to_string();
    let base_url = local.get("base_url")?.as_str()?.to_string();
    (!model.is_empty()).then_some(Adopted {
        model,
        kind,
        base_url,
    })
}

/// The adopted launch this session may start, with its proxy port: only when
/// the selected provider is `local` and its effective kind and address are
/// the ones the user file adopted, on this computer.
pub(crate) fn managed_launch(
    config: &Config,
    provider_id: Option<&str>,
    adopted: Option<Adopted>,
) -> Option<(Adopted, u16)> {
    let adopted = adopted?;
    if provider_id.unwrap_or(&config.provider.default) != "local" {
        return None;
    }
    let effective = config.providers.get("local")?;
    if effective.kind != adopted.kind
        || effective.base_url.as_deref() != Some(adopted.base_url.as_str())
    {
        return None;
    }
    let (_, port) = crate::localbox::loopback_endpoint(&adopted.base_url).ok()?;
    Some((adopted, port))
}

/// The point a start failed at.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    /// LocalBox could not start the model.
    Start,
    /// LocalBox reported the model started, but the endpoint does not answer.
    Readiness,
    /// The model answered the endpoint but the first request failed.
    FirstReply,
}

impl Stage {
    fn label(self) -> &'static str {
        match self {
            Self::Start => "start",
            Self::Readiness => "readiness",
            Self::FirstReply => "first reply",
        }
    }
}

/// Why the adopted model is not usable, with what to do next.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Guidance {
    pub(crate) stage: Stage,
    pub(crate) model: String,
    pub(crate) port: u16,
    pub(crate) why: String,
    pub(crate) log: Option<PathBuf>,
}

impl std::fmt::Display for Guidance {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        let Self {
            stage,
            model,
            port,
            why,
            log,
        } = self;
        writeln!(
            f,
            "The local model {model} is not usable (stage: {}).",
            stage.label()
        )?;
        writeln!(f, "  why:      {why}")?;
        if let Some(log) = log {
            writeln!(f, "  log:      {}", log.display())?;
        }
        writeln!(f, "  LocalBox: localbox log   (the model server's own log)")?;
        writeln!(
            f,
            "  retry:    localbox serve --again --proxy-port {port} --expect-model {model}"
        )?;
        writeln!(f, "  check:    localbox status")?;
        write!(
            f,
            "  fallback: localx setup   (set up a model again), or --provider <name> for another provider"
        )
    }
}

/// What a launch found.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum Reached {
    /// Not a LocalBox model this session may start: nothing was checked.
    NotManaged,
    /// The adopted model already answers.
    Serving { model: String, port: u16 },
    /// LocalPilot started it through LocalBox.
    Started {
        model: String,
        port: u16,
        log: PathBuf,
    },
    /// The port answers, but LocalBox does not report the adopted model there;
    /// nothing was touched.
    Other { note: String },
}

impl Reached {
    /// The model and port when this session reaches the adopted model.
    pub(crate) fn adopted(&self) -> Option<(&str, u16)> {
        match self {
            Self::Serving { model, port } | Self::Started { model, port, .. } => {
                Some((model, *port))
            }
            Self::NotManaged | Self::Other { .. } => None,
        }
    }
}

/// The effects a launch needs, so the decision can be tested without a
/// network or a LocalBox install.
pub(crate) trait Effects {
    /// Whether the endpoint answers a model listing.
    async fn answers(&self, base_url: &str) -> bool;
    /// The catalog key LocalBox reports serving on `port`, or why that is not
    /// known.
    async fn serving(&self, port: u16) -> Result<String, String>;
    /// Ask LocalBox to start its saved launch for `port`, output to `log`.
    async fn serve_again(&self, port: u16, model: &str, log: &Path) -> Result<(), String>;
    /// Where this start writes its output.
    fn log_path(&self, port: u16) -> PathBuf;
}

/// How often, and how far apart, the endpoint is checked after LocalBox says
/// the model started.
const READY_CHECKS: u32 = 10;
const READY_PAUSE: std::time::Duration = std::time::Duration::from_secs(1);

/// Reach the adopted model for a session: connect when it answers, start it
/// through LocalBox when it does not.
///
/// # Errors
/// [`Guidance`] naming the stage that failed, its log and the commands to run.
pub(crate) async fn reach<E: Effects>(
    effects: &E,
    launch: Option<(Adopted, u16)>,
    mut say: impl FnMut(&str),
) -> Result<Reached, Guidance> {
    let Some((adopted, port)) = launch else {
        return Ok(Reached::NotManaged);
    };
    let model = adopted.model;
    let guidance = |stage, why: String, log: Option<PathBuf>| Guidance {
        stage,
        model: model.clone(),
        port,
        why,
        log,
    };
    if effects.answers(&adopted.base_url).await {
        return match effects.serving(port).await {
            Ok(key) if key == model => Ok(Reached::Serving {
                model: model.clone(),
                port,
            }),
            Ok(other) => Ok(Reached::Other {
                note: format!(
                    "note: port {port} serves {other}, not the adopted {model}; LocalPilot left it as it is"
                ),
            }),
            Err(why) => Ok(Reached::Other {
                note: format!(
                    "note: port {port} answers, but LocalBox does not report {model} there ({why}); LocalPilot left it as it is"
                ),
            }),
        };
    }
    let log = effects.log_path(port);
    say(&format!(
        "Starting {model} through LocalBox (loading can take a few minutes; log: {})",
        log.display()
    ));
    effects
        .serve_again(port, &model, &log)
        .await
        .map_err(|why| guidance(Stage::Start, why, Some(log.clone())))?;
    // Ready means the address answers and LocalBox reports the adopted model
    // there: any HTTP answer alone could be another program.
    let mut last = format!("{} does not answer", adopted.base_url);
    for check in 0..READY_CHECKS {
        if effects.answers(&adopted.base_url).await {
            match effects.serving(port).await {
                Ok(key) if key == model => {
                    return Ok(Reached::Started {
                        model: model.clone(),
                        port,
                        log,
                    })
                }
                Ok(other) => last = format!("port {port} answers with {other}, not {model}"),
                Err(why) => {
                    last = format!("LocalBox does not report {model} on port {port} ({why})")
                }
            }
        }
        if check + 1 < READY_CHECKS {
            tokio::time::sleep(READY_PAUSE).await;
        }
    }
    Err(guidance(
        Stage::Readiness,
        format!("LocalBox started {model}, but {last}"),
        Some(log),
    ))
}

/// The guidance for a first request that failed on a model this session
/// reached, for a caller that saw the failure.
pub(crate) fn first_reply_guidance(reached: &Reached, why: &str) -> Option<Guidance> {
    let (model, port) = reached.adopted()?;
    let log = match reached {
        Reached::Started { log, .. } => Some(log.clone()),
        _ => None,
    };
    Some(Guidance {
        stage: Stage::FirstReply,
        model: model.to_string(),
        port,
        why: why.to_string(),
        log,
    })
}

/// The first-reply guidance one chat shows if its first turn fails to reach
/// the LocalBox model this chat started or found. Owned by the chat; the
/// first turn uses it up whatever happens.
#[cfg(feature = "tui")]
pub(crate) struct FirstChatTurn(std::cell::RefCell<Option<(String, Guidance)>>);

#[cfg(feature = "tui")]
impl FirstChatTurn {
    /// Armed for `provider_id` when this chat reached a managed LocalBox
    /// model; empty otherwise.
    pub(crate) fn new(reached: &Reached, provider_id: &str) -> Self {
        let guidance = first_reply_guidance(
            reached,
            "the model did not answer the first request in this chat",
        );
        Self(std::cell::RefCell::new(
            guidance.map(|guidance| (provider_id.to_string(), guidance)),
        ))
    }

    /// A turn on `provider_id` ended. Only the first turn, only on the provider
    /// the guidance was armed for and only after a provider failure gets the
    /// guidance; later turns and other providers keep their usual message.
    pub(crate) fn turn_ended(&self, provider_id: &str, provider_failed: bool) -> Option<String> {
        let (armed_for, guidance) = self.0.borrow_mut().take()?;
        (provider_failed && armed_for == provider_id).then(|| guidance.to_string())
    }
}

/// The real effects: HTTP for the probe, the `localbox` command for the rest.
pub(crate) struct System;

impl Effects for System {
    async fn answers(&self, base_url: &str) -> bool {
        localpilot_llm::discover_models(base_url, None)
            .await
            .is_ok()
    }

    async fn serving(&self, port: u16) -> Result<String, String> {
        let catalog = crate::localbox::read_models_catalog_for(port)
            .await
            .map_err(|error| error.to_string())?;
        crate::localbox::serving_key(&catalog, port)
    }

    async fn serve_again(&self, port: u16, model: &str, log: &Path) -> Result<(), String> {
        let program = crate::localbox::localbox_on_path().ok_or_else(|| {
            "LocalBox is not installed (no `localbox` in the LocalX bin directory or on PATH); \
             install it with `localx install localbox`"
                .to_string()
        })?;
        if let Some(dir) = log.parent() {
            std::fs::create_dir_all(dir)
                .map_err(|error| format!("could not create {}: {error}", dir.display()))?;
        }
        let file = std::fs::File::create(log)
            .map_err(|error| format!("could not create {}: {error}", log.display()))?;
        let err = file
            .try_clone()
            .map_err(|error| format!("could not open {}: {error}", log.display()))?;
        // Windows hands every inheritable handle to a child, so this process's
        // own standard streams (a caller's pipe) would reach the model server
        // LocalBox leaves running and keep that caller waiting until it stops.
        #[cfg(windows)]
        localpilot_winsec::keep_std_handles_from_children().map_err(|error| {
            format!("could not keep this terminal's streams from the model server: {error}")
        })?;
        // Both streams go to the log file, never to this process's own: the
        // server LocalBox leaves running must not hold a caller's pipe open.
        let status = tokio::process::Command::new(program)
            .args(again_args(port, model))
            .stdin(Stdio::null())
            .stdout(Stdio::from(file))
            .stderr(Stdio::from(err))
            .kill_on_drop(false)
            .status()
            .await
            .map_err(|error| format!("could not run `localbox serve --again`: {error}"))?;
        let written = std::fs::read_to_string(log).unwrap_or_default();
        let receipt = written
            .lines()
            .rev()
            .find_map(|line| serde_json::from_str::<serde_json::Value>(line.trim()).ok());
        match receipt {
            Some(receipt) if status.success() && receipt["ok"] == true => Ok(()),
            Some(receipt) => Err(receipt["error"]
                .as_str()
                .map_or_else(|| format!("`localbox serve --again` exited with {status}"), str::to_string)),
            None if status.success() => Err(
                "this LocalBox cannot start a saved launch; update it with `localx install localbox`"
                    .to_string(),
            ),
            None => Err(last_line(&written)
                .unwrap_or_else(|| format!("`localbox serve --again` exited with {status}"))),
        }
    }

    fn log_path(&self, port: u16) -> PathBuf {
        let stamp = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map_or(0, |elapsed| elapsed.as_secs());
        // One file per start: two sessions opening at once each keep their own.
        localpilot_dist::Cache::default_root("localpilot")
            .unwrap_or_else(std::env::temp_dir)
            .join("logs")
            .join(format!(
                "localbox-start-{port}-{stamp}-{}.log",
                std::process::id()
            ))
    }
}

/// The `localbox` arguments that start the saved launch for `port`, only when
/// it is `model`.
pub(crate) fn again_args(port: u16, model: &str) -> Vec<String> {
    vec![
        "serve".to_string(),
        "--again".to_string(),
        "--proxy-port".to_string(),
        port.to_string(),
        "--expect-model".to_string(),
        model.to_string(),
        "--json".to_string(),
    ]
}

fn last_line(text: &str) -> Option<String> {
    text.lines()
        .rev()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .map(|line| line.chars().take(300).collect())
}

/// Read the adoption from the standard user config file.
pub(crate) fn adopted_for(cwd: &Path) -> Option<Adopted> {
    let path = localpilot_config::ConfigPaths::standard(cwd).user?;
    adopted_from_user_file(&std::fs::read_to_string(path).ok()?)
}

/// Reach the adopted model before a session's first request, printing
/// progress and notes on standard error.
///
/// # Errors
/// [`Guidance`] when the model could not be started or does not answer.
pub(crate) async fn reach_for_session(
    cwd: &Path,
    config: &Config,
    provider_id: Option<&str>,
) -> Result<Reached, Guidance> {
    let launch = managed_launch(config, provider_id, adopted_for(cwd));
    let reached = reach(&System, launch, |line| eprintln!("{line}")).await?;
    if let Reached::Other { note } = &reached {
        eprintln!("{note}");
    }
    Ok(reached)
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use std::cell::RefCell;

    use super::*;

    const USER: &str = "[provider]\ndefault = \"local\"\n[providers.local]\nkind = \"anthropic\"\nbase_url = \"http://127.0.0.1:11435/v1\"\n[localbox]\nmodel = \"qwen3-5-9b-gguf\"\n";

    fn config(text: &str) -> Config {
        let dir = tempfile::tempdir().unwrap();
        let user = dir.path().join("user.toml");
        std::fs::write(&user, text).unwrap();
        localpilot_config::load(
            &localpilot_config::ConfigPaths {
                user: Some(user),
                project: None,
            },
            &localpilot_config::CliOverrides::default(),
        )
        .unwrap()
    }

    fn adopted() -> Option<Adopted> {
        adopted_from_user_file(USER)
    }

    #[test]
    fn the_adoption_is_read_from_the_user_file() {
        let adopted = adopted().unwrap();
        assert_eq!(adopted.model, "qwen3-5-9b-gguf");
        assert_eq!(adopted.base_url, "http://127.0.0.1:11435/v1");
        assert_eq!(adopted_from_user_file("[providers.local]\nkind = \"anthropic\"\nbase_url = \"http://127.0.0.1:11435/v1\"\n"), None);
        assert_eq!(adopted_from_user_file("[localbox]\nmodel = \"m\"\n"), None);
        assert_eq!(adopted_from_user_file("not toml ["), None);
    }

    #[test]
    fn only_the_adopted_local_provider_on_this_computer_may_be_started() {
        let base = config(USER);
        assert_eq!(
            managed_launch(&base, None, adopted()).map(|(_, port)| port),
            Some(11_435)
        );
        assert_eq!(
            managed_launch(&base, Some("local"), adopted()).map(|(_, port)| port),
            Some(11_435)
        );
        // Another provider selected, by flag or by default.
        assert!(managed_launch(&base, Some("anthropic"), adopted()).is_none());
        let mut other_default = base.clone();
        other_default.provider.default = "anthropic".to_string();
        assert!(managed_launch(&other_default, None, adopted()).is_none());
        // A project or environment layer moved the address or the kind.
        let mut moved = base.clone();
        moved.providers.get_mut("local").unwrap().base_url =
            Some("http://127.0.0.1:9999/v1".to_string());
        assert!(managed_launch(&moved, None, adopted()).is_none());
        let mut rekinded = base.clone();
        rekinded.providers.get_mut("local").unwrap().kind = "openai-compatible".to_string();
        assert!(managed_launch(&rekinded, None, adopted()).is_none());
        // Nothing adopted, or an adoption that is not on this computer.
        assert!(managed_launch(&base, None, None).is_none());
        let remote = USER.replace("127.0.0.1", "192.168.1.9");
        assert!(managed_launch(&config(&remote), None, adopted_from_user_file(&remote)).is_none());
    }

    #[derive(Default)]
    struct Fake {
        /// What successive endpoint checks answer; the last value repeats.
        answers: RefCell<Vec<bool>>,
        serving: Option<Result<String, String>>,
        start: Option<Result<(), String>>,
        started: RefCell<Vec<(u16, String)>>,
    }

    impl Effects for Fake {
        async fn answers(&self, _base_url: &str) -> bool {
            let mut answers = self.answers.borrow_mut();
            if answers.len() > 1 {
                answers.remove(0)
            } else {
                answers.first().copied().unwrap_or(false)
            }
        }
        async fn serving(&self, _port: u16) -> Result<String, String> {
            self.serving.clone().unwrap()
        }
        async fn serve_again(&self, port: u16, model: &str, _log: &Path) -> Result<(), String> {
            self.started.borrow_mut().push((port, model.to_string()));
            self.start.clone().unwrap()
        }
        fn log_path(&self, port: u16) -> PathBuf {
            PathBuf::from(format!("logs/localbox-start-{port}.log"))
        }
    }

    fn launch() -> Option<(Adopted, u16)> {
        adopted().map(|adopted| (adopted, 11_435))
    }

    #[tokio::test]
    async fn an_unmanaged_provider_is_not_checked() {
        let fake = Fake::default();
        assert_eq!(reach(&fake, None, |_| {}).await, Ok(Reached::NotManaged));
        assert!(fake.started.borrow().is_empty());
    }

    #[tokio::test]
    async fn a_running_adopted_model_is_used_as_it_is() {
        let fake = Fake {
            answers: RefCell::new(vec![true]),
            serving: Some(Ok("qwen3-5-9b-gguf".to_string())),
            ..Fake::default()
        };
        let reached = reach(&fake, launch(), |_| panic!("nothing to say"))
            .await
            .unwrap();
        assert_eq!(reached.adopted(), Some(("qwen3-5-9b-gguf", 11_435)));
        assert!(fake.started.borrow().is_empty());
    }

    #[tokio::test]
    async fn another_model_or_an_unknown_listener_is_left_alone() {
        for serving in [
            Ok("other-model".to_string()),
            Err("no proxy is running".to_string()),
        ] {
            let fake = Fake {
                answers: RefCell::new(vec![true]),
                serving: Some(serving),
                ..Fake::default()
            };
            let reached = reach(&fake, launch(), |_| {}).await.unwrap();
            assert!(matches!(reached, Reached::Other { .. }), "{reached:?}");
            assert_eq!(reached.adopted(), None);
            assert!(fake.started.borrow().is_empty());
        }
    }

    #[tokio::test]
    async fn a_stopped_adopted_model_is_started_through_its_saved_launch() {
        let fake = Fake {
            answers: RefCell::new(vec![false, true]),
            serving: Some(Ok("qwen3-5-9b-gguf".to_string())),
            start: Some(Ok(())),
            ..Fake::default()
        };
        let mut said = Vec::new();
        let reached = reach(&fake, launch(), |line| said.push(line.to_string()))
            .await
            .unwrap();
        assert_eq!(
            reached,
            Reached::Started {
                model: "qwen3-5-9b-gguf".to_string(),
                port: 11_435,
                log: PathBuf::from("logs/localbox-start-11435.log"),
            }
        );
        assert_eq!(
            *fake.started.borrow(),
            vec![(11_435, "qwen3-5-9b-gguf".to_string())]
        );
        assert!(
            said[0].contains("Starting qwen3-5-9b-gguf through LocalBox"),
            "{said:?}"
        );
        assert!(said[0].contains("localbox-start-11435.log"), "{said:?}");
    }

    #[tokio::test]
    async fn a_failed_start_names_the_stage_log_and_commands() {
        let fake = Fake {
            answers: RefCell::new(vec![false]),
            start: Some(Err(
                "LocalBox has no saved launch for port 11435".to_string()
            )),
            ..Fake::default()
        };
        let guidance = reach(&fake, launch(), |_| {}).await.unwrap_err();
        assert_eq!(guidance.stage, Stage::Start);
        let text = guidance.to_string();
        for expected in [
            "stage: start",
            "no saved launch for port 11435",
            "localbox-start-11435.log",
            "localbox serve --again --proxy-port 11435 --expect-model qwen3-5-9b-gguf",
            "localbox status",
            "localx setup",
        ] {
            assert!(text.contains(expected), "{expected} missing from:\n{text}");
        }
    }

    #[tokio::test(start_paused = true)]
    async fn an_answer_from_another_model_after_the_start_is_not_ready() {
        let fake = Fake {
            answers: RefCell::new(vec![false, true]),
            serving: Some(Ok("other-model".to_string())),
            start: Some(Ok(())),
            ..Fake::default()
        };
        let guidance = reach(&fake, launch(), |_| {}).await.unwrap_err();
        assert_eq!(guidance.stage, Stage::Readiness);
        assert!(guidance.why.contains("other-model"), "{}", guidance.why);
    }

    #[cfg(feature = "tui")]
    #[test]
    fn only_the_first_chat_turn_on_the_reached_provider_gets_first_reply_guidance() {
        let serving = Reached::Serving {
            model: "m".to_string(),
            port: 11_435,
        };
        // A failed first turn shows it; a later failure does not.
        let chat = FirstChatTurn::new(&serving, "local");
        assert!(chat
            .turn_ended("local", true)
            .unwrap()
            .contains("stage: first reply"));
        assert_eq!(chat.turn_ended("local", true), None);
        // A first turn that worked uses it up too.
        let chat = FirstChatTurn::new(&serving, "local");
        assert_eq!(chat.turn_ended("local", false), None);
        assert_eq!(chat.turn_ended("local", true), None);
        // Switching provider before the first turn: that turn's failure is
        // not the LocalBox model's, and it uses the guidance up.
        let chat = FirstChatTurn::new(&serving, "local");
        assert_eq!(chat.turn_ended("hosted", true), None);
        assert_eq!(chat.turn_ended("local", true), None);
        // A model this chat did not reach arms nothing.
        let chat = FirstChatTurn::new(&Reached::NotManaged, "local");
        assert_eq!(chat.turn_ended("local", true), None);
        // Two chats do not share it.
        let one = FirstChatTurn::new(&serving, "local");
        let two = FirstChatTurn::new(&serving, "local");
        assert!(one.turn_ended("local", true).is_some());
        assert!(two.turn_ended("local", true).is_some());
    }

    #[tokio::test(start_paused = true)]
    async fn a_started_model_that_never_answers_stops_at_readiness() {
        let fake = Fake {
            answers: RefCell::new(vec![false]),
            start: Some(Ok(())),
            ..Fake::default()
        };
        let guidance = reach(&fake, launch(), |_| {}).await.unwrap_err();
        assert_eq!(guidance.stage, Stage::Readiness);
        assert!(guidance.to_string().contains("stage: readiness"));
    }

    #[test]
    fn first_reply_guidance_is_only_for_a_reached_adopted_model() {
        let serving = Reached::Serving {
            model: "m".to_string(),
            port: 11_435,
        };
        let guidance = first_reply_guidance(&serving, "boom").unwrap();
        assert_eq!(guidance.stage, Stage::FirstReply);
        assert!(guidance.to_string().contains("stage: first reply"));
        assert!(first_reply_guidance(&Reached::NotManaged, "boom").is_none());
        assert!(first_reply_guidance(
            &Reached::Other {
                note: String::new()
            },
            "boom"
        )
        .is_none());
    }

    #[test]
    fn the_saved_launch_is_asked_for_by_port_and_model() {
        assert_eq!(
            again_args(11_435, "m"),
            [
                "serve",
                "--again",
                "--proxy-port",
                "11435",
                "--expect-model",
                "m",
                "--json"
            ]
        );
    }
}
