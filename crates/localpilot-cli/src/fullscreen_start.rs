//! The start choice behind the `/remote` actions "Start server and share"
//! and "Start server and reconnect".
//!
//! It opens only from that deliberate action, never on its own. It steps
//! aside while a permission request or question is open. What it may do is
//! decided against the sharing intent it was opened for: once that intent is
//! no longer current (stopped, shared again, another session, ownership
//! lost), a result only says what happened to the server. The start itself
//! is owned by `remote_server_start` and outlives the choice: cancelling
//! never stops a server, and a start that finishes after it was cancelled is
//! reported once.

use std::cell::RefCell;

use localpilot_terminal_ui::{AppModel, RuntimeUpdate};

use crate::remote_server_start::{Outcome, ServerStart};
use crate::remote_share::{IntentTicket, RemoteShare, ShareState};

/// What a ready server is for.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Then {
    Share,
    Reconnect,
}

/// The choice's actions, by what they do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum Act {
    Start,
    Retry,
    Cancel,
}

impl Act {
    fn label(self, then: Then) -> &'static str {
        match (self, then) {
            (Self::Start, Then::Share) => "Start server and share",
            (Self::Start, Then::Reconnect) => "Start server and reconnect",
            (Self::Retry, _) => "Retry",
            (Self::Cancel, _) => "Cancel",
        }
    }
}

#[derive(Debug, Clone, PartialEq, Eq)]
enum Stage {
    Offer,
    /// An earlier start is still running; this one begins when it ends (a
    /// fresh probe for this intent).
    Waiting,
    Starting(u64),
    Failed(String),
    NotYet,
}

struct Open {
    ticket: IntentTicket,
    then: Then,
    stage: Stage,
    acts: Vec<Act>,
}

/// What a key did to the start choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum StartKey {
    /// Still open.
    Open,
    /// Closed.
    Closed,
}

pub(super) struct StartHost {
    server: ServerStart,
    open: RefCell<Option<Open>>,
    /// Starts whose choice was cancelled or retired: each is reported once
    /// when it ends.
    orphans: RefCell<Vec<u64>>,
}

const TITLE: &str = "Start background server?";

/// The start may only start a server while `ticket` is still the intent.
fn authorized(
    share: &std::sync::Arc<RemoteShare>,
    ticket: &IntentTicket,
) -> crate::remote_server_start::Authorized {
    let share = share.clone();
    let ticket = ticket.clone();
    std::sync::Arc::new(move || share.intent_current(&ticket))
}

fn body(stage: &Stage) -> Vec<String> {
    let mut lines = vec![
        "Sharing needs localpilot serve for this workspace.".to_string(),
        "Your session keeps running in this terminal.".to_string(),
        "Starting the server does not start the gateway.".to_string(),
    ];
    match stage {
        Stage::Offer => {}
        Stage::Waiting | Stage::Starting(_) => {
            lines.push(String::new());
            lines.push("Starting background server...".to_string());
        }
        Stage::Failed(reason) => {
            lines.push(String::new());
            lines.push(format!("Could not start the background server. {reason}"));
        }
        Stage::NotYet => {
            lines.push(String::new());
            lines.push(
                "The background server has not answered yet; it may still be starting.".to_string(),
            );
        }
    }
    lines
}

fn acts(stage: &Stage) -> Vec<Act> {
    match stage {
        Stage::Offer => vec![Act::Start, Act::Cancel],
        Stage::Waiting | Stage::Starting(_) => vec![Act::Cancel],
        Stage::Failed(_) | Stage::NotYet => vec![Act::Retry, Act::Cancel],
    }
}

impl StartHost {
    pub(super) fn new(server: ServerStart) -> Self {
        Self {
            server,
            open: RefCell::new(None),
            orphans: RefCell::new(Vec::new()),
        }
    }

    pub(super) fn is_open(&self) -> bool {
        self.open.borrow().is_some()
    }

    /// Open the choice for the current intent, if `then` fits the sharing
    /// state now. Returns why not otherwise.
    pub(super) fn open(
        &self,
        app: &mut AppModel,
        share: &std::sync::Arc<RemoteShare>,
        then: Then,
    ) -> Result<(), &'static str> {
        if app.choice().is_some() || self.is_open() {
            return Err("Another choice is open.");
        }
        let fits = match then {
            Then::Share => share.server_needed(),
            Then::Reconnect => share.state() == ShareState::ServerStopped,
        };
        if !fits {
            return Err("Sharing no longer needs the background server started.");
        }
        let Some(ticket) = share.ticket() else {
            return Err("The session has not started yet.");
        };
        let stage = Stage::Offer;
        let acts = acts(&stage);
        app.open_yielding_choice(
            TITLE.to_string(),
            body(&stage),
            acts.iter().map(|act| act.label(then).to_string()).collect(),
        );
        *self.open.borrow_mut() = Some(Open {
            ticket,
            then,
            stage,
            acts,
        });
        Ok(())
    }

    /// Redraw the open choice for its stage, keeping the selected action
    /// (or selecting `select` when the stage just changed to one with it).
    fn redraw(app: &mut AppModel, open: &mut Open, select: Option<Act>) {
        open.acts = acts(&open.stage);
        app.update_choice(
            body(&open.stage),
            open.acts
                .iter()
                .map(|act| act.label(open.then).to_string())
                .collect(),
        );
        if let Some(act) = select {
            if open.acts.contains(&act) {
                app.select_choice(act.label(open.then));
            }
        }
    }

    /// Enter or Esc on the start choice (the caller routes keys here only
    /// while it is the choice shown).
    pub(super) fn key(
        &self,
        app: &mut AppModel,
        share: &std::sync::Arc<RemoteShare>,
        enter: bool,
    ) -> StartKey {
        let selected = app.choice().map(|choice| choice.selected);
        let mut slot = self.open.borrow_mut();
        let Some(open) = slot.as_mut() else {
            return StartKey::Closed;
        };
        let act = if enter {
            selected
                .and_then(|index| open.acts.get(index).copied())
                .unwrap_or(Act::Cancel)
        } else {
            Act::Cancel
        };
        match act {
            Act::Cancel => {
                let open = slot.take();
                drop(slot);
                self.cancel(app, share, open);
                StartKey::Closed
            }
            Act::Start | Act::Retry => {
                if !share.intent_current(&open.ticket) {
                    let open = slot.take();
                    drop(slot);
                    self.retire(app, open);
                    app.apply_runtime(RuntimeUpdate::Notice(
                        "Sharing changed in the meantime; nothing was started.".to_string(),
                    ));
                    return StartKey::Closed;
                }
                open.stage = match self.server.begin(authorized(share, &open.ticket)) {
                    Some(attempt) => Stage::Starting(attempt),
                    None => Stage::Waiting,
                };
                Self::redraw(app, open, None);
                StartKey::Open
            }
        }
    }

    /// Cancel: the intent is given up (never shared later); a start already
    /// running carries on and is reported when it ends.
    fn cancel(&self, app: &mut AppModel, share: &std::sync::Arc<RemoteShare>, open: Option<Open>) {
        let Some(open) = open else {
            return;
        };
        // Cancelling a start for a reconnect ends that share: it would
        // otherwise go on waiting for a server nobody is starting.
        let stopped = share.intent_current(&open.ticket) && {
            match open.then {
                Then::Share => {
                    share.cancel_intent();
                    false
                }
                Then::Reconnect => {
                    share.stop();
                    true
                }
            }
        };
        self.retire(app, Some(open));
        let text = if stopped {
            "Sharing stopped. Work continues in this terminal."
        } else {
            "Sharing cancelled. Work continues in this terminal."
        };
        app.apply_runtime(RuntimeUpdate::Notice(text.to_string()));
    }

    /// Close the choice; keep watching its start, if one runs.
    fn retire(&self, app: &mut AppModel, open: Option<Open>) {
        app.close_choice();
        if let Some(Open {
            stage: Stage::Starting(attempt),
            ..
        }) = open
        {
            self.orphans.borrow_mut().push(attempt);
        }
    }

    /// Every tick: report retired starts that ended, keep the open choice
    /// current, and return what to do when its server is ready for an intent
    /// that is still current. `ours` says whether the choice shown is this
    /// one (it is closed here if something else replaced it).
    pub(super) fn tick(
        &self,
        app: &mut AppModel,
        share: &std::sync::Arc<RemoteShare>,
        ours: bool,
    ) -> Option<Then> {
        self.orphans.borrow_mut().retain(|attempt| {
            let Some(outcome) = self.server.outcome(*attempt) else {
                return true;
            };
            let text = match outcome {
                Outcome::Ready { .. } => "Sharing cancelled. The background server is running.",
                Outcome::NotYet => {
                    "Sharing cancelled. The background server may still be starting."
                }
                Outcome::Failed(_) => "Sharing cancelled. The background server did not start.",
            };
            app.apply_runtime(RuntimeUpdate::Notice(text.to_string()));
            false
        });
        let mut slot = self.open.borrow_mut();
        let open = slot.as_mut()?;
        // Replaced by another choice, or the intent moved on: retire it.
        if !ours || !share.intent_current(&open.ticket) {
            let open = slot.take();
            drop(slot);
            if ours {
                app.close_choice();
            } else if let Some(open) = &open {
                // Replaced on screen: that cancels it, as Cancel would, so a
                // probe still out can no longer start a server for it.
                if share.intent_current(&open.ticket) {
                    match open.then {
                        Then::Share => share.cancel_intent(),
                        Then::Reconnect => {
                            share.stop();
                        }
                    }
                }
            }
            if let Some(Open {
                stage: Stage::Starting(attempt),
                ..
            }) = open
            {
                self.orphans.borrow_mut().push(attempt);
            }
            app.apply_runtime(RuntimeUpdate::Notice(
                "Sharing cancelled. Work continues in this terminal.".to_string(),
            ));
            return None;
        }
        match open.stage.clone() {
            Stage::Waiting => {
                if let Some(attempt) = self.server.begin(authorized(share, &open.ticket)) {
                    open.stage = Stage::Starting(attempt);
                    Self::redraw(app, open, None);
                }
                None
            }
            Stage::Starting(attempt) => match self.server.outcome(attempt)? {
                Outcome::Ready { started } => {
                    let then = open.then;
                    slot.take();
                    drop(slot);
                    app.close_choice();
                    if started {
                        app.apply_runtime(RuntimeUpdate::Notice(
                            "Background server started.".to_string(),
                        ));
                    }
                    Some(then)
                }
                Outcome::Failed(reason) => {
                    open.stage = Stage::Failed(reason);
                    Self::redraw(app, open, Some(Act::Retry));
                    None
                }
                Outcome::NotYet => {
                    open.stage = Stage::NotYet;
                    Self::redraw(app, open, Some(Act::Retry));
                    None
                }
            },
            Stage::Offer | Stage::Failed(_) | Stage::NotYet => None,
        }
    }
}
