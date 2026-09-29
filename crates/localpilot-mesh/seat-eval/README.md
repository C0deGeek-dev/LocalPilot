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
| `FIXTURES.sha256` | The fixtures' hashes (CRLF read as LF). `check` fails if any fixture changes. |
| `test_drive.py` | The driver's own tests (no model): it deletes only runs it made, refuses changed fixtures, and never leaves an engine running. |

The fixtures are frozen, so results from different dates and models stay comparable. To change one, add a new task or review case, and regenerate the manifest in the same change.

## Cells

- **owner**:
  - The driver, as `claude`, starts a pair session with the task's spec and hands the unit to `localpilot`. `mesh run --own` then implements it.
  - At localpilot's first `REVIEW_REQUEST`, the hidden test runs on the tree as it is. That result is the measure.
  - The driver then posts a scripted `AGREE`, so the unit closes. Its review judges nothing.
- **review-bad**: the driver submits the planted defect for review. `REVISE` is expected; an `AGREE` is a **false AGREE**.
- **review-good**: the driver submits the clean change. `AGREE` is expected; a `REVISE` is a **false REVISE**.

The clean control is what makes a false REVISE visible. Without it, a model that rejects everything would look perfect.

## Running it

You need Python 3.9 or newer, Git, and a `localpilot` binary built from this repository. You also need a model server that LocalPilot's configured provider reaches, serving the model you name.

```sh
python drive.py check          # no model needed; CI runs this too
```

Then, with the server up, run one cell at a time:

```sh
python drive.py run --model <served model name> --label a3b --cell owner --task roman --run 1 --out ../../../target/seat-eval
python drive.py run --model <served model name> --label a3b --cell review-bad --run 1 --out ../../../target/seat-eval
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
- review cells: `decision`, `expected`, `false_agree`, `false_revise`, `no_verdict`, the full `verdict`, `wall_s`.

## Things that change the numbers

- **The context window.** LocalPilot budgets against its provider's configured `context_window`, not the server's real window. Pass `--context-window` to set it for a run, and record what you used.
- **Wall time includes model speed.** Time the model's loading separately, and say which server settings or profile you used.
- **Memory.** A large model at a long server context can leave little RAM free. Run cells in the foreground, one at a time, and stop the server when you are done.
- **Samples are small.** One to three runs per cell describe a model; they do not rank models. Report counts, not percentages, and keep every run, including the failures.
