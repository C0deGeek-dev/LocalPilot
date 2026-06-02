# 09 — Golden-Task Evals and Release Hardening

## Goal
> The eval suite (`docs/08` §Golden-Task Evals, `docs/03` Phase 8) plus Phase 15
> Release Hardening (`docs/03`, `docs/09`). Build a deterministic golden-task
> eval framework that proves the agent actually completes work (not just that
> contracts hold), complete the `docs/08` Required-MVP-Test coverage across the
> workspace, then ship a public alpha: installers, supply-chain gates promoted
> to blocking, clean-room audit, docs, and the `v0.1.0-alpha.1` tag. This
> subject runs last; it depends on every prior subject.

## Boxes
> ID = `09.<box-number>`. Owners: agent · release-engineer · tech-lead.

- [ ] **09.1** (agent) Define the golden-task **fixture format** (`docs/08`,
      `docs/11`): small deterministic repos with expected outcomes. Fixtures
      authored for this repo, never copied (`docs/08` Fixture Policy). Tasks:
      create a tiny CLI; add a parser branch; fix a failing test; edit docs +
      code together; recover from a bad tool result; pause/resume after a fake
      quota window. (Verified: fixture schema + at least these six tasks exist.)
- [ ] **09.2** (agent) Implement the fake-provider **eval runner** + scorecard
      recording per task: success/failure, model turns, tool calls,
      retries/recoveries, token usage, final git diff, test output (`docs/08`,
      `docs/03` Phase 8 Done-when). Track the success rate over time. (Verified:
      runner executes the six tasks with the fake provider and emits a
      scorecard; a regression in any task shows as a score drop.)
- [ ] **09.3** (agent) Add an **optional live-provider eval mode** behind
      `UNSHACKLED_LIVE_TESTS`, skipping without credentials, never in default CI,
      avoiding destructive tools, prompts minimal (`docs/08` Live Tests,
      `docs/11` Evals). (Verified: live mode gated by env var; skips cleanly when
      unset.)
- [ ] **09.4** (agent) Sweep the `docs/08` "Required MVP Tests" matrix and
      confirm every listed test exists and passes across crates (Config,
      Provider, Tools, Harness, Recovery, Context, Store). Fill any gaps found.
      (Verified: a checklist mapping each `docs/08` MVP test to a real test name;
      `cargo nextest run --workspace` green on all three OSes.)
- [ ] **09.5** (agent) Verify **behavior parity** across Windows, Linux, macOS
      (ADR-0007, `docs/01` release requirement): every `#[cfg]` branch has a test
      that runs on its OS; CI matrix green on all three. Run `/verify`/`/run`
      against `unshackled doctor` and the Milestone-1 harness commands on at
      least the host platform. (Verified: CI green ×3; a manual run log of the
      Milestone-1 command sequence.)
- [ ] **09.6** (agent) Promote the supply-chain CI jobs to **blocking** and run
      them clean: `cargo deny check`, `cargo audit`, `cargo machete` (`docs/14`
      §4, `docs/07` Supply Chain, `docs/09`). Dependency license review against
      `deny.toml`. (Verified: CI fails on an advisory/unused-dep/forbidden
      license; current tree passes.)
- [ ] **09.7** (agent) Add the **installers** (`docs/09` Installer Targets V1):
      `cargo install` path, GitHub release archives, a PowerShell install script,
      and a shell install script. Release archives include the license files
      (`docs/09` Release Checklist). (Verified: archive build contains the binary
      + `LICENSE-MIT`; install scripts parse/lint clean.)
- [ ] **09.8** (agent) Write/refresh **public docs** for alpha (`docs/09`
      Public-Alpha Criteria): install docs, provider setup (official API + local
      server config from `docs/04`), security-model summary, and an alpha release
      notes draft. Keep all framing clean-room-compliant (`docs/00` Prohibited
      Framing, `docs/09` Clean-Room Scan Terms). Update `CHANGELOG.md`. (Verified:
      docs render; a draft `docs` set covers install + provider setup + security.)
- [ ] **09.9** (agent) Run the **clean-room scan** (`docs/00` Repository Hygiene,
      `docs/09` Clean-Room Scan Terms): text scan for prohibited framing
      ("source-map", "leaked", "free build", "fork of", "private endpoint",
      vendor names as identity, personal absolute paths, browser-cookie auth);
      verify no private endpoints in code/tests; verify no `.env`/token-like
      content committed; verify example endpoints are official public APIs or
      localhost; verify dependency licenses compatible. (Verified: scan output
      attached to the Progress log with zero unresolved hits.)
- [ ] **09.10** (tech-lead) Sign off the **clean-room audit** as a human review
      (`docs/00` Clean-Room Roles — reviewer checks provenance, not just
      correctness; `docs/09`): prompts/tests/identifiers original, official APIs
      only, no vendor branding as identity, provenance notes present where the
      read-only reference was consulted. Mirror to `manual-actions.md`.
      (Verified: §8-style sign-off line recorded; the §7 clean-room gate is
      ticked only after this.)
- [ ] **09.11** (release-engineer) Run the full **release checklist** (`docs/09`
      Before tagging): update changelog, run full test matrix, run dependency
      audit, run clean-room scan, verify license files, verify no `.env`/token
      content, verify archives contain expected files, create a signed tag if
      signing is configured. Mirror to `manual-actions.md`. (Verified: checklist
      completed with each item ticked.)
- [ ] **09.12** (release-engineer) Confirm the `docs/09` **Public-Alpha
      Criteria** all hold and tag **`v0.1.0-alpha.1`** (`docs/11` Release):
      clean-room audit complete, no private endpoints, no prohibited framing,
      `cargo test --workspace` green, TUI usable, harness completes a small repo
      task, docs explain provider setup, security model documented. Mirror to
      `manual-actions.md`. (Verified: tag created on the merge commit; criteria
      checklist attached.)
- [ ] **09.13** (release-engineer) Set up the **nightly** channel build from
      main (`docs/09` Release Channels) — no stability guarantee — so post-alpha
      iteration has a pipeline. (Verified: a nightly workflow builds main
      artifacts.)
- [ ] **09.14** (agent) Add the **LocalMind-native integration handoff** for the
      next development track: document that Unshackled ships learning as built-in
      UX, mark current `unshackled-memory` / `unshackled-skills` behavior as
      alpha bridge surfaces, and create/check in the follow-up contract or plan
      that maps Unshackled session bundles, tool events, diffs, tests, commits,
      recovery events, memory retrieval, review queues, and skill drafts onto the
      LocalMind core without requiring users to install LocalMind separately.
      (Verified: D016 referenced; follow-up plan/contract checked in; no subject
      05-08 history rewritten.)


## Hindsight checkpoint
> Run after all boxes in this subject are complete and before marking
> the subject `DONE` in §5. Use the embedded prompt in `tasks/Unshackled-Plan.md`
> "Appendix: Captain Hindsight Prompt". Record the review result here.
>
> Required output sections: Keep; Fix before closing; Record; Risk;
> Verdict (`CLOSE` or `DO NOT CLOSE`). If the verdict is `DO NOT CLOSE`,
> leave the subject open, add/reopen boxes or update decisions/lessons,
> and rerun this checkpoint after the fixes.
>
> Subjects already marked `DONE` before this checkpoint was added still need
> this section completed retroactively before the §7 gate review is ticked.

- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`
## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.
