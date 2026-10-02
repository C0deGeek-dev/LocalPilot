# Test Plan

Session scratch behavior is pinned through the sandbox profile/lease matrix,
canonical symlink/junction escapes and owned-directory lifecycle, real file and
foreground/background shell dispatch, exact structured-command grants, child
environment, host setup, session new/resume/fork/close/drop and prompt replacement.
Opaque expansion/provider targets remain gated, including under bypass. The
compaction fixtures reserve headroom for the scratch authority cue and assert
it appears once in the combined system message. No model endpoint or user store
is required for these checks; temporary parents and outside targets are owned
test fixtures.

Windows command cleanup is covered by the tools crate's drop/timeout
tests and `localpilot-harness`'s cancellation integration test. The Win32 shim's
`ffi::job::tests` additionally checks an exited parent's live descendant,
independent job isolation, and injected setup failures before/after assignment.
Run these under workspace parallel load as well as the package gate: the
original detached `taskkill` failure appeared only under load.
Background tests cover cancellation during startup grace, stop and session close.

## Automatic Work Sizing

The production-dispatch scenarios also cover short whole-file text larger in
bytes than its line count, byte-small files exceeding the line cap, tightened
line configuration, and denied reads preceding content-dependent limits.
Large files still require pages; the long-line scenario proves explicit-page
output remains bounded and retained. No user stores or model endpoints are used.

`localpilot-harness/tests/granularity.rs` drives the real shared runtime with
fake providers: oversized/whole-file requests, explicit pages, cumulative edits,
UTF-8 giant-line retention, unavailable verification, recorded passing checks,
oversized plan refusal with coverage/order intact, durable fresh-runtime resume,
opaque shell refusal before commit, malformed observations, stream truncation and
actual compaction. Pure `granularity::tests` cross context sizes independently
with weak/strong/unknown/malformed evidence, stricter caps and active-unit
monotonicity. No GPU or model name is required. Portable file/diff limits are the
same on Windows, Linux and macOS; platform shell fixtures use their existing
explicit platform branches. Live model checks remain opportunistic.

## Retrieval-Quality Measurement

Two harnesses measure whether retrieval returns the *right* things, as distinct
from how much it returns or how fast.

**Session spans** — `tests/span_retrieval_quality.rs`. Runs in CI, offline and
deterministic, over a **synthetic** corpus: a query built from a distinctive
string in a real transcript *is* transcript content, so a real-corpus query set
could not be committed. Reports recall, precision and MRR per query class, plus a
dense arm scored from a committed vector fixture.

**Accepted memory** — `examples/memory_retrieval_quality.rs`. Not a committed
test, because its judgment set names real memory ids and lives outside this
repository. It measures `context_hits` — the function that decides what the model
actually receives; measuring the store instead would score a component the model
never sees. With no embedding endpoint configured the path is pure keyword search,
which is the shipped default and deterministic.

Its scoring rules are fixed *before* any measurement, because a harness that
decides them while being written decides them to suit whatever it already
produces:

- A returned id absent from the frozen judgments is **`UNJUDGED`**, never counted
  as irrelevant. Scoring it wrong understates precision, because a genuinely
  relevant memory that never reached the candidate pool would be counted as a
  wrong answer.
- **Precision is reported as bounds**, never a point — a lower bound counting
  unjudged as wrong, an upper counting them as right. When they are far apart the
  honest statement is that the measurement is not precise enough, not that the
  truth is in the middle.
- **Judgment coverage ships with every number.** A precision at 40% coverage and
  one at 95% are not comparable, and a report omitting it invites that comparison.
- What is reported is **known-positive recall**, never "recall" unqualified. The
  denominator is the judged-relevant set, which is a floor: a relevant memory
  absent from the pool *and* never returned is invisible to every mechanism here.
- A judgment set is addressed by the **SHA-256 of its qrels file**, checked before
  any number taken against it is quoted. A top-up creates a new revision with a
  new hash rather than being folded in — adding judgments can change every number
  already reported.

Both harnesses include a **negative class**: queries whose correct answer is to
return nothing. Over-retrieval is invisible to recall — a system that answers
everything scores perfectly — so without it the other numbers cannot be trusted.

## Test Layers

### Unit Tests

Required for:

- parsers
- config precedence
- redaction
- path normalization
- command classification
- rule verdicts
- provider event parsing

### Integration Tests

Required for:

- fake provider + tool loop
- harness intake with fake provider
- harness plan with fake provider
- harness resume with fake provider
- permission prompts with scripted decisions
- session persistence
- cancellation during streaming/tool execution

### Terminal UI Tests

The full-screen UI is split at the terminal boundary so deterministic tests do
not need a real console:

- `cargo test -p localpilot-terminal-ui` covers stable content anchors,
  mixed framed/collapsed/pinned projection equivalence, 10k-item visible-row
  virtualization, grapheme/display-width editing, selection fidelity, lifecycle
  routing, held/new-output state, atomic text/image editor units, completion and
  search overlays, and 120x30/80x24/40x20 Ratatui `TestBackend` frames across
  semantic themes and no-color mode.
- Default-theme frame tests also lock the screenshot-measured application
  canvas, filled prompt/composer surfaces, one-edge composer focus, neutral
  scrollbar, outer margins, pending-prompt label, and identical in-flow/pinned
  prompt geometry.
- `cargo test -p localpilot --features tui --bin localpilot` covers the
  Crossterm host selector, provider-neutral runtime-event mapping, key mapping,
  ANSI terminal-mode ordering, best-effort workspace Git metadata, and recorded
  RuntimeEvent replay through FollowBottom and Held viewport states. Host-state
  tests also cover plain-submit versus newline, Escape cancellation, ordered
  pending typeahead, trust-gate Ctrl+C precedence, approval denial, local prompt
  timestamps, async workspace-file completion, reverse/timeline search routing,
  mouse selection/scrollbar gestures, contextual timeline-copy/composer-paste
  right-click routing, opt-in copy-on-select, isolated clipboard-image
  attachments, and
  external-editor command resolution plus terminal leave/re-entry ordering. The
  same host tests pin the truthful slash catalog, configured-provider `/model`
  values, contained quick/full help, help wheel/thumb navigation, cancelable
  whole-UI theme preview and mouse selection, contained settings and bounded
  two-pane tracked-diff review, role-labeled screen-reader frames and dialogs,
  bounded four-row/expanded tool details with target-aware lifecycle headlines,
  envelope-free muted output, elapsed time, source-versus-retained bounds,
  width-specific disclosure counts, wide metadata alignment with stable byte hits,
  F7/F8 reading-order focus, Enter/Escape and prefix-click parity, headline-row
  anchoring across expansion, deterministic bottom clamping, focus cues across
  color/no-color/screen-reader modes, ordinary-input focus release, and compact
  versus comfortable density without weakening the narration separator,
  fixed two-head/six-tail failure previews, state-change cache invalidation,
  omission-aware source-byte hits and visible-only copy markers, and narrow
  Unicode/no-color/screen-reader tail cues,
  opt-in three-success grouping boundaries, reversible collapsed/expanded
  geometry, group focus and prefix-click parity, collapsed-member anchors,
  search reveal, nonselectable synthetic summaries, counted cross-group copy,
  no-color/screen-reader disclosure, config propagation, and raw-item export,
  tool-proven assistant-progress reclassification across success, failure,
  cancellation, question, and reasoning boundaries; stable geometry/selection/
  search/export; grouped-tool integration; hollow-versus-filled no-color cues;
  explicit screen-reader progress labels; and final/no-tool answer invariants,
  Unicode wrapping, connector geometry, visible-only collapsed copy, bounded exit
  preview, full-result search reveal, and
  conservative unified-diff styling across default/no-color/colorblind modes,
  local refusal during
  active work, host-aware live profile/background/effort controls, Ctrl+Q slash
  routing without prompt enqueue, truthful no-handle refusal, and the full
  `ask_user` lifecycle: bounded typed schema, pending/
  resolved row identity, numbered modal, automatic Other editor, keyboard/mouse
  focus, Escape/closed-host cancellation, screen-reader projection and buffered
  reply cleanup. Backend-neutral question tests also pin long-answer growth,
  caret-following overflow at both ends, a visible scrollbar, grapheme-safe
  editing, exact stored-answer submission, and narrow resize/screen-reader
  reflow. Workspace-trust coverage separately pins full-width numbered
  rendering, keyboard/mouse focus, session-only versus persistent outcomes,
  deny-safe Escape, screen-reader current-selection text, selection-copy
  precedence, and double-Ctrl+C exit without touching the real trust store.
  Tests also preserve the invariant that slash input never enters the provider
  FIFO or prompt-history store.
- Active-operation pump tests use delayed synthetic key availability to prove
  input is serviced between activity frames and that scheduler-bunched text plus
  one Enter produces exactly one queued prompt. Paste seams separately pin a
  zero-timeout ordinary-key probe, permanent retirement after `Event::Paste`,
  and retained atomic multiline fallback behavior. Backend-neutral tests pin
  heartbeat motion, `MM:SS`/`HH:MM:SS` elapsed formatting, operation-label
  lifecycle, the silent 20 Hz redraw, and the `Compacting` footer contract.
- PTY checks support lifecycle diagnostics, but a physical Windows Terminal run
  gates visible terminal behavior. A snapshot or PTY result alone is not proof
  of mouse, clipboard, wide-glyph, or terminal-restore parity.

Ctrl+C has an explicit state-matrix test. Selected text copies first. With no
selection, a typed composer is atomically stashed and cleared without arming
exit; active work then cancels on the next empty-composer press and exits on the
following consecutive press. An idle typed composer clears before the ordinary
empty-composer arm/exit pair. Busy-empty and idle-empty retain their cancel/exit
and arm/exit pairs. Any other input disarms pending exit, and a host-level test
proves draft clearing does not cancel the active token.

Stream projection tests separately pin that leading CR/LF is removed only when
opening a new assistant/reasoning item, whitespace-only openers create no row,
raw byte accounting is retained, mid-segment newlines survive, post-tool
segments use the same rule, and Ratatui renders the item glyph beside first-row
prose.

Interactive-research tests pin the cross-layer completion contract: the next
provider history contains the topic, numbered finding, and source; the normal
event log persists exactly one named research-origin assistant message; resume
replays it once while still hiding unrelated synthetic repairs; and the chat
projection retains provenance, a report pointer, an open question, omission
counts, and pre-truncation redaction within 4 KiB while the disk report remains
complete. Blank, in-flight, failed, and partial-result boundaries are tested
separately.
Terminal restore tests must cover normal exit, partial setup, post-entry errors,
panic, and the later signal/suspension paths as those paths are added.

### Golden-Task Evals

The worker loop needs an eval suite before higher-level features are built. Unit
tests prove contracts; evals prove the agent actually completes work.

Golden tasks should be small, deterministic repositories with expected outcomes:

- create a tiny CLI
- add a parser branch
- fix a failing test
- edit docs and code together
- recover from a bad tool result
- pause/resume after a fake quota window

Each task records:

- success/failure
- number of model turns
- tool calls
- retries/recoveries
- token usage
- final git diff
- test output

The eval provider can be fake at first, then optional live-provider runs can be
added behind credentials. The scorecard should be tracked over time.

#### Machine-readable scorecard

Each golden-task run emits a structured `Scorecard` (JSON) so a benchmark can
grade the *harness* on more than a single pass/fail bit. It is the cross-corpus
contract: an in-repo runner and an external runner both produce the same shape.
The blocks are derived deterministically from artefacts the loop already
produces — the captured diff and the session event trace — so the offline path
stays reproducible.

| Layer | Fields | Source |
| --- | --- | --- |
| `results` | `passed`, `regression_safe`, `partial_credit`, `tests_total`, `tests_passed` | the task's own grading |
| `quality` | `diff_added`/`diff_removed`/`diff_files`, `vs_gold_ratio`, `format_clean`/`lint_clean`/`typecheck_clean`, `complexity_delta`, `tests_added` | the captured `git diff` + the quality gate's check outcomes |
| `process` | `tool_calls`, `redundant_calls`, `reproduce_before_fix`, `test_before_done`, `retrieval_used`/`retrieval_count`, `exit_reason`, `recovered_after_failure`, `discipline` | the session event log via `EvidenceLedger` |
| `speed` | `wall_ms`, `input_tokens`, `output_tokens` | runner-measured + reported usage |

`speed` is a reported guardrail, never the headline metric — correctness gates,
then quality and process rank. Nullable fields (`vs_gold_ratio`,
`complexity_delta`, `discipline`) serialize as `null` rather than being omitted,
so the shape is stable. The one-line discipline scorecard
(`tool-discipline scorecard: …`, consumed by LocalBench's TDS pipeline) is
unchanged; the JSON scorecard is the structured superset. Run it with:

```powershell
cargo test -p localpilot-harness --test evals -- --nocapture
```

#### Emitting a scorecard headless (`localpilot eval`)

`localpilot eval` runs the agent on one problem in the current workspace (a git
repository) and prints the capability scorecard JSON to stdout — the solver entry
point an external benchmark runner drives. It uses the same harness a real
session does, captures the produced diff + the session event trace, and assembles
the scorecard via the shared `build_scorecard`. Only the JSON reaches stdout
(model output is suppressed), so the line is pipe-safe.

```powershell
localpilot eval "<problem statement>" --model <m> --arm full --task <id> `
    --test "cargo test -q" --gold-diff gold.diff
```

`--test <cmd>` grades `results` (exit 0 = passed); omit it to emit an **ungraded**
run for an external grader (a benchmark's own container) to fill `results` after
applying the diff. `--gold-diff` supplies the gold patch for the `vs_gold_ratio`.

#### First-party capability corpus

A second corpus of original tasks lives under
`crates/localpilot-harness/tests/corpus/<id>/`. Each task is a small, buggy,
self-contained Rust unit with its own failing→passing test:

- `task.json` — `id`, `entry` file name, and a reworded `problem` statement;
- `base/<entry>` — the workspace with the bug present (its test is red);
- `gold/<entry>` — the reference fix (its test is green).

These fixtures are **authored for this repository** — never copied from an
external benchmark — so the corpus is clean-room-clean and contamination-proof.
The runner materializes a task's base workspace, drives the harness loop
headless to produce a fix, captures the diff and emits the scorecard, then grades
by building and running the task's own test **in isolation** (a throwaway crate
graded with `cargo test`, so grading never pollutes the loop's workspace). The
`vs_gold_ratio` is computed against the gold patch.

Offline (default) the loop is driven by the scripted fake provider applying the
gold solution, which proves the runner mechanics deterministically; a live model
path is gated behind `LOCALPILOT_LIVE_TESTS`. A companion extraction helper scans
a repository's history for the commit that flips a grader red→green and emits a
reviewable fixture stub for a human to curate into a task.

```powershell
cargo test -p localpilot-harness --test first_party -- --nocapture
```

#### Pair-seat evaluation

`crates/localpilot-mesh/seat-eval/` measures a model in LocalPilot's pair
seat (`localpilot mesh run`), where the golden tasks above measure a single
agent. A pair session needs a second participant and a review protocol, so
it has its own driver:

- `owner` cells hand a frozen task to LocalPilot and grade the result with a
  hidden test;
- `review-bad` and `review-good` cells ask LocalPilot to review a change with a
  planted defect, and the same change done correctly, and count false AGREEs
  and false REVISEs.

Live evaluation is run by hand (see its README). CI runs `drive.py check`
and the driver unittests through `mesh_seat_eval`. Checks verify frozen hashes,
passing visible suites, hidden clean/planted discrimination, and the versioned
control's bool assertions against the planted implementation. Driver tests
exercise version selection, legacy run names, recorded identity on successful
and failed reviews, refusal of unknown cases, and existing cleanup safety.

Legacy `roman-v1` remains the default; explicit `--review-case roman-v2` adds
bool coverage to the clean control without changing historical fixtures. Raw
false-revision flags compare expected decisions; finding truth/severity is
adjudicated separately under the criteria in the driver README.

Owner driver controls use real children that write their final journal and exit
before the next poll. They cover final escalation, late submission without a
scripted agreement, and absent journals and session records. Stub lifecycle
controls cover normal live submission, one hidden measurement, actual session
completion, independent escalation/request facts, and exit during hidden testing
or delivery wait. Final-tree fallback and the first live-request observed tree
have separate provenance; neither a passing hidden test nor exit zero implies
protocol completion. Frozen fixtures and historical rows stay unchanged.

Review diagnostic capture is opt-in. CLI tests exercise the production judgement
and native mock-provider timeout/repair path. Resolved deadline metadata is
tested for builtin and explicit configuration, including zero. Driver controls
separate a repaired runtime timeout, exhausted repair and a real child-process
wall kill, preserve failed-row metadata, and mark legacy/incomplete traces.
These checks do not require response capture or a live model. CLI tests also
exercise the production judgement and observer seams with scripted initial/repair responses: valid repair,
both failures, empty/syntax/schema/validation distinctions, absent turn text,
disabled capture and injected write failure. Capture tests cover canonical
redaction before UTF-8 truncation, prompt exclusion, fresh-file/mailbox refusal
and the 1 MiB cap even with JSON-escaped text. Driver tests verify explicit
forwarding, retained artifact identity on failure and refusal to reuse an
orphan diagnostic filename. Live reruns preserve captures with each run's case
and fixture hashes; they are not required for these offline invariants.

```powershell
python crates/localpilot-mesh/seat-eval/drive.py check
```

#### LLM-as-judge quality rubric

A judge model scores the quality dimensions static signals cannot see —
readability, idiomatic style, the right abstraction, and latent-bug risk — and
records the result in the scorecard's optional `judge` block (`null` when no
judge ran). The rubric and prompt are **original** artefacts (in
`crates/localpilot-harness/src/judge.rs`); each dimension is scored `1..=5`,
higher is better, and `overall` is their mean.

The discipline that makes the scores trustworthy is built in:

- **Blinded.** Single-solution scoring puts no arm identity in the prompt, so the
  judge cannot tell LocalPilot from a baseline. A comparative preference call
  presents the two solutions in a **seed-randomized order** and maps the verdict
  back, so position is not a tell.
- **Stronger judge.** The judge model must be stronger than the subject model;
  the caller configures it. (Pairing a weak judge with the subject is a known
  failure mode — the scores would be meaningless.)
- **Offline-deterministic.** Scoring answers from a prompt-addressed cache (a
  stable FNV key), so CI never calls a model; the live path is opportunistic and
  caches its response.
- **Calibrated.** `cohens_kappa` scores the judge's labels against a
  human-labelled sample, so agreement is **reported, not assumed**. Pair the
  judge with the deterministic static signals — never rely on it alone.
- **Ranking self-test (prove the instrument before trusting it).** A cheap,
  per-run gate that complements calibration: `ranking_selftest_offline` scores a
  set of **authored** fixture pairs (`RANKING_FIXTURES` — each a `better` and a
  `worse` solution to the same task, original to this repo) and requires the judge
  to score every `better` strictly above its `worse`. If it cannot — an inverted
  or a flat judge — `score_offline_gated` refuses to score
  (`JudgeError::Untrustworthy`, naming the failed fixture) rather than emit a
  believed-but-wrong number. This runs **offline with no model** (the fixtures are
  scored from the cache), so it is the CI gate; `ranking_selftest_live` runs the
  same check against a real judge model and is **opportunistic** (the offline
  gate-logic test is the accepted bar — D008). Calibration answers "does the judge
  agree with a human?"; the ranking self-test answers the cheaper, always-checked
  "can the judge tell better from worse at all?"

The judge is a complement to, not a replacement for, the deterministic `quality`
block. Treat its absolute scores with caution and prefer **deltas between
blinded arms**.

### Snapshot Tests

Useful for:

- CLI help
- error messages
- TUI render output
- `brief.md` rendering
- `PROGRESS.md` rendering
- generated prompts
- worker loop event traces

### Live Tests

Live provider tests must be opt-in:

```powershell
$env:LOCALPILOT_LIVE_TESTS = "1"
cargo test --test live_provider
```

Live tests must:

- skip when credentials are absent
- avoid destructive tools
- keep prompts minimal
- never run in default CI

## Lesson Lab Acceptance Corpus

The lesson lab is accepted on deterministic, offline tests. No model, network
or download is needed to run them. Each row names the situation, where it is
proven, and how far up the real call path the test starts.

Entry points, from the outside in:
- **run** — a real completed harness run (`harness_cmd` tests, through
  `resume_with_provider` with a scripted provider);
- **command** — the function behind a `localpilot lab …` or `learning review …`
  command (`lab_cmd` tests);
- **adapter** — the function that command calls, with the external program
  replaced by a stand-in (`localpilot-localmind/tests/`);
- **engine** — LocalMind's own tests at the pinned revision
  (`external/localmind`).

| Situation | Proven by | Entry |
|---|---|---|
| An earned lesson: facts, hindsight, candidate, Logic result, review cards | `a_completed_run_offers_an_earned_lesson_with_its_hindsight_and_facts` | run |
| No lesson / unknown cause (safe abstention), recorded only on request | `a_completed_run_that_earns_no_lesson_queues_nothing_by_default`; `an_abstention_queues_nothing_unless_the_project_asks`; engine `case_*` in `hindsight_distillation.rs` | run, adapter, engine |
| Needs review (a lesson over a damaged or incomplete record) | `a_lesson_over_a_damaged_log_is_kept_for_review_not_queued_as_a_lesson` | adapter |
| Malformed or invented evidence from the model | `a_reply_that_keeps_breaking_the_contract_gets_one_repair_and_no_more`; engine `an_invented_id_is_refused_and_named_in_the_repair`, `truncated_json_twice_ends_in_a_fallback_that_invents_nothing` | adapter, engine |
| No model or unreachable model (fallback) | `an_unreachable_model_is_recorded_for_review_with_the_facts_and_no_cause`; `a_project_with_learning_off_spends_no_model_call` | adapter |
| Redaction | `nothing_the_logic_run_stores_carries_what_capture_redacted`; `capture_redacts_what_the_event_store_does_not_know_to` | adapter |
| Not executable (preference, intent, unsafe, style) | `preferences_intent_unsafe_actions_and_unverifiable_style_are_honestly_not_executable`; `a_lesson_no_test_can_judge_says_why_in_review_and_keeps_its_review_path` | adapter |
| Logic verdicts and the full reason table | `verdict_table`; `logic_can_never_claim_supported_contradicted_or_inconclusive` | adapter |
| Replay valid, and each way it is invalid | `yes_runs_the_previewed_replay_and_the_result_reaches_review`; `replay_lab.rs` | command, adapter |
| Supported | `a_confirmed_uplift_run_reaches_review_through_the_real_localbench` (real `localbench`, local only); `the_arms_differ_only_in_the_seeded_lesson_and_a_passing_treatment_is_supported` | command, adapter |
| Harmful, both-pass, both-fail | `no_effect_and_harm_are_results_and_stay_distinct_from_invalid`; `a_harmful_result_routes_accepted_memory_to_review_only_when_asked`; engine `a_harmful_result_holds_a_lesson_for_a_person_in_every_mode` | adapter, engine |
| The verdict interpretation table | `review_verdict_mapping`; engine `every_verdict_is_named_and_explained_and_no_tier_overclaims` | adapter, engine |
| Infrastructure failure | `a_missing_program_is_an_infrastructure_failure`; `a_root_that_cannot_be_made_is_an_infrastructure_failure`; `half_a_pair_or_a_stopped_run_is_invalid_with_its_reason` | adapter |
| Cancelled, timed out, over a ceiling | `a_cancelled_run_is_an_invalid_experiment_not_a_finding`; `a_timeout_is_a_breached_budget_and_the_worktree_goes`; `a_breached_ceiling_cancels_the_run_and_is_never_a_partial_verdict`; `a_run_cancelled_between_the_arms_is_invalid_and_its_baseline_is_only_offered` | adapter |
| Process-tree cleanup on cancel | `cancelling_reaps_the_whole_tree_as_its_effect_shows`; `cancelling_the_real_runner_reaps_the_process_tree` | adapter |
| Stale results, rewrite, split, history | `a_changed_candidate_leaves_its_result_stale_and_its_assignment_unusable`; `a_rewritten_lesson_is_history_to_the_lab_and_the_rewrite_starts_untested`; engine `review_cards.rs`, `experiment_persistence.rs` | adapter, command, engine |
| A result cannot promote itself | engine `a_supported_result_cannot_promote_itself_through_any_entry_point`, `no_tool_decides_rewrites_or_promotes_a_review_item` | engine |
| Authorization, opt-in and headless refusal | `an_uplift_run_starts_only_from_an_explicit_confirmed_command`; `nothing_runs_without_a_confirmation`; `the_permission_gate_still_decides_and_a_headless_ask_is_denied` | command, adapter |
| A rerun request runs nothing | `a_rerun_request_is_shown_and_starts_nothing_until_a_person_runs_it` | command |

Limits of this corpus, stated plainly:
- The uplift outcomes other than `Supported` are proven at the adapter, with a
  stand-in for the `localbench` program. Only `Supported` and a denied command
  go through the real binary.
- The real-binary tests run only where `LOCALPILOT_TEST_LOCALBENCH` names a
  `localbench` with the per-arm surface. CI does not build one, so there they
  print a notice and skip.
- Everything here proves the contracts, the runners and the review path. None
  of it shows that a lesson helps a model.

Live, opt-in tests (they reach a model server and are never required):

| Test | Environment |
|---|---|
| `run_hindsight_live` — the distiller over six frozen cases, both strategies, with latency | `LOCALPILOT_LIVE_TESTS`, `LOCALPILOT_LIVE_BASE_URL`, `LOCALPILOT_LIVE_MODEL` |
| `a_live_uplift_project_is_laid_down_on_request`, `a_live_uplift_run_is_confirmed_at_the_prompt_on_request` | `LOCALPILOT_LIVE_TESTS`, `LOCALPILOT_LIVE_UPLIFT_DIR`, `LOCALPILOT_LIVE_MODEL`, `LOCALPILOT_TEST_LOCALBENCH`, `LOCALPILOT_LIVE_SOLVER` |

The terminal-review tests build only with the `tui` feature. Run
`cargo test -p localpilot --features tui --bin localpilot` as well as
`cargo test --workspace`; the workspace run alone does not compile them.

## Fixture Policy

Fixtures must be authored for this repository. Do not copy fixtures from
closed-source tools or leaked projects.

Allowed fixtures:

- hand-written API responses based on public docs
- fake provider event streams
- small temporary repos
- generated files used only for tests

## Required MVP Tests

### Config

- default config loads
- project config overrides user config
- env overrides project config
- CLI overrides env
- secrets are redacted in debug output

### Provider

- text request translates correctly
- tool schema translates correctly
- streaming text parses correctly
- streaming tool call parses correctly
- reasoning/thinking events parse correctly
- malformed stream returns typed error
- quota reset metadata is classified correctly

### Tools

- read file in workspace
- deny read outside workspace
- write file in workspace
- deny write outside workspace
- edit exact match
- reject ambiguous edit
- shell read-only allowed
- shell destructive denied in non-interactive mode

### Harness

- parse valid brief
- reject brief missing required section
- parse valid progress
- reject progress with duplicate step number
- next incomplete step selection
- mark step complete
- attempt counter increment
- rule retry path
- rule discard path
- replan cap
- golden-task smoke scenario
- quota pause/resume at a step boundary

### Recovery

- slash flood outside code is detected
- slash-like content inside fenced code is not detected
- repeated-token loop is detected only after a threshold
- malformed tool calls trigger recovery
- exhausted recovery cannot complete a harness step

### Context

- compaction preserves tool-result pairing
- compaction preserves current step contract
- memory injection respects token caps
- stale memory is not injected when relevance is below threshold

### Store

- transcript write/read round trip
- interrupted write leaves no corrupt session
- redaction before persistence

## CI Matrix

Platforms:

- Windows latest
- Ubuntu latest
- macOS latest

Commands:

```powershell
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo test --workspace
cargo check --workspace
```

Linux executes every workspace package in a named, resource-capped nextest
step; process-heavy tools and the harness corpus retain their serial execution.
`cargo test -p localpilot --test linux_ci_coverage` compares the gating job's
Linux test steps with `cargo metadata --offline --no-deps`. Adding or removing a
workspace package requires the matching workflow change. Build-only steps,
comments, other platforms, and the separate coverage job do not satisfy this
guard.

Supply-chain hygiene:

```powershell
cargo audit
cargo deny check
cargo machete
```

These are blocking before public release and run in CI's supply-chain job.

### Review search progress and safe completion

`cargo test -p localpilot-harness --test readonly_verification --test granularity --test verify_gate`
covers refused review writes retaining their attempt-budget cost, explicit
optional checks staying permission-gated, and authorized successful/partial
writes retaining required verification after a readonly downgrade. The partial
write fixture reports no touches and has no Git baseline. Existing changed-unit,
verification-failure and refused-checkpoint controls remain required.
`cargo test -p localpilot --bin localpilot mesh_run::tests::readonly_review_denied_mutation`
replays refused writes/test attempts through runtime and production review
judgement, asserting unchanged fixture SHA-256 and a valid verdict with Done.
This is offline protocol evidence, not a live model-quality claim.

`cargo test -p localpilot --bin localpilot mesh_run::tests::review_search_replay`
runs three offline fake-provider scenarios through real readonly tool dispatch
and production review judgement. Equivalent-query variants reach the 200-call
ceiling and malformed repair escalates; discovery of 185 new lines completes;
exact-repeat detection stops early and a valid repair can still post a verdict.
All start with legitimate oversized-read refusals under the automatic envelope.
These tests pin heuristic limitations and protocol safety, not live model
quality. See [the evaluation findings](mesh-model-evaluation.md#search-stall-investigation).

### Context metadata and budget parity

`cargo test -p localpilot-llm context_` exercises mock-server props/listing
precedence, model routing without autoload, authentication, per-slot capacity,
invalid/training-only metadata, timeout and redirect refusal.
`cargo test -p localpilot --bin localpilot context_` checks caps, output reserves,
concurrent cache reuse, failed/disabled probes, and actual headless and synchronous
server/worker runtime budgets. These checks make no live model calls.
