//! `localpilot search status|start|stop|update|remove`: the local SearXNG
//! instance setup created. Status only reads; every other command needs a
//! person at a terminal, shows the exact commands, asks, records each step in
//! the setup audit log first, and acts only on what LocalPilot recorded as its
//! own on the Docker endpoint it was set up on.

use std::path::PathBuf;

use localpilot_config::{Config, ManagedSearchBackend};

use crate::search_backend::{Launcher, SearchSpec};
use crate::search_setup::{audit_searxng, Answer, ConfigWriter, SetupConsole};
use crate::searxng::{self, Engine, Host, Owned, Staged};
use crate::user_config::{ConfigEdit, ConfigError};

/// The managed id setup writes SearXNG's bridge under.
const ID: &str = "searxng";

/// A lifecycle command.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum Action {
    Status,
    Start,
    Stop,
    Update,
    Remove,
}

/// Everything a command touches, injected so tests use fakes.
pub(crate) struct Ctx<'a> {
    pub(crate) console: &'a dyn SetupConsole,
    pub(crate) host: &'a dyn Host,
    /// `~/.localpilot/search`.
    pub(crate) state_dir: PathBuf,
    pub(crate) effective: &'a dyn Fn() -> anyhow::Result<Config>,
    pub(crate) config_path: Option<PathBuf>,
    pub(crate) write: ConfigWriter<'a>,
    pub(crate) launcher: Launcher,
}

impl Ctx<'_> {
    fn say(&self, line: &str) {
        self.console.say(line);
    }

    /// Record a step before it happens; no record, no step.
    fn gate(&self, event: &str, detail: &str) -> Result<(), String> {
        if audit_searxng(&self.state_dir, event, detail) {
            Ok(())
        } else {
            Err(
                "the step could not be recorded in the setup audit log, so it was not done"
                    .to_string(),
            )
        }
    }

    fn note(&self, event: &str, detail: &str) {
        audit_searxng(&self.state_dir, event, detail);
    }

    fn confirm(&self, question: &str) -> bool {
        self.console.ask(question) == Answer::Yes
    }

    fn config(&self) -> Option<Config> {
        (self.effective)().ok()
    }
}

/// The bridge entry in effect; `None` when the config cannot be read.
fn live_entry(ctx: &Ctx<'_>) -> Option<Option<ManagedSearchBackend>> {
    ctx.config()
        .map(|config| config.search.managed.get(ID).cloned())
}

/// Whether the entry in effect is exactly `recorded`, as LocalPilot wrote it.
fn in_effect(ctx: &Ctx<'_>, recorded: Option<&ManagedSearchBackend>) -> bool {
    recorded.is_some_and(|recorded| live_entry(ctx).flatten().as_ref() == Some(recorded))
}

/// The `[search.managed.searxng]` table of a config document, read the way
/// the config loader reads it.
fn stanza(document: &toml_edit::DocumentMut) -> Result<Option<ManagedSearchBackend>, String> {
    #[derive(serde::Deserialize)]
    struct One {
        entry: ManagedSearchBackend,
    }
    let Some(item) = document
        .get("search")
        .and_then(|search| search.get("managed"))
        .and_then(|managed| managed.get(ID))
    else {
        return Ok(None);
    };
    let mut alone = toml_edit::DocumentMut::new();
    alone.insert("entry", item.clone());
    toml_edit::de::from_str::<One>(&alone.to_string())
        .map(|one| Some(one.entry))
        .map_err(|error| format!("[search.managed.searxng] could not be read ({error})"))
}

/// Replace (`Some`) or remove (`None`) the SearXNG table under the config
/// writer's lock, only while it is still exactly one of `expected`: an entry
/// changed by hand, even after the preview, is left as it is.
fn edit_entry(
    ctx: &Ctx<'_>,
    expected: &[&ManagedSearchBackend],
    next: Option<&ManagedSearchBackend>,
) -> Result<ConfigEdit, ConfigError> {
    (ctx.write)(ctx.config_path.as_deref(), &|document| {
        let found = stanza(document)?;
        if !found
            .as_ref()
            .is_some_and(|found| expected.contains(&found))
        {
            return Err(
                "[search.managed.searxng] is no longer the entry LocalPilot wrote, so it was left as it is"
                    .to_string(),
            );
        }
        match next {
            Some(entry) => crate::search_setup::place(document, ID, entry),
            None => {
                if let Some(managed) = document
                    .get_mut("search")
                    .and_then(|search| search.get_mut("managed"))
                    .and_then(toml_edit::Item::as_table_like_mut)
                {
                    managed.remove(ID);
                }
                Ok(())
            }
        }
    })
}

/// Run one command.
pub(crate) async fn run(ctx: &Ctx<'_>, action: Action) -> Result<(), String> {
    match action {
        Action::Status => {
            status(ctx);
            Ok(())
        }
        Action::Start => start(ctx),
        Action::Stop => stop(ctx),
        Action::Update => update(ctx).await,
        Action::Remove => remove(ctx),
    }
}

fn status(ctx: &Ctx<'_>) {
    let owned = match searxng::load_owned(&ctx.state_dir) {
        None => {
            ctx.say("No SearXNG is set up. localpilot search setup sets one up.");
            return;
        }
        Some(Err(reason)) => {
            ctx.say(&format!("SearXNG: {reason}"));
            return;
        }
        Some(Ok(owned)) => owned,
    };
    ctx.say(&format!(
        "SearXNG container {} ({})",
        owned.container_name,
        searxng::short(&owned.container_id)
    ));
    let current = owned.image == searxng::IMAGE;
    ctx.say(&format!(
        "  image: {}{}",
        owned.image,
        if current {
            String::new()
        } else {
            format!(
                " (localpilot search update moves it to {})",
                searxng::IMAGE_TAG
            )
        }
    ));
    ctx.say(&format!("  address: http://127.0.0.1:{}", owned.port));
    ctx.say(&format!("  settings: {}", owned.settings_dir.display()));
    if owned.unfinished() {
        ctx.say("  an update did not finish: localpilot search update finishes or undoes it");
    }
    match searxng::recorded_engine(ctx.host, &owned) {
        Err(reason) => ctx.say(&format!("  Docker: {reason}")),
        Ok(engine) => {
            ctx.say(&format!(
                "  Docker: {} at {}",
                engine.describe(),
                engine.endpoint
            ));
            match searxng::owned_container(ctx.host, &engine, &owned) {
                Err(reason) => ctx.say(&format!("  container: {reason}")),
                Ok(seen) if seen.running => {
                    let live = matches!(ctx.host.get(owned.port, "/healthz"), Some((200, body)) if body.trim() == "OK");
                    ctx.say(&format!(
                        "  container: running; {}",
                        if live { "answering" } else { "not answering yet" }
                    ));
                    if live {
                        if let Some(engines) = searxng::live_engines(ctx.host, owned.port) {
                            ctx.say(&format!(
                                "  general web engines enabled: {}",
                                engines.join(", ")
                            ));
                        }
                    }
                }
                Ok(_) => ctx.say(
                    "  container: stopped (LocalPilot starts it at the next search; localpilot search start starts it now)",
                ),
            }
        }
    }
    ctx.say(if in_effect(ctx, owned.entry.as_ref()) {
        "  config: [search.managed.searxng] uses it"
    } else {
        "  config: [search.managed.searxng] is not the entry LocalPilot wrote for it"
    });
}

/// The lock, the record and its verified engine: what every changing command
/// starts from.
fn begin(ctx: &Ctx<'_>) -> Result<(searxng::Lock, Owned, Engine), String> {
    let lock = searxng::lock(&ctx.state_dir)?;
    let owned = searxng::load_owned(&ctx.state_dir)
        .ok_or_else(|| "No SearXNG is set up. localpilot search setup sets one up".to_string())??;
    let engine = searxng::recorded_engine(ctx.host, &owned)?;
    Ok((lock, owned, engine))
}

fn shown(engine: &Engine, args: &[&str]) -> String {
    searxng::shown(&engine.argv(args))
}

fn start(ctx: &Ctx<'_>) -> Result<(), String> {
    let (_lock, owned, engine) = begin(ctx)?;
    if owned.unfinished() {
        return Err("an update did not finish; run localpilot search update first".to_string());
    }
    let seen = searxng::owned_container(ctx.host, &engine, &owned)?;
    if !seen.running {
        ctx.say("This will run:");
        ctx.say(&format!(
            "  {}",
            shown(&engine, &["start", &owned.container_id])
        ));
        ctx.say("SearXNG uses about 105 MB of memory while it runs.");
        if !ctx.confirm("Start SearXNG now?") {
            ctx.say("Nothing was changed.");
            return Ok(());
        }
        ctx.gate(
            "searxng-start",
            &format!("container={} reason=command", owned.container_id),
        )?;
        let started = searxng::start(ctx.host, &engine, &owned.container_id);
        ctx.note(
            "searxng-start-result",
            &format!(
                "container={} ok={}",
                owned.container_id,
                started == searxng::Started::Yes
            ),
        );
        match started {
            searxng::Started::Yes => {}
            searxng::Started::PortTaken => {
                return Err(format!(
                    "port {} is taken by another program; localpilot search update moves SearXNG to a free port",
                    owned.port
                ))
            }
            searxng::Started::No(reason) => return Err(format!("SearXNG did not start ({reason})")),
        }
    }
    if searxng::wait_live(ctx.host, owned.port, searxng::LIVE_WAIT, &|| false) {
        ctx.say(&format!(
            "SearXNG runs and answers on 127.0.0.1:{}.",
            owned.port
        ));
        Ok(())
    } else {
        Err(format!(
            "SearXNG did not answer on 127.0.0.1:{} within {} seconds",
            owned.port,
            searxng::LIVE_WAIT.as_secs()
        ))
    }
}

fn stop(ctx: &Ctx<'_>) -> Result<(), String> {
    let (_lock, owned, engine) = begin(ctx)?;
    let seen = searxng::owned_container(ctx.host, &engine, &owned)?;
    if !seen.running {
        ctx.say("SearXNG is already stopped.");
        return Ok(());
    }
    ctx.say("This will run:");
    ctx.say(&format!(
        "  {}",
        shown(&engine, &["stop", &owned.container_id])
    ));
    if !ctx.confirm("Stop SearXNG now?") {
        ctx.say("Nothing was changed.");
        return Ok(());
    }
    ctx.gate("searxng-stop", &format!("container={}", owned.container_id))?;
    let ran = ctx
        .host
        .run(&engine.argv(&["stop", &owned.container_id]), true);
    ctx.note(
        "searxng-stop-result",
        &format!("container={} ok={}", owned.container_id, ran.ok),
    );
    if ran.ok {
        ctx.say("SearXNG is stopped. LocalPilot starts it again at the next search.");
        Ok(())
    } else {
        Err(format!("SearXNG did not stop ({})", ran.stderr.trim()))
    }
}

/// Remove one recorded container (ID and label checked), recorded first.
fn remove_one(ctx: &Ctx<'_>, engine: &Engine, id: &str) -> Result<(), String> {
    ctx.gate("searxng-remove", &format!("container={id}"))?;
    let result = searxng::remove_container(ctx.host, engine, id);
    ctx.note(
        "searxng-remove-result",
        &format!("container={id} ok={}", result.is_ok()),
    );
    result
}

fn remove(ctx: &Ctx<'_>) -> Result<(), String> {
    let (_lock, owned, engine) = begin(ctx)?;
    let staged: Vec<&Staged> = owned.pending.iter().chain(owned.retired.iter()).collect();
    let mut ids = vec![owned.container_id.clone()];
    ids.extend(staged.iter().map(|staged| staged.container_id.clone()));
    let mut folders = vec![owned.settings_dir.clone()];
    for staged in &staged {
        if !folders.contains(&staged.settings_dir) {
            folders.push(staged.settings_dir.clone());
        }
    }
    let recorded: Vec<&ManagedSearchBackend> = owned
        .entry
        .iter()
        .chain(staged.iter().filter_map(|staged| staged.entry.as_ref()))
        .collect();
    let live = live_entry(ctx).flatten();
    let entry_ours = live.as_ref().is_some_and(|live| recorded.contains(&live));
    ctx.say("This will:");
    for id in &ids {
        ctx.say(&format!(
            "  run: {}",
            shown(&engine, &["rm", "--force", id])
        ));
    }
    if entry_ours {
        ctx.say("  remove [search.managed.searxng] from your config");
    } else if live.is_some() {
        ctx.say(
            "  leave [search.managed.searxng] in your config: it is not the entry LocalPilot wrote",
        );
    }
    for folder in &folders {
        ctx.say(&format!(
            "  delete {} (only if LocalPilot made it)",
            folder.display()
        ));
    }
    ctx.say("  forget LocalPilot's record of it");
    ctx.say("Only a container whose ID and LocalPilot label both match the record is removed.");
    if !ctx.confirm("Remove SearXNG?") {
        ctx.say("Nothing was changed.");
        return Ok(());
    }
    ctx.gate(
        "searxng-remove-all",
        &format!("container={}", owned.container_id),
    )?;
    // The config first, so nothing tries to use what goes next.
    if entry_ours {
        match edit_entry(ctx, &recorded, None) {
            Ok(ConfigEdit::Written | ConfigEdit::Unchanged) => {
                ctx.note("searxng-config-removed", "");
            }
            Err(ConfigError::Before(reason) | ConfigError::After(reason)) => {
                return Err(format!(
                    "the config was not changed ({reason}); nothing else was removed"
                ))
            }
        }
    }
    let mut left = Vec::new();
    for id in &ids {
        if let Err(reason) = remove_one(ctx, &engine, id) {
            left.push(reason);
        }
    }
    if !left.is_empty() {
        // The record stays, so this can be run again.
        return Err(format!("{}; the record was kept", left.join("; ")));
    }
    for folder in &folders {
        if let Err(reason) = searxng::remove_settings(folder) {
            ctx.say(&format!("Note: {reason}."));
        }
    }
    searxng::forget_owned(&ctx.state_dir)?;
    ctx.note("searxng-removed-all", "");
    ctx.say("SearXNG was removed. The image stays in Docker; docker image rm removes it.");
    Ok(())
}

/// The bridge entry pointed at port.
fn moved(entry: &ManagedSearchBackend, port: u16) -> ManagedSearchBackend {
    let mut moved = entry.clone();
    crate::search_setup::at_port(&mut moved, port);
    moved
}

/// The staged container becomes the instance; the old one is retired.
fn promote(owned: &mut Owned) {
    let Some(pending) = owned.pending.take() else {
        return;
    };
    owned.retired = Some(Staged {
        container_id: std::mem::replace(&mut owned.container_id, pending.container_id),
        container_name: std::mem::replace(&mut owned.container_name, pending.container_name),
        image: std::mem::replace(&mut owned.image, pending.image),
        port: std::mem::replace(&mut owned.port, pending.port),
        settings_dir: std::mem::replace(&mut owned.settings_dir, pending.settings_dir),
        entry: std::mem::replace(&mut owned.entry, pending.entry),
    });
}

/// The record's current container, as the one an update moves away from.
fn current(owned: &Owned) -> Staged {
    Staged {
        container_id: owned.container_id.clone(),
        container_name: owned.container_name.clone(),
        image: owned.image.clone(),
        port: owned.port,
        settings_dir: owned.settings_dir.clone(),
        entry: owned.entry.clone(),
    }
}

/// Remove a staged or retired container, then its own settings folder.
fn drop_staged(
    ctx: &Ctx<'_>,
    engine: &Engine,
    owned: &Owned,
    staged: &Staged,
) -> Result<(), String> {
    remove_one(ctx, engine, &staged.container_id)?;
    if staged.settings_dir != owned.settings_dir {
        match searxng::remove_settings(&staged.settings_dir) {
            Ok(()) => {}
            // Still LocalPilot's: keep it in the record, so the next run
            // removes it (a gone container is not an error then).
            Err(reason) if searxng::settings_ours(&staged.settings_dir) => return Err(reason),
            Err(reason) => ctx.say(&format!("Note: {reason}.")),
        }
    }
    Ok(())
}

/// Remove the retired container and forget it.
fn clear_retired(ctx: &Ctx<'_>, engine: &Engine, owned: &mut Owned) -> Result<(), String> {
    if let Some(retired) = owned.retired.clone() {
        drop_staged(ctx, engine, owned, &retired)?;
        owned.retired = None;
        searxng::save_owned(&ctx.state_dir, owned)?;
    }
    Ok(())
}

/// Finish or undo an update that stopped part-way, as its own confirmed step.
/// It acts only when the entry in effect is exactly one LocalPilot recorded
/// for the old or the new container; otherwise both containers and the record
/// stay as they are.
/// Which way an unfinished update goes, and the container that goes with
/// it: decided only from the table in effect, never from a guess.
fn unfinished_way(ctx: &Ctx<'_>, owned: &Owned) -> Option<(&'static str, Staged)> {
    let live = live_entry(ctx).flatten();
    let uses = |entry: Option<&ManagedSearchBackend>| entry.is_some() && live.as_ref() == entry;
    match (&owned.pending, &owned.retired) {
        (Some(pending), _) if uses(pending.entry.as_ref()) => Some(("finish", current(owned))),
        (Some(pending), _) if uses(owned.entry.as_ref()) => Some(("undo", pending.clone())),
        (None, Some(retired)) if uses(owned.entry.as_ref()) => Some(("clear", retired.clone())),
        _ => None,
    }
}

/// Whether the table on disk is still exactly entry, read under the config
/// writer's lock; nothing is written.
fn locked_entry_is(ctx: &Ctx<'_>, entry: Option<&ManagedSearchBackend>) -> bool {
    let Some(entry) = entry else {
        return false;
    };
    let unchanged = (ctx.write)(ctx.config_path.as_deref(), &|document| {
        if stanza(document)?.as_ref() == Some(entry) {
            Ok(())
        } else {
            Err("[search.managed.searxng] changed".to_string())
        }
    });
    matches!(unchanged, Ok(ConfigEdit::Unchanged))
}

fn resolve_unfinished(ctx: &Ctx<'_>, engine: &Engine, mut owned: Owned) -> Result<(), String> {
    let unsure = "an update did not finish, and [search.managed.searxng] is not exactly the entry LocalPilot wrote for the old or the new container (or the config could not be read), so nothing was changed. Put the entry back, or run localpilot search remove";
    let Some((way, goes)) = unfinished_way(ctx, &owned) else {
        return Err(unsure.to_string());
    };
    ctx.say(match way {
        "finish" => "An update stopped after the config switched to the new container. This will keep the new one and:",
        "undo" => "An update stopped before the config switched. This will keep the current container and:",
        _ => "An update switched containers but did not remove the old one. This will:",
    });
    ctx.say(&format!(
        "  run: {}",
        shown(engine, &["rm", "--force", &goes.container_id])
    ));
    // The entry that proves which container is live, and the folder it keeps.
    let (proof, keeps) = match (way, &owned.pending) {
        ("finish", Some(pending)) => (pending.entry.clone(), pending.settings_dir.clone()),
        _ => (owned.entry.clone(), owned.settings_dir.clone()),
    };
    if goes.settings_dir != keeps {
        ctx.say(&format!(
            "  delete {} (only if LocalPilot made it)",
            goes.settings_dir.display()
        ));
    }
    if !ctx.confirm("Go ahead?") {
        ctx.say("Nothing was changed.");
        return Ok(());
    }
    // The config may have changed while the question was open: decide again,
    // and check the table on disk under the config lock before removing.
    if unfinished_way(ctx, &owned).as_ref() != Some(&(way, goes.clone()))
        || !locked_entry_is(ctx, proof.as_ref())
    {
        return Err(unsure.to_string());
    }
    ctx.gate(
        "searxng-recover",
        &format!("way={way} container={}", goes.container_id),
    )?;
    match way {
        "finish" => {
            promote(&mut owned);
            searxng::save_owned(&ctx.state_dir, &owned)?;
        }
        "undo" => {
            drop_staged(ctx, engine, &owned, &goes)?;
            owned.pending = None;
            searxng::save_owned(&ctx.state_dir, &owned)?;
        }
        _ => {}
    }
    clear_retired(ctx, engine, &mut owned)?;
    ctx.say("Done. localpilot search update now updates SearXNG.");
    Ok(())
}

/// Remove a staged container, its settings copy and its record after a
/// failed update; the running instance and the config are untouched.
fn undo_staged(ctx: &Ctx<'_>, engine: &Engine, owned: &mut Owned, reason: String) -> String {
    let Some(pending) = owned.pending.take() else {
        return reason;
    };
    match drop_staged(ctx, engine, owned, &pending) {
        Ok(()) => match searxng::save_owned(&ctx.state_dir, owned) {
            Ok(()) => format!("{reason}; the new container was removed and SearXNG is as it was"),
            Err(more) => format!("{reason}; {more}"),
        },
        Err(more) => {
            owned.pending = Some(pending);
            format!("{reason}; {more}: run localpilot search update again to clean up")
        }
    }
}

/// Re-create SearXNG from the pinned image on a free port, staged: the new
/// container gets its own copy of the settings, starts, answers and passes
/// the bridge check before the config switches to it, and only then is the
/// old one removed.
async fn update(ctx: &Ctx<'_>) -> Result<(), String> {
    let (_lock, mut owned, engine) = begin(ctx)?;
    if owned.unfinished() {
        return resolve_unfinished(ctx, &engine, owned);
    }
    let Some(entry) = owned
        .entry
        .clone()
        .filter(|entry| in_effect(ctx, Some(entry)))
    else {
        return Err("[search.managed.searxng] is not the entry LocalPilot wrote for this instance; run localpilot search remove, then localpilot search setup".to_string());
    };
    let port = (searxng::FIRST_PORT..searxng::FIRST_PORT.saturating_add(100))
        .find(|port| *port != owned.port && ctx.host.port_free(*port))
        .ok_or_else(|| "no free local port was found for the new container".to_string())?;
    let copy = owned
        .settings_dir
        .with_file_name(format!("settings-{port}"));
    if copy.exists() {
        return Err(format!(
            "{} already exists; move it away, then run localpilot search update again",
            copy.display()
        ));
    }
    let mount = searxng::mount_path(ctx.host, &engine, &copy)?;
    let create = searxng::create_args(port, &mount);
    let create: Vec<&str> = create.iter().map(String::as_str).collect();
    let next = moved(&entry, port);
    ctx.say("This will:");
    ctx.say(&format!(
        "  copy the settings to {} for the new container (the running one keeps its own)",
        copy.display()
    ));
    ctx.say(&format!(
        "  create a new container from {} ({}):",
        searxng::IMAGE_TAG,
        searxng::IMAGE
    ));
    ctx.say(&format!("    {}", shown(&engine, &create)));
    ctx.say(&format!(
        "  start it on 127.0.0.1:{port}, wait for it to answer and check the search bridge"
    ));
    ctx.say(&format!(
        "  then point [search.managed.searxng] at port {port} and remove the old container {} ({}) and {}",
        owned.container_name,
        searxng::short(&owned.container_id),
        owned.settings_dir.display()
    ));
    ctx.say("If anything fails before the config changes, the new container and the copy are removed and the old ones are kept.");
    if !ctx.confirm("Update SearXNG?") {
        ctx.say("Nothing was changed.");
        return Ok(());
    }
    ctx.gate("searxng-create", &format!("port={port} reason=update"))?;
    searxng::copy_settings(&owned.settings_dir, &copy)?;
    ctx.say("Creating the new container (downloading the image can take a few minutes)...");
    let id = match searxng::create(ctx.host, &engine, port, &mount) {
        Ok(id) => id,
        Err(reason) => {
            return Err(match searxng::remove_settings(&copy) {
                Ok(()) => reason,
                Err(more) => format!("{reason}; {more}"),
            })
        }
    };
    let staged = Staged {
        container_id: id.clone(),
        container_name: searxng::container_name(port),
        image: searxng::IMAGE.to_string(),
        port,
        settings_dir: copy,
        entry: Some(next.clone()),
    };
    owned.pending = Some(staged.clone());
    ctx.note(
        "searxng-created",
        &format!("container={id} port={port} reason=update"),
    );
    if let Err(reason) = searxng::save_owned(&ctx.state_dir, &owned) {
        // Not recorded: remove it now, while its ID is known.
        owned.pending = None;
        return Err(match drop_staged(ctx, &engine, &owned, &staged) {
            Ok(()) => format!("{reason}; the new container was removed and SearXNG is as it was"),
            Err(more) => format!(
                "{reason}; the new container {} ({id}) could not be removed either ({more}): remove it with docker rm --force {id}",
                staged.container_name
            ),
        });
    }
    if let Err(reason) = ctx.gate("searxng-start", &format!("container={id} reason=update")) {
        return Err(undo_staged(ctx, &engine, &mut owned, reason));
    }
    let started = searxng::start(ctx.host, &engine, &id);
    ctx.note(
        "searxng-start-result",
        &format!("container={id} ok={}", started == searxng::Started::Yes),
    );
    if started != searxng::Started::Yes {
        return Err(undo_staged(
            ctx,
            &engine,
            &mut owned,
            "the new container did not start".to_string(),
        ));
    }
    if !searxng::wait_live(ctx.host, port, searxng::LIVE_WAIT, &|| false) {
        return Err(undo_staged(
            ctx,
            &engine,
            &mut owned,
            "the new container did not answer".to_string(),
        ));
    }
    let spec = SearchSpec {
        server: ID.to_string(),
        tool: next.tool.clone(),
        launch: next.server(),
        managed: Some(next.clone()),
    };
    match crate::search_backend::start_managed(&spec, &next, &ctx.launcher, false, |_, _| true)
        .await
    {
        Ok(transport) => drop(transport),
        Err(error) => {
            return Err(undo_staged(ctx, &engine, &mut owned, error.describe(ID)));
        }
    }
    // The switch. The record keeps the staged container until the config
    // names it, so an interruption here can be finished or undone next time.
    ctx.gate("searxng-switch", &format!("container={id} port={port}"))
        .map_err(|reason| undo_staged(ctx, &engine, &mut owned, reason))?;
    match edit_entry(ctx, &[&entry], Some(&next)) {
        Ok(_) => {}
        Err(ConfigError::Before(reason)) => {
            return Err(undo_staged(
                ctx,
                &engine,
                &mut owned,
                format!("the config was not changed ({reason})"),
            ));
        }
        Err(ConfigError::After(reason)) => {
            return Err(format!(
                "the config was written but could not be confirmed ({reason}); run localpilot search update again to finish"
            ));
        }
    }
    promote(&mut owned);
    searxng::save_owned(&ctx.state_dir, &owned)?;
    clear_retired(ctx, &engine, &mut owned)?;
    ctx.say(&format!(
        "SearXNG now runs from {} on 127.0.0.1:{}.",
        searxng::IMAGE_TAG,
        owned.port
    ));
    Ok(())
}

// ------------------------------------------------------------- production

/// The managed search backends for `localpilot doctor`: each entry as
/// configured, whether its command exists, and for the SearXNG LocalPilot set
/// up, what its record, Docker and 127.0.0.1 say. Starts nothing; Docker inside
/// WSL is not asked, because asking could start the distribution. Docker
/// problems are reported in fixed words, never as command output.
pub(crate) fn readiness(
    config: &Config,
    state_dir: Option<&std::path::Path>,
    host: &dyn Host,
) -> Vec<crate::search_readiness::SearchReadiness> {
    let (specs, _) = crate::search_backend::search_specs(config);
    config
        .search
        .managed
        .iter()
        .map(|(id, entry)| {
            let research = config.research.web.enabled;
            let order = research
                .then(|| {
                    specs
                        .iter()
                        .position(|spec| spec.managed.is_some() && spec.server == *id)
                        .map(|index| index + 1)
                })
                .flatten();
            let engine = (id == ID && entry.tier == localpilot_config::SearchTier::SelfHosted)
                .then(|| engine_readiness(host, entry, state_dir));
            crate::search_readiness::SearchReadiness {
                id: id.clone(),
                tier: entry.tier.as_str().to_string(),
                tool: entry.tool.clone(),
                order,
                chat_web_search: research
                    && config.tools.web == localpilot_config::ChatWebMode::Ask,
                identity_valid: entry.identity_matches(),
                command_present: crate::search_setup::command_present(&entry.command),
                unattended: entry.unattended,
                engine,
            }
        })
        .collect()
}

/// How long one Docker command may take in `doctor`.
const DOCTOR_DOCKER_WAIT: std::time::Duration = std::time::Duration::from_secs(10);

/// [`readiness`] for this computer: the effective config from the working
/// directory and the real Docker, each command bounded. Empty when the config
/// does not load.
pub(crate) fn doctor_readiness() -> Vec<crate::search_readiness::SearchReadiness> {
    let Ok(cwd) = std::env::current_dir() else {
        return Vec::new();
    };
    let Ok(config) = localpilot_config::load(
        &localpilot_config::ConfigPaths::standard(&cwd),
        &localpilot_config::CliOverrides::default(),
    ) else {
        return Vec::new();
    };
    let state_dir = crate::search_setup::state_dir();
    readiness(
        &config,
        state_dir.as_deref(),
        &searxng::BoundedHost(DOCTOR_DOCKER_WAIT),
    )
}

fn engine_readiness(
    host: &dyn Host,
    entry: &ManagedSearchBackend,
    state_dir: Option<&std::path::Path>,
) -> crate::search_readiness::EngineReadiness {
    let unknown = |record| crate::search_readiness::EngineReadiness {
        record,
        port: None,
        image_current: None,
        table_matches_record: None,
        update_unfinished: None,
        container: None,
        answering: None,
    };
    let owned = match state_dir.and_then(searxng::load_owned) {
        None => return unknown("missing"),
        Some(Err(_)) => return unknown("unreadable"),
        Some(Ok(owned)) => owned,
    };
    let (container, running) = match &owned.route {
        searxng::Route::Wsl { .. } => (
            "not checked: Docker runs inside WSL, and checking could start the distribution",
            false,
        ),
        searxng::Route::Path { .. } => match searxng::recorded_engine(host, &owned) {
            Err(_) => (
                "not checked: Docker did not answer, or now uses another endpoint than the recorded one",
                false,
            ),
            Ok(engine) => match searxng::owned_container(host, &engine, &owned) {
                Err(_) => ("missing, or not the one LocalPilot created", false),
                Ok(seen) if seen.running => ("running", true),
                Ok(_) => ("stopped", false),
            },
        },
    };
    crate::search_readiness::EngineReadiness {
        record: "recorded",
        port: Some(owned.port),
        image_current: Some(owned.image == searxng::IMAGE),
        table_matches_record: Some(owned.entry.as_ref() == Some(entry)),
        update_unfinished: Some(owned.unfinished()),
        container: Some(container.to_string()),
        answering: running.then(
            || matches!(host.get(owned.port, "/healthz"), Some((200, body)) if body.trim() == "OK"),
        ),
    }
}

/// `localpilot search <action>`.
pub(crate) async fn run_command(action: Action) -> anyhow::Result<()> {
    use std::io::IsTerminal as _;
    if action != Action::Status
        && !(std::io::stdin().is_terminal() && std::io::stdout().is_terminal())
    {
        anyhow::bail!(
            "this command changes SearXNG and asks first; run it in a terminal (localpilot search status only reads)"
        );
    }
    let state_dir = crate::search_setup::state_dir()
        .ok_or_else(|| anyhow::anyhow!("no home folder for SearXNG's record"))?;
    let cwd = std::env::current_dir()?;
    let effective = || {
        localpilot_config::load(
            &localpilot_config::ConfigPaths::standard(&cwd),
            &localpilot_config::CliOverrides::default(),
        )
        .map_err(anyhow::Error::from)
    };
    let write = |path: Option<&std::path::Path>,
                 edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>| {
        crate::user_config::edit_user_config(path, &|| Ok(()), &|_| {}, edit)
    };
    let ctx = Ctx {
        console: &crate::search_setup::Terminal,
        host: &searxng::LocalHost,
        state_dir,
        effective: &effective,
        config_path: localpilot_config::user_config_path(),
        write: &write,
        launcher: crate::search_backend::spawn_launcher(),
    };
    run(&ctx, action)
        .await
        .map_err(|reason| anyhow::anyhow!(reason))
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;
    use crate::searxng::tests::{after_host, failed, ok, FakeHost};
    use localpilot_config::{McpEnvEntry, SearchTier};
    use std::cell::RefCell;
    use std::collections::BTreeMap;
    use std::rc::Rc;

    /// Containers a fake Docker holds: ID to (port, running, ours).
    type Containers = Rc<RefCell<BTreeMap<String, (u16, bool, bool)>>>;

    fn docker(containers: &Containers) -> FakeHost {
        let containers = Rc::clone(containers);
        FakeHost::new(move |argv| {
            if argv.iter().any(|word| word == "context") {
                return ok(r#"{"Host":"unix:///var/run/docker.sock"}"#);
            }
            let words = after_host(argv);
            let mut held = containers.borrow_mut();
            match words.as_slice() {
                ["version", ..] => ok("29.6.0"),
                ["create", ..] => {
                    let port = words
                        .iter()
                        .find_map(|word| {
                            word.strip_prefix("127.0.0.1:")?
                                .split(':')
                                .next()?
                                .parse()
                                .ok()
                        })
                        .unwrap();
                    let id = format!("{:064x}", held.len() + 0xabc0);
                    held.insert(id.clone(), (port, false, true));
                    ok(&id)
                }
                ["start", id] => match held.get_mut(*id) {
                    Some(container) => {
                        container.1 = true;
                        ok(id)
                    }
                    None => failed("No such container"),
                },
                ["stop", id] => match held.get_mut(*id) {
                    Some(container) => {
                        container.1 = false;
                        ok(id)
                    }
                    None => failed("No such container"),
                },
                ["rm", "--force", id] => {
                    held.remove(*id);
                    ok(id)
                }
                ["container", "inspect", _, _, which] => match held.get(*which) {
                    Some((_, running, ours)) => {
                        let label = if *ours {
                            format!("{{\"{}\":\"{}\"}}", searxng::LABEL, searxng::LABEL_VALUE)
                        } else {
                            "{}".to_string()
                        };
                        ok(&format!("\"{which}\" {label} {running}"))
                    }
                    None => failed("No such container"),
                },
                _ => failed("unexpected"),
            }
        })
    }

    struct Fixture {
        _dir: tempfile::TempDir,
        state: PathBuf,
        config: PathBuf,
        containers: Containers,
    }

    const OLD_IMAGE: &str = "docker.io/searxng/searxng@sha256:0000000000000000000000000000000000000000000000000000000000000000";

    /// A set-up instance: a running container on 8888, its record, and the
    /// bridge entry in the config.
    fn fixture(image: &str) -> Fixture {
        let dir = tempfile::tempdir().unwrap();
        let state = dir.path().join("search");
        let config = dir.path().join("config.toml");
        let containers: Containers = Rc::new(RefCell::new(BTreeMap::new()));
        let id = format!("{:064x}", 0x1111);
        containers
            .borrow_mut()
            .insert(id.clone(), (8888, true, true));
        let settings = searxng::settings_dir(&state);
        searxng::prepare_settings(&settings, "secret", &|| true).unwrap();
        let mut entry = crate::search_backend::tests::managed_backend(searxng::BRIDGE_TOOL);
        entry.tier = SearchTier::SelfHosted;
        entry.args = vec!["-y".to_string(), searxng::BRIDGE_PACKAGE.to_string()];
        crate::search_setup::at_port(&mut entry, 8888);
        searxng::save_owned(
            &state,
            &Owned {
                route: searxng::Route::Path {
                    program: PathBuf::from("/usr/bin/docker"),
                },
                endpoint: "unix:///var/run/docker.sock".to_string(),
                container_id: id,
                container_name: "localpilot-searxng-old-8888".to_string(),
                image: image.to_string(),
                port: 8888,
                settings_dir: settings,
                entry: Some(entry.clone()),
                pending: None,
                retired: None,
            },
        )
        .unwrap();
        let mut document = toml_edit::DocumentMut::new();
        crate::search_setup::place(&mut document, ID, &entry).unwrap();
        std::fs::write(
            &config,
            format!("# mine\n[model]\nname = \"x\"\n\n{document}"),
        )
        .unwrap();
        Fixture {
            _dir: dir,
            state,
            config,
            containers,
        }
    }

    struct Yes(RefCell<Vec<String>>, bool);

    impl SetupConsole for Yes {
        fn say(&self, line: &str) {
            self.0.borrow_mut().push(line.to_string());
        }
        fn pick(&self, _prompt: &str, _options: &[String]) -> crate::search_setup::Pick {
            crate::search_setup::Pick::Cancel
        }
        fn ask(&self, _prompt: &str) -> Answer {
            if self.1 {
                Answer::Yes
            } else {
                Answer::No
            }
        }
        fn secret(&self, _prompt: &str) -> Option<localpilot_core::Secret> {
            None
        }
    }

    fn effective(path: &std::path::Path) -> anyhow::Result<Config> {
        Ok(localpilot_config::load(
            &localpilot_config::ConfigPaths {
                user: Some(path.to_path_buf()),
                project: None,
            },
            &localpilot_config::CliOverrides::default(),
        )?)
    }

    fn real_write(
        path: Option<&std::path::Path>,
        edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>,
    ) -> Result<ConfigEdit, ConfigError> {
        crate::user_config::edit_user_config(path, &|| Ok(()), &|_| {}, edit)
    }

    fn launcher(works: bool) -> Launcher {
        if works {
            let transport = std::sync::Arc::new(localpilot_mcp::ScriptedTransport::new().with(
                "tools/list",
                crate::search_backend::tests::search_tool_list(searxng::BRIDGE_TOOL),
            ));
            crate::search_backend::tests::counting_launcher(transport).0
        } else {
            std::sync::Arc::new(|_config, _handshake| Box::pin(async { Err("no".to_string()) }))
        }
    }

    async fn act(
        fixture: &Fixture,
        host: &FakeHost,
        action: Action,
        yes: bool,
        bridge: bool,
    ) -> (Result<(), String>, String) {
        let console = Yes(RefCell::new(Vec::new()), yes);
        let config = fixture.config.clone();
        let read = move || effective(&config);
        let ctx = Ctx {
            console: &console,
            host,
            state_dir: fixture.state.clone(),
            effective: &read,
            config_path: Some(fixture.config.clone()),
            write: &real_write,
            launcher: launcher(bridge),
        };
        let result = run(&ctx, action).await;
        let said = console.0.borrow().join("\n");
        (result, said)
    }

    /// The port the bridge entry points at, if any.
    fn entry_port(entry: &ManagedSearchBackend) -> Option<u16> {
        match entry.env.get("SEARXNG_URL")? {
            McpEnvEntry::Plain(url) => url.strip_prefix("http://127.0.0.1:")?.parse().ok(),
            McpEnvEntry::Object(_) => None,
        }
    }

    fn port_in_config(fixture: &Fixture) -> Option<u16> {
        effective(&fixture.config)
            .ok()?
            .search
            .managed
            .get(ID)
            .and_then(entry_port)
    }

    #[test]
    fn doctor_readiness_reads_without_starting_anything() {
        let fixture = fixture(OLD_IMAGE);
        let host = docker(&fixture.containers);
        let config = effective(&fixture.config).unwrap();
        let found = readiness(&config, Some(&fixture.state), &host);
        assert_eq!(found.len(), 1);
        let backend = &found[0];
        assert_eq!(backend.id, ID);
        assert_eq!(backend.order, Some(1));
        assert!(backend.identity_valid);
        let engine = backend.engine.as_ref().unwrap();
        assert_eq!(engine.port, Some(8888));
        assert_eq!(engine.container.as_deref(), Some("running"));
        assert_eq!(engine.answering, Some(true));
        assert_eq!(engine.image_current, Some(false));
        assert_eq!(engine.table_matches_record, Some(true));
        assert!(!host.commands().iter().any(|c| c.contains(" start ")
            || c.contains(" stop ")
            || c.contains(" rm ")
            || c.contains(" create ")));
        assert!(!fixture.state.join("setup-audit.log").exists());

        // Stopped: reported, not started, and health is not asked.
        for container in fixture.containers.borrow_mut().values_mut() {
            container.1 = false;
        }
        let engine = readiness(&config, Some(&fixture.state), &host)[0]
            .engine
            .clone()
            .unwrap();
        assert_eq!(engine.container.as_deref(), Some("stopped"));
        assert_eq!(engine.answering, None);
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| !running));

        // Docker inside WSL is not asked at all.
        let mut owned = searxng::load_owned(&fixture.state).unwrap().unwrap();
        owned.route = searxng::Route::Wsl {
            wsl: PathBuf::from("wsl.exe"),
            distro: "Ubuntu".to_string(),
        };
        searxng::save_owned(&fixture.state, &owned).unwrap();
        let quiet = FakeHost::new(|_| failed("must not run"));
        let engine = readiness(&config, Some(&fixture.state), &quiet)[0]
            .engine
            .clone()
            .unwrap();
        assert!(engine.container.unwrap().contains("inside WSL"));
        // The trap: no command at all, so wsl.exe never ran.
        assert!(quiet.commands().is_empty());

        // Docker that fails with endpoint details in its error: fixed words only.
        let mut owned = searxng::load_owned(&fixture.state).unwrap().unwrap();
        owned.route = searxng::Route::Path {
            program: PathBuf::from("/usr/bin/docker"),
        };
        searxng::save_owned(&fixture.state, &owned).unwrap();
        let leaky = FakeHost::new(|_| failed("tcp://user:hunter2@remote:2376 refused"));
        let engine = readiness(&config, Some(&fixture.state), &leaky)[0]
            .engine
            .clone()
            .unwrap();
        let found = readiness(&config, Some(&fixture.state), &leaky);
        let mut text = String::new();
        crate::search_readiness::render(&mut text, &found);
        let json = serde_json::to_string(&found).unwrap();
        for shown in [format!("{engine:?}"), text, json] {
            assert!(
                !shown.contains("hunter2") && !shown.contains("tcp://"),
                "{shown}"
            );
        }

        // No record, then an unreadable one: said so, nothing claimed.
        std::fs::remove_file(fixture.state.join("searxng").join("state.json")).unwrap();
        let engine = readiness(&config, Some(&fixture.state), &quiet)[0]
            .engine
            .clone()
            .unwrap();
        assert_eq!(engine.record, "missing");
        assert_eq!(engine.image_current, None);
        std::fs::write(
            fixture.state.join("searxng").join("state.json"),
            "{ not json",
        )
        .unwrap();
        let engine = readiness(&config, Some(&fixture.state), &quiet)[0]
            .engine
            .clone()
            .unwrap();
        assert_eq!(engine.record, "unreadable");
        assert_eq!(engine.table_matches_record, None);
    }

    #[test]
    fn doctor_readiness_order_follows_precedence_and_the_web_switch() {
        let fixture = fixture(searxng::IMAGE);
        let host = docker(&fixture.containers);
        // A hand-designated search tool comes first.
        let text = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            format!(
                "{text}\n[mcp.servers.finder]\ncommand = \"finder\"\n\n[research.mcp]\ntools = [{{ server = \"finder\", tool = \"search\" }}]\n"
            ),
        )
        .unwrap();
        let config = effective(&fixture.config).unwrap();
        let found = readiness(&config, Some(&fixture.state), &host);
        assert_eq!(found[0].order, Some(2));
        let mut rendered = String::new();
        crate::search_readiness::render(&mut rendered, &found);
        assert!(
            rendered.contains("search candidate 2nd for /research and chat web_search"),
            "{rendered}"
        );
        let json = serde_json::to_string(&found).unwrap();
        assert!(json.contains(r#""order":2"#), "{json}");
        // Chat web access off: still a /research candidate, not a chat one.
        let mut chat_off = config.clone();
        chat_off.tools.web = localpilot_config::ChatWebMode::Off;
        let found = readiness(&chat_off, Some(&fixture.state), &host);
        assert_eq!(found[0].order, Some(2));
        assert!(!found[0].chat_web_search);
        let mut rendered = String::new();
        crate::search_readiness::render(&mut rendered, &found);
        assert!(
            rendered.contains("/research only (chat web access is off)"),
            "{rendered}"
        );
        assert!(serde_json::to_string(&found)
            .unwrap()
            .contains(r#""chat_web_search":false"#));
        // Web research off: not a candidate at all.
        let mut off = config.clone();
        off.research.web.enabled = false;
        let found = readiness(&off, Some(&fixture.state), &host);
        assert_eq!(found[0].order, None);
        let mut rendered = String::new();
        crate::search_readiness::render(&mut rendered, &found);
        assert!(
            rendered.contains("not a search candidate while web research is off"),
            "{rendered}"
        );
        assert!(serde_json::to_string(&found)
            .unwrap()
            .contains(r#""order":null"#));
    }

    #[tokio::test]
    async fn status_only_reads() {
        let fixture = fixture(OLD_IMAGE);
        let host = docker(&fixture.containers);
        let (result, said) = act(&fixture, &host, Action::Status, false, true).await;
        result.unwrap();
        for needed in [
            "127.0.0.1:8888",
            "running; answering",
            "localpilot search update moves it",
            "[search.managed.searxng] uses it",
            "duckduckgo, wikipedia",
        ] {
            assert!(said.contains(needed), "{needed}: {said}");
        }
        assert!(!host.commands().iter().any(|c| c.contains(" start ")
            || c.contains(" stop ")
            || c.contains(" rm ")
            || c.contains(" create ")));
        assert!(!fixture.state.join("setup-audit.log").exists());
    }

    #[tokio::test]
    async fn stop_and_start_act_only_on_the_recorded_container() {
        let fixture = fixture(searxng::IMAGE);
        let host = docker(&fixture.containers);
        let (result, said) = act(&fixture, &host, Action::Stop, true, true).await;
        result.unwrap();
        assert!(
            said.contains("--host unix:///var/run/docker.sock stop"),
            "{said}"
        );
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| !running));
        let (result, _) = act(&fixture, &host, Action::Start, true, true).await;
        result.unwrap();
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| *running));
        let audit = std::fs::read_to_string(fixture.state.join("setup-audit.log")).unwrap();
        assert!(
            audit.contains("event=searxng-stop ") && audit.contains("event=searxng-start "),
            "{audit}"
        );

        // Saying no changes nothing.
        let (result, said) = act(&fixture, &host, Action::Stop, false, true).await;
        result.unwrap();
        assert!(said.contains("Nothing was changed."));
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| *running));

        // A container that lost LocalPilot's label is not touched.
        for container in fixture.containers.borrow_mut().values_mut() {
            container.2 = false;
        }
        let (result, _) = act(&fixture, &host, Action::Stop, true, true).await;
        assert!(result
            .unwrap_err()
            .contains("not the one LocalPilot created"));
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| *running));
    }

    #[tokio::test]
    async fn a_moved_docker_endpoint_is_refused() {
        let fixture = fixture(searxng::IMAGE);
        let host = FakeHost::new(|argv| {
            if argv.iter().any(|word| word == "context") {
                ok(r#"{"Host":"unix:///run/user/1000/docker.sock"}"#)
            } else {
                ok("29.6.0")
            }
        });
        for action in [Action::Start, Action::Stop, Action::Update, Action::Remove] {
            let (result, _) = act(&fixture, &host, action, true, true).await;
            assert!(
                result
                    .unwrap_err()
                    .contains("not unix:///var/run/docker.sock"),
                "{action:?}"
            );
        }
        assert!(!host
            .commands()
            .iter()
            .any(|c| c.contains(" stop ") || c.contains(" rm ") || c.contains(" create ")));
    }

    #[tokio::test]
    async fn remove_takes_the_config_entry_then_the_container_then_the_record() {
        let fixture = fixture(searxng::IMAGE);
        let host = docker(&fixture.containers);
        let (result, said) = act(&fixture, &host, Action::Remove, true, true).await;
        result.unwrap();
        assert!(
            said.contains("remove [search.managed.searxng] from your config"),
            "{said}"
        );
        assert!(fixture.containers.borrow().is_empty());
        assert!(port_in_config(&fixture).is_none());
        assert!(std::fs::read_to_string(&fixture.config)
            .unwrap()
            .contains("# mine"));
        assert!(searxng::load_owned(&fixture.state).is_none());
        assert!(!searxng::settings_dir(&fixture.state).exists());
    }

    #[tokio::test]
    async fn remove_leaves_an_entry_changed_after_setup_and_a_foreign_container() {
        let fixture = fixture(searxng::IMAGE);
        let text = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            text.replace("mcp-searxng@2.11.0", "mcp-searxng@9.9.9"),
        )
        .unwrap();
        for container in fixture.containers.borrow_mut().values_mut() {
            container.2 = false;
        }
        let host = docker(&fixture.containers);
        let (result, said) = act(&fixture, &host, Action::Remove, true, true).await;
        assert!(said.contains("leave [search.managed.searxng]"), "{said}");
        assert!(result.unwrap_err().contains("the record was kept"));
        assert_eq!(fixture.containers.borrow().len(), 1);
        assert!(port_in_config(&fixture).is_some());
        assert!(searxng::load_owned(&fixture.state).is_some());
    }

    #[tokio::test]
    async fn update_switches_only_after_the_new_container_answers() {
        let fixture = fixture(OLD_IMAGE);
        let host = docker(&fixture.containers);
        let old = searxng::load_owned(&fixture.state).unwrap().unwrap();
        let (result, said) = act(&fixture, &host, Action::Update, true, true).await;
        result.unwrap();
        assert!(said.contains("-p 127.0.0.1:8889:8080"), "{said}");
        assert!(said.contains("settings-8889:/etc/searxng/"), "{said}");
        assert!(!said.contains("stopped"), "{said}");
        let now = searxng::load_owned(&fixture.state).unwrap().unwrap();
        assert_eq!(now.image, searxng::IMAGE);
        assert_eq!(now.port, 8889);
        assert!(now.settings_dir.ends_with("settings-8889"));
        assert!(now.settings_dir.join("settings.yml").exists());
        assert!(!old.settings_dir.exists());
        assert!(!now.unfinished());
        assert_eq!(port_in_config(&fixture), Some(8889));
        let held = fixture.containers.borrow();
        assert!(!held.contains_key(&old.container_id));
        assert_eq!(held.len(), 1);
        let audit = std::fs::read_to_string(fixture.state.join("setup-audit.log")).unwrap();
        let switch = audit.find("event=searxng-switch ").unwrap();
        let removal = audit.rfind("event=searxng-remove ").unwrap();
        assert!(switch < removal, "{audit}");
    }

    #[tokio::test]
    async fn a_failed_update_keeps_the_old_container_and_config() {
        let fixture = fixture(OLD_IMAGE);
        let host = docker(&fixture.containers);
        let old = searxng::load_owned(&fixture.state).unwrap().unwrap();
        let before = std::fs::read_to_string(&fixture.config).unwrap();
        // The bridge check fails against the new container.
        let (result, _) = act(&fixture, &host, Action::Update, true, false).await;
        assert!(result.unwrap_err().contains("SearXNG is as it was"));
        assert_eq!(searxng::load_owned(&fixture.state).unwrap().unwrap(), old);
        assert!(old.settings_dir.exists());
        assert!(!old.settings_dir.with_file_name("settings-8889").exists());
        assert_eq!(std::fs::read_to_string(&fixture.config).unwrap(), before);
        let held = fixture.containers.borrow();
        assert_eq!(held.len(), 1);
        assert!(held.contains_key(&old.container_id));
    }

    /// Record a staged container on 8890 with its own settings copy; with
    /// switched, the config already points at it.
    fn stage(fixture: &Fixture, id: &str, switched: bool) -> Owned {
        let mut owned = searxng::load_owned(&fixture.state).unwrap().unwrap();
        fixture
            .containers
            .borrow_mut()
            .insert(id.to_string(), (8890, true, true));
        let copy = owned.settings_dir.with_file_name("settings-8890");
        searxng::copy_settings(&owned.settings_dir, &copy).unwrap();
        let next = moved(owned.entry.as_ref().unwrap(), 8890);
        owned.pending = Some(Staged {
            container_id: id.to_string(),
            container_name: "new".to_string(),
            image: searxng::IMAGE.to_string(),
            port: 8890,
            settings_dir: copy,
            entry: Some(next.clone()),
        });
        searxng::save_owned(&fixture.state, &owned).unwrap();
        if switched {
            let text = std::fs::read_to_string(&fixture.config).unwrap();
            let mut document: toml_edit::DocumentMut = text.parse().unwrap();
            crate::search_setup::place(&mut document, ID, &next).unwrap();
            std::fs::write(&fixture.config, document.to_string()).unwrap();
        }
        owned
    }

    fn ids(fixture: &Fixture) -> Vec<String> {
        fixture.containers.borrow().keys().cloned().collect()
    }

    #[tokio::test]
    async fn an_update_interrupted_around_the_switch_is_finished_or_undone_when_confirmed() {
        // Interrupted after the config switched: the staged container becomes
        // the instance, the old one and its settings go.
        let first = fixture(OLD_IMAGE);
        let host = docker(&first.containers);
        let staged_id = format!("{:064x}", 0x2222);
        let before = stage(&first, &staged_id, true);
        // A search refuses until it is resolved.
        assert!(
            searxng::ensure_running(&host, &first.state, 8890, &|_, _| true)
                .unwrap_err()
                .contains("did not finish")
        );
        // Saying no changes nothing.
        let (result, said) = act(&first, &host, Action::Update, false, true).await;
        result.unwrap();
        assert!(said.contains("stopped after the config switched"), "{said}");
        assert!(said.contains("rm --force"), "{said}");
        assert!(
            said.contains(&format!("delete {}", before.settings_dir.display())),
            "{said}"
        );
        assert!(said.contains("Nothing was changed."), "{said}");
        assert_eq!(searxng::load_owned(&first.state).unwrap().unwrap(), before);
        assert_eq!(ids(&first).len(), 2);
        // Saying yes finishes it, and does no new update.
        let (result, said) = act(&first, &host, Action::Update, true, true).await;
        result.unwrap();
        let now = searxng::load_owned(&first.state).unwrap().unwrap();
        assert_eq!(now.container_id, staged_id);
        assert!(now.settings_dir.ends_with("settings-8890"));
        assert!(!now.unfinished());
        assert_eq!(ids(&first), vec![staged_id]);
        assert!(!before.settings_dir.exists(), "{said}");
        assert!(now.settings_dir.exists());
        assert_eq!(port_in_config(&first), Some(8890));
        let audit = std::fs::read_to_string(first.state.join("setup-audit.log")).unwrap();
        assert!(
            audit.contains("event=searxng-recover ") && audit.contains("way=finish"),
            "{audit}"
        );
        assert!(!audit.contains("event=searxng-create"), "{audit}");

        // Interrupted before the switch: the staged container and its copy
        // go, the old one stays.
        let second = fixture(OLD_IMAGE);
        let host = docker(&second.containers);
        let staged_id = format!("{:064x}", 0x3333);
        let before = stage(&second, &staged_id, false);
        let (result, said) = act(&second, &host, Action::Update, true, true).await;
        result.unwrap();
        assert!(
            said.contains("stopped before the config switched"),
            "{said}"
        );
        let now = searxng::load_owned(&second.state).unwrap().unwrap();
        assert_eq!(now.container_id, before.container_id);
        assert!(!now.unfinished());
        assert_eq!(ids(&second), vec![before.container_id.clone()]);
        assert!(!before.pending.unwrap().settings_dir.exists());
        assert!(before.settings_dir.exists());
        assert_eq!(port_in_config(&second), Some(8888));
    }

    /// A console that edits the config by hand while the question is open,
    /// then says yes.
    struct EditWhileAsked(RefCell<Vec<String>>, PathBuf);

    impl SetupConsole for EditWhileAsked {
        fn say(&self, line: &str) {
            self.0.borrow_mut().push(line.to_string());
        }
        fn pick(&self, _prompt: &str, _options: &[String]) -> crate::search_setup::Pick {
            crate::search_setup::Pick::Cancel
        }
        fn ask(&self, _prompt: &str) -> Answer {
            let text = std::fs::read_to_string(&self.1).unwrap();
            std::fs::write(
                &self.1,
                text.replace("call_timeout_secs = 5", "call_timeout_secs = 9"),
            )
            .unwrap();
            Answer::Yes
        }
        fn secret(&self, _prompt: &str) -> Option<localpilot_core::Secret> {
            None
        }
    }

    #[tokio::test]
    async fn an_edit_while_recovery_asks_stops_every_recovery_way() {
        for way in ["finish", "undo", "clear"] {
            let fixture = fixture(OLD_IMAGE);
            let host = docker(&fixture.containers);
            let mut owned = stage(&fixture, &format!("{:064x}", 0x5555), way == "finish");
            if way == "clear" {
                owned.retired = owned.pending.take();
                searxng::save_owned(&fixture.state, &owned).unwrap();
            }
            let console = EditWhileAsked(RefCell::new(Vec::new()), fixture.config.clone());
            let config = fixture.config.clone();
            let read = move || effective(&config);
            let ctx = Ctx {
                console: &console,
                host: &host,
                state_dir: fixture.state.clone(),
                effective: &read,
                config_path: Some(fixture.config.clone()),
                write: &real_write,
                launcher: launcher(true),
            };
            let result = run(&ctx, Action::Update).await;
            assert!(result.unwrap_err().contains("nothing was changed"), "{way}");
            assert_eq!(
                searxng::load_owned(&fixture.state).unwrap().unwrap(),
                owned,
                "{way}"
            );
            assert_eq!(ids(&fixture).len(), 2, "{way}");
            let staged = owned.pending.as_ref().or(owned.retired.as_ref()).unwrap();
            assert!(staged.settings_dir.exists(), "{way}");
            assert!(owned.settings_dir.exists(), "{way}");
        }
    }

    #[tokio::test]
    async fn an_unfinished_update_with_an_unknown_entry_is_left_alone() {
        let fixture = fixture(OLD_IMAGE);
        let host = docker(&fixture.containers);
        let before = stage(&fixture, &format!("{:064x}", 0x4444), true);
        // The entry was changed by hand: neither the old nor the new one.
        let text = std::fs::read_to_string(&fixture.config).unwrap();
        std::fs::write(
            &fixture.config,
            text.replace("call_timeout_secs = 5", "call_timeout_secs = 6"),
        )
        .unwrap();
        let after_edit = std::fs::read_to_string(&fixture.config).unwrap();
        assert_ne!(after_edit, text);
        let (result, _) = act(&fixture, &host, Action::Update, true, true).await;
        assert!(result.unwrap_err().contains("nothing was changed"));
        assert_eq!(
            searxng::load_owned(&fixture.state).unwrap().unwrap(),
            before
        );
        assert_eq!(ids(&fixture).len(), 2);
        assert_eq!(
            std::fs::read_to_string(&fixture.config).unwrap(),
            after_edit
        );
        // An unreadable config is just as unsure.
        std::fs::write(&fixture.config, "not [ toml").unwrap();
        let (result, _) = act(&fixture, &host, Action::Update, true, true).await;
        assert!(result.unwrap_err().contains("nothing was changed"));
        assert_eq!(ids(&fixture).len(), 2);
    }

    /// A config writer that edits the file by hand just before it takes the
    /// lock, as a person could between the preview and the write.
    fn hand_edit_then_write(
        path: Option<&std::path::Path>,
        edit: &dyn Fn(&mut toml_edit::DocumentMut) -> Result<(), String>,
    ) -> Result<ConfigEdit, ConfigError> {
        let file = path.unwrap();
        let text = std::fs::read_to_string(file).unwrap();
        std::fs::write(
            file,
            text.replace("call_timeout_secs = 5", "call_timeout_secs = 7"),
        )
        .unwrap();
        real_write(path, edit)
    }

    async fn act_with_writer(
        fixture: &Fixture,
        host: &FakeHost,
        action: Action,
    ) -> (Result<(), String>, String) {
        let console = Yes(RefCell::new(Vec::new()), true);
        let config = fixture.config.clone();
        let read = move || effective(&config);
        let ctx = Ctx {
            console: &console,
            host,
            state_dir: fixture.state.clone(),
            effective: &read,
            config_path: Some(fixture.config.clone()),
            write: &hand_edit_then_write,
            launcher: launcher(true),
        };
        let result = run(&ctx, action).await;
        let said = console.0.borrow().join("\n");
        (result, said)
    }

    #[tokio::test]
    async fn a_hand_edit_after_the_preview_survives_remove_and_update() {
        let fixture = fixture(OLD_IMAGE);
        let host = docker(&fixture.containers);
        let old = searxng::load_owned(&fixture.state).unwrap().unwrap();
        let (result, _) = act_with_writer(&fixture, &host, Action::Update).await;
        let reason = result.unwrap_err();
        assert!(reason.contains("left as it is"), "{reason}");
        assert!(reason.contains("SearXNG is as it was"), "{reason}");
        assert_eq!(searxng::load_owned(&fixture.state).unwrap().unwrap(), old);
        assert_eq!(ids(&fixture), vec![old.container_id.clone()]);
        assert!(!old.settings_dir.with_file_name("settings-8889").exists());
        let text = std::fs::read_to_string(&fixture.config).unwrap();
        assert!(text.contains("call_timeout_secs = 7"), "{text}");
        assert_eq!(port_in_config(&fixture), Some(8888));

        // Put the entry back, then remove with another hand edit on the way.
        std::fs::write(
            &fixture.config,
            text.replace("call_timeout_secs = 7", "call_timeout_secs = 5"),
        )
        .unwrap();
        let (result, _) = act_with_writer(&fixture, &host, Action::Remove).await;
        assert!(result.unwrap_err().contains("nothing else was removed"));
        assert_eq!(ids(&fixture), vec![old.container_id.clone()]);
        assert!(searxng::load_owned(&fixture.state).is_some());
        assert!(std::fs::read_to_string(&fixture.config)
            .unwrap()
            .contains("call_timeout_secs = 7"));
    }

    #[tokio::test]
    async fn the_first_search_starts_a_stopped_instance_it_owns() {
        let fixture = fixture(searxng::IMAGE);
        let host = docker(&fixture.containers);
        for container in fixture.containers.borrow_mut().values_mut() {
            container.1 = false;
        }
        let events = RefCell::new(Vec::new());
        searxng::ensure_running(&host, &fixture.state, 8888, &|event, _| {
            events.borrow_mut().push(event.to_string());
            true
        })
        .unwrap();
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| *running));
        assert_eq!(
            *events.borrow(),
            vec![
                "searxng-start".to_string(),
                "searxng-start-result".to_string()
            ]
        );
        // Not recorded, not started.
        for container in fixture.containers.borrow_mut().values_mut() {
            container.1 = false;
        }
        assert!(searxng::ensure_running(&host, &fixture.state, 8888, &|_, _| false).is_err());
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| !running));
        // A port the record does not name is refused.
        assert!(searxng::ensure_running(&host, &fixture.state, 9999, &|_, _| true).is_err());
        // Held lock: refused, nothing started.
        let held = searxng::lock(&fixture.state).unwrap();
        assert!(
            searxng::ensure_running(&host, &fixture.state, 8888, &|_, _| true)
                .unwrap_err()
                .contains("another SearXNG")
        );
        drop(held);
        assert!(fixture
            .containers
            .borrow()
            .values()
            .all(|(_, running, _)| !running));
    }
}
