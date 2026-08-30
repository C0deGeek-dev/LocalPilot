//! Asking a person before anything leaves the machine.
//!
//! The policy lives in `localpilot-research`; this is the terminal that asks and
//! the words it uses. Kept apart so the decision logic stays testable without a
//! console, and so the wording is in one place rather than duplicated at each of
//! the three egress consumers.

use localpilot_research::{ConsentAnswer, EgressApproval, EgressRequest};
use std::io::{BufRead, IsTerminal, Write};

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

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
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
