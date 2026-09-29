//! `localpilot mesh cockpit`: the human's view of a pair session.
//!
//! The cockpit is an observer. It has no role, registers no delivery
//! endpoint and writes nothing: it re-reads a typed snapshot of the session
//! (`Mesh::snapshot`) every half second. `--json` prints one snapshot and
//! exits; without it, a full-screen view shows the snapshot until `q`.

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
    full_screen(&mesh)
}

#[cfg(not(feature = "tui"))]
fn full_screen(_mesh: &Mesh) -> ExitCode {
    eprintln!(
        "localpilot mesh cockpit: this build has no terminal UI (the `tui` feature); use --json"
    );
    ExitCode::from(2)
}

#[cfg(feature = "tui")]
fn full_screen(mesh: &Mesh) -> ExitCode {
    use std::io::IsTerminal;
    if !std::io::stdout().is_terminal() {
        eprintln!("localpilot mesh cockpit: the full-screen view needs a terminal; use --json");
        return ExitCode::from(2);
    }
    match view::run(mesh) {
        Ok(()) => ExitCode::SUCCESS,
        Err(e) => {
            eprintln!("localpilot mesh cockpit: {e}");
            ExitCode::from(1)
        }
    }
}

#[cfg(feature = "tui")]
mod view {
    //! The full-screen view: a pure rendering of one snapshot, and a loop
    //! that re-reads the snapshot every half second until `q` or `Esc`.

    use std::io::{self, Stdout};
    use std::time::{Duration, Instant};

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
    use ratatui::widgets::{Block, Borders, Paragraph, Wrap};
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

    pub(super) fn run(mesh: &Mesh) -> io::Result<()> {
        let _screen = Screen::enter()?;
        let mut terminal: Terminal<CrosstermBackend<Stdout>> =
            Terminal::new(CrosstermBackend::new(io::stdout()))?;
        let styles = Styles::from_env();
        let mut snap = mesh.snapshot();
        let mut scroll: u16 = 0;
        let mut last = Instant::now();
        loop {
            terminal.draw(|f| draw(f, &snap, &styles, scroll))?;
            let wait = REFRESH.saturating_sub(last.elapsed());
            if event::poll(wait)? {
                if let Event::Key(k) = event::read()? {
                    if k.kind != KeyEventKind::Press {
                        continue;
                    }
                    match k.code {
                        KeyCode::Char('q') | KeyCode::Esc => return Ok(()),
                        KeyCode::Char('c') if k.modifiers.contains(KeyModifiers::CONTROL) => {
                            return Ok(())
                        }
                        KeyCode::Up => scroll = scroll.saturating_add(1),
                        KeyCode::Down => scroll = scroll.saturating_sub(1),
                        KeyCode::PageUp => scroll = scroll.saturating_add(10),
                        KeyCode::PageDown => scroll = scroll.saturating_sub(10),
                        KeyCode::Home => scroll = 0,
                        _ => {}
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

    /// Draw one snapshot. Pure: everything shown comes from `snap`.
    pub(super) fn draw(f: &mut Frame, snap: &Snapshot, s: &Styles, scroll: u16) {
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
            let [body, foot] =
                Layout::vertical([Constraint::Min(3), Constraint::Length(1)]).areas(area);
            f.render_widget(Paragraph::new(lines).block(block("cockpit", s)), body);
            f.render_widget(footer(snap, s), foot);
            return;
        };
        let rows = v.participants.len() as u16;
        let waits = waits_and_pauses(v, s);
        let [head, people, review, pending, activity, foot] = Layout::vertical([
            Constraint::Length(4),
            Constraint::Length(rows + 3),
            Constraint::Length(6),
            Constraint::Length(waits.len() as u16 + 2),
            Constraint::Min(3),
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
        f.render_widget(footer(snap, s), foot);
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
        let inner = area.height.saturating_sub(2);
        let max_scroll = (lines.len() as u16).saturating_sub(inner);
        let top = max_scroll.saturating_sub(scroll.min(max_scroll));
        Paragraph::new(lines)
            .block(block("activity (Up/Down to scroll)", s))
            .wrap(Wrap { trim: false })
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
            "   refreshes every 0.5 s · read-only · q quits",
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
            let mut t = Terminal::new(TestBackend::new(110, 30)).unwrap();
            t.draw(|f| draw(f, snap, &styles(), 0)).unwrap();
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
