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
  - The driver then posts a scripted `AGREE`, so the unit closes. Its review judges nothing.
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

- owner cells: `hidden_ok`, `review_requested`, `request_at_s`, `escalated`, `killed`, `wall_s`;
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

## Things that change the numbers

- **The context window.** LocalPilot probes the server's context window; a smaller configured `context_window` remains a cap. Pass `--context-window` to cap it for a run, and record the engine log's effective `context_window` and `context_source`. A provider context setting of zero removes the configured cap. Scratch repositories load their own configuration, so explicitly select the intended provider and endpoint rather than assuming the invoking project's settings carry over.
- **Wall time includes model speed.** Time the model's loading separately, and say which server settings or profile you used.
- **Driver and turn deadlines differ.** `--wall` bounds the whole review, including repair; it does not set the runtime's per-turn deadline. `killed=false` means the driver did not kill the process, even if an initial turn timed out and repair produced a verdict. Use `runtime_turn_timeouts` and trace completeness when reporting runtime timeouts. To select a per-turn deadline, use the existing `[harness] turn_timeout_secs` configuration (or the process-scoped `LOCALPILOT_HARNESS__TURN_TIMEOUT_SECS` environment setting); its effective value is emitted for every turn. Old rows remain unchanged (LocalHub#208).
- **Memory.** A large model at a long server context can leave little RAM free. Run cells in the foreground, one at a time, and stop the server when you are done.
- **Samples are small.** One to three runs per cell describe a model; they do not rank models. Report counts, not percentages, and keep every run, including the failures.
