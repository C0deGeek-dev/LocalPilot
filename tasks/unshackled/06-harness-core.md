# 06 — Harness Core: Documents, Intake, Planning, Rules, Worker

## Goal
> Phases 5–8 (`docs/03`) — the product's defining loop (`docs/06`). Parse and
> render `brief.md`/`PROGRESS.md` losslessly; implement `init`, `status`,
> `intake`, `plan`, `feature`, `resume`; author original Unshackled prompts;
> build the deterministic rule engine (triggers, verdicts, baseline rules); and
> the worker that executes plan steps one at a time with the anti-sunk-cost
> replan loop and per-step commits. Project files are source of truth
> (ADR-0003); the rule engine layers on top of the permission engine and MUST
> NOT bypass it (`docs/06`). Prompts are original, tested as product behaviour
> (`docs/00`, `CONTRIBUTING.md`).

## Boxes
> ID = `06.<box-number>`. Owners: agent · tech-lead.

- [x] **06.1** (agent) Define + implement the `brief.md` schema and a
      parser/renderer (`docs/06`): required sections Summary, Requirements,
      Constraints, Non-Goals, Acceptance Criteria; lossless where possible;
      accept both `\n` and `\r\n` (`docs/13` §7). (Verified: `docs/08` Harness
      tests — parse valid brief; reject brief missing a required section, naming
      the section.)
- [x] **06.2** (agent) Define + implement the `PROGRESS.md` schema and
      parser/renderer (`docs/06`): `Branch:` line, numbered steps with `[ ]`/`[x]`,
      completed-step metadata (`commit:`, `attempts:`). User-edited files
      accepted if semantically valid; malformed files report exact section/line
      (`docs/03` Phase 5 Done-when). (Verified: `docs/08` Harness tests — parse
      valid progress; reject duplicate step number; round-trip render is
      lossless.)
- [x] **06.3** (agent) Implement `unshackled init` (`docs/06`): create
      `.unshackled.toml`, add `.gitignore` entry for `.unshackled/`, optionally
      init git. (Verified: `assert_cmd` test on a temp dir — files created,
      gitignore entry present.)
- [x] **06.4** (agent) Implement `unshackled harness status` (`docs/06`):
      read-only summary — current branch, next step, completed count, dirty
      state, test command, provider config status — and it MUST work without a
      model provider (`docs/03` Phase 5 Done-when). (Verified: snapshot test of
      status output on a fixture repo, no provider configured.)
- [x] **06.5** (agent) Author the **original Unshackled intake prompt** and
      implement `unshackled harness intake` (`--idea`, `--refine`, `--continue`,
      `--auto`, `docs/06`): idea → `brief.md` + `.unshackled/intake.jsonl`.
      Validate generated artifacts before writing; invalid model output is
      retried with validation feedback (`docs/03` Phase 6). Prompt lives in a
      harness module and is snapshot-tested (`docs/13` §10). (Verified: idea →
      `brief.md` works with the fake provider; invalid-output retry test;
      prompt snapshot.)
- [x] **06.6** (agent) Author the **original Unshackled planner prompt** and
      implement `unshackled harness plan` (`--replan`, `docs/06`): `brief.md` +
      repo summary → `PROGRESS.md` with numbered steps, branch name, test
      strategy. Validate before writing (`docs/03` Phase 6). (Verified:
      `brief.md` → `PROGRESS.md` works with the fake provider; prompt snapshot;
      invalid-output retry test.)
- [x] **06.7** (agent) Create prompt fixtures + snapshot tests and iterate
      prompts against golden tasks; prompt changes are reviewed through snapshot
      diffs and eval scores (`docs/03` Phase 6 Done-when, `docs/08` Snapshot).
      Fixtures authored for this repo, never copied (`docs/08` Fixture Policy).
      (Verified: `cargo insta` snapshots for intake + planner prompts; eval hook
      ready for subject 09.)
- [x] **06.8** (agent) Implement `unshackled harness feature` (`docs/06`): a
      feature description → appended brief notes + appended/inserted progress
      steps without renumbering completed steps. (Verified: test — feature
      append leaves existing step IDs/commit metadata intact.)
- [x] **06.9** (agent) Define rule-engine **trigger** types (`docs/06`):
      `session_start`, `pre_tool`, `post_tool`, `pre_edit`, `post_edit`,
      `pre_shell`, `post_shell`, `pre_commit`, `post_test`, `step_complete`.
      (Verified: trigger enum + dispatch test.)
- [x] **06.10** (agent) Define **verdict** types + the rule registry (`docs/06`):
      `allow`, `warn`, `retry`, `discard`, `block`. Config can tighten policy but
      MUST NOT silently bypass critical rules (`docs/03` Phase 7 Done-when).
      (Verified: verdict enum; a test that config cannot downgrade a critical
      rule to allow.)
- [x] **06.11** (agent) Implement the **baseline rules** (`docs/06`):
      `no_stale_uncommitted` (session_start blocks on unrelated uncommitted
      files), `workspace_boundary` (pre file-tool), `secret_file_guard` (ask
      before `.env`/keys/credential stores/token-bearing cloud config),
      `test_first_when_configured` (warn/block), `suite_green` (tests pass before
      step completion), `progress_updated` (PROGRESS.md reflects completion
      before final commit), `commit_message_clean` (no secrets / vendor-internal
      refs / private impl names), `attempt_limit`. Each rule is unit tested;
      failures are visible to model and user (`docs/03` Phase 7 Done-when).
      (Verified: `docs/08` Harness tests — rule retry path; rule discard path;
      one unit test per baseline rule.)
- [x] **06.12** (agent) Implement rule **config overrides** and attempt counters
      driven by `[harness.rules]` and `attempts_per_step` (`docs/06`). (Verified:
      `docs/08` Harness tests — attempt counter increment; config override
      changes a non-critical rule's verdict.)
- [x] **06.13** (agent) Implement the worker role + **next-incomplete-step
      selection** (`docs/03` Phase 8, `docs/06`): start from committed state,
      build the worker prompt from the step + current state, run the subject-05
      agent loop for one step. (Verified: `docs/08` Harness tests — next
      incomplete step selection; mark step complete.)
- [x] **06.14** (agent) Implement step completion flow (`docs/02` §Harness
      Resume, `docs/06` Commit Policy): run post-step rules → run tests if
      configured (`suite_green`) → commit if rules pass (one commit per completed
      step, `harness: <step description>` message) → update PROGRESS.md → commit
      progress update. Commits go through the permission engine + `git_commit`
      tool (subject 04). (Verified: end-to-end on a sample repo — one commit per
      completed step; PROGRESS.md updated.)
- [x] **06.15** (agent) Implement the **anti-sunk-cost loop** (`docs/06`,
      `docs/03` Phase 8): `retry` keeps context + feeds back the reason;
      `discard` saves an attempt log and restores committed state with **fresh**
      context; after capped discard/retry failures, replan the step with the
      attempt logs; cap replans to avoid runaway automation. Discards reset the
      working tree only inside the target workspace (`docs/01` Job 4). (Verified:
      `docs/08` Harness tests — replan cap; a repeated-failure scenario triggers
      context reset + replan; attempt logs persisted.)
- [x] **06.16** (agent) Implement `unshackled harness resume` (`docs/06`,
      `docs/02` §Harness Resume) tying it together: load config/brief/progress,
      validate repo state, select next step, run worker, pause-point hook for
      quota (subject 07), run rules/tests, commit, mark done, stop/continue.
      Implement the three harness entry paths (`docs/06`): ground-up, single
      task, adopt-existing (summarize repo → generate/import brief+progress →
      resume). Implement mode switching at safe boundaries (`docs/11`).
      (Verified: a small sample repo completes a task end to end via resume;
      adopt-existing path generates a valid brief+progress.)
- [x] **06.17** (agent) Implement worker-loop **trace events** (`docs/03` Phase
      8, `docs/11`) instrumented via `tracing` spans (chat turn, tool call,
      harness step, provider request; skip secret/large fields, `docs/13` §11).
      Snapshot-test the trace event shape (`docs/08` Snapshot). (Verified: trace
      snapshot test; spans skip secret fields.)
- [x] **06.18** (tech-lead) Review the intake + planner prompts and the rule
      verdict severities (which rules are `block` vs `warn`) for product
      correctness and clean-room provenance before they are locked
      (`docs/00`, `docs/06`). Record any prompt/severity amendment in §4; mirror
      to `manual-actions.md`. (Verified: §4 row or explicit sign-off that
      defaults stand.)


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

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

### Review result

1. **Keep:** Project files are the source of truth — `brief.md`/`PROGRESS.md`
   parse/render losslessly and the harness reads the edited file as truth. The
   rule engine is deterministic and layers on top of the permission engine (it
   can block or warn but never grants a denied effect), with critical rules that
   config cannot silently disable. The anti-sunk-cost loop is a clean, tested
   state machine, and resume composes the existing session loop + rules + git into
   a real end-to-end step (a sample repo completes a step with a per-step commit
   and a progress update). Intake/planner prompts are original and snapshot-tested.
2. **Fix before closing:** None blocking. `repo_summary` for planning is a
   shallow top-level listing (adequate for alpha); the interactive REPL and live
   approval prompting remain the TUI's job (D013). `test_first_when_configured`
   relies on the caller supplying `editing_impl_before_tests`, which the worker
   wires conservatively for now.
3. **Record:** No new decisions required beyond D013 (session location). Lessons
   unchanged. 06.18 recorded in `manual-actions.md` (prompts/severities stand).
4. **Risk:** The resume end-to-end test depends on a real `git` binary (present in
   CI); the planner's repo summary is coarse. Both acceptable for alpha.
5. **Verdict:** CLOSE.

## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.

- 2026-06-02 · slice 1 · 06.1, 06.2 · `brief.md` + `PROGRESS.md` document model in
  `unshackled-harness`: section-based parsers that accept `\n`/`\r\n`, name a
  missing required brief section, reject duplicate progress step numbers, and a
  lossless render round-trip; `Progress` helpers (`next_incomplete`,
  `completed_count`, `mark_complete`). Verified: 8 doc tests (valid parse, missing
  section named, CRLF, duplicate-step rejection, render round-trips); clippy(-D)/
  fmt clean.
- 2026-06-02 · slice 2 · 06.9, 06.10, 06.12 · deterministic rule engine: typed
  triggers + verdicts, 8 baseline rules, config severities with critical-rule
  clamping (Off on a critical rule falls back to default). 6 tests.
- 2026-06-02 · slice 3 · 06.3, 06.4 · CLI `init` (config + idempotent gitignore +
  optional git) and `harness status` (read-only, no provider). assert_cmd init +
  status snapshot.
- 2026-06-02 · slice 4 · 06.5–06.8 · original intake/planner prompts + validated
  generation with retry-on-parse-error, deterministic feature append. Fake-provider
  + prompt-snapshot + feature e2e tests.
- 2026-06-02 · slice 5 · 06.11, 06.13–06.17 · worker step selection, completion
  gate (suite_green/commit-clean block a commit), anti-sunk-cost retry/discard/
  replan-cap loop, structural step trace; resume runs a step end-to-end on a temp
  git repo (file written, per-step commit, PROGRESS.md updated) and the CLI
  `harness resume` loops steps. 10 worker tests + resume e2e. 06.18 prompts/
  severities reviewed (defaults stand). All gates green.
