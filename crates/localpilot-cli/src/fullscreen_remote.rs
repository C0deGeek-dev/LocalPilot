//! The full-screen host's side of sharing a session with paired devices.
//!
//! The terminal stays the session's only owner: device requests wait on the
//! controller's queue (see `remote_share`) and are taken here, between
//! keystrokes, in the idle loop and in every running operation's frame tick.
//! Each is decided immediately before it acts:
//!
//! - a request not yet accepted needs the share, the terminal's ownership and
//!   the device's current standing ([`Ticket::accept_now`]);
//! - an accepted follow-up waiting in the queue needs the device's current
//!   standing when it starts or is promoted into steering
//!   ([`Ticket::may_start`]); it outlives the share stopping.
//!
//! A queued phone prompt carries its ticket as a [`RemoteTag`]. Whatever path
//! the prompt then takes (it starts, is promoted, is removed, or is discarded),
//! the device is told: dropping an unsettled tag reports it as not run.

use std::cell::{Cell, RefCell};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use localpilot_core::SessionId;
use localpilot_harness::SessionRuntime;
use localpilot_rpc::{ClientCommand, InputDisposition, RelayRequestState};
use localpilot_slash::{parse_slash_for, Host, RemoteAction, SlashAction};
use localpilot_terminal_ui::{AppModel, ItemId, RuntimeUpdate};
use tokio_util::sync::CancellationToken;

use crate::relay_host::{SourceRequest, Ticket};
use crate::remote_share::{
    RemoteShare, ShareNotice, ShareOutcome, ShareState, StopOutcome, Tracked,
};

/// Why a device's steering prompt is refused (the terminal does not inject
/// a device's text into its running turn).
const STEER_REFUSED: &str = "steering from a device is not available; send it as a follow-up";

/// Device requests taken per tick (the rest wait for the next tick, so a burst
/// never starves the keyboard).
const REQUESTS_PER_TICK: usize = 8;

/// One device request's authority, carried with its queued prompt.
pub(super) struct RemoteOrigin {
    ticket: Ticket,
    /// The device's name, for the transcript.
    pub(super) device: String,
    settled: AtomicBool,
}

impl RemoteOrigin {
    fn settle(&self, state: RelayRequestState) {
        if !self.settled.swap(true, Ordering::SeqCst) {
            self.ticket.report(state);
        }
    }
}

impl Drop for RemoteOrigin {
    fn drop(&mut self) {
        self.settle(RelayRequestState::Rejected {
            reason: "it was removed before it ran".to_string(),
        });
    }
}

/// A shared handle to a queued request's authority; equal only to itself.
#[derive(Clone)]
pub(super) struct RemoteTag(Arc<RemoteOrigin>);

impl PartialEq for RemoteTag {
    fn eq(&self, other: &Self) -> bool {
        Arc::ptr_eq(&self.0, &other.0)
    }
}

impl Eq for RemoteTag {}

impl std::fmt::Debug for RemoteTag {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter
            .debug_struct("RemoteTag")
            .field("device", &self.0.device)
            .finish()
    }
}

impl RemoteTag {
    fn new(ticket: Ticket, device: String) -> Self {
        Self(Arc::new(RemoteOrigin {
            ticket,
            device,
            settled: AtomicBool::new(false),
        }))
    }

    /// Whether the device may still have this start now (its current standing).
    pub(super) fn may_start(&self) -> bool {
        self.0.ticket.may_start()
    }

    /// The request started.
    pub(super) fn accept(&self) {
        self.0.settle(RelayRequestState::Accepted);
    }

    /// The request will not run.
    pub(super) fn reject(&self, reason: &str) {
        self.0.settle(RelayRequestState::Rejected {
            reason: reason.to_string(),
        });
    }

    /// The request ran; `output` is what it produced.
    pub(super) fn done(&self, output: Option<String>) {
        self.0.settle(RelayRequestState::Done { output });
    }

    pub(super) fn device(&self) -> &str {
        &self.0.device
    }
}

/// The host's sharing state: the controller, plus what the host itself tracks.
pub(super) struct RemoteHost {
    pub(super) share: Arc<RemoteShare>,
    /// Phone prompts promoted into steering, kept (with their reserved payload)
    /// until the steer is consumed or the operation ends.
    steering: RefCell<Vec<(ItemId, RemoteTag)>>,
    /// A phone `/compact` waiting for its result.
    compact: RefCell<Option<RemoteTag>>,
    /// Which choice the app shows for this host, if one is open.
    choice: Cell<Option<ChoiceKind>>,
    /// The session and history last published to the feed.
    published: RefCell<Option<(SessionId, crate::relay_source::History)>>,
    /// The admission state last published to the feed.
    admission: Cell<Option<crate::server_feed::Admission>>,
    /// How `/remote` reads the gateway and configuration (off the terminal's
    /// loop; tests replace it).
    probe: StatusProbe,
    /// The `/remote` view being filled in, if one is: only it may be updated,
    /// and only while it is the report shown, for the same session.
    status: RefCell<Option<PendingStatus>>,
    /// Pairing a device (`/remote pair`).
    pub(super) pairing: super::pairing_host::PairingHost,
    /// The paired devices (`/remote devices`).
    pub(super) devices: super::devices_host::DevicesHost,
}

/// A `/remote` view waiting for its probe.
struct PendingStatus {
    /// The report the view opened as (see `AppModel::report_generation`).
    report: u64,
    session: Option<SessionId>,
    sharing: Vec<String>,
    task: tokio::task::JoinHandle<()>,
    result: tokio::sync::oneshot::Receiver<Vec<String>>,
}

impl Drop for PendingStatus {
    fn drop(&mut self) {
        // A view closed, replaced or superseded stops its probe.
        self.task.abort();
    }
}

/// Reads remote access settings, gateway health and the device count.
pub(super) type StatusProbe = Arc<
    dyn Fn() -> std::pin::Pin<Box<dyn std::future::Future<Output = String> + Send>> + Send + Sync,
>;

/// How long a `/remote` snapshot may take before it reports a timeout: the
/// gateway check's own bounds (connect, then answer) plus a margin.
const STATUS_DEADLINE: std::time::Duration = std::time::Duration::from_secs(6);

impl RemoteHost {
    pub(super) fn new(share: Arc<RemoteShare>, cwd: &std::path::Path) -> Self {
        let pairing = super::pairing_host::PairingHost::new(
            share.dial(),
            crate::remote_pairing::prerequisites_for(cwd.to_path_buf()),
        );
        let devices = super::devices_host::DevicesHost::new(std::sync::Arc::new(
            crate::server_devices::DeviceRegistry::for_user(cwd),
        ));
        Self {
            share,
            pairing,
            devices,
            steering: RefCell::new(Vec::new()),
            compact: RefCell::new(None),
            choice: Cell::new(None),
            published: RefCell::new(None),
            admission: Cell::new(None),
            probe: Arc::new(|| {
                Box::pin(async {
                    crate::remote_status::render_status(&crate::remote_gateway::status().await)
                })
            }),
            status: RefCell::new(None),
        }
    }

    /// The same host, listing devices with `devices`.
    #[cfg(test)]
    pub(super) fn with_devices(mut self, devices: super::devices_host::DevicesHost) -> Self {
        self.devices = devices;
        self
    }

    /// The same host, pairing with `pairing`.
    #[cfg(test)]
    pub(super) fn with_pairing(mut self, pairing: super::pairing_host::PairingHost) -> Self {
        self.pairing = pairing;
        self
    }

    /// The same host, reading remote status with `probe`.
    #[cfg(test)]
    pub(super) fn with_probe(mut self, probe: StatusProbe) -> Self {
        self.probe = probe;
        self
    }

    /// Open the `/remote` view: the sharing part now, remote access checked
    /// off the terminal's loop within [`STATUS_DEADLINE`] and filled into this
    /// same view. An earlier view's check is stopped.
    fn request_status(&self, app: &mut AppModel) {
        let session = self.share.tracked_session();
        let sharing = sharing_lines(&self.share);
        let mut lines = sharing.clone();
        lines.push(String::new());
        lines.push("Remote access: checking...".to_string());
        app.open_report("Remote status (checking...)".to_string(), lines);
        let Some(report) = app.report_generation() else {
            return;
        };
        let probe = self.probe.clone();
        let (tx, result) = tokio::sync::oneshot::channel();
        let task = tokio::spawn(async move {
            let probed = match tokio::time::timeout(STATUS_DEADLINE, probe()).await {
                Ok(text) => text.lines().map(str::to_string).collect::<Vec<_>>(),
                Err(_) => vec![format!(
                    "  Could not check remote access within {} s; reopen /remote to try again.",
                    STATUS_DEADLINE.as_secs()
                )],
            };
            let _ = tx.send(probed);
        });
        // Replacing the pending view drops (and so stops) the earlier check.
        *self.status.borrow_mut() = Some(PendingStatus {
            report,
            session,
            sharing,
            task,
            result,
        });
    }

    /// Fill in the `/remote` view once its check finished, if it is still the
    /// report shown for the same session; otherwise stop the check.
    fn show_status(&self, app: &mut AppModel) {
        let mut slot = self.status.borrow_mut();
        let Some(pending) = slot.as_mut() else {
            return;
        };
        if app.report_generation() != Some(pending.report)
            || pending.session != self.share.tracked_session()
        {
            *slot = None;
            return;
        }
        let probed = match pending.result.try_recv() {
            Ok(probed) => probed,
            Err(tokio::sync::oneshot::error::TryRecvError::Empty) => return,
            Err(tokio::sync::oneshot::error::TryRecvError::Closed) => {
                vec!["  Could not check remote access; reopen /remote to try again.".to_string()]
            }
        };
        let mut lines = std::mem::take(&mut pending.sharing);
        lines.push(String::new());
        lines.push("Remote access:".to_string());
        lines.extend(probed);
        let report = pending.report;
        *slot = None;
        let _ = app.update_report(
            report,
            format!(
                "Remote status (snapshot at {}; reopen /remote to refresh)",
                super::local_prompt_time()
            ),
            lines,
        );
    }

    /// Publish whether work is running and how many follow-ups wait, when it
    /// changed (a device's snapshot shows it).
    fn publish_admission(&self, busy: bool, queued: usize) {
        let admission = crate::server_feed::Admission { busy, queued };
        if self.admission.get() != Some(admission) {
            self.admission.set(Some(admission));
            self.share.source().feed().set_admission(admission);
        }
    }

    /// Take the phone `/compact` waiting for its result, if any.
    pub(super) fn take_compact(&self) -> Option<RemoteTag> {
        self.compact.borrow_mut().take()
    }

    /// Keep a promoted phone prompt's tag until its steer is consumed.
    pub(super) fn keep_steering(&self, item: ItemId, tag: RemoteTag) {
        self.steering.borrow_mut().push((item, tag));
    }

    /// Release the tags of steers no longer pending (consumed or dropped).
    pub(super) fn release_steering(&self, pending: &std::collections::VecDeque<ItemId>) {
        self.steering
            .borrow_mut()
            .retain(|(item, _)| pending.contains(item));
    }

    /// Ownership of the session or conversation may have changed: publish
    /// what the host shows now, and refresh the feed after `/clear` or a
    /// compaction (`refresh`).
    pub(super) fn publish(&self, runtime: &SessionRuntime, refresh: bool) {
        let source = self.share.source();
        let session = runtime.session_id();
        source.follow(session);
        // What a device is shown, compared by content: any change to it (not
        // only a longer conversation) is published.
        let history = crate::relay_source::history_of(runtime.conversation());
        let changed = self
            .published
            .borrow()
            .as_ref()
            .is_none_or(|(known, shown)| *known != session || *shown != history);
        if refresh {
            source.refresh(history.clone());
        } else if changed {
            source.publish(history.clone());
        }
        *self.published.borrow_mut() = Some((session, history));
    }
}

/// The status-line label: sharing, and a pairing code still being settled.
fn status_label(host: &RemoteHost, app: &AppModel) -> Option<String> {
    let sharing = label(&host.share.state(), host.share.devices().as_deref());
    match (sharing, host.pairing.status(app)) {
        (Some(sharing), Some(pairing)) => Some(format!("{sharing} · {pairing}")),
        (sharing, pairing) => sharing.or_else(|| pairing.map(str::to_string)),
    }
}

/// The short status-line label for a sharing state (`None` when not shared).
fn label(state: &ShareState, devices: Option<&[localpilot_rpc::AttendedDevice]>) -> Option<String> {
    match state {
        ShareState::Off => None,
        ShareState::Connecting => Some("sharing: connecting".to_string()),
        ShareState::Shared => Some(format!("shared · {}", devices_text(devices))),
        ShareState::Interrupted { retry_in } => Some(format!(
            "sharing interrupted · retry in {}s",
            retry_in.as_secs().max(1)
        )),
    }
}

/// Show what the relay reported and keep the status line current.
pub(super) fn show_notices(app: &mut AppModel, host: &RemoteHost) {
    for notice in host.share.take_notices() {
        match notice {
            ShareNotice::Shared => app.apply_runtime(RuntimeUpdate::Notice(
                "Session shared. Paired devices can attach while this terminal is open."
                    .to_string(),
            )),
            ShareNotice::Restored => app.apply_runtime(RuntimeUpdate::Notice(
                "Sharing restored. Devices can reconnect.".to_string(),
            )),
            ShareNotice::Interrupted { retry_in } => {
                app.apply_runtime(RuntimeUpdate::Warning(format!(
                    "Sharing interrupted. Local work continues. Reconnecting in {}s.",
                    retry_in.as_secs().max(1)
                )))
            }
            ShareNotice::Ended(why) => app.apply_runtime(RuntimeUpdate::Warning(why)),
            ShareNotice::NoDevicesYet => app.apply_runtime(RuntimeUpdate::Notice(
                "No devices attached yet.".to_string(),
            )),
            ShareNotice::Attached { name, scope } => {
                let scope = match scope {
                    Some(localpilot_rpc::Scope::Control) => " (control)",
                    Some(localpilot_rpc::Scope::Observe) => " (observe)",
                    None => "",
                };
                app.apply_runtime(RuntimeUpdate::Notice(format!(
                    "Device attached: {name}{scope}."
                )));
            }
            ShareNotice::Detached { name } => {
                app.apply_runtime(RuntimeUpdate::Notice(format!("Device detached: {name}.")));
            }
            ShareNotice::NotAllowed { name, standing } => {
                let text = match standing {
                    // A failed read is not a known refusal: nothing is being
                    // disconnected because of it.
                    localpilot_rpc::DeviceStanding::Unknown => format!(
                        "{name}: its access cannot be confirmed right now (the device registry cannot be read)."
                    ),
                    other => format!(
                        "{name} is {}; it is being disconnected.",
                        standing_text(other)
                    ),
                };
                app.apply_runtime(RuntimeUpdate::Warning(text));
            }
            ShareNotice::Summary { attached, detached } => {
                app.apply_runtime(RuntimeUpdate::Notice(format!(
                    "{attached} device(s) attached, {detached} detached."
                )));
            }
            ShareNotice::CameAndWent(count) => app.apply_runtime(RuntimeUpdate::Notice(format!(
                "{count} device(s) attached and left again since the last update."
            ))),
            ShareNotice::Activity { arrived, left } => app.apply_runtime(RuntimeUpdate::Notice(
                format!(
                    "Since the last update: {arrived} device arrival(s), {left} departure(s) (which devices is not known)."
                ),
            )),
            ShareNotice::Overflow(count) => app.apply_runtime(RuntimeUpdate::Notice(format!(
                "{count} more sharing update(s) not shown."
            ))),
        }
    }
    let status = status_label(host, app);
    app.set_remote_label(status);
}

/// A device standing, as a notice says it.
fn standing_text(standing: localpilot_rpc::DeviceStanding) -> &'static str {
    use localpilot_rpc::DeviceStanding;
    match standing {
        DeviceStanding::Active => "allowed",
        DeviceStanding::NotGranted => "no longer allowed in this workspace",
        DeviceStanding::Revoked => "revoked",
        DeviceStanding::Removed => "no longer paired",
        DeviceStanding::Unknown => "of unknown standing (the device registry cannot be read)",
    }
}

/// This session's sharing, as the `/remote` snapshot starts.
fn sharing_lines(share: &RemoteShare) -> Vec<String> {
    let mut lines = vec![format!(
        "Sharing: {}",
        match share.state() {
            ShareState::Off => "off".to_string(),
            ShareState::Connecting => "connecting".to_string(),
            ShareState::Shared =>
                "shared (phone access lasts while this terminal is open)".to_string(),
            ShareState::Interrupted { retry_in } => format!(
                "interrupted; reconnecting in {}s (phone actions are unavailable)",
                retry_in.as_secs().max(1)
            ),
        }
    )];
    let devices = share.devices();
    lines.push(format!("Attached: {}", devices_text(devices.as_deref())));
    for device in devices.unwrap_or_default() {
        let scope = match device.scope {
            Some(localpilot_rpc::Scope::Control) => "control",
            Some(localpilot_rpc::Scope::Observe) => "observe",
            None => "scope unknown",
        };
        let standing = match device.standing {
            localpilot_rpc::DeviceStanding::Active => String::new(),
            localpilot_rpc::DeviceStanding::Unknown => {
                ", access unconfirmed (registry unreadable)".to_string()
            }
            other => format!(", {}, being disconnected", standing_text(other)),
        };
        lines.push(format!(
            "  {} [{}], {scope}{standing}",
            device.name.as_deref().unwrap_or("(name unknown)"),
            device.id
        ));
    }
    lines.push(
        "Commands: /remote share | /remote stop | /remote pair [observe] | /remote devices."
            .to_string(),
    );
    lines
}

/// How many devices are attached, for the status line and the exit question.
fn devices_text(devices: Option<&[localpilot_rpc::AttendedDevice]>) -> String {
    match devices {
        None => "devices unknown".to_string(),
        Some([]) => "no devices".to_string(),
        Some([_]) => "1 device".to_string(),
        Some(devices) => format!("{} devices", devices.len()),
    }
}

/// What a device asked the idle host to run.
pub(super) enum IdleWork {
    /// A prompt, already shown in the transcript as the device's.
    Prompt(super::QueuedOperation),
    /// A `/compact` (its tag waits in the host for the result).
    Compact { force: bool },
}

/// The idle loop's tick: track the session the runtime holds now, publish
/// it, show notices, and take device requests. Returns work to run as an
/// operation, if a device asked for one.
pub(super) fn idle_tick(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    host: &RemoteHost,
) -> Option<IdleWork> {
    let tracked = Tracked {
        session: runtime.session_id(),
        claim: runtime.ownership(),
        gate: runtime.admission_gate(),
    };
    if host.share.track(tracked) {
        app.apply_runtime(RuntimeUpdate::Notice(
            "Sharing stopped for the previous session. Use /remote share for this session."
                .to_string(),
        ));
    }
    host.publish(runtime, false);
    host.publish_admission(false, 0);
    show_notices(app, host);
    host.show_status(app);
    tick_pairing(app, host);
    for _ in 0..REQUESTS_PER_TICK {
        let request = host.share.next_request()?;
        if let Some(work) = idle_request(app, runtime, host, request) {
            return Some(work);
        }
    }
    None
}

fn device_name(host: &RemoteHost, ticket: &Ticket) -> String {
    host.share
        .device_name(ticket.device())
        .unwrap_or_else(|| "a paired device".to_string())
}

/// A phone prompt as a queued operation, shown in the transcript as the
/// device's. It always takes an ordinary model turn.
fn phone_prompt(
    app: &mut AppModel,
    text: String,
    tag: RemoteTag,
    pending: bool,
) -> Option<super::QueuedOperation> {
    let display = format!("(phone: {}) {text}", tag.device());
    let item_id = app.append_prompt(display, Some(super::local_prompt_time()), pending)?;
    Some(super::QueuedOperation::Prompt(super::QueuedPrompt {
        text,
        attachments: Vec::new(),
        item_id,
        target: super::PromptTarget::Agent,
        origin: Some(tag),
    }))
}

fn idle_request(
    app: &mut AppModel,
    runtime: &mut SessionRuntime,
    host: &RemoteHost,
    request: SourceRequest,
) -> Option<IdleWork> {
    let SourceRequest { command, ticket } = request;
    if !ticket.accept_now() {
        ticket.report(RelayRequestState::Rejected {
            reason: "this device may no longer do that".to_string(),
        });
        return None;
    }
    let name = device_name(host, &ticket);
    let tag = RemoteTag::new(ticket, name);
    match command {
        // Idle: "now" and "follow-up" both run now. Steering from a device is
        // refused, not reinterpreted.
        ClientCommand::Prompt {
            disposition: InputDisposition::Steer,
            ..
        } => {
            tag.reject(STEER_REFUSED);
            None
        }
        ClientCommand::Prompt { text, .. } => match phone_prompt(app, text, tag.clone(), false) {
            Some(operation) => Some(IdleWork::Prompt(operation)),
            None => {
                tag.reject("the terminal could not take the prompt");
                None
            }
        },
        ClientCommand::Cancel => {
            tag.reject("no turn is running");
            None
        }
        ClientCommand::RunCommand { name, args } => {
            let line = if args.trim().is_empty() {
                format!("/{name}")
            } else {
                format!("/{name} {args}")
            };
            match parse_slash_for(Host::Remote, &line) {
                Some(SlashAction::Clear) => {
                    super::clear_session(app, runtime);
                    host.publish(runtime, true);
                    tag.done(Some("conversation cleared".to_string()));
                }
                Some(SlashAction::NameSession(text)) => {
                    match super::name_session(app, runtime, &text) {
                        Ok(output) => tag.done(Some(output)),
                        Err(reason) => tag.reject(&reason),
                    }
                }
                Some(SlashAction::SetEffort(level)) => {
                    match super::set_effort_idle(app, runtime, &level) {
                        Ok(output) => tag.done(Some(output)),
                        Err(reason) => tag.reject(&reason),
                    }
                }
                Some(SlashAction::Compact { force }) => {
                    *host.compact.borrow_mut() = Some(tag);
                    return Some(IdleWork::Compact { force });
                }
                _ => tag.reject("that command is not available on a shared session"),
            }
            None
        }
        _ => {
            tag.reject("not available on a shared session");
            None
        }
    }
}

/// A running operation's frame tick: show notices, and take device requests
/// the running operation can serve (prompts queue, cancel stops the turn,
/// effort applies live; anything else waits for idle and is refused now).
pub(super) fn busy_tick(
    app: &mut AppModel,
    host: &RemoteHost,
    queue: &mut std::collections::VecDeque<super::QueuedOperation>,
    cancel: &CancellationToken,
    live: Option<&super::LiveControls>,
    pending_steers: &std::collections::VecDeque<ItemId>,
) {
    host.release_steering(pending_steers);
    show_notices(app, host);
    host.show_status(app);
    tick_pairing(app, host);
    host.publish_admission(true, queue.len());
    for _ in 0..REQUESTS_PER_TICK {
        let Some(SourceRequest { command, ticket }) = host.share.next_request() else {
            return;
        };
        if !ticket.accept_now() {
            ticket.report(RelayRequestState::Rejected {
                reason: "this device may no longer do that".to_string(),
            });
            continue;
        }
        let name = device_name(host, &ticket);
        let tag = RemoteTag::new(ticket, name);
        match command {
            // Busy: "now" is refused (nothing runs, nothing queues); a
            // follow-up queues as its own turn; steering is refused.
            ClientCommand::Prompt {
                disposition: InputDisposition::Immediate,
                ..
            } => tag.reject("a turn is running; send it as a follow-up or cancel first"),
            ClientCommand::Prompt {
                disposition: InputDisposition::Steer,
                ..
            } => tag.reject(STEER_REFUSED),
            ClientCommand::Prompt {
                text,
                disposition: InputDisposition::FollowUp,
            } => match phone_prompt(app, text, tag.clone(), true) {
                Some(operation) => {
                    tag.0.ticket.report(RelayRequestState::Queued);
                    queue.push_back(operation);
                }
                None => tag.reject("the terminal could not take the prompt"),
            },
            ClientCommand::Cancel => {
                cancel.cancel();
                app.apply_runtime(RuntimeUpdate::Notice(format!(
                    "cancelling the current work (from {})",
                    tag.device()
                )));
                tag.done(None);
            }
            ClientCommand::RunCommand { name, args } => {
                let line = if args.trim().is_empty() {
                    format!("/{name}")
                } else {
                    format!("/{name} {args}")
                };
                match (parse_slash_for(Host::Remote, &line), live) {
                    (Some(SlashAction::SetEffort(level)), Some(live)) => {
                        match super::set_effort_live(app, live, &level) {
                            Ok(output) => tag.done(Some(output)),
                            Err(reason) => tag.reject(&reason),
                        }
                    }
                    _ => tag.reject("a turn is running"),
                }
            }
            _ => tag.reject("not available on a shared session"),
        }
    }
}

/// `/remote ...` in this terminal (idle or during an operation: it never waits
/// for the current work).
pub(super) fn command(app: &mut AppModel, host: &RemoteHost, action: RemoteAction) {
    let notice = |app: &mut AppModel, text: &str| {
        app.apply_runtime(RuntimeUpdate::Notice(text.to_string()));
    };
    match action {
        RemoteAction::Status => {
            notice(app, "Checking remote status...");
            host.request_status(app);
        }
        RemoteAction::Share => match host.share.share() {
            ShareOutcome::Connecting => {
                notice(app, "Connecting this session to the background server...");
            }
            ShareOutcome::AlreadyShared => notice(app, "This session is already shared."),
            ShareOutcome::AlreadyConnecting => notice(app, "Sharing is connecting."),
            ShareOutcome::Refused(reason) => app.apply_runtime(RuntimeUpdate::Warning(format!(
                "Could not share this session. {reason}"
            ))),
        },
        RemoteAction::Stop => match host.share.stop() {
            StopOutcome::Stopped => notice(
                app,
                "Sharing stopped here: devices can no longer act on this session. Their disconnect is not confirmed yet. Work continues in this terminal.",
            ),
            StopOutcome::Cancelled => {
                notice(app, "Sharing cancelled. Work continues in this terminal.");
            }
            StopOutcome::NotShared => notice(app, "This session is not shared."),
        },
        RemoteAction::Pair { observe } => {
            host.pairing
                .open(app, observe, host.share.tracked_session());
        }
        RemoteAction::Devices => {
            host.pairing.close_for_devices(app);
            host.devices.open(app, host.share.tracked_session());
        }
        RemoteAction::Usage => notice(
            app,
            "Use /remote [pair [observe|control] | devices | share | stop].",
        ),
    }
    let status = status_label(host, app);
    app.set_remote_label(status);
}

/// Keep the pairing overlay current (and closed if the session changed).
fn tick_pairing(app: &mut AppModel, host: &RemoteHost) {
    let shared = !matches!(host.share.state(), ShareState::Off);
    host.pairing.tick(app, host.share.tracked_session(), shared);
    // Attendance here: only this session's confirmed list (when shared).
    let attached = host
        .share
        .devices()
        .filter(|_| matches!(host.share.state(), ShareState::Shared))
        .map(|devices| {
            devices
                .into_iter()
                .map(|device| device.id)
                .collect::<Vec<_>>()
        });
    let incognito = app.incognito();
    host.devices.tick(
        app,
        host.share.tracked_session(),
        attached.as_deref(),
        incognito,
    );
    let status = status_label(host, app);
    app.set_remote_label(status);
}

/// The pairing pump and status line, as a tick runs them.
#[cfg(test)]
pub(super) fn idle_status_for_test(app: &mut AppModel, host: &RemoteHost) {
    tick_pairing(app, host);
}

/// Route a key to the open pairing overlay.
pub(super) fn pairing_key(app: &mut AppModel, host: &RemoteHost, key: crossterm::event::KeyEvent) {
    if app.devices_view().is_some() {
        let incognito = app.incognito();
        if host.devices.key(app, key, incognito) == super::devices_host::DevicesKey::Pair {
            host.pairing.open(app, false, host.share.tracked_session());
        }
        return;
    }
    let shared = !matches!(host.share.state(), ShareState::Off);
    match host.pairing.key(app, key, shared) {
        super::pairing_host::PairingKey::Share => command(app, host, RemoteAction::Share),
        super::pairing_host::PairingKey::Devices => command(app, host, RemoteAction::Devices),
        super::pairing_host::PairingKey::Handled => {}
    }
}

/// End sharing before a command replaces the session.
pub(super) fn end_for_identity(app: &mut AppModel, host: &RemoteHost) {
    if host.share.end_for_identity() {
        app.apply_runtime(RuntimeUpdate::Notice(
            "Sharing stopped for the previous session. Use /remote share for this session."
                .to_string(),
        ));
        app.set_remote_label(None);
    }
}

/// Which choice the host has open.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ChoiceKind {
    /// Closing a shared session: "Keep terminal open" or "Close session and
    /// exit".
    Exit,
    /// Remote access during a permission ask or question (F6). `stop` is the
    /// action the menu showed: Enter does exactly that, whatever changed since.
    Remote { stop: bool },
}

/// Whether an exit request may close the terminal now. While sharing is on
/// (connecting, shared or reconnecting) it opens the close confirmation
/// instead, with "Keep terminal open" selected, and the terminal closes only
/// when "Close session and exit" is chosen there. `turn` names what the
/// session is doing, for the confirmation.
pub(super) fn request_exit(app: &mut AppModel, host: &RemoteHost, turn: &str) -> bool {
    if !host.share.active() {
        return true;
    }
    if app.choice().is_none() {
        let sharing = match host.share.state() {
            ShareState::Interrupted { .. } => {
                "Phone access is interrupted. Closing ends this session and stops reconnecting."
            }
            ShareState::Connecting => {
                "Sharing is connecting. Closing ends this session; it does not keep running on the server."
            }
            _ => "This session is shared with paired devices. Closing ends this session and phone access; it does not keep running on the server.",
        };
        host.choice.set(Some(ChoiceKind::Exit));
        app.open_choice(
            "Close this shared session?".to_string(),
            vec![
                sharing.to_string(),
                format!(
                    "Devices attached: {}.",
                    devices_text(host.share.devices().as_deref())
                ),
                format!("Current turn: {turn}"),
            ],
            vec![
                "Keep terminal open".to_string(),
                "Close session and exit".to_string(),
            ],
        );
    }
    false
}

/// Remote access without leaving a permission ask or question (F6): share or
/// stop sharing, or go back to the ask unchanged.
pub(super) fn open_remote_menu(app: &mut AppModel, host: &RemoteHost) {
    if app.choice().is_some() {
        return;
    }
    let (status, stop) = match host.share.state() {
        ShareState::Off => ("This session is not shared.", false),
        ShareState::Connecting => ("Sharing is connecting.", true),
        ShareState::Shared => ("This session is shared.", true),
        ShareState::Interrupted { .. } => ("Sharing is interrupted; reconnecting.", true),
    };
    let action = if stop {
        "Stop sharing"
    } else {
        "Share this session"
    };
    host.choice.set(Some(ChoiceKind::Remote { stop }));
    app.open_choice(
        "Remote access".to_string(),
        vec![
            status.to_string(),
            "The permission request stays open on this terminal.".to_string(),
        ],
        vec![action.to_string(), "Back".to_string()],
    );
}

/// What a key did to the host's open choice.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum ChoiceKey {
    /// Still open (the selection moved, or the key does nothing here).
    Open,
    /// Closed; the terminal stays open and whatever was beneath is unchanged.
    Back,
    /// "Close session and exit" was chosen: sharing is already stopped.
    Close,
}

/// Route a key to the host's open choice. Only an explicit Enter on "Close
/// session and exit" closes the terminal; no quit shortcut does.
pub(super) fn choice_key(
    app: &mut AppModel,
    host: &RemoteHost,
    key: crossterm::event::KeyEvent,
) -> ChoiceKey {
    use crossterm::event::KeyCode;
    let Some(selected) = app.choice().map(|choice| choice.selected) else {
        return ChoiceKey::Back;
    };
    match key.code {
        KeyCode::Down | KeyCode::Tab => {
            app.move_choice(true);
            ChoiceKey::Open
        }
        KeyCode::Up | KeyCode::BackTab => {
            app.move_choice(false);
            ChoiceKey::Open
        }
        KeyCode::Esc => {
            app.close_choice();
            host.choice.set(None);
            ChoiceKey::Back
        }
        KeyCode::Enter => {
            app.close_choice();
            match (host.choice.take(), selected) {
                (Some(ChoiceKind::Exit), 1) => {
                    host.share.stop();
                    app.apply_runtime(RuntimeUpdate::Notice(
                        "Closing session. Ending phone access...".to_string(),
                    ));
                    ChoiceKey::Close
                }
                // Exactly the action shown: a "Stop sharing" left open while
                // sharing ended on its own stops nothing (and never shares).
                (Some(ChoiceKind::Remote { stop }), 0) => {
                    let action = if stop {
                        RemoteAction::Stop
                    } else {
                        RemoteAction::Share
                    };
                    command(app, host, action);
                    ChoiceKey::Back
                }
                _ => ChoiceKey::Back,
            }
        }
        _ => ChoiceKey::Open,
    }
}
