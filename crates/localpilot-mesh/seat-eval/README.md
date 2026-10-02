# Pair-seat evaluation

How well does a model do in LocalPilot's pair seat, `localpilot mesh run`?
This directory is a small, repeatable harness for that question. It measures
the model's judgement; the pair protocol itself is held to the conformance
suite in `../conformance`.

## What is here

| Path | What it is |
|---|---|
| `drive.py` | The driver: `check` verifies the fixtures, `run` drives one cell. |
| `tasks/<task>/spec.md` | A frozen task (`slug`, `roman`, `duration`), given to the owner as the session's task. |
| `tasks/<task>/hidden.py` | Its hidden acceptance test, run on the owner's tree. It prints `HIDDEN_OK` on success. |
| `review/planted/` | A roman change with a planted defect: `bool` is accepted, which the spec forbids. |
| `review/clean/` | The same change done correctly: the clean control. |
| `review/roman-v2/clean/` | A versioned clean control that also pins the explicit bool requirement in its visible tests. |
| `FIXTURES.sha256` | The fixtures' hashes (CRLF read as LF). `check` fails if any fixture changes. |
| `test_drive.py` | The driver's own tests (no model): it deletes only runs it made, refuses changed fixtures, and never leaves an engine running. |

The fixtures are frozen, so results from different dates and models stay comparable. To change one, add a new task or review case, and regenerate the manifest in the same change.

## Cells

- **owner**:
  - The driver, as `claude`, starts a pair session with the task's spec and hands the unit to `localpilot`. `mesh run --own` then implements it.
  - At localpilot's first `REVIEW_REQUEST`, the hidden test runs on the tree as it is. That result is the measure.
  - While the engine is still running, the driver then posts a scripted `AGREE` to let the unit close. Its review judges nothing.
  - After the engine exits or is killed, the driver reads the final owner journal again. A late request is recorded without posting an agreement. If no live-request hidden measurement exists, the hidden test runs on the final tree; that is fallback evidence, not a captured submission snapshot. Escalation and submission are independent facts.
- **review-bad**: the driver submits the planted defect for review. `REVISE` is expected; an `AGREE` is a **false AGREE**.
- **review-good**: the driver submits the selected clean change. `AGREE` is expected; a `REVISE` sets the raw **false REVISE** flag. That flag compares a decision with the declared expectation; it does not adjudicate whether each finding is true.

The clean control is what makes a false REVISE visible. Without it, a model that rejects everything would look perfect.

## Review cases and severity criteria

Both the implementation and submitted tests are in review scope. The evaluation
criteria are:

- **Blocking:** demonstrated behavior contradicts a required result, such as accepting True instead of raising ValueError.
- **Important:** a required behavior or explicitly named input category has no direct regression assertion, such as omitting bool tests. REVISE is warranted even if implementation behavior is currently correct.
- **Minor:** optional exhaustive, round-trip or additional sample coverage without a demonstrated defect or missing required category. Such suggestions alone do not require REVISE.

AGREE means no supported blocking or important finding in either file. These
criteria guide fixture design and human adjudication; they do not change the
production review prompt, validator or permissions. Structured acceptance and
verified anchors do not establish finding truth. Keep raw driver flags and
adjudicated finding validity separate.

`--review-case roman-v1` is the default and preserves legacy run names and all
original fixtures. Its implementation is correct, but its clean tests omit
True/False. Under the settled criteria, a supported coverage finding may warrant
REVISE; historical AGREE expectations are therefore ambiguous. Retain the raw
scores and do not relabel those samples as v2.

`--review-case roman-v2` selects new clean tests that assert both True and False
raise ValueError, with the identical clean implementation and task spec. It
reuses the unchanged planted implementation **and its original visible tests**,
so the planted review still needs to discover the bool defect rather than read
a reported failing test. The two cells intentionally have different test suites.
All submitted visible suites pass; hidden acceptance passes clean and fails
planted. The driver's offline tests also prove the v2 clean suite fails when
paired with the bool-accepting implementation.

V2 run names include `-roman-v2-`; review rows, including driver failures, record
`review_case`, normalized SHA-256 `review_fixture_hashes` and `review_spec_hash`.
Rows predating these fields used roman-v1. Owner cells remain unchanged and
reject a v2 review-case option. Compare versions separately; never pool v1/v2
clean scores or replace old rows, logs, fixtures or hashes.

For new review samples, add `--review-diagnostics` to opt into the CLI's bounded
redacted initial/repair attempt capture. The driver saves `<name>.review.jsonl`
beside the engine log, outside the scratch repo/mailbox, and records the relative
artifact name and `review_diagnostics_present` in success/failure rows. No
capture flag is sent by default; owner cells reject it. An orphan capture also
makes a run name occupied, so diagnostics cannot be silently overwritten.
Preserve this artifact with the run's results, log, participant journal and
fixture identity. It is a manual export (8 KiB response samples, 1 MiB file cap),
not an exact provider wire transcript. Inspect before sharing; captured assistant
text can quote code and canonical redaction is best-effort. See configuration
docs for the versioned record format and null/truncation semantics.

## Running it

You need Python 3.9 or newer, Git, and a `localpilot` binary built from this repository. You also need a model server that LocalPilot's configured provider reaches, serving the model you name.

```sh
python drive.py check          # no model needed; CI runs this too
```

Then, with the server up, run one cell at a time:

```sh
python drive.py run --model <served model name> --label a3b --cell owner --task roman --run 1 --out ../../../target/seat-eval
python drive.py run --model <served model name> --label a3b --cell review-bad --run 1 --out ../../../target/seat-eval
python drive.py run --model <served model name> --label flash-v2 --cell review-good --review-case roman-v2 --review-diagnostics --run 1 --wall 900 --out ../../../target/seat-eval-v2
```

The served model's name is the one the server lists, for example the `id` in `GET /v1/models`.

A full set for one model is `owner` for each of the three tasks, plus `review-bad` and `review-good` repeated as often as you want samples. For example:

```sh
for t in slug roman duration; do python drive.py run --model "$M" --label "$L" --cell owner --task "$t" --run 1 --out "$OUT"; done
for n in 1 2 3; do
  python drive.py run --model "$M" --label "$L" --cell review-bad --run "$n" --out "$OUT"
  python drive.py run --model "$M" --label "$L" --cell review-good --run "$n" --out "$OUT"
done
```

`run` refuses to start if any fixture differs from the manifest. It also refuses a `--label` other than 1-40 of `A-Z a-z 0-9 . _ -`, starting with a letter or digit. Every run's directory is a direct child of `--out`, and a run name is used once: if its directory, log or results row already exists, `run` refuses, so every row keeps its evidence. Repeat a cell with a new `--run`. The driver marks each directory it makes with `.seat-eval-run`, and never deletes one without it. The engine is always stopped when a run ends, including when the driver itself fails. The failure is recorded as `driver_error` in `results.jsonl`, and the command exits non-zero.

Each run leaves its scratch repository and engine log under `--out`, and appends one JSON line to `<out>/results.jsonl`. The fields are:

- owner cells: `hidden_ok`, `review_requested`, `request_at_s`, `escalated`, `killed`, `wall_s`, plus the provenance fields below;
- review cells: `decision`, `expected`, `false_agree`, `false_revise`, `no_verdict`, the full `verdict`, `wall_s`, `review_case`, `review_fixture_hashes`, `review_spec_hash`.
  Rows also record `review_diagnostics` (relative artifact name, or null) and
  `review_diagnostics_present` (whether the file was created); false does not
  imply a successful review or complete diagnostic evidence.
- all cells: `wall_cap_s`, `runtime_turn_deadlines` (one `{seconds, source}`
  per started model turn), `runtime_turn_stops`, `runtime_turn_timeouts`
  (observed TimedOut count, null when no turn trace exists), and
  `runtime_trace_complete`. Complete means matched starts/stops, clean engine
  exit, no driver error and no kill; otherwise counts are partial observations.
  Builtin/config sources come from the runtime's resolved configuration, not
  the wall cap. Config includes explicit file/environment settings. Metadata
  also survives failed rows and does not require review response capture.

Review scoring now separates protocol outcomes from model-quality evidence.
`decision`, `expected`, `false_agree`, `false_revise` and `no_verdict` keep their
raw protocol meanings. An automatic integrity `REVISE` can set `false_revise`
on a clean request; it is not a model judging the clean change incorrectly.
Use `model_quality_eligible` to select model-quality samples, and report excluded
samples separately. For eligible rows, `model_decision`, `model_expected_match`,
`model_false_agree` and `model_false_revise` carry the model-only comparison.
Unknown/ineligible comparisons are null, not a successful expected match.

`review_source` is `model`, `automatic` or `unknown`. Automatic final verdicts
have a bounded `review_refusal_reason`: `request_integrity` for manifest checks
before judgement, or `tree_changed` for the integrity recheck after judgement.
`review_sample_status` is `model_judged`, `invalid_sample`, `incomplete` (known
model origin but engine exit/kill incomplete), or `unknown`. Model origin is
identified by the engine's successful native post boundary, never by tool counts,
absence of diagnostic capture, verdict prose or inferred legacy runtime traces.
The driver requires one valid native provenance record matching the final
verdict, request, session and unit. Missing, malformed, oversized, duplicate or
mismatched records leave origin unknown; final decisions remain visible.
Historical rows remain unchanged and missing provenance is never backfilled.

Native `REVIEW_PROVENANCE` trace records contain fixed categories and protocol
identifiers only, are redacted and limited to 2,048 UTF-8 bytes per line, and
remain available with review capture disabled. Oversized identities produce
an unavailable notice; partial/unconfirmed posts do not emit provenance. These
records are local observation evidence, not authenticated attestations.

Owner provenance is additive; historical rows are preserved. `review_observation`
is `live`, `after_exit`, or null, and `review_request_id` identifies the first
observed request when present. `request_at_s` is elapsed time at live observation,
not the journal's send time. `hidden_measure=first_request_observed_tree` means
the check ran on the tree at the first live request observation; polling does
not guarantee an immutable snapshot at the exact instant of submission.
`final_tree_after_exit` means the check ran after the child stopped, including
when a request was discovered only then. A passing fallback does not establish
successful submission or closure. `protocol_status` comes from the single
retained scratch session record; `protocol_completed` is true only for
`completed`, false for other known states, and null when no status is available.
Neither exit zero nor a posted scripted agreement establishes completion.

`--wall` keeps its 2700-second default. For owner cells it is a **soft loop
budget**, checked between synchronous pair commands and hidden tests. Idle poll
sleep uses the remaining budget, but an operation already in progress can
overrun. After the loop, `--owner-exit-grace` allows an additional engine wait
(default 60 seconds); zero skips that grace and kills a still-running engine.
The option is owner-only. Neither setting is a hard whole-command deadline.
For review cells, `--wall` is the direct engine wait budget, including repair.
Setup, journal reconciliation, and final assessment are outside that wait.

New rows name this distinction with `wall_semantics`:
`owner_loop_budget_plus_exit_grace` or `review_engine_wait`. Owner rows also
record `owner_loop_budget_s`, `owner_exit_grace_s`, `owner_loop_s`,
`owner_loop_deadline_reached` (observed elapsed loop time reached its budget),
`owner_loop_overrun_s`, and `owner_exit_wait_s` (including kill/cleanup).
`killed=false` can coexist with a reached loop deadline when the engine exits
during grace. Neither field is a native turn timeout or a protocol outcome.
`hidden_check_s` measures hidden testing; a live-request check is inside loop
time, while fallback checking is inside `post_exit_s`. `cell_wall_s` measures
the full timed owner cell from engine startup through final assessment, excluding
scratch/session setup. Legacy `wall_s` remains elapsed time before fallback
hidden testing, including any live-request check and journal reconciliation.
Failed rows retain the selected settings; absent elapsed/observation fields are
unknown. Historical rows and their meaning are unchanged.

## Things that change the numbers

- **The context window.** LocalPilot probes the server's context window; a smaller configured `context_window` remains a cap. Pass `--context-window` to cap it for a run, and record the engine log's effective `context_window` and `context_source`. A provider context setting of zero removes the configured cap. Scratch repositories load their own configuration, so explicitly select the intended provider and endpoint rather than assuming the invoking project's settings carry over.
- **Wall time includes model speed.** Time the model's loading separately, and say which server settings or profile you used.
- **Driver and turn deadlines differ.** `--wall` selects the loop/wait budget described above; it does not set the runtime's per-turn deadline. `killed=false` means the driver did not kill the process, even if an initial turn timed out and repair produced a verdict. Use `runtime_turn_timeouts` and trace completeness when reporting runtime timeouts. To select a per-turn deadline, use the existing `[harness] turn_timeout_secs` configuration (or the process-scoped `LOCALPILOT_HARNESS__TURN_TIMEOUT_SECS` environment setting); its effective value is emitted for every turn. Old rows remain unchanged (LocalHub#208).
- **Memory.** A large model at a long server context can leave little RAM free. Run cells in the foreground, one at a time, and stop the server when you are done.
- **Samples are small.** One to three runs per cell describe a model; they do not rank models. Report counts, not percentages, and keep every run, including the failures.
