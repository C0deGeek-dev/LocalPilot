//! The launch flag that answers every prompt in advance.
//!
//! `--dangerously-skip-permissions` (aliases: `--yolo`, `--full-auto`) approves
//! every tool permission request and every outbound request for the whole run.
//! It is the familiar escape hatch from comparable agents, and it exists for the
//! same reason: a person driving a long unattended run does not want to answer
//! the same question forty times.
//!
//! It is a **process-level** latch rather than a value threaded through each
//! command, because that is honestly what it is — one decision, made once, at
//! launch, covering everything the process subsequently does. Threading it would
//! imply some paths could opt out, and none can.
//!
//! What it does not do: it is never persisted, never read from config, and never
//! inferred. It has to be typed, every time, by someone who meant it.

use std::io::Write;
use std::sync::atomic::{AtomicBool, Ordering};

static ENGAGED: AtomicBool = AtomicBool::new(false);

/// Engage the bypass for the rest of this process.
///
/// One-way on purpose: nothing turns it back off mid-run. A flag that could be
/// revoked halfway would mean prompts reappearing partway through an unattended
/// run, which is the failure the flag exists to prevent.
pub fn engage() {
    ENGAGED.store(true, Ordering::SeqCst);
}

/// Whether this run was launched with every prompt answered in advance.
#[must_use]
pub fn engaged() -> bool {
    ENGAGED.load(Ordering::SeqCst)
}

/// Say what was just switched off, in terms of what it costs.
///
/// Written to stderr so it survives a piped stdout, and printed once at launch
/// rather than per action — a warning repeated forty times is not read the
/// fortieth time, or the second.
pub fn announce(out: &mut dyn Write) {
    let _ = writeln!(
        out,
        "
  ⚠  Every permission and every outbound request is pre-approved for this run.

     Tools run without asking — including editing and deleting files outside
     this workspace, and running shell commands. Web requests leave this
     machine without asking, to any host that is not explicitly disallowed.
     Your prompts travel with them.

     Nothing here is saved: the next run asks again unless you pass the flag
     again. Two things still hold — [research.web] enabled = false remains an
     absolute kill switch, and a host on [research.web] disallowlist stays
     blocked.
"
    );
    let _ = out.flush();
}

#[cfg(test)]
#[allow(clippy::unwrap_used, clippy::expect_used)]
mod tests {
    use super::*;

    #[test]
    fn the_warning_says_what_it_costs_rather_than_what_it_switches_off() {
        let mut out = Vec::new();
        announce(&mut out);
        let shown = String::from_utf8(out).unwrap();
        for phrase in [
            "without asking",
            "outside\n     this workspace",
            "leave this\n     machine",
            "prompts travel",
            "next run asks again",
            "kill switch",
            "disallowlist",
        ] {
            assert!(
                shown.contains(phrase),
                "warning must say {phrase:?}: {shown}"
            );
        }
    }

    #[test]
    fn it_is_off_until_someone_asks_for_it() {
        // `engage` is deliberately one-way, so this also pins that no other test
        // in this binary turns it on — which would silently un-gate every
        // permission assertion in the suite.
        assert!(
            !engaged(),
            "the bypass must be off unless a launch flag asked for it"
        );
    }

    #[test]
    fn the_flag_is_parsed_under_the_names_people_already_know() {
        use clap::Parser;
        for name in ["--dangerously-skip-permissions", "--yolo", "--full-auto"] {
            let cli = crate::Cli::try_parse_from(["localpilot", name])
                .unwrap_or_else(|e| panic!("{name} must parse: {e}"));
            assert!(cli.dangerously_skip_permissions, "{name}");
        }
        let plain = crate::Cli::try_parse_from(["localpilot"]).unwrap();
        assert!(
            !plain.dangerously_skip_permissions,
            "nothing is bypassed unless it was asked for"
        );
    }
}
