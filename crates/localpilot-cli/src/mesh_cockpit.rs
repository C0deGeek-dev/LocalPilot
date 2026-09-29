//! `localpilot mesh cockpit`: the human's view of a pair session.
//!
//! The cockpit is an observer. It has no role and registers no delivery
//! endpoint: it re-reads a typed snapshot of the session (`Mesh::snapshot`)
//! every half second. `--json` prints one snapshot and exits; without it, a
//! full-screen view shows the snapshot until `q`.
//!
//! The view never writes the mailbox itself. Each human action runs exactly
//! one protocol command, as a separate process, through a role the human
//! picks for that action, with the self-asserted `--actor human` claim.

use std::process::ExitCode;

use clap::Parser;
use localpilot_mesh::Mesh;

use crate::mesh_cmd::{resolve_anchor, MeshArgs};

#[derive(Debug, Parser)]
#[command(name = "localpilot mesh cockpit", no_binary_name = true)]
struct CockpitCli {
    /// Print one snapshot of the session as JSON and exit.
    #[arg(long)]
    json: bool,
}

/// Whether these mesh arguments ask for the cockpit.
pub(crate) fn is_cockpit(args: &MeshArgs) -> bool {
    args.rest.first().is_some_and(|op| op == "cockpit")
}

/// Show the cockpit; 0 on a clean exit, 1 on an error, 2 on a usage error.
pub(crate) fn run(args: &MeshArgs) -> ExitCode {
    let cli = match CockpitCli::try_parse_from(&args.rest[1..]) {
        Ok(cli) => cli,
        Err(e) => {
            let _ = e.print();
            return ExitCode::from(u8::try_from(e.exit_code()).unwrap_or(2));
        }
    };
    let (anchor, source) = match resolve_anchor(args.repo.as_deref()) {
        Ok(a) => a,
        Err(msg) => {
            eprintln!("{msg}");
            return ExitCode::from(1);
        }
    };
    let mesh = Mesh::at(&anchor, source);
    if cli.json {
        let snap = mesh.snapshot();
        println!(
            "{}",
            serde_json::to_string_pretty(&snap).unwrap_or_default()
        );
        return ExitCode::SUCCESS;
    }
    full_screen(&mesh, &anchor)
}

#[cfg(not(feature = "tui"))]
fn full_screen(_mesh: &Mesh, _anchor: &std::path::Path) -> ExitCode {
    eprintln!(
        "localpilot mesh cockpit: this build has no terminal UI (the `tui` feature); use --json"
    );
    ExitCode::from(2)
}

#[cfg(feature = "tui")]
fn full_screen(mesh: &Mesh, anchor: &std::path::Path) -> ExitCode {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        eprintln!("localpilot mesh cockpit: the full-screen view needs a terminal; use --json");
        return ExitCode::from(2);
    }
    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            eprintln!("localpilot mesh cockpit: cannot find this program to run actions with: {e}");
            return ExitCode::from(1);
        }
    };
    // Park and resume are pair.py's, and pair.py is the configured delegate:
    // the same trusted, user-only configuration `localpilot mesh` reads.
    let reference = match crate::mesh_cmd::trusted_mesh_config() {
        Ok(c) => c.delegate_command,
        Err(e) => {
            eprintln!("localpilot mesh cockpit: cannot load configuration: {e}");
            return ExitCode::from(1);
        }
    };
    let runner = act::Runner {
        exe,
        anchor: anchor.to_path_buf(),
        reference,
    };
    match view::run(mesh, &runner) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("localpilot mesh cockpit: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(feature = "tui")]
mod act {
    //! The cockpit's actions. Each is exactly one protocol command, run as
    //! its own process through a role the human picks for that action:
    //! `localpilot mesh <op>` for the participant operations (so the
    //! configured writer, native or delegate, applies), and the configured
    //! pair.py for park and resume, which only the full profile has. Every
    //! command carries the self-asserted `--actor human` claim; the claim
    //! is never an identity and grants nothing the role does not have.

    use std::ffi::OsString;
    use std::path::PathBuf;
    use std::process::{Command, Stdio};

    use crossterm::event::KeyCode;
    use localpilot_mesh::ops::snapshot::SessionView;
    use localpilot_mesh::ops::Snapshot;
    use serde_json::Value;

    /// The roles offered when no session can be read (resuming one).
    const ROLES: &[&str] = &["claude", "codex", "localpilot"];

    /// What a key starts.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub(super) enum Kind {
        Stop,
        Answer,
        Accept,
        Decline,
        Withdraw,
        Park,
        Resume,
    }

    impl Kind {
        const fn from_key(c: char) -> Option<Self> {
            Some(match c {
                's' => Self::Stop,
                'a' => Self::Answer,
                'h' => Self::Accept,
                'd' => Self::Decline,
                'w' => Self::Withdraw,
                'p' => Self::Park,
                'r' => Self::Resume,
                _ => return None,
            })
        }

        const fn label(self) -> &'static str {
            match self {
                Self::Stop => "STOP",
                Self::Answer => "answer",
                Self::Accept => "accept the handoff",
                Self::Decline => "decline the handoff",
                Self::Withdraw => "withdraw the handoff",
                Self::Park => "park the session",
                Self::Resume => "resume a parked session",
            }
        }

        /// The text the action asks for, if any, and whether it may be empty.
        const fn asks(self) -> Option<(&'static str, bool)> {
            match self {
                Self::Stop => Some(("why", false)),
                Self::Answer => Some(("the answer", false)),
                Self::Park => Some(("the reason (optional)", true)),
                _ => None,
            }
        }

        const fn via_reference(self) -> bool {
            matches!(self, Self::Park | Self::Resume)
        }

        const fn about_handoff(self) -> bool {
            matches!(self, Self::Accept | Self::Decline | Self::Withdraw)
        }
    }

    /// One action, fully chosen: what, through which role, with what text.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Plan {
        pub(super) kind: Kind,
        pub(super) role: String,
        pub(super) text: String,
        pub(super) reply_to: Option<String>,
        /// Shown with the confirmation: the wait an answer answers.
        pub(super) about: String,
        /// Named recipients (`--to`), when the session needs them.
        pub(super) to: Vec<String>,
    }

    impl Plan {
        /// The operation and its arguments, as both `localpilot mesh` and
        /// pair.py take them.
        pub(super) fn args(&self) -> Vec<String> {
            let r = self.role.as_str();
            let mut a: Vec<String> = match self.kind {
                Kind::Stop => {
                    let mut a: Vec<String> =
                        ["post", "--role", r, "--kind", "STOP", "--body", &self.text]
                            .map(String::from)
                            .to_vec();
                    if !self.to.is_empty() {
                        a.extend(["--to".to_owned(), self.to.join(",")]);
                    }
                    a
                }
                Kind::Answer => {
                    let mut a: Vec<String> = [
                        "post", "--role", r, "--kind", "ANSWER", "--body", &self.text,
                    ]
                    .map(String::from)
                    .to_vec();
                    if let Some(m) = &self.reply_to {
                        a.extend(["--reply-to".to_owned(), m.clone()]);
                    }
                    a
                }
                Kind::Accept => ["handoff-accept", "--role", r].map(String::from).to_vec(),
                Kind::Decline => ["handoff-decline", "--role", r].map(String::from).to_vec(),
                Kind::Withdraw => ["handoff-withdraw", "--role", r].map(String::from).to_vec(),
                Kind::Park => {
                    let mut a: Vec<String> = ["park", "--role", r].map(String::from).to_vec();
                    if !self.text.is_empty() {
                        a.extend(["--reason".to_owned(), self.text.clone()]);
                    }
                    a
                }
                Kind::Resume => ["resume", "--role", r].map(String::from).to_vec(),
            };
            a.extend(["--actor".to_owned(), "human".to_owned()]);
            a
        }
    }

    /// How the cockpit runs a plan.
    pub(super) struct Runner {
        /// This program, run as `<exe> mesh`.
        pub(super) exe: PathBuf,
        pub(super) anchor: PathBuf,
        /// The configured `delegate_command` (pair.py), for park and resume.
        pub(super) reference: Vec<String>,
    }

    /// What a command did: its exit code (-1 when it did not run) and the
    /// first line it printed.
    #[derive(Debug, Clone, PartialEq, Eq)]
    pub(super) struct Outcome {
        pub(super) code: i32,
        pub(super) text: String,
    }

    impl Runner {
        /// The whole command line, or why there is none.
        pub(super) fn argv(&self, p: &Plan) -> Result<Vec<OsString>, String> {
            let mut v: Vec<OsString> = if p.kind.via_reference() {
                let Some((prog, lead)) = self.reference.split_first() else {
                    return Err("park and resume are pair.py commands; set [mesh] delegate_command to pair.py to run them from here".to_owned());
                };
                std::iter::once(prog)
                    .chain(lead)
                    .map(OsString::from)
                    .collect()
            } else {
                vec![self.exe.clone().into_os_string(), "mesh".into()]
            };
            v.push("--repo".into());
            v.push(self.anchor.clone().into_os_string());
            v.extend(p.args().into_iter().map(OsString::from));
            Ok(v)
        }

        /// Run the plan's command and wait for it.
        pub(super) fn run(&self, p: &Plan) -> Outcome {
            let argv = match self.argv(p) {
                Ok(v) => v,
                Err(text) => return Outcome { code: -1, text },
            };
            let Some((prog, rest)) = argv.split_first() else {
                return Outcome {
                    code: -1,
                    text: "empty command".to_owned(),
                };
            };
            match Command::new(prog).args(rest).stdin(Stdio::null()).output() {
                Ok(o) => {
                    let code = o.status.code().unwrap_or(-1);
                    let (first, second) = if code == 0 {
                        (&o.stdout, &o.stderr)
                    } else {
                        (&o.stderr, &o.stdout)
                    };
                    let line = |b: &Vec<u8>| {
                        String::from_utf8_lossy(b)
                            .lines()
                            .map(str::trim)
                            .find(|l| !l.is_empty())
                            .map(str::to_owned)
                    };
                    Outcome {
                        code,
                        text: line(first).or_else(|| line(second)).unwrap_or_default(),
                    }
                }
                Err(e) => Outcome {
                    code: -1,
                    text: format!("cannot start {}: {e}", prog.to_string_lossy()),
                },
            }
        }
    }

    /// The command line exactly as it will run, quoted for display.
    fn shown(runner: &Runner, p: &Plan) -> Result<String, String> {
        let argv = runner.argv(p)?;
        Ok(argv
            .iter()
            .map(|a| {
                let a = a.to_string_lossy();
                if a.is_empty() || a.contains(char::is_whitespace) || a.contains('"') {
                    format!("{a:?}")
                } else {
                    a.into_owned()
                }
            })
            .collect::<Vec<_>>()
            .join(" "))
    }

    /// An open QUESTION or ESCALATE: its id, and the roles still owed an
    /// answer to it.
    #[derive(Debug, Clone, PartialEq, Eq)]
    struct Wait {
        id: String,
        kind: String,
        from: String,
        pending: Vec<String>,
    }

    impl Wait {
        fn about(&self) -> String {
            format!("{} {} from {}", self.id, self.kind, self.from)
        }
    }

    /// Every open QUESTION or ESCALATE, oldest first. The session's waits
    /// are either one legacy wait (`kind`, `from_role`, `for_role`, `seq`)
    /// or a map keyed by msg_id with its `pending` roles, whatever the
    /// schema.
    fn open_waits(v: &SessionView) -> Vec<Wait> {
        let str_at = |e: &Value, k: &str| {
            e.get(k)
                .and_then(Value::as_str)
                .unwrap_or_default()
                .to_owned()
        };
        let w = &v.waiting;
        let mut all: Vec<(String, Wait)> = Vec::new();
        let mut add = |id: String, e: &Value, pending: Vec<String>| {
            let kind = str_at(e, "kind");
            if matches!(kind.as_str(), "QUESTION" | "ESCALATE") {
                let from = Some(str_at(e, "from"))
                    .filter(|f| !f.is_empty())
                    .unwrap_or_else(|| str_at(e, "from_role"));
                all.push((
                    str_at(e, "since"),
                    Wait {
                        id,
                        kind,
                        from,
                        pending,
                    },
                ));
            }
        };
        if w.get("kind").is_some() {
            let seq = w.get("seq").and_then(Value::as_i64).unwrap_or_default();
            let for_role = str_at(w, "for_role");
            add(
                format!("{}:{seq}", str_at(w, "from_role")),
                w,
                (!for_role.is_empty())
                    .then_some(for_role)
                    .into_iter()
                    .collect(),
            );
        } else if let Some(o) = w.as_object() {
            for (id, e) in o {
                let pending = e
                    .get("pending")
                    .and_then(Value::as_array)
                    .map(|a| {
                        a.iter()
                            .filter_map(Value::as_str)
                            .map(str::to_owned)
                            .collect()
                    })
                    .unwrap_or_default();
                add(id.clone(), e, pending);
            }
        }
        all.sort_by(|x, y| (&x.0, &x.1.id).cmp(&(&y.0, &y.1.id)));
        all.into_iter().map(|(_, w)| w).collect()
    }

    #[derive(Debug, Default)]
    enum Mode {
        #[default]
        Idle,
        PickWait {
            waits: Vec<Wait>,
            threaded: bool,
            typed: String,
        },
        Role {
            kind: Kind,
            roles: Vec<String>,
            reply_to: Option<String>,
            about: String,
            typed: String,
            /// Address every other role listed (a schema-2 STOP: only an
            /// ESCALATE may be broadcast, so the rest are named).
            address: bool,
        },
        Text {
            plan: Plan,
            input: String,
        },
        Confirm {
            plan: Plan,
        },
        Running {
            plan: Plan,
        },
    }

    /// The action being chosen, and the outcome of the last one.
    #[derive(Debug, Default)]
    pub(super) struct Prompt {
        mode: Mode,
        last: Option<Outcome>,
        /// The screen's width and height as last measured; `None` until then.
        room: Option<(u16, u16)>,
    }

    /// What the action line shows.
    pub(super) enum PromptView {
        Keys,
        Ask(String),
        Done(Outcome),
    }

    impl Prompt {
        pub(super) const fn is_open(&self) -> bool {
            !matches!(self.mode, Mode::Idle)
        }

        /// The screen the action line is drawn on.
        pub(super) fn set_room(&mut self, width: u16, height: u16) {
            self.room = Some((width, height));
        }

        /// The rows `c` needs on the measured screen and the rows it has,
        /// when it needs more.
        fn overflow(&self, c: &str) -> Option<(usize, usize)> {
            let (w, h) = self.room?;
            let (need, have) = (rows(c, w).len(), action_rows(h));
            (need > have).then_some((need, have))
        }

        /// One key press. Returns the plan once the human has confirmed it;
        /// the caller runs it and reports back through [`Prompt::finished`].
        pub(super) fn key(
            &mut self,
            code: KeyCode,
            snap: &Snapshot,
            runner: &Runner,
        ) -> Option<Plan> {
            match std::mem::take(&mut self.mode) {
                Mode::Idle => {
                    if let Some(kind) = char_of(code).and_then(Kind::from_key) {
                        self.start(kind, snap);
                    }
                    None
                }
                Mode::PickWait {
                    waits,
                    threaded,
                    mut typed,
                } => {
                    if code == KeyCode::Esc {
                        return None;
                    }
                    match choose(&mut typed, code, waits.len()) {
                        Some(i) => self.answer(waits[i].clone(), threaded),
                        None => {
                            self.mode = Mode::PickWait {
                                waits,
                                threaded,
                                typed,
                            }
                        }
                    }
                    None
                }
                Mode::Role {
                    kind,
                    roles,
                    reply_to,
                    about,
                    mut typed,
                    address,
                } => {
                    if code == KeyCode::Esc {
                        return None;
                    }
                    match choose(&mut typed, code, roles.len()).map(|i| roles[i].clone()) {
                        Some(role) => {
                            let to = if address {
                                roles.iter().filter(|r| **r != role).cloned().collect()
                            } else {
                                Vec::new()
                            };
                            let plan = Plan {
                                kind,
                                role,
                                text: String::new(),
                                reply_to,
                                about: about.clone(),
                                to,
                            };
                            self.mode = if kind.asks().is_some() {
                                Mode::Text {
                                    plan,
                                    input: String::new(),
                                }
                            } else {
                                Mode::Confirm { plan }
                            };
                        }
                        None => {
                            self.mode = Mode::Role {
                                kind,
                                roles,
                                reply_to,
                                about,
                                typed,
                                address,
                            };
                        }
                    }
                    None
                }
                Mode::Text {
                    mut plan,
                    mut input,
                } => {
                    match code {
                        KeyCode::Esc => return None,
                        KeyCode::Enter => {
                            let optional = plan.kind.asks().is_some_and(|(_, o)| o);
                            if optional || !input.trim().is_empty() {
                                plan.text = input.trim().to_owned();
                                self.mode = Mode::Confirm { plan };
                                return None;
                            }
                        }
                        KeyCode::Backspace => {
                            input.pop();
                        }
                        KeyCode::Char(c) => input.push(c),
                        _ => {}
                    }
                    self.mode = Mode::Text { plan, input };
                    None
                }
                Mode::Confirm { plan } => match code {
                    // Only a command shown in full, on this screen, may run.
                    KeyCode::Enter | KeyCode::Char('y') if self.fits(runner, &plan) => Some(plan),
                    KeyCode::Esc | KeyCode::Char('n') => None,
                    _ => {
                        self.mode = Mode::Confirm { plan };
                        None
                    }
                },
                running @ Mode::Running { .. } => {
                    self.mode = running;
                    None
                }
            }
        }

        fn start(&mut self, kind: Kind, snap: &Snapshot) {
            let v = snap.session.as_ref();
            let refuse = |text: String| {
                Some(Outcome {
                    code: -1,
                    text: format!("{}: {text}", kind.label()),
                })
            };
            let Some(v) = v else {
                if kind == Kind::Resume {
                    self.mode = Mode::Role {
                        kind,
                        roles: ROLES.iter().map(|r| (*r).to_owned()).collect(),
                        reply_to: None,
                        about: String::new(),
                        typed: String::new(),
                        address: false,
                    };
                } else {
                    self.last = refuse("no active session is shown".to_owned());
                }
                return;
            };
            if kind.about_handoff() && v.handoff.is_none() {
                self.last = refuse("no handoff is pending".to_owned());
                return;
            }
            if kind == Kind::Answer {
                let threaded = v.schema >= 2;
                let mut waits = open_waits(v);
                match waits.len() {
                    0 => self.last = refuse("no open QUESTION or ESCALATE to answer".to_owned()),
                    1 => self.answer(waits.remove(0), threaded),
                    _ => {
                        self.mode = Mode::PickWait {
                            waits,
                            threaded,
                            typed: String::new(),
                        }
                    }
                }
                return;
            }
            self.mode = Mode::Role {
                kind,
                roles: v.participants.iter().map(|p| p.role.clone()).collect(),
                reply_to: None,
                about: String::new(),
                typed: String::new(),
                address: kind == Kind::Stop && v.schema >= 2,
            };
        }

        /// Answer `w`: only a role it still waits on may, since the writer
        /// counts an answer only from a pending role. Schema 2 threads the
        /// reply to it; schema 1 has no threads.
        fn answer(&mut self, w: Wait, threaded: bool) {
            if w.pending.is_empty() {
                self.last = Some(Outcome {
                    code: -1,
                    text: format!("answer: no role is still owed an answer to {}", w.id),
                });
                return;
            }
            self.mode = Mode::Role {
                kind: Kind::Answer,
                about: w.about(),
                reply_to: threaded.then(|| w.id.clone()),
                roles: w.pending,
                typed: String::new(),
                address: false,
            };
        }

        /// The confirmation's text: what it answers, the command line, and
        /// the keys.
        fn confirmation(runner: &Runner, plan: &Plan) -> Result<String, String> {
            let about = if plan.about.is_empty() {
                String::new()
            } else {
                format!("answering {}: ", plan.about)
            };
            Ok(format!(
                "{about}run {} ?  y/Enter runs, Esc cancels",
                shown(runner, plan)?
            ))
        }

        /// Whether the whole confirmation fits the screen as last measured.
        fn fits(&self, runner: &Runner, plan: &Plan) -> bool {
            Self::confirmation(runner, plan).is_ok_and(|c| self.overflow(&c).is_none())
        }

        pub(super) fn running(&mut self, plan: &Plan) {
            self.mode = Mode::Running { plan: plan.clone() };
        }

        pub(super) fn finished(&mut self, out: Outcome) {
            self.mode = Mode::Idle;
            self.last = Some(out);
        }

        pub(super) fn view(&self, runner: &Runner) -> PromptView {
            match &self.mode {
                Mode::Idle => self.last.clone().map_or(PromptView::Keys, PromptView::Done),
                Mode::PickWait { waits, typed, .. } => {
                    let list: Vec<String> = waits
                        .iter()
                        .enumerate()
                        .map(|(i, w)| format!("{} {}", i + 1, w.about()))
                        .collect();
                    PromptView::Ask(format!(
                        "answer which? {}  {}(Esc cancels)",
                        list.join("  "),
                        typed_hint(typed)
                    ))
                }
                Mode::Role {
                    kind,
                    roles,
                    about,
                    typed,
                    ..
                } => {
                    let list: Vec<String> = roles
                        .iter()
                        .enumerate()
                        .map(|(i, r)| format!("{} {r}", i + 1))
                        .collect();
                    let about = if about.is_empty() {
                        String::new()
                    } else {
                        format!(" ({about})")
                    };
                    PromptView::Ask(format!(
                        "{}{about} as which role? {}  {}(Esc cancels)",
                        kind.label(),
                        list.join("  "),
                        typed_hint(typed)
                    ))
                }
                Mode::Text { plan, input } => PromptView::Ask(format!(
                    "{} as {}, {}: {input}_  (Enter, Esc cancels)",
                    plan.kind.label(),
                    plan.role,
                    plan.kind.asks().map_or("", |(q, _)| q)
                )),
                Mode::Confirm { plan } => PromptView::Ask(match Self::confirmation(runner, plan) {
                    Ok(c) => match self.overflow(&c) {
                        Some((need, have)) => format!(
                            "the command needs {need} rows and this screen gives it {have}, so it cannot be confirmed here: Esc, then shorten the text or enlarge the terminal"
                        ),
                        None => c,
                    },
                    Err(e) => format!("cannot run: {e}  (Esc)"),
                }),
                Mode::Running { plan } => PromptView::Ask(format!(
                    "running {} ...",
                    shown(runner, plan).unwrap_or_default()
                )),
            }
        }
    }

    /// Most rows the action line may take: half the screen.
    pub(super) fn action_rows(height: u16) -> usize {
        usize::from((height / 2).max(1))
    }

    /// `text` cut into rows of `width` terminal cells. A character never
    /// straddles a row edge: a wide one that does not fit starts the next row.
    pub(super) fn rows(text: &str, width: u16) -> Vec<String> {
        use unicode_width::UnicodeWidthChar;
        let width = usize::from(width.max(1));
        let mut out = Vec::new();
        let (mut row, mut used) = (String::new(), 0);
        for c in text.chars() {
            let w = c.width().unwrap_or(0);
            if used + w > width && !row.is_empty() {
                out.push(std::mem::take(&mut row));
                used = 0;
            }
            row.push(c);
            used += w;
        }
        if !row.is_empty() || out.is_empty() {
            out.push(row);
        }
        out
    }

    /// A number typed to choose one of `len` items (1 is the first). It is
    /// taken as soon as no longer number could follow it, or on Enter; there
    /// is never a default. Returns the chosen index.
    fn choose(typed: &mut String, code: KeyCode, len: usize) -> Option<usize> {
        let valid = |s: &str| s.parse::<usize>().ok().filter(|n| (1..=len).contains(n));
        match code {
            KeyCode::Char(c) if c.is_ascii_digit() => {
                typed.push(c);
                match valid(typed) {
                    // Nothing longer can follow: take it now.
                    Some(n) if n.saturating_mul(10) > len => {
                        typed.clear();
                        Some(n - 1)
                    }
                    Some(_) => None,
                    None => {
                        typed.clear();
                        None
                    }
                }
            }
            KeyCode::Enter => {
                let n = valid(typed);
                typed.clear();
                n.map(|n| n - 1)
            }
            KeyCode::Backspace => {
                typed.pop();
                None
            }
            _ => None,
        }
    }

    /// The number typed so far, as the prompt shows it.
    fn typed_hint(typed: &str) -> String {
        if typed.is_empty() {
            String::new()
        } else {
            format!("> {typed}_ (Enter)  ")
        }
    }

    const fn char_of(code: KeyCode) -> Option<char> {
        match code {
            KeyCode::Char(c) => Some(c),
            _ => None,
        }
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use localpilot_mesh::ops::snapshot::ParticipantView;
        use serde_json::json;

        fn runner(reference: &[&str]) -> Runner {
            Runner {
                exe: "lp".into(),
                anchor: "/repo".into(),
                reference: reference.iter().map(|s| (*s).to_owned()).collect(),
            }
        }

        fn snap(v: SessionView) -> Snapshot {
            Snapshot {
                session: Some(v),
                consistent: true,
                ..Snapshot::default()
            }
        }

        fn session() -> SessionView {
            SessionView {
                schema: 2,
                participants: ["claude", "codex", "localpilot"]
                    .iter()
                    .map(|r| ParticipantView {
                        role: (*r).to_owned(),
                        ..ParticipantView::default()
                    })
                    .collect(),
                handoff: Some(json!({"from": "claude", "to": "codex"})),
                waiting: json!({}),
                ..SessionView::default()
            }
        }

        fn keys(p: &mut Prompt, s: &Snapshot, ks: &[KeyCode]) -> Option<Plan> {
            let mut out = None;
            for k in ks {
                out = p.key(*k, s, &runner(&[]));
            }
            out
        }

        fn chars(text: &str) -> Vec<KeyCode> {
            text.chars().map(KeyCode::Char).collect()
        }

        fn plan(kind: Kind, role: &str, text: &str, reply_to: Option<&str>) -> Plan {
            Plan {
                kind,
                role: role.into(),
                text: text.into(),
                reply_to: reply_to.map(str::to_owned),
                about: String::new(),
                to: Vec::new(),
            }
        }

        fn asked(p: &Prompt, r: &Runner) -> String {
            match p.view(r) {
                PromptView::Ask(q) => q,
                _ => panic!("nothing asked"),
            }
        }

        #[test]
        fn each_action_is_one_protocol_command_with_the_actor_claim() {
            let cases = [
                (
                    plan(Kind::Stop, "codex", "wrong file", None),
                    "post --role codex --kind STOP --body wrong file",
                ),
                (
                    plan(Kind::Answer, "claude", "yes", Some("codex:4")),
                    "post --role claude --kind ANSWER --body yes --reply-to codex:4",
                ),
                (
                    plan(Kind::Answer, "claude", "yes", None),
                    "post --role claude --kind ANSWER --body yes",
                ),
                (
                    plan(Kind::Accept, "codex", "", None),
                    "handoff-accept --role codex",
                ),
                (
                    plan(Kind::Decline, "codex", "", None),
                    "handoff-decline --role codex",
                ),
                (
                    plan(Kind::Withdraw, "claude", "", None),
                    "handoff-withdraw --role claude",
                ),
                (
                    plan(Kind::Park, "claude", "lunch", None),
                    "park --role claude --reason lunch",
                ),
                (plan(Kind::Park, "claude", "", None), "park --role claude"),
                (
                    plan(Kind::Resume, "claude", "", None),
                    "resume --role claude",
                ),
            ];
            for (p, want) in cases {
                assert_eq!(p.args().join(" "), format!("{want} --actor human"));
            }
        }

        #[test]
        fn the_confirmation_shows_the_exact_command_line_that_runs() {
            // Native: this program with `mesh` and the anchor. Delegated: the
            // configured program and its own arguments first. Arguments with
            // spaces are quoted for display only.
            let native = runner(&[]);
            let delegated = runner(&["python", "C:/pair skill/pair.py"]);
            let s = snap(session());
            let mut p = Prompt::default();
            keys(&mut p, &s, &chars("s1"));
            keys(&mut p, &s, &chars("no go"));
            keys(&mut p, &s, &[KeyCode::Enter]);
            assert_eq!(
                asked(&p, &native),
                "run lp mesh --repo /repo post --role claude --kind STOP --body \"no go\" --to codex,localpilot --actor human ?  y/Enter runs, Esc cancels"
            );
            let mut p = Prompt::default();
            keys(&mut p, &s, &chars("p1"));
            keys(&mut p, &s, &[KeyCode::Enter]);
            assert_eq!(
                asked(&p, &delegated),
                "run python \"C:/pair skill/pair.py\" --repo /repo park --role claude --actor human ?  y/Enter runs, Esc cancels"
            );
            assert!(
                asked(&p, &native).starts_with("cannot run: park and resume are pair.py commands")
            );
            // What is shown is what runs.
            let pl = plan(Kind::Park, "claude", "", None);
            let argv: Vec<String> = delegated
                .argv(&pl)
                .unwrap()
                .iter()
                .map(|a| a.to_string_lossy().into_owned())
                .collect();
            assert_eq!(
                argv,
                [
                    "python",
                    "C:/pair skill/pair.py",
                    "--repo",
                    "/repo",
                    "park",
                    "--role",
                    "claude",
                    "--actor",
                    "human"
                ]
            );
        }

        #[test]
        fn a_stop_in_an_n_party_session_names_everyone_else() {
            // Only an ESCALATE may be broadcast, so a schema-2 STOP names the
            // other participants; schema 1 has an implicit peer.
            let s = snap(session());
            let mut p = Prompt::default();
            keys(&mut p, &s, &chars("s2no"));
            let got = keys(&mut p, &s, &[KeyCode::Enter, KeyCode::Enter]).unwrap();
            assert_eq!(
                got.args().join(" "),
                "post --role codex --kind STOP --body no --to claude,localpilot --actor human"
            );
            let mut v = session();
            v.schema = 1;
            let s = snap(v);
            let mut p = Prompt::default();
            keys(&mut p, &s, &chars("s2no"));
            let got = keys(&mut p, &s, &[KeyCode::Enter, KeyCode::Enter]).unwrap();
            assert_eq!(
                got.args().join(" "),
                "post --role codex --kind STOP --body no --actor human"
            );
        }

        #[test]
        fn a_decline_asks_for_the_role_then_confirms_then_runs() {
            let s = snap(session());
            let r = runner(&[]);
            let mut p = Prompt::default();
            assert_eq!(keys(&mut p, &s, &[KeyCode::Char('d')]), None);
            assert!(
                asked(&p, &r)
                    .contains("decline the handoff as which role? 1 claude  2 codex  3 localpilot"),
                "{}",
                asked(&p, &r)
            );
            // No default role: a key that names none changes nothing.
            assert_eq!(
                keys(&mut p, &s, &[KeyCode::Enter, KeyCode::Char('9')]),
                None
            );
            assert_eq!(keys(&mut p, &s, &[KeyCode::Char('2')]), None);
            assert!(asked(&p, &r).starts_with(
                "run lp mesh --repo /repo handoff-decline --role codex --actor human ?"
            ));
            assert_eq!(
                keys(&mut p, &s, &[KeyCode::Char('y')]),
                Some(plan(Kind::Decline, "codex", "", None))
            );
        }

        #[test]
        fn a_stop_needs_its_reason_and_escape_cancels_anywhere() {
            let s = snap(session());
            let mut p = Prompt::default();
            keys(
                &mut p,
                &s,
                &[KeyCode::Char('s'), KeyCode::Char('1'), KeyCode::Enter],
            );
            assert!(p.is_open(), "an empty STOP is not confirmed");
            let got = keys(
                &mut p,
                &s,
                &[
                    KeyCode::Char('n'),
                    KeyCode::Char('o'),
                    KeyCode::Backspace,
                    KeyCode::Char('!'),
                    KeyCode::Enter,
                    KeyCode::Enter,
                ],
            );
            assert_eq!(
                got,
                Some(Plan {
                    to: vec!["codex".into(), "localpilot".into()],
                    ..plan(Kind::Stop, "claude", "n!", None)
                })
            );
            for at in 1..=3 {
                let mut p = Prompt::default();
                let ks = [
                    KeyCode::Char('s'),
                    KeyCode::Char('1'),
                    KeyCode::Char('x'),
                    KeyCode::Enter,
                ];
                keys(&mut p, &s, &ks[..at]);
                assert!(p.is_open());
                assert_eq!(p.key(KeyCode::Esc, &s, &runner(&[])), None);
                assert!(!p.is_open(), "Esc after {at} keys");
            }
        }

        #[test]
        fn with_several_open_waits_the_human_picks_one_with_no_default() {
            // Two waits in the same second, and a later one: all are listed,
            // oldest first, and nothing is chosen for the human.
            let mut v = session();
            v.waiting = json!({
                "codex:3": {"kind": "ESCALATE", "from": "codex", "pending": ["claude"], "since": "2026-09-29T12:00:00Z"},
                "localpilot:2": {"kind": "QUESTION", "from": "localpilot", "pending": ["claude", "codex"], "since": "2026-09-29T11:00:00Z"},
                "codex:1": {"kind": "QUESTION", "from": "codex", "pending": ["localpilot"], "since": "2026-09-29T11:00:00Z"},
                "claude:9": {"kind": "REVIEW_REQUEST", "from": "claude", "pending": ["codex"], "since": "2026-09-29T13:00:00Z"},
            });
            let s = snap(v);
            let r = runner(&[]);
            let mut p = Prompt::default();
            keys(&mut p, &s, &[KeyCode::Char('a')]);
            assert_eq!(
                asked(&p, &r),
                "answer which? 1 codex:1 QUESTION from codex  2 localpilot:2 QUESTION from localpilot  3 codex:3 ESCALATE from codex  (Esc cancels)"
            );
            keys(
                &mut p,
                &s,
                &[KeyCode::Enter, KeyCode::Char('4'), KeyCode::Char('x')],
            );
            assert!(
                asked(&p, &r).starts_with("answer which?"),
                "no default wait"
            );
            // Each same-second wait keeps its own id through to the command.
            for (n, id, roles) in [
                ('1', "codex:1", "1 localpilot  (Esc"),
                ('2', "localpilot:2", "1 claude  2 codex  (Esc"),
            ] {
                let mut p = Prompt::default();
                keys(&mut p, &s, &[KeyCode::Char('a'), KeyCode::Char(n)]);
                let q = asked(&p, &r);
                assert!(q.starts_with(&format!("answer ({id} ")), "{q}");
                assert!(
                    q.ends_with(&format!("as which role? {roles} cancels)")),
                    "{q}"
                );
                keys(&mut p, &s, &chars("1ok"));
                keys(&mut p, &s, &[KeyCode::Enter]);
                let q = asked(&p, &r);
                assert!(q.starts_with(&format!("answering {id} ")), "{q}");
                assert!(
                    q.contains(&format!("--reply-to {id} --actor human ?")),
                    "{q}"
                );
                let got = keys(&mut p, &s, &[KeyCode::Enter]).unwrap();
                assert_eq!(got.reply_to.as_deref(), Some(id));
            }
        }

        #[test]
        fn any_of_many_waits_can_be_chosen_by_its_number() {
            // Twelve open waits: 1 could still become 10, 11 or 12, so it
            // waits for another digit or Enter; 10 and 12 are taken at once.
            let mut v = session();
            let mut w = serde_json::Map::new();
            for n in 1..=12 {
                w.insert(
                    format!("codex:{n}"),
                    json!({"kind": "QUESTION", "from": "codex", "pending": ["claude"], "since": format!("2026-09-29T10:00:{n:02}Z")}),
                );
            }
            v.waiting = Value::Object(w);
            let s = snap(v);
            let r = runner(&[]);
            for (typed, id) in [
                (vec![KeyCode::Char('1'), KeyCode::Char('0')], "codex:10"),
                (vec![KeyCode::Char('1'), KeyCode::Char('2')], "codex:12"),
                (vec![KeyCode::Char('1'), KeyCode::Enter], "codex:1"),
                (vec![KeyCode::Char('9')], "codex:9"),
                (
                    vec![
                        KeyCode::Char('1'),
                        KeyCode::Backspace,
                        KeyCode::Char('1'),
                        KeyCode::Char('1'),
                    ],
                    "codex:11",
                ),
            ] {
                let mut p = Prompt::default();
                keys(&mut p, &s, &[KeyCode::Char('a')]);
                keys(&mut p, &s, &typed);
                let q = asked(&p, &r);
                assert!(
                    q.starts_with(&format!("answer ({id} QUESTION")),
                    "{typed:?}: {q}"
                );
            }
            // A 1 alone is shown and waits: it could become 10, 11 or 12.
            let mut p = Prompt::default();
            keys(&mut p, &s, &[KeyCode::Char('a'), KeyCode::Char('1')]);
            assert!(asked(&p, &r).contains("> 1_ (Enter)"), "{}", asked(&p, &r));
            // Still no default: Enter alone, 0 and 13 choose nothing.
            let mut p = Prompt::default();
            keys(
                &mut p,
                &s,
                &[
                    KeyCode::Char('a'),
                    KeyCode::Enter,
                    KeyCode::Char('0'),
                    KeyCode::Char('1'),
                    KeyCode::Char('3'),
                ],
            );
            assert!(
                asked(&p, &r).starts_with("answer which?"),
                "{}",
                asked(&p, &r)
            );
        }

        #[test]
        fn a_confirmation_the_screen_cannot_show_in_full_cannot_run() {
            let s = snap(session());
            let r = runner(&[]);
            let mut p = Prompt::default();
            keys(&mut p, &s, &chars("s1"));
            keys(&mut p, &s, &chars(&"x".repeat(500)));
            keys(&mut p, &s, &[KeyCode::Enter]);
            // 60 columns and half of 20 rows: 10 rows, and the command needs 11.
            p.set_room(60, 20);
            let q = asked(&p, &r);
            assert!(
                q.starts_with("the command needs 11 rows and this screen gives it 10, so it cannot be confirmed here"),
                "{q}"
            );
            assert_eq!(
                keys(&mut p, &s, &[KeyCode::Char('y'), KeyCode::Enter]),
                None
            );
            assert!(p.is_open(), "still waiting for Esc");
            // A larger screen shows it all, and then it may run.
            p.set_room(120, 40);
            assert!(asked(&p, &r).ends_with("--actor human ?  y/Enter runs, Esc cancels"));
            assert!(keys(&mut p, &s, &[KeyCode::Char('y')]).is_some());
        }

        #[test]
        fn rows_are_measured_in_terminal_cells() {
            // A wide character takes two cells and never straddles an edge.
            assert_eq!(rows("aaaaaaaaa界b", 10), ["aaaaaaaaa", "界b"]);
            assert_eq!(rows("aaaaaaaa界b", 10), ["aaaaaaaa界", "b"]);
            assert_eq!(rows("", 10), [""]);
            // 250 wide characters: fewer characters than 10 rows of 60 hold,
            // but more cells, so the confirmation cannot run there.
            let s = snap(session());
            let r = runner(&[]);
            let mut p = Prompt::default();
            keys(&mut p, &s, &chars("s1"));
            keys(&mut p, &s, &chars(&"界".repeat(250)));
            keys(&mut p, &s, &[KeyCode::Enter]);
            p.set_room(60, 20);
            assert!(
                asked(&p, &r).starts_with("the command needs 11 rows"),
                "{}",
                asked(&p, &r)
            );
            assert_eq!(keys(&mut p, &s, &[KeyCode::Char('y')]), None);
            p.set_room(120, 40);
            assert!(keys(&mut p, &s, &[KeyCode::Char('y')]).is_some());
        }

        #[test]
        fn only_a_role_the_wait_is_pending_on_may_answer_it() {
            // The writer clears a wait only for a pending role, so offering
            // any other role would report success and leave it open.
            let mut v = session();
            v.waiting = json!({"codex:3": {"kind": "ESCALATE", "from": "codex", "pending": ["localpilot"], "since": "2026-09-29T12:00:00Z"}});
            let s = snap(v);
            let r = runner(&[]);
            let mut p = Prompt::default();
            keys(&mut p, &s, &[KeyCode::Char('a')]);
            assert!(
                asked(&p, &r).ends_with("as which role? 1 localpilot  (Esc cancels)"),
                "{}",
                asked(&p, &r)
            );
            keys(&mut p, &s, &[KeyCode::Char('2'), KeyCode::Char('3')]);
            assert!(
                asked(&p, &r).contains("as which role?"),
                "claude and codex are not offered"
            );
            let got = keys(
                &mut p,
                &s,
                &[
                    KeyCode::Char('1'),
                    KeyCode::Char('y'),
                    KeyCode::Enter,
                    KeyCode::Enter,
                ],
            );
            assert_eq!(got.map(|g| g.role), Some("localpilot".to_owned()));
            // Nobody left to answer: said, and no role asked for.
            let mut v = session();
            v.waiting = json!({"codex:3": {"kind": "ESCALATE", "from": "codex", "pending": [], "since": "x"}});
            let mut p = Prompt::default();
            p.key(KeyCode::Char('a'), &snap(v), &runner(&[]));
            assert!(!p.is_open());
            match p.view(&r) {
                PromptView::Done(o) => {
                    assert_eq!(o.text, "answer: no role is still owed an answer to codex:3")
                }
                _ => panic!("nothing said"),
            }
        }

        #[test]
        fn what_cannot_apply_is_said_without_asking_for_a_role() {
            let mut v = session();
            v.handoff = None;
            let s = snap(v);
            for (k, why) in [
                ('a', "answer: no open QUESTION or ESCALATE to answer"),
                ('h', "accept the handoff: no handoff is pending"),
                ('w', "withdraw the handoff: no handoff is pending"),
            ] {
                let mut p = Prompt::default();
                p.key(KeyCode::Char(k), &s, &runner(&[]));
                assert!(!p.is_open());
                match p.view(&runner(&[])) {
                    PromptView::Done(o) => assert_eq!((o.code, o.text.as_str()), (-1, why)),
                    _ => panic!("{k}: nothing said"),
                }
            }
            let none = Snapshot::default();
            let mut p = Prompt::default();
            p.key(KeyCode::Char('s'), &none, &runner(&[]));
            assert!(!p.is_open());
            p.key(KeyCode::Char('r'), &none, &runner(&[]));
            assert!(asked(&p, &runner(&[])).contains("1 claude  2 codex  3 localpilot"));
        }

        #[test]
        fn schema_one_waits_are_answered_by_their_pending_role_without_a_thread() {
            // A schema-1 session keys its waits by msg_id too, but its records
            // carry no msg_id and a reply is not threaded.
            let r = runner(&[]);
            let mut v = session();
            v.schema = 1;
            v.waiting = json!({"codex:2": {"kind": "ESCALATE", "from": "codex", "pending": ["claude"], "since": "2026-09-29T17:14:45Z"}});
            let s = snap(v);
            let mut p = Prompt::default();
            keys(&mut p, &s, &[KeyCode::Char('a')]);
            assert!(
                asked(&p, &r).starts_with(
                    "answer (codex:2 ESCALATE from codex) as which role? 1 claude  (Esc"
                ),
                "{}",
                asked(&p, &r)
            );
            let got = keys(
                &mut p,
                &s,
                &[KeyCode::Char('1'), KeyCode::Char('y'), KeyCode::Enter],
            );
            assert_eq!(got, None);
            assert!(asked(&p, &r).starts_with("answering codex:2 ESCALATE from codex: run lp mesh --repo /repo post --role claude --kind ANSWER --body y --actor human ?"), "{}", asked(&p, &r));
            let got = keys(&mut p, &s, &[KeyCode::Enter]).unwrap();
            assert_eq!(got.reply_to, None);
            // The legacy single wait: its for_role answers.
            let mut v = session();
            v.schema = 1;
            v.waiting =
                json!({"from_role": "codex", "for_role": "claude", "kind": "QUESTION", "seq": 4});
            let mut p = Prompt::default();
            p.key(KeyCode::Char('a'), &snap(v), &runner(&[]));
            assert!(
                asked(&p, &r).starts_with(
                    "answer (codex:4 QUESTION from codex) as which role? 1 claude  (Esc"
                ),
                "{}",
                asked(&p, &r)
            );
        }
    }
}

#[cfg(feature = "tui")]
mod view {
    //! The full-screen view: a pure rendering of one snapshot, and a loop
    //! that re-reads the snapshot every half second until `q` or `Esc`.

    use std::io::{self, Stdout};
    use std::time::{Duration, Instant};

    use super::act::{action_rows, rows, Outcome, Prompt, PromptView, Runner};
    use crossterm::event::{self, Event, KeyCode, KeyEventKind, KeyModifiers};
    use crossterm::terminal::{self, EnterAlternateScreen, LeaveAlternateScreen};
    use localpilot_mesh::ops::snapshot::{ParticipantView, SessionView};
    use localpilot_mesh::ops::Snapshot;
    use localpilot_mesh::Mesh;
    use localpilot_terminal_ui::{sanitize_text, ColorSupport, Theme, ThemeResolver, UiRole};
    use ratatui::backend::CrosstermBackend;
    use ratatui::layout::{Constraint, Layout, Rect};
    use ratatui::style::{Modifier, Style};
    use ratatui::text::{Line, Span};
    use ratatui::widgets::{Block, Borders, Clear, Paragraph};
    use ratatui::{Frame, Terminal};
    use serde_json::Value;

    /// How often the snapshot is re-read.
    const REFRESH: Duration = Duration::from_millis(500);
    /// How many records the activity pane shows.
    const ACTIVITY: usize = 200;

    /// Raw mode and the alternate screen, left on drop whatever happened.
    struct Screen;

    impl Screen {
        fn enter() -> io::Result<Self> {
            terminal::enable_raw_mode()?;
            if let Err(e) = crossterm::execute!(io::stdout(), EnterAlternateScreen) {
                let _ = terminal::disable_raw_mode();
                return Err(e);
            }
            Ok(Self)
        }
    }

    impl Drop for Screen {
        fn drop(&mut self) {
            let _ = crossterm::execute!(io::stdout(), LeaveAlternateScreen);
            let _ = terminal::disable_raw_mode();
        }
    }

    pub(super) fn run(mesh: &Mesh, runner: &Runner) -> io::Result<()> {
        let _screen = Screen::enter()?;
        let mut terminal: Terminal<CrosstermBackend<Stdout>> =
            Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let styles = Styles::from_env();
        let mut snap = mesh.snapshot();
        let mut scroll: u16 = 0;
        let mut prompt = Prompt::default();
        let mut last = Instant::now();
        loop {
            terminal.draw(|f| draw(f, &snap, &styles, scroll, &prompt, runner))?;
            let wait = REFRESH.saturating_sub(last.elapsed());
            if event::poll(wait)? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    if k.code == KeyCode::Char('c') && k.modifiers.contains(KeyModifiers::CONTROL) {
                        return Ok(());
                    }
                    let size = terminal.size()?;
                    prompt.set_room(size.width, size.height);
                    if prompt.is_open() {
                        // A prompt takes every key until it runs or is cancelled.
                        if let Some(plan) = prompt.key(k.code, &snap, runner) {
                            prompt.running(&plan);
                            terminal.draw(|f| draw(f, &snap, &styles, scroll, &prompt, runner))?;
                            let out = runner.run(&plan);
                            prompt.finished(out);
                            snap = mesh.snapshot();
                            last = Instant::now();
                        }
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Up => scroll = scroll.saturating_add(1),
                        KeyCode::Down => scroll = scroll.saturating_sub(1),
                        KeyCode::PageUp => scroll = scroll.saturating_add(10),
                        KeyCode::PageDown => scroll = scroll.saturating_sub(10),
                        KeyCode::Home => scroll = 0,
                        other => {
                            prompt.key(other, &snap, runner);
                        }
                    }
                }
            }
            if last.elapsed() >= REFRESH {
                snap = mesh.snapshot();
                last = Instant::now();
            }
        }
    }

    /// The styles the view uses, from the shared theme.
    pub(super) struct Styles {
        text: Style,
        muted: Style,
        accent: Style,
        warn: Style,
        error: Style,
        ok: Style,
        border: Style,
    }

    impl Styles {
        fn from_env() -> Self {
            let color = if std::env::var_os("NO_COLOR").is_some() {
                ColorSupport::NoColor
            } else {
                ColorSupport::Color
            };
            Self::with(ThemeResolver::new(Theme::Default, color))
        }

        pub(super) fn with(r: ThemeResolver) -> Self {
            Self {
                text: r.ui(UiRole::Foreground),
                muted: r.ui(UiRole::Muted),
                accent: r.ui(UiRole::Accent).add_modifier(Modifier::BOLD),
                warn: r.ui(UiRole::Warning),
                error: r.ui(UiRole::Error),
                ok: r.ui(UiRole::Success),
                border: r.ui(UiRole::Border),
            }
        }
    }

    fn clean(s: &str) -> String {
        sanitize_text(s).replace(['\r', '\n', '\t'], " ")
    }

    fn first_line(v: &Value) -> String {
        clean(
            v.as_str()
                .unwrap_or_default()
                .lines()
                .next()
                .unwrap_or_default(),
        )
    }

    fn text(v: &Value, k: &str) -> String {
        clean(v.get(k).and_then(Value::as_str).unwrap_or_default())
    }

    fn block<'a>(title: &'a str, s: &Styles) -> Block<'a> {
        Block::default()
            .borders(Borders::ALL)
            .border_style(s.border)
            .title(Span::styled(format!(" {title} "), s.accent))
    }

    /// Draw one snapshot and the action prompt. Pure: everything shown comes
    /// from `snap`, `prompt` and the command lines `runner` would run.
    pub(super) fn draw(
        f: &mut Frame,
        snap: &Snapshot,
        s: &Styles,
        scroll: u16,
        prompt: &Prompt,
        runner: &Runner,
    ) {
        let area = f.area();
        let Some(v) = &snap.session else {
            // A failed read is not "no session": say which it is.
            let lines = if snap.read_errors.is_empty() && snap.consistent {
                vec![
                    Line::styled("No active pair session in this tree.", s.text),
                    Line::styled("Waiting; q quits.", s.muted),
                ]
            } else {
                vec![
                    Line::styled("The session could not be read.", s.error),
                    Line::styled("Retrying every 0.5 s; q quits.", s.muted),
                ]
            };
            let action = prompt_lines(prompt, runner, s, area.width, area.height);
            let [body, ask, foot] = Layout::vertical([
                Constraint::Min(3),
                Constraint::Length(1),
                Constraint::Length(1),
            ])
            .areas(area);
            f.render_widget(Paragraph::new(lines).block(block("cockpit", s)), body);
            render_action(f, ask, action);
            f.render_widget(footer(snap, s), foot);
            return;
        };
        let rows = v.participants.len() as u16;
        let waits = waits_and_pauses(v, s);
        let action = prompt_lines(prompt, runner, s, area.width, area.height);
        let [head, people, review, pending, activity, ask, foot] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(rows + 3),
            Constraint::Length(6),
            Constraint::Length(waits.len() as u16 + 2),
            Constraint::Min(3),
            Constraint::Length(1),
            Constraint::Length(1),
        ])
        .areas(area);
        f.render_widget(header(v, s), head);
        f.render_widget(participants(v, s), people);
        f.render_widget(review_pane(v, s), review);
        f.render_widget(
            Paragraph::new(waits).block(block("waits and pauses", s)),
            pending,
        );
        f.render_widget(activity_pane(v, s, activity, scroll), activity);
        render_action(f, ask, action);
        f.render_widget(footer(snap, s), foot);
    }

    /// Draw the action line in its reserved row. When it needs more rows, it
    /// is drawn over the panes above, up to half the screen, so the rows it
    /// gets are exactly the rows `room` counts, whatever the panes need.
    fn render_action(f: &mut Frame, row: Rect, action: Vec<Line>) {
        let rows = (action.len() as u16).min(row.y + 1);
        let area = Rect {
            x: row.x,
            y: row.y + 1 - rows,
            width: row.width,
            height: rows,
        };
        f.render_widget(Clear, area);
        f.render_widget(Paragraph::new(action), area);
    }

    /// The action line: the open prompt, or the last action's outcome, or
    /// the keys that start one. It is cut into rows of the screen's width
    /// rather than clipped; a confirmation too long for its rows cannot be
    /// confirmed (see `Prompt::fits`), so nothing runs unseen.
    fn prompt_lines<'a>(
        p: &Prompt,
        runner: &Runner,
        s: &Styles,
        width: u16,
        height: u16,
    ) -> Vec<Line<'a>> {
        let (text, style) = prompt_text(p, runner, s);
        rows(&text, width)
            .into_iter()
            .take(action_rows(height))
            .map(|r| Line::styled(r, style))
            .collect()
    }

    fn prompt_text(p: &Prompt, runner: &Runner, s: &Styles) -> (String, Style) {
        match p.view(runner) {
            PromptView::Keys => (
                "s STOP · a answer · h accept · d decline · w withdraw · p park · r resume (each asks for the role)".to_owned(),
                s.muted,
            ),
            PromptView::Ask(text) => (clean(&text), s.accent),
            PromptView::Done(Outcome { code, text }) => {
                let style = match code {
                    0 => s.ok,
                    7 | 6 => s.warn,
                    _ => s.error,
                };
                let label = if code < 0 {
                    "not run".to_owned()
                } else {
                    format!("exit {code}")
                };
                let text = clean(&text);
                if text.is_empty() {
                    (label, style)
                } else {
                    (format!("{label}: {text}"), style)
                }
            }
        }
    }

    fn header<'a>(v: &SessionView, s: &Styles) -> Paragraph<'a> {
        let unit = format!(
            "{}#{}",
            clean(v.work_unit.as_deref().unwrap_or("-")),
            clean(v.unit_id.as_deref().unwrap_or("-"))
        );
        let reviewers = if v.required_reviewers.is_empty() {
            "-".to_owned()
        } else {
            v.required_reviewers.join(", ")
        };
        let mut lines = vec![
            Line::from(vec![
                Span::styled(format!("{}  ", clean(&v.session_id)), s.accent),
                Span::styled(
                    format!("{} / {}", clean(&v.status), clean(&v.phase)),
                    s.text,
                ),
                Span::styled(format!("   unit {unit}"), s.muted),
            ]),
            Line::from(vec![
                Span::styled("owner ", s.muted),
                Span::styled(clean(&v.owner), s.text),
                Span::styled("   reviewers ", s.muted),
                Span::styled(clean(&reviewers), s.text),
                Span::styled(format!("   delivery {}", clean(&v.delivery)), s.muted),
            ]),
        ];
        if let Some(h) = &v.handoff {
            lines[1].spans.push(Span::styled(
                format!(
                    "   handoff pending: {} -> {}",
                    text(h, "from"),
                    text(h, "to")
                ),
                s.warn,
            ));
        }
        Paragraph::new(lines).block(block("session", s))
    }

    fn names(v: &Value) -> String {
        match v {
            Value::Array(a) if !a.is_empty() => a
                .iter()
                .filter_map(Value::as_str)
                .map(clean)
                .collect::<Vec<_>>()
                .join(","),
            Value::String(x) => clean(x),
            _ => "-".to_owned(),
        }
    }

    /// Every open wait (a message still owed replies, and by whom) and every
    /// recorded pause (who, why, until when); one line each.
    fn waits_and_pauses<'a>(v: &SessionView, s: &Styles) -> Vec<Line<'a>> {
        let mut lines = Vec::new();
        if let Some(w) = v.waiting.as_object() {
            for (mid, e) in w {
                lines.push(Line::from(vec![
                    Span::styled("wait  ", s.warn),
                    Span::styled(format!("{} {} ", clean(mid), text(e, "kind")), s.text),
                    Span::styled(format!("from {} ", text(e, "from")), s.muted),
                    Span::styled(format!("pending {} ", names(&e["pending"])), s.warn),
                    Span::styled(format!("answered {}", names(&e["answered_by"])), s.muted),
                ]));
            }
        }
        if let Some(pz) = v.pauses.as_object() {
            for (role, p) in pz {
                let until = p
                    .get("resume_at")
                    .and_then(Value::as_str)
                    .filter(|x| !x.is_empty())
                    .map_or_else(|| "-".to_owned(), clean);
                lines.push(Line::from(vec![
                    Span::styled("pause ", s.error),
                    Span::styled(format!("{} ", clean(role)), s.text),
                    Span::styled(format!("reason {} ", text(p, "reason")), s.muted),
                    Span::styled(format!("resume at {until}"), s.muted),
                ]));
            }
        }
        if lines.is_empty() {
            lines.push(Line::styled("no open wait, no pause", s.muted));
        }
        lines
    }

    fn health_style(p: &ParticipantView, s: &Styles) -> Style {
        match p.health.as_str() {
            "ready" | "active" | "working" => s.ok,
            "rate_limited" | "paused" | "offline" => s.error,
            _ => s.warn,
        }
    }

    fn participants<'a>(v: &SessionView, s: &Styles) -> Paragraph<'a> {
        let lines: Vec<Line> = v
            .participants
            .iter()
            .map(|p| {
                let unacked = if p.unacked.is_empty() {
                    "-".to_owned()
                } else {
                    p.unacked
                        .iter()
                        .map(|(snd, a, b)| format!("{snd}:{a}..{b}"))
                        .collect::<Vec<_>>()
                        .join(", ")
                };
                let mut spans = vec![
                    Span::styled(format!("{:<11}", clean(&p.role)), s.text),
                    Span::styled(format!("{:<13}", clean(&p.health)), health_style(p, s)),
                    Span::styled("unacked ", s.muted),
                    Span::styled(clean(&unacked), s.text),
                ];
                if let Some(at) = &p.resume_at {
                    spans.push(Span::styled(format!("   resume at {}", clean(at)), s.warn));
                }
                if p.recent_truncated {
                    spans.push(Span::styled("   (older mail not read)", s.muted));
                }
                Line::from(spans)
            })
            .collect();
        Paragraph::new(lines).block(block("participants", s))
    }

    fn review_pane<'a>(v: &SessionView, s: &Styles) -> Paragraph<'a> {
        let mut lines = Vec::new();
        match (&v.review, v.review_state.as_str()) {
            (Some(r), _) => {
                lines.push(Line::from(vec![
                    Span::styled("request ", s.muted),
                    Span::styled(text(&r.request, "msg_id"), s.text),
                    Span::styled("  ", s.muted),
                    Span::styled(first_line(&r.request["body"]), s.text),
                ]));
                if r.verdicts.is_empty() {
                    lines.push(Line::styled("no verdict names this request yet", s.muted));
                }
                for vd in &r.verdicts {
                    let head = first_line(&vd["body"]);
                    let style = if head.starts_with("AGREE") {
                        s.ok
                    } else {
                        s.warn
                    };
                    lines.push(Line::from(vec![
                        Span::styled(format!("{:<11}", text(vd, "role")), s.text),
                        Span::styled(head, style),
                    ]));
                }
                if !r.unlinked.is_empty() {
                    lines.push(Line::styled(
                        format!(
                            "{} verdict(s) in this unit name no request; not shown as answers",
                            r.unlinked.len()
                        ),
                        s.muted,
                    ));
                }
            }
            (None, "beyond_window") => lines.push(Line::styled(
                "no review request in the part of the owner's journal read",
                s.muted,
            )),
            (None, _) => lines.push(Line::styled("no open review request", s.muted)),
        }
        Paragraph::new(lines).block(block("review", s))
    }

    /// Every participant's recent records, oldest first, newest at the
    /// bottom; `scroll` lines up from the newest.
    fn activity_pane<'a>(v: &SessionView, s: &Styles, area: Rect, scroll: u16) -> Paragraph<'a> {
        let mut all: Vec<&Value> = v
            .participants
            .iter()
            .flat_map(|p| p.recent.iter())
            .collect();
        all.sort_by(|a, b| {
            (text(a, "at"), text(a, "role"), a["seq"].as_i64()).cmp(&(
                text(b, "at"),
                text(b, "role"),
                b["seq"].as_i64(),
            ))
        });
        let start = all.len().saturating_sub(ACTIVITY);
        let lines: Vec<Line> = all[start..]
            .iter()
            .map(|m| {
                let actor = if m.get("actor").and_then(Value::as_str) == Some("human") {
                    " (human, claimed)"
                } else {
                    ""
                };
                // The thread: who it went to, and what it answers.
                let mut thread = String::new();
                if !m["to"].is_null() {
                    thread.push_str(&format!("-> {} ", names(&m["to"])));
                }
                if let Some(r) = m.get("reply_to").and_then(Value::as_str) {
                    thread.push_str(&format!("re {} ", clean(r)));
                }
                Line::from(vec![
                    Span::styled(format!("{} ", text(m, "at")), s.muted),
                    Span::styled(format!("{}#{}{actor} ", text(m, "role"), m["seq"]), s.text),
                    Span::styled(format!("{} ", text(m, "kind")), s.accent),
                    Span::styled(thread, s.muted),
                    Span::styled(first_line(&m["body"]), s.text),
                ])
            })
            .collect();
        // One record, one row (clipped at the pane's edge, never wrapped), so
        // the newest record is always the bottom row.
        let inner = area.height.saturating_sub(2);
        let max_scroll = (lines.len() as u16).saturating_sub(inner);
        let top = max_scroll.saturating_sub(scroll.min(max_scroll));
        Paragraph::new(lines)
            .block(block("activity (Up/Down to scroll)", s))
            .scroll((top, 0))
    }

    fn footer<'a>(snap: &Snapshot, s: &Styles) -> Paragraph<'a> {
        let mut spans = Vec::new();
        if snap.consistent {
            spans.push(Span::styled("consistent", s.ok));
        } else if !snap.inconsistent.is_empty() {
            spans.push(Span::styled(
                format!(
                    "INCONSISTENT: {} changed while reading",
                    snap.inconsistent.join(", ")
                ),
                s.error,
            ));
        } else {
            // Not checked, not changed: the read itself failed (named below).
            spans.push(Span::styled("unverified", s.error));
        }
        if !snap.read_errors.is_empty() {
            spans.push(Span::styled(
                format!("   read errors: {}", clean(&snap.read_errors.join("; "))),
                s.error,
            ));
        }
        spans.push(Span::styled(
            "   refreshes every 0.5 s · writes only through protocol commands · q quits",
            s.muted,
        ));
        Paragraph::new(Line::from(spans))
    }

    #[cfg(test)]
    mod tests {
        use super::*;
        use localpilot_mesh::ops::snapshot::ReviewView;
        use ratatui::backend::TestBackend;
        use serde_json::json;

        fn styles() -> Styles {
            Styles::with(ThemeResolver::new(Theme::Default, ColorSupport::NoColor))
        }

        fn screen(snap: &Snapshot) -> String {
            screen_with(snap, &Prompt::default())
        }

        fn runner() -> Runner {
            Runner {
                exe: "localpilot".into(),
                anchor: "/repo".into(),
                reference: vec!["python".into(), "pair.py".into()],
            }
        }

        fn screen_with(snap: &Snapshot, prompt: &Prompt) -> String {
            let mut t = Terminal::new(TestBackend::new(110, 30)).unwrap();
            t.draw(|f| draw(f, snap, &styles(), 0, prompt, &runner()))
                .unwrap();
            let buf = t.backend().buffer().clone();
            (0..buf.area.height)
                .map(|y| {
                    (0..buf.area.width)
                        .map(|x| buf[(x, y)].symbol().to_owned())
                        .collect::<String>()
                })
                .collect::<Vec<_>>()
                .join("\n")
        }

        fn session() -> SessionView {
            SessionView {
                session_id: "20260929T120000Z-abc".into(),
                status: "active".into(),
                phase: "review".into(),
                owner: "claude".into(),
                required_reviewers: vec!["codex".into(), "localpilot".into()],
                unit_id: Some("1-x".into()),
                work_unit: Some("cockpit".into()),
                delivery: "ack".into(),
                review_state: "open".into(),
                participants: vec![
                    ParticipantView {
                        role: "claude".into(),
                        health: "ready".into(),
                        recent: vec![
                            json!({"role": "claude", "seq": 1, "at": "2026-09-29T12:00:01Z", "kind": "REVIEW_REQUEST", "body": "please review\nmore"}),
                        ],
                        ..ParticipantView::default()
                    },
                    ParticipantView {
                        role: "codex".into(),
                        health: "rate_limited".into(),
                        resume_at: Some("2026-09-29T13:00:00Z".into()),
                        unacked: vec![("claude".into(), 1, 1)],
                        recent: vec![
                            json!({"role": "codex", "seq": 1, "at": "2026-09-29T12:00:02Z", "kind": "NOTE", "actor": "human", "body": "stop \u{1b}[31mnow"}),
                            json!({"role": "codex", "seq": 2, "at": "2026-09-29T12:00:03Z", "kind": "ANSWER", "to": ["claude"], "reply_to": "claude:1", "body": "here is why"}),
                        ],
                        ..ParticipantView::default()
                    },
                ],
                review: Some(ReviewView {
                    request: json!({"msg_id": "claude:1", "body": "please review"}),
                    verdicts: vec![
                        json!({"role": "localpilot", "body": "REVISE round=1 blocking=1 important=0\nfix"}),
                    ],
                    unlinked: vec![json!({"role": "codex"})],
                }),
                handoff: Some(json!({"from": "claude", "to": "codex"})),
                waiting: json!({"claude:1": {"kind": "REVIEW_REQUEST", "from": "claude", "pending": ["localpilot"], "answered_by": ["codex"]}}),
                pauses: json!({"codex": {"reason": "quota", "resume_at": "2026-09-29T13:00:00Z"}}),
                ..SessionView::default()
            }
        }

        #[test]
        fn the_view_shows_authority_health_review_and_activity() {
            let snap = Snapshot {
                session: Some(session()),
                consistent: true,
                ..Snapshot::default()
            };
            let out = screen(&snap);
            for want in [
                "20260929T120000Z-abc",
                "active / review",
                "owner claude",
                "reviewers codex, localpilot",
                "handoff pending: claude -> codex",
                "rate_limited",
                "resume at 2026-09-29T13:00:00Z",
                "unacked claude:1..1",
                "request claude:1",
                "REVISE round=1 blocking=1",
                "1 verdict(s) in this unit name no request",
                "REVIEW_REQUEST please review",
                "codex#1 (human, claimed) NOTE",
                "codex#2 ANSWER -> claude re claude:1 here is why",
                "wait  claude:1 REVIEW_REQUEST from claude pending localpilot answered codex",
                "pause codex reason quota resume at 2026-09-29T13:00:00Z",
                "consistent",
            ] {
                assert!(out.contains(want), "missing {want:?} in:\n{out}");
            }
            // Only the first line of a body, and no terminal escape reaches
            // the screen.
            assert!(!out.contains("more"), "{out}");
            assert!(!out.contains('\u{1b}'), "{out}");
        }

        #[test]
        fn an_inconsistent_or_unreadable_snapshot_says_so() {
            let snap = Snapshot {
                session: Some(session()),
                consistent: false,
                inconsistent: vec!["phase".into(), "health:codex".into()],
                read_errors: vec!["localpilot journal: denied".into()],
            };
            let out = screen(&snap);
            assert!(
                out.contains("INCONSISTENT: phase, health:codex changed while reading"),
                "{out}"
            );
            assert!(
                out.contains("read errors: localpilot journal: denied"),
                "{out}"
            );
        }

        #[test]
        fn a_session_that_cannot_be_read_is_not_shown_as_no_session() {
            let out = screen(&Snapshot {
                consistent: false,
                read_errors: vec!["session record: permission denied".into()],
                ..Snapshot::default()
            });
            assert!(out.contains("The session could not be read."), "{out}");
            assert!(
                out.contains("read errors: session record: permission denied"),
                "{out}"
            );
            assert!(!out.contains("No active pair session"), "{out}");
            assert!(!out.contains("INCONSISTENT"), "{out}");
            assert!(out.contains("unverified"), "{out}");
        }

        #[test]
        fn with_no_open_wait_or_pause_the_pane_says_so() {
            let mut v = session();
            v.waiting = json!({});
            v.pauses = json!({});
            let out = screen(&Snapshot {
                session: Some(v),
                consistent: true,
                ..Snapshot::default()
            });
            assert!(out.contains("no open wait, no pause"), "{out}");
        }

        #[test]
        fn with_no_session_the_view_says_so() {
            let out = screen(&Snapshot {
                consistent: true,
                ..Snapshot::default()
            });
            assert!(
                out.contains("No active pair session in this tree."),
                "{out}"
            );
        }

        #[test]
        fn the_newest_record_is_shown_however_long_the_records_are() {
            let mut v = session();
            v.participants[0].recent = (1..=40)
                .map(|n| json!({"role": "claude", "seq": n, "at": format!("2026-09-29T12:{n:02}:00Z"), "kind": "NOTE", "body": format!("note {n} {}", "long ".repeat(40))}))
                .collect();
            let out = screen(&Snapshot {
                session: Some(v),
                consistent: true,
                ..Snapshot::default()
            });
            assert!(out.contains("claude#40 NOTE note 40"), "{out}");
        }

        #[test]
        fn a_long_confirmation_wraps_and_is_never_cut_off() {
            let snap = Snapshot {
                session: Some(session()),
                consistent: true,
                ..Snapshot::default()
            };
            let runner = Runner {
                exe: "D:/a/very/long/path/to/the/build/output/directory/localpilot.exe".into(),
                anchor: "C:/Users/someone/AppData/Local/Temp/a/deep/scratch/tree/repo".into(),
                reference: Vec::new(),
            };
            let mut p = Prompt::default();
            for k in [KeyCode::Char('d'), KeyCode::Char('2')] {
                p.key(k, &snap, &runner);
            }
            // 24 rows: the panes above take most of them, so the action line
            // must not depend on what they leave.
            let mut t = Terminal::new(TestBackend::new(60, 24)).unwrap();
            t.draw(|f| draw(f, &snap, &styles(), 0, &p, &runner))
                .unwrap();
            let buf = t.backend().buffer().clone();
            let all: String = (0..buf.area.height)
                .flat_map(|y| (0..buf.area.width).map(move |x| (x, y)))
                .map(|(x, y)| buf[(x, y)].symbol().to_owned())
                .collect();
            let want = "run D:/a/very/long/path/to/the/build/output/directory/localpilot.exe mesh --repo C:/Users/someone/AppData/Local/Temp/a/deep/scratch/tree/repo handoff-decline --role codex --actor human ?";
            assert!(all.contains(want), "{all}");
        }

        #[test]
        fn the_prompt_line_shows_keys_then_the_question_then_the_outcome() {
            let snap = Snapshot {
                session: Some(session()),
                consistent: true,
                ..Snapshot::default()
            };
            let mut p = Prompt::default();
            assert!(screen_with(&snap, &p).contains("s STOP · a answer"));
            p.key(KeyCode::Char('d'), &snap, &runner());
            let out = screen_with(&snap, &p);
            assert!(
                out.contains("decline the handoff as which role? 1 claude  2 codex"),
                "{out}"
            );
            p.finished(Outcome {
                code: 7,
                text: "HANDOFF_DECLINED_NOTE_MISSING epoch=2: ...".into(),
            });
            let out = screen_with(&snap, &p);
            assert!(
                out.contains("exit 7: HANDOFF_DECLINED_NOTE_MISSING epoch=2"),
                "{out}"
            );
        }

        #[test]
        fn a_review_before_the_window_is_not_guessed() {
            let mut v = session();
            v.review = None;
            v.review_state = "beyond_window".into();
            let out = screen(&Snapshot {
                session: Some(v),
                consistent: true,
                ..Snapshot::default()
            });
            assert!(
                out.contains("no review request in the part of the owner's journal read"),
                "{out}"
            );
        }
    }
}
