//! Pairing a device from the full-screen terminal: the setup form, the view
//! that shows the code, and the attempt behind them (see `remote_pairing`).
//!
//! The view is transient. Its code is taken from the attempt only while the
//! view shows it, and wiped from the view as soon as the attempt ends, its
//! deadline passes, the view closes or the session changes. A permission
//! request or question suspends the view (it stays, hidden); closing the view
//! while its code is live invalidates the code. An attempt whose ending is not
//! confirmed outlives its view, so `/remote pair` reopens it instead of making
//! a second code beside it.

use std::cell::{Cell, RefCell};
use std::sync::Arc;
use std::time::Instant;

use crossterm::event::{KeyCode, KeyEvent, KeyModifiers};
use localpilot_core::SessionId;
use localpilot_rpc::{PairingOutcome, Scope};
use localpilot_terminal_ui::{
    AppModel, PairingField, PairingSecret, PairingSetup, PairingView, RuntimeUpdate,
};

use crate::remote_pairing::{Attempt, Ended, Grant, Issuer, Phase, Prerequisites};
use crate::remote_share::Dial;

/// Puts the link on the clipboard; `Err` says why it could not (never the
/// link).
pub(super) type Copier = Arc<dyn Fn(&str) -> Result<(), String> + Send + Sync>;

/// The system clipboard.
fn system_copier() -> Copier {
    Arc::new(|text: &str| {
        arboard::Clipboard::new()
            .and_then(|mut clipboard| clipboard.set_text(text.to_string()))
            .map_err(|error| error.to_string())
    })
}

const TITLE: &str = "Pair a device";
const SHOW_LINK: &str = "Show link";
const SHOW_QR: &str = "Show QR code";
const COPY: &str = "Copy link";
const CLOSE: &str = "Close";
const CANCEL: &str = "Cancel";
const HIDE: &str = "Hide";
const RETRY: &str = "Retry invalidation";
const CHECK: &str = "Check status";
const AGAIN: &str = "Create a new code";
const SHARE: &str = "Share this session";

/// What a key in the pairing overlay asks the terminal to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum PairingKey {
    /// Nothing beyond the overlay.
    Handled,
    /// Share this session (offered after a device paired).
    Share,
}

/// The host's pairing state.
pub(super) struct PairingHost {
    issuer: Issuer,
    dial: Dial,
    prerequisites: Prerequisites,
    copier: Copier,
    /// The attempt, kept while its view shows it or while its ending is not
    /// confirmed.
    attempt: RefCell<Option<Attempt>>,
    /// The session the overlay was opened for.
    session: Cell<Option<SessionId>>,
    /// The last ending (or unconfirmed state) the user was actually told:
    /// on screen in the view, or as a notice. A different one is told anew,
    /// so a warning never stands in for the outcome that follows it.
    told: RefCell<Option<Phase>>,
    /// A short note in the view (a copy that worked or failed).
    note: RefCell<Option<String>>,
}

impl PairingHost {
    pub(super) fn new(dial: Dial, prerequisites: Prerequisites) -> Self {
        Self {
            issuer: Issuer::new(),
            dial,
            prerequisites,
            copier: system_copier(),
            attempt: RefCell::new(None),
            session: Cell::new(None),
            told: RefCell::new(None),
            note: RefCell::new(None),
        }
    }

    #[cfg(test)]
    pub(super) fn with_copier(mut self, copier: Copier) -> Self {
        self.copier = copier;
        self
    }

    /// Whether the kept attempt has a Retry or Check running.
    #[cfg(test)]
    pub(super) fn settling_for_test(&self) -> bool {
        self.attempt
            .borrow()
            .as_ref()
            .is_some_and(Attempt::settling)
    }

    /// The kept attempt's phase, if one is kept.
    #[cfg(test)]
    pub(super) fn phase(&self) -> Option<Phase> {
        self.attempt.borrow().as_ref().map(Attempt::phase)
    }

    /// `/remote pair`: reopen an attempt not yet settled, or open the form.
    pub(super) fn open(&self, app: &mut AppModel, observe: bool, session: Option<SessionId>) {
        self.session.set(session);
        let unresolved = self
            .attempt
            .borrow()
            .as_ref()
            .is_some_and(Attempt::unresolved);
        if unresolved {
            self.open_view(app);
            return;
        }
        self.attempt.borrow_mut().take();
        app.open_pairing_setup(PairingSetup::new(!observe));
    }

    fn open_view(&self, app: &mut AppModel) {
        let show_link = app.capabilities.screen_reader;
        app.open_pairing_view(PairingView::new(
            TITLE.to_string(),
            Vec::new(),
            Vec::new(),
            show_link,
        ));
        self.note.borrow_mut().take();
        self.refresh(app, false);
    }

    /// Start an attempt for the form's grant.
    fn create(&self, app: &mut AppModel) {
        let Some(setup) = app.pairing_setup() else {
            return;
        };
        let name = match setup.device_name() {
            Ok(name) => name,
            Err(problem) => {
                if let Some(setup) = app.pairing_setup_mut() {
                    setup.problem = Some(problem);
                }
                return;
            }
        };
        let grant = Grant {
            name,
            scope: if setup.control {
                Scope::Control
            } else {
                Scope::Observe
            },
            this_workspace_only: setup.this_workspace_only,
        };
        let Some(attempt) = Attempt::start(
            &self.issuer,
            self.dial.clone(),
            self.prerequisites.clone(),
            grant,
        ) else {
            if let Some(setup) = app.pairing_setup_mut() {
                setup.problem =
                    Some("This terminal cannot make more codes; restart it to pair.".to_string());
            }
            return;
        };
        // Any earlier attempt is replaced, and ends with its drop.
        *self.attempt.borrow_mut() = Some(attempt);
        self.told.borrow_mut().take();
        self.open_view(app);
    }

    /// Route a key to the open pairing overlay.
    pub(super) fn key(&self, app: &mut AppModel, key: KeyEvent, shared: bool) -> PairingKey {
        // A window too small to show the controls acts only on Esc.
        let visible = app
            .pairing_setup()
            .map(PairingSetup::controls_visible)
            .or_else(|| app.pairing_view().map(PairingView::controls_visible))
            .unwrap_or(true);
        if !visible && key.code != KeyCode::Esc {
            return PairingKey::Handled;
        }
        if app.pairing_setup().is_some() {
            self.setup_key(app, key);
            return PairingKey::Handled;
        }
        self.view_key(app, key, shared)
    }

    fn setup_key(&self, app: &mut AppModel, key: KeyEvent) {
        let Some(setup) = app.pairing_setup_mut() else {
            return;
        };
        match key.code {
            KeyCode::Esc => app.close_pairing_setup(),
            KeyCode::Tab | KeyCode::Down => setup.move_focus(true),
            KeyCode::BackTab | KeyCode::Up => setup.move_focus(false),
            KeyCode::Left | KeyCode::Right if setup.focus == PairingField::Name => {
                setup.move_cursor(key.code == KeyCode::Right);
            }
            KeyCode::Left | KeyCode::Right => {
                setup.toggle();
            }
            KeyCode::Home | KeyCode::End => {
                setup.cursor_to(key.code == KeyCode::End);
            }
            KeyCode::Delete => {
                setup.delete();
            }
            KeyCode::Char(' ') if setup.focus != PairingField::Name => {
                setup.toggle();
            }
            KeyCode::Backspace => {
                setup.backspace();
            }
            KeyCode::Char(character)
                if !key
                    .modifiers
                    .intersects(KeyModifiers::CONTROL | KeyModifiers::ALT) =>
            {
                setup.type_char(character);
            }
            KeyCode::Enter => match setup.focus {
                PairingField::Cancel => app.close_pairing_setup(),
                PairingField::Scope | PairingField::Reach => {
                    setup.toggle();
                }
                // Only Create makes a code; Enter in the name moves on.
                PairingField::Name => setup.move_focus(true),
                PairingField::Create => self.create(app),
            },
            _ => {}
        }
    }

    fn view_key(&self, app: &mut AppModel, key: KeyEvent, shared: bool) -> PairingKey {
        let Some(view) = app.pairing_view_mut() else {
            return PairingKey::Handled;
        };
        match key.code {
            KeyCode::Tab | KeyCode::Down => view.move_action(true),
            KeyCode::BackTab | KeyCode::Up => view.move_action(false),
            KeyCode::PageDown => view.scroll_link(4),
            KeyCode::PageUp => view.scroll_link(-4),
            KeyCode::Esc => self.leave(app),
            KeyCode::Enter => {
                let action = view.selected_action().map(str::to_string);
                return self.act(app, action.as_deref(), shared);
            }
            _ => {}
        }
        PairingKey::Handled
    }

    /// Esc: a live attempt is closed (its code invalidated) and the view
    /// stays to say how that went; otherwise the view goes.
    fn leave(&self, app: &mut AppModel) {
        let live = self
            .attempt
            .borrow()
            .as_ref()
            .is_some_and(|attempt| live(&attempt.phase()));
        if live {
            self.close_attempt(app);
        } else {
            self.hide(app);
        }
    }

    fn act(&self, app: &mut AppModel, action: Option<&str>, shared: bool) -> PairingKey {
        match action {
            Some(SHOW_LINK | SHOW_QR) => {
                if let Some(view) = app.pairing_view_mut() {
                    view.show_link = !view.show_link;
                    view.scroll = 0;
                }
                self.refresh(app, shared);
            }
            Some(COPY) => {
                let link = app
                    .pairing_view()
                    .and_then(PairingView::secret)
                    .map(|secret| secret.link().to_string());
                if let Some(mut link) = link {
                    let note = match (self.copier)(&link) {
                        Ok(()) => "Link copied.".to_string(),
                        Err(why) => format!("Could not copy the link: {why}"),
                    };
                    let mut bytes = std::mem::take(&mut link).into_bytes();
                    bytes.fill(0);
                    *self.note.borrow_mut() = Some(note);
                    self.refresh(app, shared);
                }
            }
            Some(CANCEL) => self.close_attempt(app),
            Some(CLOSE) => self.leave(app),
            Some(HIDE) => self.hide(app),
            Some(RETRY) => {
                if let Some(attempt) = self.attempt.borrow().as_ref() {
                    attempt.retry();
                }
                self.refresh(app, shared);
            }
            Some(CHECK) => {
                if let Some(attempt) = self.attempt.borrow().as_ref() {
                    attempt.check();
                }
                self.refresh(app, shared);
            }
            Some(AGAIN) => {
                let grant = self.attempt.borrow().as_ref().map(|a| a.grant().clone());
                self.attempt.borrow_mut().take();
                let mut setup = PairingSetup::new(true);
                if let Some(grant) = grant {
                    setup = setup.named(&grant.name);
                    setup.control = grant.scope == Scope::Control;
                    setup.this_workspace_only = grant.this_workspace_only;
                }
                app.open_pairing_setup(setup);
            }
            Some(SHARE) => {
                self.hide(app);
                return PairingKey::Share;
            }
            _ => {}
        }
        PairingKey::Handled
    }

    /// Close the attempt: the code is wiped from the view at once and
    /// invalidated with the server.
    fn close_attempt(&self, app: &mut AppModel) {
        if let Some(view) = app.pairing_view_mut() {
            view.wipe_secret();
        }
        if let Some(attempt) = self.attempt.borrow().as_ref() {
            attempt.close();
        }
        self.refresh(app, false);
    }

    /// The view goes. A live attempt is closed with it (its code is never
    /// left working behind a view that is gone); one not yet settled stays
    /// with the host, which keeps saying so.
    fn hide(&self, app: &mut AppModel) {
        app.close_pairing_view();
        let mut slot = self.attempt.borrow_mut();
        let Some(attempt) = slot.as_ref() else {
            return;
        };
        // A live code is never left behind its view; anything else (ended,
        // closing, unconfirmed) keeps its phase and only loses its view.
        if live(&attempt.phase()) {
            attempt.close();
        } else {
            attempt.retire();
        }
        // One reading decides what is told and what is kept: anything not
        // yet told is told now, and nothing is told twice.
        let phase = attempt.phase();
        match &phase {
            Phase::Finished(_) | Phase::Unconfirmed { restarted: true } => {
                self.tell(app, &phase);
                slot.take();
            }
            Phase::Unconfirmed { restarted: false } => {
                // Kept, and said again whenever its view goes.
                self.told.borrow_mut().take();
                self.tell(app, &phase);
            }
            // Still settling: told when it ends.
            _ => {}
        }
    }

    /// Tell `phase` (an ending, or that it could not be confirmed) unless it
    /// is exactly what the user was last told.
    fn tell(&self, app: &mut AppModel, phase: &Phase) {
        if self.told.borrow().as_ref() == Some(phase) {
            return;
        }
        match phase {
            Phase::Finished(ended) => {
                app.apply_runtime(RuntimeUpdate::Notice(ended_text(ended)));
            }
            Phase::Unconfirmed { restarted } => {
                app.apply_runtime(RuntimeUpdate::Warning(unconfirmed_text(*restarted)));
            }
            _ => return,
        }
        *self.told.borrow_mut() = Some(phase.clone());
    }

    /// The status-line note while an attempt is kept without its view.
    pub(super) fn status(&self, app: &AppModel) -> Option<&'static str> {
        if app.pairing_view().is_some() {
            return None;
        }
        match self.attempt.borrow().as_ref().map(Attempt::phase) {
            Some(Phase::Closing) => Some("pairing: invalidating code"),
            Some(Phase::Unconfirmed { restarted: false }) => {
                Some("pairing code unconfirmed · /remote pair")
            }
            _ => None,
        }
    }

    /// Every tick: close the overlay if the session changed, keep the view
    /// current, and tell how a hidden attempt ended.
    pub(super) fn tick(&self, app: &mut AppModel, session: Option<SessionId>, shared: bool) {
        let open = app.pairing_setup().is_some() || app.pairing_view().is_some();
        let why = if open && self.session.get() != session {
            Some("Pairing closed because the session changed.")
        } else if app.pairing_replaced() {
            Some("Pairing closed because another view opened.")
        } else {
            None
        };
        if let Some(why) = why {
            app.close_pairing_setup();
            if app.pairing_view().is_some() {
                app.apply_runtime(RuntimeUpdate::Notice(why.to_string()));
                if let Some(view) = app.pairing_view_mut() {
                    view.wipe_secret();
                }
                self.hide(app);
            }
        }
        // However its view went, a live code never outlives it.
        if app.pairing_view().is_none() {
            if let Some(attempt) = self.attempt.borrow().as_ref() {
                if live(&attempt.phase()) {
                    attempt.close();
                }
            }
        }
        if app.pairing_view().is_some() {
            self.refresh(app, shared);
        } else {
            self.tell_hidden(app);
        }
    }

    /// A hidden attempt that ended (or could not be confirmed) is told, once
    /// per state.
    fn tell_hidden(&self, app: &mut AppModel) {
        let mut slot = self.attempt.borrow_mut();
        let Some(attempt) = slot.as_ref() else {
            return;
        };
        let phase = attempt.phase();
        match &phase {
            Phase::Finished(_) | Phase::Unconfirmed { restarted: true } => {
                self.tell(app, &phase);
                slot.take();
            }
            Phase::Unconfirmed { restarted: false } => self.tell(app, &phase),
            _ => {}
        }
    }

    /// Bring the view up to date with its attempt: take a new code, wipe an
    /// ended or expired one, and say what can be done now.
    fn refresh(&self, app: &mut AppModel, shared: bool) {
        // A replaced view is never handed a code. Only a view actually on
        // screen tells a result: one held under a permission request has not
        // shown it yet.
        let replaced = app.pairing_replaced();
        let shown = app.pairing_shown() && app.pairing_view().is_some();
        let slot = self.attempt.borrow();
        let Some(attempt) = slot.as_ref() else {
            if let Some(view) = app.pairing_view_mut() {
                view.wipe_secret();
            }
            return;
        };
        let phase = attempt.phase();
        let settling = attempt.settling();
        let grant = attempt.grant().clone();
        let code = if replaced { None } else { attempt.take_code() };
        drop(slot);
        let Some(view) = app.pairing_view_mut() else {
            return;
        };
        let now = Instant::now();
        let deadline = match &phase {
            Phase::Active { deadline } if *deadline > now => Some(*deadline),
            _ => None,
        };
        let mut qr_failed = false;
        if let Some(code) = code {
            match (&code.qr, deadline) {
                (_, None) => {}
                (Ok(rows), Some(_)) => view.show(PairingSecret::new(
                    code.link.expose().to_string(),
                    Some(rows.iter().map(|row| row.expose().to_string()).collect()),
                )),
                (Err(_), Some(_)) => {
                    qr_failed = true;
                    view.show_link = true;
                    view.show(PairingSecret::new(code.link.expose().to_string(), None));
                }
            }
        }
        if deadline.is_none() {
            view.wipe_secret();
        }
        if shown && matches!(phase, Phase::Finished(_) | Phase::Unconfirmed { .. }) {
            *self.told.borrow_mut() = Some(phase.clone());
        }
        let qr_available = view.secret().is_some_and(|secret| secret.qr().is_some());
        let (title, mut lines, actions) = describe(
            &phase,
            &grant,
            deadline,
            now,
            settling,
            shared,
            view.show_link,
            qr_available,
        );
        if qr_failed {
            lines.push("The QR code could not be drawn; use the link.".to_string());
        }
        if let Some(note) = self.note.borrow().as_ref() {
            lines.push(note.clone());
        }
        view.update(title, lines, actions);
    }
}

/// Whether an attempt's code may still be live and is not yet closing.
fn live(phase: &Phase) -> bool {
    matches!(
        phase,
        Phase::Checking | Phase::Opening | Phase::Active { .. }
    )
}

/// The view's title, lines and actions for an attempt's phase.
#[allow(clippy::too_many_arguments, clippy::fn_params_excessive_bools)]
fn describe(
    phase: &Phase,
    grant: &Grant,
    deadline: Option<Instant>,
    now: Instant,
    settling: bool,
    shared: bool,
    show_link: bool,
    qr_available: bool,
) -> (String, Vec<String>, Vec<String>) {
    let strings = |items: &[&str]| items.iter().map(|item| (*item).to_string()).collect();
    let title = TITLE.to_string();
    match phase {
        Phase::Checking => (
            title,
            strings(&["Checking remote access..."]),
            strings(&[CANCEL]),
        ),
        Phase::Opening => (
            title,
            strings(&["Creating a pairing code..."]),
            strings(&[CANCEL]),
        ),
        Phase::Active { .. } => {
            let Some(deadline) = deadline else {
                return (
                    title,
                    strings(&["The code expired. Waiting for the server to confirm..."]),
                    strings(&[HIDE]),
                );
            };
            let left = deadline.saturating_duration_since(now).as_secs();
            let reach = if grant.this_workspace_only {
                "this workspace"
            } else {
                "every workspace this server serves"
            };
            let lines = vec![
                if show_link {
                    "Open this link on the phone.".to_string()
                } else {
                    "Scan this QR code with the phone.".to_string()
                },
                format!(
                    "{} gets {} access to {reach}.",
                    grant.name,
                    scope_text(grant.scope)
                ),
                format!(
                    "The code works once, for {}:{:02} more. Closing this view invalidates it.",
                    left / 60,
                    left % 60
                ),
            ];
            let mut actions = Vec::new();
            if qr_available {
                actions.push(if show_link { SHOW_QR } else { SHOW_LINK }.to_string());
            }
            actions.push(COPY.to_string());
            actions.push(CLOSE.to_string());
            (title, lines, actions)
        }
        Phase::Closing => (
            title,
            strings(&["Closing: invalidating the code..."]),
            strings(&[HIDE]),
        ),
        Phase::Unconfirmed { restarted: true } => {
            (title, vec![unconfirmed_text(true)], strings(&[CLOSE]))
        }
        Phase::Unconfirmed { restarted: false } => {
            let mut lines = vec![unconfirmed_text(false)];
            if settling {
                lines.push("Checking with the server...".to_string());
                (title, lines, strings(&[HIDE]))
            } else {
                (title, lines, strings(&[RETRY, CHECK, HIDE]))
            }
        }
        Phase::Finished(ended) => {
            let used = matches!(ended, Ended::Outcome(PairingOutcome::Used { .. }));
            let actions = match (used, shared) {
                (true, false) => strings(&[SHARE, CLOSE]),
                (true, true) => strings(&[CLOSE]),
                (false, _) => strings(&[AGAIN, CLOSE]),
            };
            let title = if used { "Device paired" } else { TITLE };
            (title.to_string(), vec![ended_text(ended)], actions)
        }
    }
}

fn scope_text(scope: Scope) -> &'static str {
    match scope {
        Scope::Control => "control",
        Scope::Observe => "observe",
    }
}

/// How an attempt ended, as the terminal says it.
fn ended_text(ended: &Ended) -> String {
    match ended {
        Ended::NoCode(why) if why == "cancelled" => {
            "Pairing cancelled. No code was created.".to_string()
        }
        Ended::NoCode(why) => format!("No code was created. {why}"),
        Ended::Refused(why) => format!("The server refused to create a code: {why}"),
        Ended::Outcome(outcome) => match outcome {
            PairingOutcome::Used { name, scope, .. } => format!(
                "Paired {} with {} access.",
                localpilot_terminal_ui::sanitize_text(name),
                scope_text(*scope)
            ),
            PairingOutcome::Cancelled => "Pairing closed. Code invalidated.".to_string(),
            PairingOutcome::Expired => "The pairing code expired unused.".to_string(),
            PairingOutcome::Evicted => {
                "The pairing code was replaced by newer codes and no longer works.".to_string()
            }
            PairingOutcome::Withdrawn => {
                "The pairing code was withdrawn after too many wrong attempts.".to_string()
            }
            PairingOutcome::IssuerGone => {
                "The pairing code ended when its connection to the server closed.".to_string()
            }
        },
    }
}

fn unconfirmed_text(restarted: bool) -> String {
    if restarted {
        "The background server restarted, so codes from before the restart no longer work. A phone may already have paired before it restarted: check `localpilot devices list`."
            .to_string()
    } else {
        "Could not confirm the code was invalidated. It may still work until it expires; use /remote pair to retry."
            .to_string()
    }
}
