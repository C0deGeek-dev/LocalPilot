# Lessons (disposable run-notes)

> Append as slices teach something. Durable lessons migrate to `tasks/lessons.md`
> at the gate before this folder is deleted.

- 2026-06-04 · The rule engine already models `Verdict::Retry/Discard/Block` and
  `Trigger` is `#[non_exhaustive]`, so the gate extends cleanly — add a
  `PhaseComplete` trigger and a `quality_gate` rule rather than a parallel system.
- 2026-06-04 · `HarnessConfig.test_command` is `Option<String>`; the gate must
  keep it as a back-compat synonym for a single phase `test` check.
- 2026-06-04 · `run_shell` runs program+args with NO shell. Store checks as
  program+args (D002), not a command string — avoids unsafe word-splitting.
- 2026-06-04 · The Relaxed permission allowlist matches by TOOL NAME, not by
  command. The gate needs its own tool identity (D003) or allowlisting it would
  open all shell. Ratification must also grant that identity an allowance (D005),
  because cargo checks are `ProjectWrite` = deny non-interactively.
- 2026-06-04 · No `phase` boundary exists in the engine yet; `Trigger` is
  `#[non_exhaustive]` so `PhaseComplete` adds cleanly, but the loop must define
  what a phase is (subject 04, after reading worker.rs/resume.rs).
- 2026-06-04 · BLOCKER: baseline `clippy --all-targets` red on config tests
  (D006), pre-existing. Every checkpoint gate is red until fixed.
