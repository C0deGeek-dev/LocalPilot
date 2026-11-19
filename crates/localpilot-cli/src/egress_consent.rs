//! Asking a person before anything leaves the machine.
//!
//! The policy lives in `localpilot-research`; this is the terminal that asks and
//! the words it uses. Kept apart so the decision logic stays testable without a
//! console, and so the wording is in one place rather than duplicated at each of
//! the three egress consumers.

use localpilot_research::{ConsentAnswer, EgressApproval, EgressRequest, SharedApproval};
use localpilot_tools::{QuestionOption, UserAnswer, UserPrompter, UserQuestion};
use std::io::{BufRead, IsTerminal, Write};
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;
use tokio_util::sync::CancellationToken;

/// Whether there is a person on both ends to ask.
///
/// Both directions matter: a prompt written to a redirected stdout is a prompt
/// nobody reads, and a read from a piped stdin is not an answer.
#[must_use]
pub fn someone_to_ask() -> bool {
    std::io::stdin().is_terminal() && std::io::stderr().is_terminal()
}

/// Ask on the terminal.
///
/// Writes to stderr rather than stdout on purpose: research output is piped and
/// parsed, and a question mixed into a report is both a corrupt report and an
/// unanswered question.
pub struct TerminalApproval<R: BufRead, W: Write> {
    input: R,
    output: W,
}

impl<R: BufRead, W: Write> TerminalApproval<R, W> {
    /// Ask through `input`, showing the question on `output`.
    pub const fn new(input: R, output: W) -> Self {
        Self { input, output }
    }

    fn line(&mut self) -> String {
        let mut answer = String::new();
        match self.input.read_line(&mut answer) {
            // End of input is not an answer, and must not read as one. A closed
            // stdin mid-run means nobody is there any more.
            Ok(0) | Err(_) => String::new(),
            Ok(_) => answer.trim().to_ascii_lowercase(),
        }
    }

    /// The blanket grant, and the warning that has to earn it.
    ///
    /// The warning describes what will happen, not what the flag is called. A
    /// person who reads "disables the per-request egress gate for the session"
    /// has learned nothing they can weigh; a person who reads that their prompts
    /// will travel to hosts they have never seen can actually decide.
    ///
    /// Confirmation is a second keystroke rather than a typed phrase. The
    /// deliberateness lives in having to answer a *different* question after
    /// reading what it costs — `a` opens the warning and grants nothing on its
    /// own — rather than in the cost of typing.
    fn confirm_session_wide(&mut self) -> ConsentAnswer {
        let _ = write!(
            self.output,
            "
  Allow every outbound request for the rest of this session?

  Nothing will ask again. Every page this research pass decides to fetch will
  leave this machine — including hosts you have not seen yet, and pages reached
  by redirect from a host you did approve. Your query text travels with those
  requests, and what comes back is read into the model's context.

  This lasts until this session ends and is not saved. Hosts you have
  explicitly disallowed stay blocked.

  [y] allow everything for this session   anything else cancels
  > "
        );
        let _ = self.output.flush();
        if matches!(self.line().as_str(), "y" | "yes") {
            let _ = writeln!(self.output, "  Allowing every request for this session.\n");
            ConsentAnswer::AllowSessionWide
        } else {
            let _ = writeln!(self.output, "  Cancelled — this request is denied.\n");
            ConsentAnswer::Deny
        }
    }
}

impl<R: BufRead, W: Write> EgressApproval for TerminalApproval<R, W> {
    fn ask(&mut self, request: &EgressRequest<'_>) -> ConsentAnswer {
        // The host alone does not let anyone decide anything: the same domain is
        // reasonable for one purpose and not another. Show the URL and say what
        // it is for.
        let _ = write!(
            self.output,
            "
  This would leave your machine:

    {}
    for: {}

  [y] allow once   [n] deny   [a] allow every request this session
  > ",
            request.url, request.purpose
        );
        let _ = self.output.flush();
        match self.line().as_str() {
            "y" | "yes" => ConsentAnswer::AllowOnce,
            "a" | "all" => self.confirm_session_wide(),
            // Anything else is a no, including an empty line and a closed
            // stdin. The safe reading of an unclear answer is the one that does
            // not send anything.
            _ => ConsentAnswer::Deny,
        }
    }
}

/// The terminal as the answerer of egress questions, when a person is on both
/// ends; `None` (refuse what nobody approved) otherwise.
#[must_use]
pub fn terminal_approval() -> Option<SharedApproval> {
    someone_to_ask().then(|| {
        localpilot_research::shared_approval(TerminalApproval::new(
            std::io::BufReader::new(std::io::stdin()),
            std::io::stderr(),
        ))
    })
}

#[cfg_attr(not(feature = "tui"), allow(dead_code))]
const ALLOW_ONCE: &str = "Allow once";
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
const DENY: &str = "Deny";
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
const ALLOW_EVERY: &str = "Allow every request this run";
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
const CONFIRM_EVERY: &str = "Allow everything for this run";
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
const CANCEL: &str = "Cancel";

/// Ask through a full-screen session's own question prompt.
///
/// The same three answers as [`TerminalApproval`], and the same second
/// question before a blanket grant. Only an exact offered choice counts: a
/// dismissal, typed text, a closed prompt or anything unexpected is a denial.
///
/// `ask` is synchronous because the egress policy is. It waits on the answer
/// without blocking a runtime worker, so it must run on a multi-threaded
/// runtime, off the thread that draws the prompt (the full-screen session runs
/// research on a worker of its own); anywhere else it refuses without asking.
#[cfg_attr(not(feature = "tui"), allow(dead_code))]
pub struct PromptedApproval {
    prompter: Arc<dyn UserPrompter>,
    /// The run's cancellation and stop flag: an answer that arrives after the
    /// run was cancelled (the session closing dismisses what it showed) marks
    /// the run stopped before anything else can happen, and counts as a no.
    abandoned: Option<(CancellationToken, Arc<AtomicBool>)>,
}

#[cfg_attr(not(feature = "tui"), allow(dead_code))]
impl PromptedApproval {
    /// Ask through `prompter`.
    #[must_use]
    pub fn new(prompter: Arc<dyn UserPrompter>) -> Self {
        Self {
            prompter,
            abandoned: None,
        }
    }

    /// Treat any answer that arrives once `cancel` is cancelled as a no, and
    /// set `stop` first, so the run sends nothing more.
    #[must_use]
    pub fn when_abandoned(mut self, cancel: CancellationToken, stop: Arc<AtomicBool>) -> Self {
        self.abandoned = Some((cancel, stop));
        self
    }

    fn abandoned(&self) -> bool {
        let Some((cancel, stop)) = &self.abandoned else {
            return false;
        };
        if cancel.is_cancelled() {
            stop.store(true, Ordering::Relaxed);
            return true;
        }
        false
    }

    /// The single offered choice picked for `question`, if that is how it
    /// was answered.
    fn choice(&self, question: UserQuestion) -> Option<String> {
        let handle = tokio::runtime::Handle::try_current().ok()?;
        if handle.runtime_flavor() != tokio::runtime::RuntimeFlavor::MultiThread {
            return None;
        }
        let offered: Vec<String> = question.options.iter().map(|o| o.label.clone()).collect();
        let (tx, rx) = std::sync::mpsc::channel();
        let prompter = Arc::clone(&self.prompter);
        handle.spawn(async move {
            let questions = [question];
            let _ = tx.send(prompter.ask(&questions).await);
        });
        let answers = tokio::task::block_in_place(|| rx.recv()).ok()?;
        if self.abandoned() {
            return None;
        }
        match answers.as_slice() {
            [UserAnswer::Selected(labels)] => match labels.as_slice() {
                [label] if offered.contains(label) => Some(label.clone()),
                _ => None,
            },
            _ => None,
        }
    }

    fn confirm_every_request(&self) -> ConsentAnswer {
        let question = UserQuestion {
            header: Some("Web research".to_string()),
            question: "Allow every outbound request for the rest of this research run? \
                       Nothing will ask again. Every page this run decides to fetch will leave \
                       this machine, including hosts you have not seen yet and pages reached by \
                       redirect from a host you did approve. Your query text travels with those \
                       requests, and what comes back is read into the model's context. This \
                       lasts until the run ends and is not saved. Hosts you have explicitly \
                       disallowed stay blocked."
                .to_string(),
            options: vec![
                option(CONFIRM_EVERY, None),
                option(CANCEL, Some("This request is denied.")),
            ],
            multi_select: false,
        };
        match self.choice(question).as_deref() {
            Some(CONFIRM_EVERY) => ConsentAnswer::AllowSessionWide,
            _ => ConsentAnswer::Deny,
        }
    }
}

#[cfg_attr(not(feature = "tui"), allow(dead_code))]
fn option(label: &str, description: Option<&str>) -> QuestionOption {
    QuestionOption {
        label: label.to_string(),
        description: description.map(str::to_string),
    }
}

impl EgressApproval for PromptedApproval {
    fn ask(&mut self, request: &EgressRequest<'_>) -> ConsentAnswer {
        let question = UserQuestion {
            header: Some("Web research".to_string()),
            question: format!(
                "This would leave your machine: {} for: {}",
                localpilot_config::redact::redact(request.url),
                localpilot_config::redact::redact(request.purpose)
            ),
            options: vec![
                option(ALLOW_ONCE, Some("Send this request only.")),
                option(DENY, Some("Do not send it.")),
                option(
                    ALLOW_EVERY,
                    Some("Asks once more, after saying what that allows."),
                ),
            ],
            multi_select: false,
        };
        match self.choice(question).as_deref() {
            Some(ALLOW_ONCE) => ConsentAnswer::AllowOnce,
            Some(ALLOW_EVERY) => self.confirm_every_request(),
            _ => ConsentAnswer::Deny,
        }
    }
}

/// Ask one yes-or-no question on `output` and read the answer from `input`.
/// Only "y" or "yes" is a yes; anything else, an empty line and a closed
/// input included, is a no.
pub fn confirm<R: BufRead, W: Write>(mut input: R, mut output: W, question: &str) -> bool {
    let _ = write!(
        output,
        "\n  {question}\n\n  [y] allow for this run   anything else declines\n  > "
    );
    let _ = output.flush();
    let mut answer = String::new();
    match input.read_line(&mut answer) {
        Ok(0) | Err(_) => false,
        Ok(_) => matches!(answer.trim().to_ascii_lowercase().as_str(), "y" | "yes"),
    }
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use localpilot_tools::{UserAnswer, UserPrompter, UserQuestion};
    use std::sync::Mutex;

    /// Answers each question with the next scripted answer and keeps what it
    /// was asked.
    struct Scripted {
        answers: Mutex<Vec<UserAnswer>>,
        asked: Mutex<Vec<UserQuestion>>,
    }

    impl Scripted {
        fn new(answers: Vec<UserAnswer>) -> Arc<Self> {
            Arc::new(Self {
                answers: Mutex::new(answers.into_iter().rev().collect()),
                asked: Mutex::new(Vec::new()),
            })
        }
    }

    impl UserPrompter for Scripted {
        fn ask<'a>(
            &'a self,
            questions: &'a [UserQuestion],
        ) -> std::pin::Pin<Box<dyn std::future::Future<Output = Vec<UserAnswer>> + Send + 'a>>
        {
            self.asked.lock().unwrap().extend(questions.iter().cloned());
            let answer = self
                .answers
                .lock()
                .unwrap()
                .pop()
                .unwrap_or(UserAnswer::Dismissed);
            Box::pin(async move { vec![answer] })
        }
    }

    fn pick(label: &str) -> UserAnswer {
        UserAnswer::Selected(vec![label.to_string()])
    }

    /// Ask the way the full-screen session does: from a thread of its own,
    /// inside the multi-threaded runtime.
    async fn ask_off_thread(prompter: Arc<Scripted>) -> ConsentAnswer {
        let handle = tokio::runtime::Handle::current();
        tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let mut approval = PromptedApproval::new(prompter);
                approval.ask(&EgressRequest {
                    host: "example.com",
                    url: "https://example.com/a?api_key=sk-abcdefghijklmnopqrstuvwx",
                    purpose: "how does tokio schedule tasks",
                })
            })
        })
        .await
        .unwrap()
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn the_session_prompt_gives_the_three_terminal_answers() {
        let cases = [
            (vec![pick("Allow once")], ConsentAnswer::AllowOnce, 1),
            (vec![pick("Deny")], ConsentAnswer::Deny, 1),
            (
                vec![
                    pick("Allow every request this run"),
                    pick("Allow everything for this run"),
                ],
                ConsentAnswer::AllowSessionWide,
                2,
            ),
            // Choosing "every request" grants nothing until it is confirmed.
            (
                vec![pick("Allow every request this run"), pick("Cancel")],
                ConsentAnswer::Deny,
                2,
            ),
            (
                vec![pick("Allow every request this run"), UserAnswer::Dismissed],
                ConsentAnswer::Deny,
                2,
            ),
            // Anything but one offered choice is a no.
            (vec![UserAnswer::Dismissed], ConsentAnswer::Deny, 1),
            (
                vec![UserAnswer::Other("yes".to_string())],
                ConsentAnswer::Deny,
                1,
            ),
            (
                vec![pick("Allow everything for this run")],
                ConsentAnswer::Deny,
                1,
            ),
            (
                vec![UserAnswer::Selected(vec![
                    "Allow once".to_string(),
                    "Deny".to_string(),
                ])],
                ConsentAnswer::Deny,
                1,
            ),
            (
                vec![UserAnswer::Selected(Vec::new())],
                ConsentAnswer::Deny,
                1,
            ),
        ];
        for (answers, expected, questions) in cases {
            let prompter = Scripted::new(answers.clone());
            assert_eq!(
                ask_off_thread(Arc::clone(&prompter)).await,
                expected,
                "{answers:?}"
            );
            let asked = prompter.asked.lock().unwrap();
            assert_eq!(asked.len(), questions, "{answers:?}");
            assert!(asked[0].question.contains("example.com/a"));
            assert!(asked[0].question.contains("how does tokio schedule tasks"));
            assert!(
                !asked[0].question.contains("sk-abcdefghijklmnopqrstuvwx"),
                "the shown address is redacted: {}",
                asked[0].question
            );
            if questions == 2 {
                assert!(asked[1]
                    .question
                    .contains("including hosts you have not seen yet"));
            }
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn a_yes_that_arrives_after_the_run_was_cancelled_is_a_no_and_stops_it() {
        let cancel = CancellationToken::new();
        let stop = Arc::new(AtomicBool::new(false));
        // The run is cancelled while the question is on screen; the answer
        // that arrives afterwards (the dismissal, or a late "Allow once") must
        // not send anything.
        cancel.cancel();
        let prompter = Scripted::new(vec![
            pick("Allow every request this run"),
            pick("Allow everything for this run"),
        ]);
        let handle = tokio::runtime::Handle::current();
        let (approval_cancel, approval_stop) = (cancel.clone(), Arc::clone(&stop));
        let answer = tokio::task::spawn_blocking(move || {
            handle.block_on(async move {
                let mut approval =
                    PromptedApproval::new(prompter).when_abandoned(approval_cancel, approval_stop);
                approval.ask(&EgressRequest {
                    host: "example.com",
                    url: "https://example.com/a",
                    purpose: "q",
                })
            })
        })
        .await
        .unwrap();
        assert_eq!(answer, ConsentAnswer::Deny);
        assert!(stop.load(Ordering::Relaxed), "the run is marked stopped");
    }

    #[tokio::test]
    async fn without_a_multi_threaded_runtime_nobody_is_asked() {
        let prompter = Scripted::new(vec![pick("Allow once")]);
        let mut approval = PromptedApproval::new(prompter.clone());
        let answer = approval.ask(&EgressRequest {
            host: "example.com",
            url: "https://example.com/a",
            purpose: "q",
        });
        assert_eq!(answer, ConsentAnswer::Deny);
        assert!(prompter.asked.lock().unwrap().is_empty());
    }

    #[test]
    fn confirm_takes_only_an_explicit_yes() {
        for (input, expected) in [
            ("y\n", true),
            ("YES\n", true),
            ("n\n", false),
            ("\n", false),
            ("", false),
            ("sure\n", false),
        ] {
            let mut shown = Vec::new();
            assert_eq!(
                super::confirm(input.as_bytes(), &mut shown, "Start it?"),
                expected,
                "{input:?}"
            );
            assert!(String::from_utf8(shown).unwrap().contains("Start it?"));
        }
    }

    use super::*;
    use localpilot_research::{DenyWhenNobodyToAsk, EgressApproval};

    fn ask(input: &str) -> ConsentAnswer {
        let mut out = Vec::new();
        let mut approval = TerminalApproval::new(std::io::Cursor::new(input.to_string()), &mut out);
        approval.ask(&EgressRequest {
            host: "example.com",
            url: "https://example.com/a",
            purpose: "a query",
        })
    }

    #[test]
    fn yes_allows_one_request_and_nothing_more() {
        assert_eq!(ask("y\n"), ConsentAnswer::AllowOnce);
    }

    #[test]
    fn anything_unclear_is_a_no() {
        // Including an empty line, a stray keystroke, and a closed stdin. The
        // reading of an unclear answer that cannot hurt is the one that sends
        // nothing.
        for input in ["n\n", "\n", "maybe\n", ""] {
            assert_eq!(ask(input), ConsentAnswer::Deny, "input {input:?}");
        }
    }

    #[test]
    fn a_session_wide_grant_needs_a_second_answer_after_the_warning() {
        // `a` opens the warning; it does not grant anything on its own.
        assert_eq!(ask("a\ny\n"), ConsentAnswer::AllowSessionWide);
        assert_eq!(ask("a\nn\n"), ConsentAnswer::Deny);
        assert_eq!(ask("a\n\n"), ConsentAnswer::Deny);
        // Reaching the end of input at the confirmation is not a yes.
        assert_eq!(ask("a\n"), ConsentAnswer::Deny);
    }

    #[test]
    fn the_warning_describes_the_consequence_not_the_mechanism() {
        let mut out = Vec::new();
        {
            let mut approval =
                TerminalApproval::new(std::io::Cursor::new("a\nno\n".to_string()), &mut out);
            let _ = approval.ask(&EgressRequest {
                host: "example.com",
                url: "https://example.com/a",
                purpose: "a query",
            });
        }
        let shown = String::from_utf8(out).unwrap();
        for phrase in [
            "leave this machine",
            "hosts you have not seen",
            "redirect",
            "query text travels",
            "not saved",
        ] {
            assert!(
                shown.contains(phrase),
                "warning must say {phrase:?}: {shown}"
            );
        }
    }

    #[test]
    fn with_nobody_to_ask_the_answer_is_no() {
        let answer = DenyWhenNobodyToAsk.ask(&EgressRequest {
            host: "example.com",
            url: "https://example.com/a",
            purpose: "a query",
        });
        assert_eq!(answer, ConsentAnswer::Deny);
    }
}
