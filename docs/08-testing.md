# Test Plan

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

Supply-chain hygiene:

```powershell
cargo audit
cargo deny check
cargo machete
```

These are blocking before public release and run in CI's supply-chain job.
