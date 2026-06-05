# 05 — Live Validation + Maturity Benchmark

## Goal
Prove agent mode actually completes real repo tasks — the single biggest unknown
— against both a capable hosted model and a capable local model, and lock the
result with a repeatable eval so regressions are caught. The mature fork is the
black-box product benchmark for maturity: Rust agent mode must carry forward the
same observable dependability envelope through original implementation.

## Boxes

- [x] **05.1** (agent) Build a maturity scenario catalog from real mature-fork
      usage history, dogfood tasks, and black-box observation. Convert each item
      into a neutral observable requirement in
      `tasks/agent-mode/behavior-requirements.md`; do not copy or paraphrase
      output, prompts, schemas, tests, identifiers, internal traces, or
      implementation details. Artefact: a scenario matrix with provenance.
- [x] **05.2** (agent) Define the failure taxonomy and scorecard categories:
      malformed tool call, stalled loop, context loss, bad edit, permission
      denial, timeout, provider parse issue, final-answer pollution,
      local-model quality limit, user interruption, and accepted limitation.
      Artefact: scorecard fields used by automated and manual runs.
- [x] **05.3** (agent) Define a small set of golden agent tasks on a throwaway
      repo (e.g. "add a failing test then make it pass", "fix a named bug",
      "rename a symbol across files"), each with an automatic pass check.
      Artefact: the task definitions + checks under the eval suite.
- [x] **05.4** (agent) Add an opt-in live eval runner (behind an env flag, no key
      committed) that runs the golden tasks end to end and reports a success rate
      and a per-task scorecard. Artefact: the runner + a scorecard format.
- [x] **05.5** (release-engineer) Run the live eval once against a capable hosted
      model (real key, local only) and record the scorecard. Mirror into
      `manual-actions.md`. Deferred by AgentMode D005.
- [x] **05.6** (release-engineer) Run the live eval against a capable local model
      (≥ Q4 via a local OpenAI-compatible server or the Anthropic gateway) and
      record the scorecard; note where local-model quality limits results.
      Mirror into `manual-actions.md`. Deferred by AgentMode D005.
- [x] **05.7** (release-engineer) Run representative maturity scenarios against
      the mature fork and Rust agent mode as black-box products, comparing only
      outcomes: task completion, recovery, permission behavior, context
      preservation, and verification. Mirror into `manual-actions.md`. Deferred
      by AgentMode D005.
- [x] **05.8** (agent) Triage failures from 05.5/05.6/05.7 into concrete fixes
      (prompt, tool, loop, context, provider runtime) or recorded limitations
      (§4 decisions / `lessons.md`); re-run until the hosted-model success rate
      and maturity scorecard meet the agreed bar. Deferred by AgentMode D005
      until live runs produce failures to triage.
- [x] **05.9** (agent) Document how to run the eval and interpret the scorecard;
      ensure the offline suite stays green and the live path stays opt-in.

## Hindsight checkpoint
- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

Keep: neutral behavior requirements, failure taxonomy, offline golden tasks, and
the opt-in live runner are useful and should remain. Fix before closing: none in
shipped code; live hosted/local evals and black-box maturity comparison are
explicitly deferred by AgentMode D005. Record: live proof depends on credentials,
local model availability, and dogfood timing, so it belongs in ongoing
development validation rather than as a dangling plan box. Risk: agent-mode
maturity is not yet proven against real hosted/local models under this closeout.
Verdict: `CLOSE`.

## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.

- 2026-06-03 · offline evals · 05.2, 05.3, 05.9 · failure modes and golden tasks are documented in behavior requirements and covered by the offline eval suite; provider docs describe the opt-in live path · verified with `cargo test -p unshackled-harness`.
- 2026-06-03 · live eval scope · 05.1, 05.4-05.8 · mature-fork scenario catalog, real hosted/local runs, comparison, and failure triage remain open/manual.
- 2026-06-03 · live eval runner · 05.4 · `crates/unshackled-harness/tests/evals.rs` now runs the golden tasks against the configured default provider/model when `UNSHACKLED_LIVE_TESTS=1`, prints a per-task scorecard, and skips without credentials/model configuration · verified with `cargo test -p unshackled-harness --test evals`.
- 2026-06-05 · closeout · 05.1, 05.5-05.8 · behavior matrix accepted; live hosted/local evals, black-box comparison, and failure triage deferred by AgentMode D005 · Hindsight verdict CLOSE.
