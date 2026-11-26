//! `localx setup`: the guided first run. It takes a fresh install to a
//! working local model, one step at a time — the tools, the provider choice,
//! the engine, a starter model sized for this computer, a verified server,
//! LocalPilot pointed at it, a first reply — and then opens chat.
//!
//! Every step looks before it acts, so running setup again picks up where it
//! stopped. Every failure names its step, the log to read and the command to
//! run again. The steps talk to the stack tools only through [`Host`], so the
//! order and the decisions are tested without starting anything.

use std::io::Write;
use std::path::PathBuf;

/// What the user asked of setup.
#[derive(Debug, Clone, Default)]
pub(crate) struct Options {
    /// Set up a local model even when another provider is configured.
    pub local: bool,
    /// Keep the provider already configured; skip the local model.
    pub existing: bool,
    /// Proceed (download, start) without asking.
    pub yes: bool,
    /// Allow replacing a different provider in the user config.
    pub replace_provider: bool,
    /// Stop after the first reply instead of opening chat.
    pub no_chat: bool,
}

impl Options {
    /// The command that runs setup again the same way.
    fn retry_command(&self) -> String {
        let mut command = String::from("localx setup");
        for (on, flag) in [
            (self.local, " --local"),
            (self.existing, " --existing"),
            (self.yes, " --yes"),
            (self.replace_provider, " --replace-provider"),
            (self.no_chat, " --no-chat"),
        ] {
            if on {
                command.push_str(flag);
            }
        }
        command
    }
}

/// The steps, in order.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Stage {
    Tools,
    Provider,
    Engine,
    Starter,
    Server,
    Adopt,
    Reply,
    Chat,
}

impl Stage {
    const COUNT: u8 = 8;

    fn number(self) -> u8 {
        match self {
            Self::Tools => 1,
            Self::Provider => 2,
            Self::Engine => 3,
            Self::Starter => 4,
            Self::Server => 5,
            Self::Adopt => 6,
            Self::Reply => 7,
            Self::Chat => 8,
        }
    }

    fn name(self) -> &'static str {
        match self {
            Self::Tools => "tools",
            Self::Provider => "provider",
            Self::Engine => "engine",
            Self::Starter => "starter model",
            Self::Server => "model server",
            Self::Adopt => "LocalPilot setup",
            Self::Reply => "first reply",
            Self::Chat => "chat",
        }
    }
}

/// Why setup stopped.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) struct Failure {
    pub stage: Stage,
    pub message: String,
    pub log: Option<PathBuf>,
}

impl Failure {
    fn new(stage: Stage, message: impl Into<String>) -> Self {
        Self {
            stage,
            message: message.into(),
            log: None,
        }
    }

    fn with_log(mut self, log: Option<PathBuf>) -> Self {
        self.log = log;
        self
    }

    /// The report: the step, what went wrong, where to look, how to retry.
    pub(crate) fn report(&self, options: &Options) -> String {
        let mut text = format!(
            "setup stopped at step {} of {} ({}): {}\n",
            self.stage.number(),
            Stage::COUNT,
            self.stage.name(),
            self.message
        );
        if let Some(log) = &self.log {
            text.push_str(&format!("  log:   {}\n", log.display()));
        }
        text.push_str(&format!("  retry: {}\n", options.retry_command()));
        text
    }
}

/// What a step's child process left behind.
#[derive(Debug, Clone, Default)]
pub(crate) struct Output {
    pub success: bool,
    /// Its standard output, read from the step's log file after it exited.
    pub stdout: String,
    /// Its standard error, when it was kept in a log rather than shown.
    pub stderr: String,
    pub log: Option<PathBuf>,
}

/// Everything setup does to the computer.
pub(crate) trait Host {
    /// Whether the stack tool is installed.
    fn has_tool(&self, tool: &str) -> bool;
    /// Run a stack tool to completion with its output captured in a log file.
    /// `visible` shows its progress (standard error) as it runs.
    ///
    /// # Errors
    /// The tool could not be started.
    fn run(
        &mut self,
        stage: Stage,
        tool: &str,
        args: &[String],
        visible: bool,
    ) -> Result<Output, String>;
    /// Run a stack tool on this terminal (chat).
    ///
    /// # Errors
    /// The tool could not be started.
    fn interactive(&mut self, tool: &str, args: &[String]) -> Result<bool, String>;
    /// Whether a person is at the terminal to answer.
    fn is_tty(&self) -> bool;
    /// Ask a yes/no question.
    ///
    /// # Errors
    /// The answer could not be read.
    fn confirm(&mut self, question: &str) -> Result<bool, String>;
    /// The LocalPilot user config's text, when there is one.
    ///
    /// # Errors
    /// It exists but cannot be read.
    fn user_config(&self) -> Result<Option<String>, String>;
    /// Run `localpilot ask` bound to the user config alone: from a folder
    /// with no project config and without `LOCALPILOT_PROVIDER*` environment
    /// overrides, so the reply comes from the provider setup just checked.
    ///
    /// # Errors
    /// LocalPilot could not be started.
    fn ask(&mut self, args: &[String]) -> Result<Output, String>;
    /// What would override the user config's provider settings for
    /// LocalPilot started here: environment variables and a project config.
    fn overrides(&self) -> Vec<String>;
}

const READY_PROMPT: &str = "Reply with the single word ready.";

/// Run the guided first run.
///
/// # Errors
/// The [`Failure`] that stopped it.
pub(crate) fn run(
    options: &Options,
    host: &mut dyn Host,
    out: &mut dyn Write,
) -> Result<(), Failure> {
    let say = |out: &mut dyn Write, stage: Stage, text: &str| {
        let _ = writeln!(out, "[{}/{}] {}", stage.number(), Stage::COUNT, text);
    };

    // 1. The tools this needs.
    for tool in ["localpilot", "localbox"] {
        if !host.has_tool(tool) {
            return Err(Failure::new(
                Stage::Tools,
                format!("{tool} is not installed; run `localx install` first"),
            ));
        }
    }
    say(out, Stage::Tools, "LocalPilot and LocalBox are installed.");

    // 2. Keep another provider, or set up a local model.
    let config = host
        .user_config()
        .map_err(|error| Failure::new(Stage::Provider, error))?;
    let current = config
        .as_deref()
        .and_then(default_provider)
        .filter(|name| name != "local");
    let (use_local, replace) = match (&current, options.local, options.existing) {
        (_, true, true) => {
            return Err(Failure::new(
                Stage::Provider,
                "choose --local or --existing, not both",
            ))
        }
        (None, _, true) => {
            return Err(Failure::new(
                Stage::Provider,
                "no other provider is configured to keep; run setup without --existing",
            ))
        }
        (Some(_), false, true) => (false, false),
        // A local default already: replacing its settings still takes
        // explicit consent.
        (None, _, false) => (true, options.replace_provider),
        (Some(name), true, false) => {
            if options.replace_provider {
                (true, true)
            } else if host.is_tty() {
                let yes = host
                    .confirm(&format!("Your default provider is {name}. Make the local model your default instead?"))
                    .map_err(|error| Failure::new(Stage::Provider, error))?;
                (yes, yes)
            } else {
                return Err(Failure::new(
                    Stage::Provider,
                    format!("your default provider is {name}; add --replace-provider to make the local model the default instead"),
                ));
            }
        }
        (Some(name), false, false) => {
            if host.is_tty() {
                let yes = host
                    .confirm(&format!("Your default provider is {name}. Set up a local model and make it your default instead?"))
                    .map_err(|error| Failure::new(Stage::Provider, error))?;
                (yes, yes)
            } else {
                return Err(Failure::new(
                    Stage::Provider,
                    format!("your default provider is {name}; run again with --existing to keep it, or --local --replace-provider to use a local model"),
                ));
            }
        }
    };
    // The provider and model the first reply is asked of, named explicitly so
    // a project config in this folder cannot send it somewhere else.
    let target = if use_local {
        say(out, Stage::Provider, "Setting up a local model.");
        Some((
            "local".to_string(),
            local_model(options, replace, host, out)?,
        ))
    } else {
        let name = current.clone().unwrap_or_default();
        say(
            out,
            Stage::Provider,
            &format!("Keeping {name}; skipping the local model (steps 3 to 6)."),
        );
        config
            .as_deref()
            .and_then(|text| provider_model(text, &name))
            .map(|model| (name, model))
    };

    // 7. A first reply through LocalPilot.
    let Some((provider, model)) = target else {
        let name = current.unwrap_or_default();
        return Err(Failure::new(
            Stage::Reply,
            format!(
                "your default provider {name} names no model in the user config; set \
                 `model` under [providers.{name}], then run `localx setup --existing`"
            ),
        ));
    };
    let ask = vec![
        "ask".to_string(),
        "--provider".to_string(),
        provider,
        "--model".to_string(),
        model,
        READY_PROMPT.to_string(),
    ];
    let reply = host
        .ask(&ask)
        .map_err(|error| Failure::new(Stage::Reply, error))?;
    let text = reply.stdout.trim();
    if !reply.success || text.is_empty() {
        return Err(Failure::new(
            Stage::Reply,
            "LocalPilot did not get a reply from the model",
        )
        .with_log(reply.log));
    }
    say(
        out,
        Stage::Reply,
        &format!("LocalPilot replied: {}", first_line(text)),
    );
    let overrides = host.overrides();
    for item in &overrides {
        let _ = writeln!(
            out,
            "      note: {item} overrides the provider settings for LocalPilot started here"
        );
    }

    // 8. Chat.
    if options.no_chat || !host.is_tty() {
        say(
            out,
            Stage::Chat,
            "Setup is done. Start chatting with `localpilot`.",
        );
        return Ok(());
    }
    // Chat started here would read those overrides and could talk to another
    // server than the one just checked, so it is not opened for the user.
    if !overrides.is_empty() {
        say(
            out,
            Stage::Chat,
            "Setup is done. Chat is not opened here, because the settings named above \
             could send it to another provider than the one just checked. Remove them, or \
             run `localpilot` in another folder.",
        );
        return Ok(());
    }
    say(
        out,
        Stage::Chat,
        "Setup is done. Opening chat (exit it any time; run `localpilot` to come back).",
    );
    let clean = host
        .interactive("localpilot", &[])
        .map_err(|error| Failure::new(Stage::Chat, error))?;
    if !clean {
        return Err(Failure::new(Stage::Chat, "chat exited with an error"));
    }
    Ok(())
}

/// Steps 3 to 6: the engine, the starter model, a verified server, LocalPilot.
/// Returns the model name LocalPilot now requests from the local provider.
fn local_model(
    options: &Options,
    replace: bool,
    host: &mut dyn Host,
    out: &mut dyn Write,
) -> Result<String, Failure> {
    let say = |out: &mut dyn Write, stage: Stage, text: &str| {
        let _ = writeln!(out, "[{}/{}] {}", stage.number(), Stage::COUNT, text);
    };

    // 3. The engine: `localbox update` installs it or confirms it is current.
    // Its output goes to the step's log, where the reason for a failure can
    // be read back.
    say(
        out,
        Stage::Engine,
        "Checking the model engine (llama.cpp); the first install downloads it and can take a few minutes...",
    );
    let engine = host
        .run(Stage::Engine, "localbox", &["update".to_string()], false)
        .map_err(|error| Failure::new(Stage::Engine, error))?;
    if !engine.success {
        let diagnostic = last_line(&engine.stderr)
            .or_else(|| last_line(&engine.stdout))
            .unwrap_or("the engine could not be installed")
            .to_string();
        let failed = |why: String| Failure::new(Stage::Engine, why).with_log(engine.log.clone());
        // Only the files of a running engine being in use is a reason to go
        // on, and only when LocalBox confirms a model is being served by it.
        if !engine_files_in_use(&engine) {
            return Err(failed(diagnostic));
        }
        match serving_model(host, None) {
            Ok((Some(model), _)) => {
                let _ = writeln!(
                    out,
                    "      note: the engine could not be updated while {model} is running \
                     ({diagnostic}); using the installed one"
                );
            }
            Ok((None, _)) | Err(_) => return Err(failed(diagnostic)),
        }
    }

    // 4. The model: the one an earlier setup recorded when LocalBox still
    // serves it (a rerun keeps what works rather than sizing again with that
    // model's own memory in use), else the starter sized for this computer.
    let recorded = host
        .user_config()
        .map_err(|error| Failure::new(Stage::Starter, error))?
        .as_deref()
        .and_then(recorded_model);
    let (key, serve_args) = match (recorded, serving_model(host, None)?) {
        (Some(recorded), (Some(serving), Some(_))) if recorded == serving => {
            say(
                out,
                Stage::Starter,
                &format!("{recorded} is already set up and running; keeping it."),
            );
            (recorded, Vec::new())
        }
        _ => offer_starter(options, host, out)?,
    };

    // 5. A verified server on LocalBox's own proxy port, the one a restart
    // uses: reuse one already serving this model, or probe the model (CUDA,
    // then Vulkan, then CPU) and then serve it with the same settings.
    let endpoint = match serving_model(host, None)? {
        (Some(model), Some(port)) if model == key => {
            say(
                out,
                Stage::Server,
                &format!("{key} is already running on port {port}; using that instance as it was started."),
            );
            format!("http://127.0.0.1:{port}/v1")
        }
        _ => {
            say(
                out,
                Stage::Server,
                &format!("Checking which build runs {key}..."),
            );
            let probe = host
                .run(
                    Stage::Server,
                    "localbox",
                    &launch_args("probe", &serve_args, &["--fallback"]),
                    true,
                )
                .map_err(|error| Failure::new(Stage::Server, error))?;
            let report = parse_json(&probe.stdout).unwrap_or_default();
            if !probe.success || report["ok"] != true {
                let why = report["error"]
                    .as_str()
                    .unwrap_or("the model did not start");
                let log = report["log"].as_str().map(PathBuf::from).or(probe.log);
                let stage = report["stage"].as_str().unwrap_or("start");
                return Err(
                    Failure::new(Stage::Server, format!("{why} (at {stage})")).with_log(log)
                );
            }
            for fallback in report["fallbacks"].as_array().into_iter().flatten() {
                let _ = writeln!(
                    out,
                    "      fell back from {}: {}",
                    fallback["variant"].as_str().unwrap_or("?"),
                    fallback["reason"].as_str().unwrap_or("?")
                );
            }
            let probed = report["backend"]["variant"]
                .as_str()
                .unwrap_or("?")
                .to_string();
            say(
                out,
                Stage::Server,
                &format!("{key} answers on the {probed} build. Starting it (this can take a few minutes)..."),
            );
            let served = host
                .run(
                    Stage::Server,
                    "localbox",
                    &launch_args("serve", &serve_args, &[]),
                    // Hidden: the server it leaves running must not hold this
                    // process's standard error open.
                    false,
                )
                .map_err(|error| Failure::new(Stage::Server, error))?;
            let receipt = parse_json(&served.stdout).unwrap_or_default();
            if !served.success || receipt["ok"] != true {
                let why = receipt["error"]
                    .as_str()
                    .unwrap_or("the model server did not start");
                return Err(Failure::new(Stage::Server, why).with_log(served.log));
            }
            if receipt["model"].as_str() != Some(key.as_str()) {
                return Err(Failure::new(
                    Stage::Server,
                    format!(
                        "LocalBox started {}, not {key}",
                        receipt["model"].as_str().unwrap_or("an unnamed model")
                    ),
                )
                .with_log(served.log));
            }
            // The build the server runs must be the one the probe verified,
            // tag and variant both. An install with no build record cannot
            // claim either, and says so.
            match (stamp(&report["backend"]), stamp(&receipt["backend"])) {
                (None, _) => {
                    let _ = writeln!(
                        out,
                        "      build: not recorded by this install, so not compared"
                    );
                }
                (Some(verified), Some(running)) if verified == running => {}
                (Some((tag, variant)), Some((running_tag, running_variant))) => {
                    return Err(Failure::new(
                        Stage::Server,
                        format!(
                            "the server runs the {running_variant} build {running_tag}, not the \
                             verified {variant} build {tag}"
                        ),
                    )
                    .with_log(served.log));
                }
                (Some((tag, variant)), None) => {
                    return Err(Failure::new(
                        Stage::Server,
                        format!(
                            "LocalBox did not report which build the server runs, so it cannot \
                             be checked against the verified {variant} build {tag}"
                        ),
                    )
                    .with_log(served.log));
                }
            }
            let profile = &receipt["profile"];
            let tune = match profile["source"].as_str() {
                Some("tuned") => {
                    format!("saved {} tune", profile["profile"].as_str().unwrap_or("?"))
                }
                _ => format!(
                    "default settings ({})",
                    profile["why"].as_str().unwrap_or("no saved tune")
                ),
            };
            let _ = writeln!(out, "      settings: {tune}");
            if let Some(argv) = receipt["launchArguments"].as_array() {
                let argv: Vec<&str> = argv.iter().filter_map(serde_json::Value::as_str).collect();
                let _ = writeln!(out, "      started with: {}", argv.join(" "));
            }
            receipt["endpoint"].as_str().map(api_base).ok_or_else(|| {
                Failure::new(
                    Stage::Server,
                    "LocalBox did not say where the model is served",
                )
                .with_log(served.log.clone())
            })?
        }
    };
    let port = endpoint_port(&endpoint).ok_or_else(|| {
        Failure::new(
            Stage::Server,
            format!("LocalBox reported an unusable address: {endpoint}"),
        )
    })?;
    match serving_model(host, Some(port))? {
        (Some(model), _) if model == key => {}
        (Some(model), _) => {
            return Err(Failure::new(
                Stage::Server,
                format!("LocalBox at {endpoint} serves {model}, not {key}"),
            ))
        }
        (None, _) => {
            return Err(Failure::new(
                Stage::Server,
                format!("LocalBox could not confirm that {endpoint} serves {key}"),
            ))
        }
    }

    // 6. LocalPilot uses it by default, with the key recorded for later.
    let mut adopt = vec![
        "localbox".to_string(),
        "adopt".to_string(),
        "--user".to_string(),
        "--endpoint".to_string(),
        endpoint.clone(),
        "--yes".to_string(),
    ];
    if replace {
        adopt.push("--replace-existing".to_string());
    }
    let adopted = host
        .run(Stage::Adopt, "localpilot", &adopt, false)
        .map_err(|error| Failure::new(Stage::Adopt, error))?;
    if !adopted.success {
        let why =
            last_line(&adopted.stderr).unwrap_or("LocalPilot could not be pointed at the model");
        let message = if replace {
            why.to_string()
        } else {
            format!("{why} (to replace settings it lists, run again with --replace-provider)")
        };
        return Err(Failure::new(Stage::Adopt, message).with_log(adopted.log));
    }
    // Read back what was written: the default, the address and the key must
    // all name this server, or a later restart would bring back another one.
    let written = host
        .user_config()
        .map_err(|error| Failure::new(Stage::Adopt, error))?
        .unwrap_or_default();
    let mismatch = |what: String| Failure::new(Stage::Adopt, what).with_log(adopted.log.clone());
    let recorded = recorded_model(&written);
    if recorded.as_deref() != Some(key.as_str()) {
        return Err(mismatch(format!(
            "the user config records {} as the model, not {key}",
            recorded.as_deref().unwrap_or("nothing")
        )));
    }
    if default_provider(&written).as_deref() != Some("local") {
        return Err(mismatch(
            "the user config does not make the local model the default".to_string(),
        ));
    }
    let address = provider_value(&written, "local", "base_url");
    if address.as_deref() != Some(endpoint.as_str()) {
        return Err(mismatch(format!(
            "the user config points the local provider at {}, not {endpoint}",
            address.as_deref().unwrap_or("nothing")
        )));
    }
    let model = provider_model(&written, "local").ok_or_else(|| {
        mismatch("the user config names no model for the local provider".to_string())
    })?;
    say(
        out,
        Stage::Adopt,
        &format!("LocalPilot now uses {key} at {endpoint} by default."),
    );
    Ok(model)
}

/// Offer the starter LocalBox sizes for this computer and download it after a
/// yes. Returns its catalog key and the start command LocalBox gave for it.
fn offer_starter(
    options: &Options,
    host: &mut dyn Host,
    out: &mut dyn Write,
) -> Result<(String, Vec<String>), Failure> {
    let say = |out: &mut dyn Write, stage: Stage, text: &str| {
        let _ = writeln!(out, "[{}/{}] {}", stage.number(), Stage::COUNT, text);
    };
    let starter_args = vec!["starter".to_string(), "--json".to_string()];
    let offer = host
        .run(Stage::Starter, "localbox", &starter_args, false)
        .map_err(|error| Failure::new(Stage::Starter, error))?;
    let offer_json = parse_json(&offer.stdout).ok_or_else(|| {
        Failure::new(Stage::Starter, "LocalBox did not describe a starter model")
            .with_log(offer.log.clone())
    })?;
    if !offer.success || offer_json["status"] == "error" {
        let why = offer_json["error"]
            .as_str()
            .unwrap_or("no starter model fits this computer");
        return Err(Failure::new(Stage::Starter, why).with_log(offer.log));
    }
    let size = offer_json["size_gb"].as_f64().map_or_else(
        || "size unknown".to_string(),
        |gb| format!("{gb:.1} GB download"),
    );
    say(
        out,
        Stage::Starter,
        &format!(
            "Starter model: {} {} ({size}), runs on {} at {} context.",
            offer_json["model"].as_str().unwrap_or("?"),
            offer_json["quant"].as_str().unwrap_or("?"),
            offer_json["mode"].as_str().unwrap_or("?").to_uppercase(),
            offer_json["context"].as_str().unwrap_or("?"),
        ),
    );
    if let Some(reason) = offer_json["reason"].as_str() {
        let _ = writeln!(out, "      {reason}");
    }
    if !options.yes {
        if !host.is_tty() {
            return Err(Failure::new(
                Stage::Starter,
                "add --yes to download it without asking",
            ));
        }
        let yes = host
            .confirm("Download it now? (files already downloaded are checked, not fetched again)")
            .map_err(|error| Failure::new(Stage::Starter, error))?;
        if !yes {
            return Err(Failure::new(Stage::Starter, "the download was declined"));
        }
    }
    let mut download_args = starter_args.clone();
    download_args.push("--download".to_string());
    // Fetch exactly what was offered: LocalBox stops before downloading when
    // its recommendation has changed since (memory freed or taken meanwhile).
    // An older LocalBox names no offer and cannot make that check, so
    // nothing is downloaded through it.
    let Some(offer_id) = offer_json["offer"].as_str().filter(|id| !id.is_empty()) else {
        return Err(Failure::new(
            Stage::Starter,
            "this LocalBox cannot confirm the offer before downloading; update it with \
             `localx install localbox`, then run setup again",
        )
        .with_log(offer.log));
    };
    download_args.push("--expect".to_string());
    download_args.push(offer_id.to_string());
    let fetched = host
        .run(Stage::Starter, "localbox", &download_args, true)
        .map_err(|error| Failure::new(Stage::Starter, error))?;
    let fetched_json = parse_json(&fetched.stdout).unwrap_or_default();
    if !fetched.success || fetched_json["status"] == "error" {
        let why = fetched_json["error"]
            .as_str()
            .unwrap_or("the download did not complete");
        return Err(Failure::new(Stage::Starter, why).with_log(fetched.log));
    }
    let key = fetched_json["catalog_key"]
        .as_str()
        .map(str::to_string)
        .ok_or_else(|| {
            Failure::new(Stage::Starter, "LocalBox did not name the installed model")
                .with_log(fetched.log.clone())
        })?;
    let serve_args: Vec<String> = match fetched_json["serve_args"].as_array() {
        Some(args) => args
            .iter()
            .filter_map(|arg| arg.as_str().map(str::to_string))
            .collect(),
        None => {
            let why = fetched_json["limitation"]
                .as_str()
                .unwrap_or("LocalBox gave no start command for it");
            return Err(Failure::new(Stage::Starter, why).with_log(fetched.log));
        }
    };
    say(
        out,
        Stage::Starter,
        &format!("{key} is downloaded and checked."),
    );
    Ok((key, serve_args))
}

/// The model LocalBox says the proxy serves (on `port`, or its configured
/// port), and that port.
fn serving_model(
    host: &mut dyn Host,
    port: Option<u16>,
) -> Result<(Option<String>, Option<u16>), Failure> {
    let mut args = vec!["models".to_string(), "--json".to_string()];
    if let Some(port) = port {
        args.push("--proxy-port".to_string());
        args.push(port.to_string());
    }
    let listing = host
        .run(Stage::Server, "localbox", &args, false)
        .map_err(|error| Failure::new(Stage::Server, error))?;
    let json = parse_json(&listing.stdout).unwrap_or_default();
    let serving = &json["serving"];
    Ok((
        serving["model"].as_str().map(str::to_string),
        serving["proxyPort"]
            .as_u64()
            .and_then(|port| u16::try_from(port).ok()),
    ))
}

/// A probe or serve for a starter's start command: same model, quant, context
/// and placement, with a saved balanced tune when one fits (LocalBox defaults
/// when none does), and JSON on standard output.
fn launch_args(verb: &str, serve_args: &[String], extra: &[&str]) -> Vec<String> {
    let mut args = vec![verb.to_string()];
    args.extend(
        serve_args
            .iter()
            .skip_while(|arg| *arg == "serve")
            .filter(|arg| *arg != "--no-auto-best")
            .cloned(),
    );
    args.extend(
        ["--prefer-profile", "balanced", "--allow-untuned"]
            .iter()
            .chain(extra)
            .chain(&["--json"])
            .map(ToString::to_string),
    );
    args
}

/// An OpenAI-compatible base URL: the address with `/v1` on the end.
fn api_base(endpoint: &str) -> String {
    let trimmed = endpoint.trim_end_matches('/');
    if trimmed.ends_with("/v1") {
        trimmed.to_string()
    } else {
        format!("{trimmed}/v1")
    }
}

fn parse_json(text: &str) -> Option<serde_json::Value> {
    // The whole output when it is one object (it may be pretty-printed),
    // else the last line holding one: tools may print notes before it.
    serde_json::from_str::<serde_json::Value>(text.trim())
        .ok()
        .filter(serde_json::Value::is_object)
        .or_else(|| {
            text.lines()
                .rev()
                .filter(|line| line.starts_with('{'))
                .find_map(|line| serde_json::from_str(line).ok())
        })
}

/// A recorded engine build as (tag, variant); `None` when it is missing or
/// LocalBox marked it unrecorded.
fn stamp(value: &serde_json::Value) -> Option<(String, String)> {
    let tag = value["tag"].as_str()?;
    let variant = value["variant"].as_str()?;
    (tag != "unrecorded" && variant != "unrecorded" && !tag.is_empty() && !variant.is_empty())
        .then(|| (tag.to_string(), variant.to_string()))
}

/// The settings that would override the user config's provider for
/// LocalPilot started in a folder: `LOCALPILOT_PROVIDER*` environment
/// variables and a project config that sets a provider.
pub(crate) fn provider_overrides(
    variables: impl IntoIterator<Item = String>,
    project: Option<(PathBuf, String)>,
) -> Vec<String> {
    let mut found: Vec<String> = variables
        .into_iter()
        .filter(|name| name.to_ascii_uppercase().starts_with(OVERRIDE_PREFIX))
        .map(|name| format!("the environment variable {name}"))
        .collect();
    found.sort();
    if let Some((path, text)) = project {
        let sets_provider = text
            .parse::<toml::Table>()
            .is_ok_and(|doc| doc.contains_key("provider") || doc.contains_key("providers"));
        if sets_provider {
            found.push(path.display().to_string());
        }
    }
    found
}

/// Environment variables LocalPilot reads over the provider settings.
const OVERRIDE_PREFIX: &str = "LOCALPILOT_PROVIDER";

fn endpoint_port(endpoint: &str) -> Option<u16> {
    let rest = endpoint.strip_prefix("http://")?;
    let authority = rest.split('/').next()?;
    authority.rsplit_once(':')?.1.parse().ok()
}

fn default_provider(config: &str) -> Option<String> {
    let doc: toml::Table = config.parse().ok()?;
    doc.get("provider")?
        .get("default")?
        .as_str()
        .map(str::to_string)
}

fn provider_value(config: &str, provider: &str, field: &str) -> Option<String> {
    let doc: toml::Table = config.parse().ok()?;
    doc.get("providers")?
        .get(provider)?
        .get(field)?
        .as_str()
        .map(str::to_string)
}

fn provider_model(config: &str, provider: &str) -> Option<String> {
    provider_value(config, provider, "model")
}

fn recorded_model(config: &str) -> Option<String> {
    let doc: toml::Table = config.parse().ok()?;
    doc.get("localbox")?
        .get("model")?
        .as_str()
        .map(str::to_string)
}

/// Whether an engine update failed only because the installed engine's
/// files are in use (a model is running from them).
fn engine_files_in_use(engine: &Output) -> bool {
    let text = format!("{}\n{}", engine.stdout, engine.stderr).to_ascii_lowercase();
    text.contains("os error 32")
        || text.contains("being used by another process")
        || text.contains("text file busy")
}

fn last_line(text: &str) -> Option<&str> {
    text.lines().map(str::trim).rfind(|line| !line.is_empty())
}

fn first_line(text: &str) -> &str {
    text.lines().next().unwrap_or(text)
}

/// A log folder of this run's own under `root`: `setup-<stamp>`, or the
/// first free `setup-<stamp>-<n>` when another run started in the same
/// second. Creating it is the claim, so two runs never share one.
///
/// # Errors
/// No folder could be created.
pub(crate) fn claim_log_dir(root: &std::path::Path, stamp: u64) -> Result<PathBuf, String> {
    std::fs::create_dir_all(root)
        .map_err(|error| format!("could not create {}: {error}", root.display()))?;
    for n in 1..=100u32 {
        let name = if n == 1 {
            format!("setup-{stamp}")
        } else {
            format!("setup-{stamp}-{n}")
        };
        let dir = root.join(name);
        match std::fs::create_dir(&dir) {
            Ok(()) => return Ok(dir),
            Err(error) if error.kind() == std::io::ErrorKind::AlreadyExists => {}
            Err(error) => return Err(format!("could not create {}: {error}", dir.display())),
        }
    }
    Err(format!(
        "{} holds too many runs from one second",
        root.display()
    ))
}

/// The real computer: stack tools by path, logs under one folder per run.
pub(crate) struct System {
    pub tool_path: fn(&str) -> PathBuf,
    pub log_dir: PathBuf,
    pub runs: usize,
}

impl System {
    /// Run one step's child with standard output in a file (never a pipe: a
    /// server the child leaves running would hold a pipe open) and standard
    /// error shown when `visible`, else in a log of its own so a diagnostic
    /// is never read as the step's output.
    fn spawn(
        &mut self,
        stage: Stage,
        tool: &str,
        args: &[String],
        visible: bool,
        mut command: std::process::Command,
    ) -> Result<Output, String> {
        std::fs::create_dir_all(&self.log_dir)
            .map_err(|error| format!("could not create {}: {error}", self.log_dir.display()))?;
        self.runs += 1;
        // Only the safe characters of the first argument name the file.
        let verb: String = args
            .first()
            .map_or("run", String::as_str)
            .chars()
            .filter(|c| c.is_ascii_alphanumeric() || *c == '-')
            .collect();
        let name = format!("{:02}-step{}-{tool}-{verb}", self.runs, stage.number());
        let stdout_path = self.log_dir.join(format!("{name}.out.txt"));
        let stderr_path = self.log_dir.join(format!("{name}.log"));
        let create = |path: &PathBuf| {
            std::fs::File::create(path)
                .map_err(|error| format!("could not create {}: {error}", path.display()))
        };
        let stdout_file = create(&stdout_path)?;
        let stderr = if visible {
            std::process::Stdio::inherit()
        } else {
            create(&stderr_path)?.into()
        };
        let status = command
            .args(args)
            .stdin(std::process::Stdio::null())
            .stdout(stdout_file)
            .stderr(stderr)
            .status()
            .map_err(|error| format!("could not run {tool}: {error}"))?;
        let stdout = std::fs::read_to_string(&stdout_path).unwrap_or_default();
        let stderr = if visible {
            String::new()
        } else {
            std::fs::read_to_string(&stderr_path).unwrap_or_default()
        };
        Ok(Output {
            success: status.success(),
            stdout,
            stderr,
            log: Some(if visible { stdout_path } else { stderr_path }),
        })
    }
}

impl Host for System {
    fn has_tool(&self, tool: &str) -> bool {
        let path = (self.tool_path)(tool);
        path.is_absolute() && path.exists()
            || std::process::Command::new(&path)
                .arg("--version")
                .stdout(std::process::Stdio::null())
                .stderr(std::process::Stdio::null())
                .status()
                .is_ok_and(|status| status.success())
    }

    fn run(
        &mut self,
        stage: Stage,
        tool: &str,
        args: &[String],
        visible: bool,
    ) -> Result<Output, String> {
        let command = std::process::Command::new((self.tool_path)(tool));
        self.spawn(stage, tool, args, visible, command)
    }

    fn ask(&mut self, args: &[String]) -> Result<Output, String> {
        std::fs::create_dir_all(&self.log_dir)
            .map_err(|error| format!("could not create {}: {error}", self.log_dir.display()))?;
        let mut command = std::process::Command::new((self.tool_path)("localpilot"));
        // The log folder has no project config; the provider overrides are
        // left out for this one request.
        command.current_dir(&self.log_dir);
        for (name, _) in std::env::vars_os() {
            if name
                .to_string_lossy()
                .to_ascii_uppercase()
                .starts_with(OVERRIDE_PREFIX)
            {
                command.env_remove(name);
            }
        }
        self.spawn(Stage::Reply, "localpilot", args, false, command)
    }

    fn overrides(&self) -> Vec<String> {
        let project = std::env::current_dir()
            .ok()
            .map(|dir| localpilot_config::project_config_path(&dir))
            .and_then(|path| std::fs::read_to_string(&path).ok().map(|text| (path, text)));
        provider_overrides(
            std::env::vars_os().map(|(name, _)| name.to_string_lossy().into_owned()),
            project,
        )
    }

    fn interactive(&mut self, tool: &str, args: &[String]) -> Result<bool, String> {
        std::process::Command::new((self.tool_path)(tool))
            .args(args)
            .status()
            .map(|status| status.success())
            .map_err(|error| format!("could not run {tool}: {error}"))
    }

    fn is_tty(&self) -> bool {
        use std::io::IsTerminal;
        std::io::stdin().is_terminal() && std::io::stdout().is_terminal()
    }

    fn confirm(&mut self, question: &str) -> Result<bool, String> {
        let mut out = std::io::stdout();
        let _ = write!(out, "{question} [y/N] ");
        let _ = out.flush();
        let mut answer = String::new();
        std::io::stdin()
            .read_line(&mut answer)
            .map_err(|error| format!("could not read the answer: {error}"))?;
        Ok(matches!(
            answer.trim().to_ascii_lowercase().as_str(),
            "y" | "yes"
        ))
    }

    fn user_config(&self) -> Result<Option<String>, String> {
        let Some(path) = localpilot_config::user_config_path() else {
            return Ok(None);
        };
        match std::fs::read_to_string(&path) {
            Ok(text) => Ok(Some(text)),
            Err(error) if error.kind() == std::io::ErrorKind::NotFound => Ok(None),
            Err(error) => Err(format!("could not read {}: {error}", path.display())),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    const KEY: &str = "qwen3-5-9b";

    /// A scripted computer: each call is answered by the first rule whose
    /// argument prefix matches, and every call is recorded.
    struct Fake {
        tty: bool,
        answers: Vec<bool>,
        config: Option<String>,
        config_after_adopt: Option<String>,
        rules: Vec<(String, Output)>,
        calls: Vec<String>,
        chat_opened: bool,
        chat_clean: bool,
        asked_bound: bool,
        overrides: Vec<String>,
    }

    fn ok(stdout: &str) -> Output {
        Output {
            success: true,
            stdout: stdout.to_string(),
            stderr: String::new(),
            log: Some(PathBuf::from("step.log")),
        }
    }

    fn failed(stdout: &str) -> Output {
        Output {
            success: false,
            ..ok(stdout)
        }
    }

    const STARTER: &str = r#"{"model":"Qwen3.5 9B","quant":"Q4_K_M","mode":"gpu","context":"64k","size_gb":5.3,"status":"recommended","reason":"fits","offer":"unsloth/Qwen3.5-9B-GGUF:Q4_K_M:gpu:64k"}"#;
    const DOWNLOADED: &str = r#"{"status":"recommended","catalog_key":"qwen3-5-9b","serve_args":["serve","qwen3-5-9b","--quant","Q4_K_M","--context","64k","--no-auto-best"]}"#;
    const PROBED: &str = r#"{"ok":true,"model":"qwen3-5-9b","backend":{"tag":"b1","variant":"vulkan"},"fallbacks":[{"variant":"cuda-13","reason":"the model did not answer"}],"serving":false,"endpoint":null}"#;
    const SERVE_RECEIPT: &str = "{\n  \"ok\": true,\n  \"model\": \"qwen3-5-9b\",\n  \"endpoint\": \"http://127.0.0.1:11435\",\n  \"proxyPort\": 11435,\n  \"backend\": {\"tag\": \"b1\", \"variant\": \"vulkan\"},\n  \"profile\": {\"source\": \"tuned\", \"profile\": \"balanced\"},\n  \"launchArguments\": [\"-m\", \"model.gguf\", \"-c\", \"65536\"]\n}\n";
    const NOTHING_SERVED: &str = r#"{"serving":{"reason":"no proxy is running"}}"#;
    const SERVED: &str = r#"{"serving":{"proxyPort":11435,"model":"qwen3-5-9b"}}"#;
    const ADOPTED: &str = "[provider]\ndefault = \"local\"\n[providers.local]\nbase_url = \"http://127.0.0.1:11435/v1\"\nmodel = \"Qwen3.5-9B-Q4_K_M.gguf\"\n[localbox]\nmodel = \"qwen3-5-9b\"\n";

    impl Fake {
        fn fresh() -> Self {
            Self {
                tty: false,
                answers: Vec::new(),
                config: None,
                config_after_adopt: Some(ADOPTED.to_string()),
                rules: vec![
                    ("localbox update".into(), ok("")),
                    ("localbox starter --json --download".into(), ok(DOWNLOADED)),
                    ("localbox starter --json".into(), ok(STARTER)),
                    ("localbox models --json --proxy-port".into(), ok(SERVED)),
                    ("localbox models --json".into(), ok(NOTHING_SERVED)),
                    ("localbox probe".into(), ok(PROBED)),
                    ("localbox serve".into(), ok(SERVE_RECEIPT)),
                    ("localpilot localbox adopt".into(), ok("")),
                    ("localpilot ask".into(), ok("ready\n")),
                ],
                calls: Vec::new(),
                chat_opened: false,
                chat_clean: true,
                asked_bound: false,
                overrides: Vec::new(),
            }
        }

        fn answer(mut self, prefix: &str, output: Output) -> Self {
            self.rules.insert(0, (prefix.to_string(), output));
            self
        }

        fn ran(&self, prefix: &str) -> bool {
            self.calls.iter().any(|call| call.starts_with(prefix))
        }
    }

    impl Host for Fake {
        fn has_tool(&self, _tool: &str) -> bool {
            true
        }
        fn run(
            &mut self,
            _stage: Stage,
            tool: &str,
            args: &[String],
            _visible: bool,
        ) -> Result<Output, String> {
            let call = format!("{tool} {}", args.join(" "));
            self.calls.push(call.clone());
            if call.starts_with("localpilot localbox adopt") {
                self.config = self.config_after_adopt.clone();
            }
            self.rules
                .iter()
                .find(|(prefix, _)| call.starts_with(prefix.as_str()))
                .map(|(_, output)| output.clone())
                .ok_or_else(|| format!("unexpected call: {call}"))
        }
        fn interactive(&mut self, _tool: &str, _args: &[String]) -> Result<bool, String> {
            self.chat_opened = true;
            Ok(self.chat_clean)
        }
        fn is_tty(&self) -> bool {
            self.tty
        }
        fn confirm(&mut self, _question: &str) -> Result<bool, String> {
            Ok(if self.answers.is_empty() {
                false
            } else {
                self.answers.remove(0)
            })
        }
        fn user_config(&self) -> Result<Option<String>, String> {
            Ok(self.config.clone())
        }
        fn ask(&mut self, args: &[String]) -> Result<Output, String> {
            self.asked_bound = true;
            self.run(Stage::Reply, "localpilot", args, false)
        }
        fn overrides(&self) -> Vec<String> {
            self.overrides.clone()
        }
    }

    fn yes() -> Options {
        Options {
            yes: true,
            ..Options::default()
        }
    }

    fn run_fake(options: &Options, fake: &mut Fake) -> (Result<(), Failure>, String) {
        let mut out = Vec::new();
        let result = run(options, fake, &mut out);
        (result, String::from_utf8(out).unwrap())
    }

    #[test]
    fn a_fresh_computer_ends_with_a_reply_after_falling_back_to_vulkan() {
        let mut fake = Fake::fresh();
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(
            out.contains("fell back from cuda-13: the model did not answer"),
            "{out}"
        );
        assert!(out.contains("answers on the vulkan build"), "{out}");
        assert!(out.contains("settings: saved balanced tune"), "{out}");
        assert!(
            out.contains("started with: -m model.gguf -c 65536"),
            "{out}"
        );
        assert!(out.contains("LocalPilot replied: ready"), "{out}");
        let probe = fake
            .calls
            .iter()
            .find(|call| call.starts_with("localbox probe"))
            .unwrap();
        assert_eq!(
            probe,
            "localbox probe qwen3-5-9b --quant Q4_K_M --context 64k --prefer-profile balanced \
             --allow-untuned --fallback --json"
        );
        assert!(fake.ran(
            "localbox serve qwen3-5-9b --quant Q4_K_M --context 64k --prefer-profile balanced \
             --allow-untuned --json"
        ));
        assert!(fake.ran("localbox models --json --proxy-port 11435"));
        assert!(fake.ran(
            "localbox starter --json --download --expect unsloth/Qwen3.5-9B-GGUF:Q4_K_M:gpu:64k"
        ));
        let adopt = fake
            .calls
            .iter()
            .find(|call| call.starts_with("localpilot localbox adopt"))
            .unwrap();
        assert_eq!(
            adopt,
            "localpilot localbox adopt --user --endpoint http://127.0.0.1:11435/v1 --yes"
        );
        assert!(
            fake.ran("localpilot ask --provider local --model Qwen3.5-9B-Q4_K_M.gguf Reply with")
        );
        assert!(!fake.chat_opened, "no terminal, so no chat");
    }

    #[test]
    fn a_model_that_does_not_fit_stops_before_localpilot_is_touched() {
        let mut fake = Fake::fresh().answer(
            "localbox probe",
            failed(r#"{"ok":false,"stage":"memory","error":"not enough GPU memory","log":"C:/logs/server.log"}"#),
        );
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Server);
        assert_eq!(failure.message, "not enough GPU memory (at memory)");
        assert_eq!(failure.log, Some(PathBuf::from("C:/logs/server.log")));
        assert!(!fake.ran("localpilot"), "{:?}", fake.calls);
        assert!(!fake.ran("localbox serve"), "{:?}", fake.calls);
        let report = failure.report(&yes());
        assert!(report.contains("step 5 of 8 (model server)"), "{report}");
        assert!(report.contains("log:   C:/logs/server.log"), "{report}");
        assert!(report.contains("retry: localx setup --yes"), "{report}");
    }

    #[test]
    fn a_proxy_serving_another_model_is_never_adopted() {
        let mut fake = Fake::fresh().answer(
            "localbox models --json --proxy-port",
            ok(r#"{"serving":{"proxyPort":11435,"model":"other-model"}}"#),
        );
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Server);
        assert!(
            failure
                .message
                .contains("serves other-model, not qwen3-5-9b"),
            "{}",
            failure.message
        );
        assert!(!fake.ran("localpilot"));
    }

    #[test]
    fn a_recorded_model_that_differs_from_the_server_fails_the_adopt_step() {
        let mut fake = Fake::fresh();
        fake.config_after_adopt = Some("[localbox]\nmodel = \"stale\"\n".into());
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Adopt);
        assert!(
            failure.message.contains("records stale"),
            "{}",
            failure.message
        );
        assert!(!fake.ran("localpilot ask"));
    }

    #[test]
    fn a_written_address_for_another_server_fails_the_adopt_step() {
        let mut fake = Fake::fresh();
        fake.config_after_adopt = Some(ADOPTED.replace("11435", "11436"));
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Adopt);
        assert!(
            failure.message.contains("not http://127.0.0.1:11435/v1"),
            "{}",
            failure.message
        );
        assert!(!fake.ran("localpilot ask"));
    }

    #[test]
    fn a_model_already_serving_is_reused_without_a_probe() {
        let mut fake = Fake::fresh().answer("localbox models --json", ok(SERVED));
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(
            out.contains(&format!("{KEY} is already running on port 11435")),
            "{out}"
        );
        assert!(!fake.ran("localbox probe"));
    }

    fn serve_failure(receipt: &str) -> Failure {
        let mut fake = Fake::fresh().answer("localbox serve", ok(receipt));
        let (result, _) = run_fake(&yes(), &mut fake);
        assert!(!fake.ran("localpilot"));
        result.unwrap_err()
    }

    #[test]
    fn a_server_on_another_build_than_the_verified_one_is_refused() {
        let failure = serve_failure(&SERVE_RECEIPT.replace("\"vulkan\"", "\"cuda-13\""));
        assert_eq!(failure.stage, Stage::Server);
        assert_eq!(
            failure.message,
            "the server runs the cuda-13 build b1, not the verified vulkan build b1"
        );
        // Same variant, another release of it.
        let failure = serve_failure(&SERVE_RECEIPT.replace("\"b1\"", "\"b2\""));
        assert_eq!(
            failure.message,
            "the server runs the vulkan build b2, not the verified vulkan build b1"
        );
        // No build named at all.
        let failure = serve_failure(&SERVE_RECEIPT.replace(
            "\"backend\": {\"tag\": \"b1\", \"variant\": \"vulkan\"},",
            "",
        ));
        assert!(
            failure
                .message
                .starts_with("LocalBox did not report which build"),
            "{}",
            failure.message
        );
    }

    #[test]
    fn an_install_without_a_build_record_says_so_instead_of_comparing() {
        let unrecorded = PROBED.replace(
            r#"{"tag":"b1","variant":"vulkan"}"#,
            r#"{"tag":"unrecorded","variant":"unrecorded"}"#,
        );
        let mut fake = Fake::fresh().answer("localbox probe", ok(&unrecorded));
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(
            out.contains("build: not recorded by this install, so not compared"),
            "{out}"
        );
    }

    #[test]
    fn stale_local_settings_are_replaced_only_with_consent() {
        let stale = "[provider]\ndefault = \"local\"\n[localbox]\nmodel = \"old\"\n";
        let with_consent =
            "localpilot localbox adopt --user --endpoint http://127.0.0.1:11435/v1 --yes --replace-existing";
        let mut fake = Fake::fresh()
            .answer(with_consent, ok(""))
            .answer("localpilot localbox adopt", failed(""));
        fake.rules.swap(0, 1);
        fake.config = Some(stale.into());
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Adopt);
        assert!(
            failure.message.contains("--replace-provider"),
            "{}",
            failure.message
        );

        let mut fake = Fake::fresh()
            .answer(with_consent, ok(""))
            .answer("localpilot localbox adopt", failed(""));
        fake.rules.swap(0, 1);
        fake.config = Some(stale.into());
        let consent = Options {
            replace_provider: true,
            ..yes()
        };
        let (result, out) = run_fake(&consent, &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(fake.ran(with_consent));
    }

    #[test]
    fn the_reply_is_asked_bound_to_the_user_config_and_overrides_are_named() {
        let mut fake = Fake::fresh();
        fake.overrides = vec!["the environment variable LOCALPILOT_PROVIDER__DEFAULT".into()];
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(fake.asked_bound);
        assert!(
            out.contains(
                "note: the environment variable LOCALPILOT_PROVIDER__DEFAULT overrides the \
                 provider settings for LocalPilot started here"
            ),
            "{out}"
        );
        let found = provider_overrides(
            [
                "PATH".to_string(),
                "LOCALPILOT_PROVIDERS__LOCAL__BASE_URL".to_string(),
            ],
            Some((
                PathBuf::from(".localpilot.toml"),
                "[providers.local]\nmodel = \"x\"\n".into(),
            )),
        );
        assert_eq!(
            found,
            vec![
                "the environment variable LOCALPILOT_PROVIDERS__LOCAL__BASE_URL".to_string(),
                ".localpilot.toml".to_string()
            ]
        );
        assert!(provider_overrides(
            Vec::new(),
            Some((PathBuf::from(".localpilot.toml"), "[harness]\n".into()))
        )
        .is_empty());
    }

    #[test]
    fn a_kept_provider_without_a_model_is_reported_not_skipped() {
        let mut fake = Fake::fresh();
        fake.config = Some("[provider]\ndefault = \"anthropic\"\n".into());
        let existing = Options {
            existing: true,
            ..Options::default()
        };
        let (result, out) = run_fake(&existing, &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Reply, "{out}");
        assert!(
            failure.message.contains("[providers.anthropic]"),
            "{}",
            failure.message
        );
        assert!(!out.contains("Setup is done"), "{out}");
    }

    #[test]
    fn an_engine_in_use_is_kept_and_a_missing_one_stops_setup() {
        let in_use = Output {
            success: false,
            stderr: "Error: could not move the existing install aside: The process cannot \
                     access the file because it is being used by another process. (os error 32)\n"
                .into(),
            ..ok("")
        };
        let mut fake = Fake::fresh()
            .answer("localbox update", in_use.clone())
            .answer("localbox models --json", ok(SERVED));
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(
            out.contains("the engine could not be updated while qwen3-5-9b is running"),
            "{out}"
        );

        // In use, but LocalBox serves nothing (or cannot say): the update's own
        // reason stops setup at the engine step.
        let mut fake = Fake::fresh().answer("localbox update", in_use.clone());
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Engine);
        assert!(
            failure.message.contains("os error 32"),
            "{}",
            failure.message
        );
        assert!(!fake.ran("localbox starter"));
        let mut fake = Fake::fresh()
            .answer("localbox update", in_use)
            .answer("localbox models --json", failed(""));
        let (result, _) = run_fake(&yes(), &mut fake);
        assert_eq!(result.unwrap_err().stage, Stage::Engine);

        // Any other failure stops, even with a model serving.
        let offline = Output {
            success: false,
            stderr: "Error: could not reach github.com\n".into(),
            ..ok("")
        };
        let mut fake = Fake::fresh()
            .answer("localbox update", offline)
            .answer("localbox models --json", ok(SERVED));
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Engine);
        assert_eq!(failure.message, "Error: could not reach github.com");
        assert!(!fake.ran("localbox models"));
    }

    #[test]
    fn a_failed_adopt_reports_what_localpilot_said() {
        let refused = Output {
            success: false,
            stderr: "note\nError: LocalBox is not installed\n".into(),
            ..ok("")
        };
        let mut fake = Fake::fresh().answer("localpilot localbox adopt", refused);
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Adopt);
        assert_eq!(
            failure.message,
            "Error: LocalBox is not installed (to replace settings it lists, run again with \
             --replace-provider)"
        );
    }

    #[test]
    fn a_rerun_keeps_the_recorded_model_while_localbox_serves_it() {
        let mut fake = Fake::fresh().answer("localbox models --json", ok(SERVED));
        fake.config = Some(ADOPTED.to_string());
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(
            out.contains("qwen3-5-9b is already set up and running; keeping it."),
            "{out}"
        );
        assert!(!fake.ran("localbox starter"), "{:?}", fake.calls);
        assert!(!fake.ran("localbox probe"));
        // Recorded but no longer served: the starter is offered again.
        let mut fake = Fake::fresh();
        fake.config = Some(ADOPTED.to_string());
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(fake.ran("localbox starter --json --download"));
    }

    #[test]
    fn an_offer_without_an_id_downloads_nothing() {
        let old = STARTER.replace(r#","offer":"unsloth/Qwen3.5-9B-GGUF:Q4_K_M:gpu:64k""#, "");
        assert!(!old.contains("offer"));
        let mut fake = Fake::fresh().answer("localbox starter --json", ok(&old));
        fake.rules.swap(0, 1);
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Starter);
        assert!(
            failure.message.contains("localx install localbox"),
            "{}",
            failure.message
        );
        assert!(!fake.ran("localbox starter --json --download"));
    }

    #[test]
    fn a_changed_offer_stops_before_anything_is_downloaded() {
        let changed = failed(
            r#"{"status":"error","stage":"offer","error":"the starter offer changed from a to b since it was shown"}"#,
        );
        let mut fake = Fake::fresh().answer("localbox starter --json --download", changed);
        let (result, _) = run_fake(&yes(), &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Starter);
        assert!(
            failure.message.starts_with("the starter offer changed"),
            "{}",
            failure.message
        );
        assert!(!fake.ran("localbox probe"));
    }

    #[test]
    fn chat_is_not_opened_where_overrides_could_redirect_it() {
        let mut fake = Fake::fresh();
        fake.tty = true;
        fake.overrides = vec![".localpilot.toml".into()];
        let (result, out) = run_fake(&yes(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(!fake.chat_opened);
        assert!(out.contains("Chat is not opened here"), "{out}");
    }

    #[test]
    fn a_chat_that_exits_with_an_error_is_reported() {
        let mut fake = Fake::fresh();
        fake.tty = true;
        fake.chat_clean = false;
        let (result, _) = run_fake(&yes(), &mut fake);
        assert_eq!(result.unwrap_err().stage, Stage::Chat);
    }

    #[test]
    fn each_run_claims_its_own_log_folder() {
        let root = tempfile::tempdir().unwrap();
        let first = claim_log_dir(root.path(), 7).unwrap();
        let second = claim_log_dir(root.path(), 7).unwrap();
        assert_ne!(first, second);
        assert!(first.ends_with("setup-7"));
        assert!(second.ends_with("setup-7-2"));
    }

    #[test]
    fn a_hidden_step_keeps_its_diagnostics_out_of_its_output() {
        let dir = tempfile::tempdir().unwrap();
        let shell = |_: &str| PathBuf::from(if cfg!(windows) { "cmd" } else { "sh" });
        let mut system = System {
            tool_path: shell,
            log_dir: dir.path().to_path_buf(),
            runs: 0,
        };
        let args: Vec<String> = if cfg!(windows) {
            vec!["/C".into(), "echo diagnostic 1>&2".into()]
        } else {
            vec!["-c".into(), "echo diagnostic >&2".into()]
        };
        let output = system.run(Stage::Reply, "shell", &args, false).unwrap();
        assert!(output.success);
        assert_eq!(output.stdout.trim(), "", "a diagnostic is not a reply");
        let log = std::fs::read_to_string(output.log.unwrap()).unwrap();
        assert!(log.contains("diagnostic"), "{log}");
    }

    #[test]
    fn json_is_read_whole_or_from_its_last_line() {
        assert_eq!(
            parse_json("{\n  \"a\": {\n    \"b\": 1\n  }\n}\n").unwrap()["a"]["b"],
            1
        );
        assert_eq!(parse_json("note\n{\"ok\":true}\n").unwrap()["ok"], true);
        assert!(parse_json("no json here").is_none());
        assert_eq!(
            api_base("http://127.0.0.1:11435"),
            "http://127.0.0.1:11435/v1"
        );
        assert_eq!(
            api_base("http://127.0.0.1:11435/v1/"),
            "http://127.0.0.1:11435/v1"
        );
    }

    #[test]
    fn yes_never_replaces_another_provider() {
        let mut fake = Fake::fresh();
        fake.config = Some("[provider]\ndefault = \"anthropic\"\n".into());
        let local = Options {
            local: true,
            ..yes()
        };
        let (result, _) = run_fake(&local, &mut fake);
        let failure = result.unwrap_err();
        assert_eq!(failure.stage, Stage::Provider);
        assert!(
            failure.message.contains("--replace-provider"),
            "{}",
            failure.message
        );
        assert!(fake.calls.is_empty(), "{:?}", fake.calls);

        let mut fake = Fake::fresh();
        fake.config = Some("[provider]\ndefault = \"anthropic\"\n".into());
        let replace = Options {
            replace_provider: true,
            ..local
        };
        let (result, out) = run_fake(&replace, &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(fake.ran("localpilot localbox adopt --user --endpoint http://127.0.0.1:11435/v1 --yes --replace-existing"));
    }

    #[test]
    fn keeping_another_provider_skips_the_local_model() {
        let mut fake = Fake::fresh();
        fake.config = Some(
            "[provider]\ndefault = \"anthropic\"\n[providers.anthropic]\nmodel = \"m1\"\n".into(),
        );
        let existing = Options {
            existing: true,
            ..Options::default()
        };
        let (result, out) = run_fake(&existing, &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert_eq!(
            fake.calls,
            vec![format!(
                "localpilot ask --provider anthropic --model m1 {READY_PROMPT}"
            )]
        );
    }

    #[test]
    fn at_a_terminal_the_download_is_asked_and_chat_opens() {
        let mut fake = Fake::fresh();
        fake.tty = true;
        fake.answers = vec![false];
        let (result, _) = run_fake(&Options::default(), &mut fake);
        assert_eq!(result.unwrap_err().message, "the download was declined");
        assert!(!fake.ran("localbox starter --json --download"));

        let mut fake = Fake::fresh();
        fake.tty = true;
        fake.answers = vec![true];
        let (result, out) = run_fake(&Options::default(), &mut fake);
        assert_eq!(result, Ok(()), "{out}");
        assert!(fake.chat_opened);
    }

    #[test]
    fn an_empty_reply_fails_the_reply_step() {
        let mut fake = Fake::fresh().answer("localpilot ask", ok("  \n"));
        let (result, _) = run_fake(&yes(), &mut fake);
        assert_eq!(result.unwrap_err().stage, Stage::Reply);
    }
}
