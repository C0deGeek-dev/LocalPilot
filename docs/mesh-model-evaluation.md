# Pair-seat model evaluation: recorded results and limits

Results transcribed from the evaluation record on 2026-09-30. The runs were
made on 2026-09-29 with build `c309f38`, each
model's tuned LocalBox profile (256k server context), and LocalPilot's window
set to 131072. The repeatable frozen tasks, hidden tests, planted defect,
clean control and hash manifest live in
[`crates/localpilot-mesh/seat-eval/`](../crates/localpilot-mesh/seat-eval/README.md).

| Model | Owner hidden tests passed | Planted defect correctly revised | Clean change correctly agreed | Owner wall time |
|---|---|---|---|---|
| Qwen 3.6 35B A3B APEX | 2/3 | 3/3 | 1/3 | 34–88 s |
| Qwen 3.8 Flash Next heretic 2 | 3/3 | 0/1 | Not run | 145–595 s |
| Qwen 3.8 27B heretic abliterated | 0/3 | Not run | Not run | Stopped after 360–540 s |

The A3B duration implementation rejected valid zero input. Its two false
revisions contained reasoning that contradicted the blocking finding. Flash
Next missed the planted boolean-input defect. The 27B model invented external
paths, wrote an incorrect Roman-numeral conversion, or did not produce the
requested module. The protocol kept verdicts well formed, checked cited anchors,
required agreement to close, and refused out-of-tree writes. Those checks do
not make model judgement reliable. A3B's issued anchors verified in the review
runs.

These samples are descriptive (one to three runs per cell), not a statistical
result, a release-quality threshold, or a measurement of the current build.
Raw runs were retained only on the original machine and are not available in
this repository. Cloud models were not evaluated. Missing cells are unknown,
not passes.

Further runs, additional planted defects, the proposed review-brief change,
and whether a LocalPilot agreement may be the only agreement remain unresolved.
This report summarizes existing evidence; it neither runs nor closes that
evaluation.

## Current-build review baseline (2026-10-02)

Six serial reviews ran on Windows against source revision
`22f007c7551ae169bf4f60c849745b2fe25213f2`, using Qwen 3.6 A3B APEX's
saved LocalBox profile: APEX-I-Quality, 256k context, turboquant, reasoning
off. The direct OpenAI-compatible LocalBox endpoint reported a 262144-token
window (`context_source=server_props`); no smaller configured cap applied.
Global memory was isolated to each scratch project with
`LOCALMIND_GLOBAL_ROOT=@project`. Each review had a 180-second wall cap.

| Cell | Decisions, in run order | Correct | Wall time per run |
|---|---|---|---|
| Planted boolean defect | REVISE, REVISE, REVISE | 3/3 | 29.8, 8.6, 8.8 s |
| Clean control | AGREE, REVISE, AGREE | 2/3 | 6.9, 11.9, 29.3 s |

All six produced verdicts and exited normally; none timed out. Each planted
defect finding identified the missing boolean exclusion. The false revision
on clean run 2 concluded "No actual bug found" and "Verdict should be AGREE"
inside a blocking finding, yet still emitted REVISE. Structural verdict
validation therefore still permits a finding that contradicts its decision.
Read requests without explicit page bounds were refused; one planted review
also attempted a patch and an unapproved verification command. Those refusals
are visible in its retained engine log; they did not prevent the final verdict.

An initial 8k-context review correctly revised the planted defect but is
excluded from these six samples and retained separately. These are descriptive
samples, not evidence of improvement over September: the source revision,
context budget and provider route changed together. No review prompt or
semantic verdict guard was changed. Flash Next, 27B, owner-task reruns, larger
samples and additional defect cases remain unevaluated in this batch. Future
Flash Next reviews need a larger wall cap; 900 seconds per review is the
declared follow-up budget, not a measured result.

## Flash Next review batch (2026-10-02)

Six serial reviews ran on Windows at source revision
`c9c268c292cef61ae3b9ca1eaab7d6ea27e3d6cb` with the saved LocalBox Flash Next
heretic 2 tune: IQ3_XXS, 256k, native, reasoning off. The runtime and frozen
fixtures were unchanged from the preceding A3B batch. The direct endpoint,
server-discovered 262144-token window, scratch memory isolation and review
permissions were the same; the wall cap was **900 seconds per review**.
Available RAM after loading was about 5.2 GiB; loading was not included in
review wall times and no GPU-memory measurement was captured.

| Cell | Outcomes, in run order | Correct | Wall time per run |
|---|---|---|---|
| Planted boolean defect | REVISE, REVISE, REVISE | 3/3 | 143.5, 58.7, 92.6 s |
| Clean control | ESCALATE, ESCALATE, AGREE | 1/3 | 72.5, 613.4, 132.1 s |

The escalations contain no verdict: they are failed completions, not false
revisions or timeouts. All six engine exits were zero; none hit the wall cap.
Each planted review identified the true missing boolean exclusion. Secondary
claims still need checking: bad run 1 incorrectly claimed that `False` returns
an empty string, although the fixture's range guard raises `ValueError`.

Two harness follow-ups are recorded. The whole-file read exception compares
file bytes with `max_read_lines`, so these 18/25-line files exceed the small-file
exception (LocalHub#203). Clean run 2 then made 200 tool calls, including 185
searches with 170 distinct arguments, before its tool budget stopped the turn.
The subsequent repair produced no JSON verdict, so the engine escalated
(LocalHub#204). The hard budget and safe escalation worked; investigate read
guidance and progress detection before attributing this outcome solely to
model judgement. Reviewer edits and unsafe commands remain deliberately refused.

This closes the six-sample Flash Next review batch, not the broader evaluation.
The September false agreement and this batch use different builds, contexts and
provider routes; do not combine them into one success rate or infer a causal
improvement. Larger samples, 27B, owner reruns and controlled interventions remain.

## Clean controls after the read correction (2026-10-02)

Three fresh clean reviews used source revision
`e25af739ece0fde81a662aca987c896ffe3bcc9c` after fixing LocalHub#203's separate
byte/line bounds. The saved Flash Next LocalBox tune, frozen fixtures, request,
server-discovered 262144-token context, isolated memory, permissions and
900-second wall cap stayed the same. No prompt or progress-policy change applied.

| Run | Outcome | Wall time | Tool calls | Successful reads / read errors |
|---|---|---|---|---|
| 1 | ESCALATE: no JSON verdict after repair | 153.2 s | 5 | 2 / 0 |
| 2 | REVISE: important bool-test coverage gap | 101.5 s | 5 | 1 / 0 |
| 3 | AGREE: minor bool-test coverage gap | 164.3 s | 25 | 3 / 0 |

The driver records 1/3 expected agreements, one false revision and one missing
verdict. All engines exited zero; none timed out or exhausted the tool budget.
All logged reads succeeded; page arguments are absent from trace summaries,
so these traces alone do not prove which reads were whole-file reads. The
production-dispatch regressions establish that behavior and permission ordering.
Shell attempts remained refused. LocalBox was stopped after the batch.

The revision's factual finding is supported: the nominal clean tests omit
True/False even though the implementation correctly rejects bool. Run 3 identifies
the same omission at minor severity and agrees. LocalHub#205 records this
fixture/severity ambiguity; preserve raw driver classifications while adjudicating
finding validity separately. Run 3's proposed monotonic-length check is invalid
for Roman numerals (III to IV decreases length), despite its correct agreement.

The prior clean batch also had 1/3 agreements, but only one completed verdict;
this batch has two. Three unseeded samples cannot establish a causal reliability
improvement. LocalHub#204's search investigation, fixture calibration and the
broader evaluation remain open. The frozen fixture and historical results were
kept unchanged throughout this rerun.

## Review-control calibration (2026-10-02)

LocalHub#205 settles evaluation review scope: both submitted implementation
and tests matter. Demonstrated requirement violations are blocking; missing
direct regression assertions for an explicitly required behavior or named
input category are important. Optional exhaustive/property coverage is minor
without a demonstrated defect. AGREE requires no supported blocking/important
finding. These are evaluation/adjudication criteria, not a production prompt
or validator change.

The original control is `roman-v1`, still the driver's default. Its bool-test
omission makes its declared AGREE expectation ambiguous under these criteria.
The Flash Next read-fix run 2 REVISE is supported; its raw `false_revise` flag
remains an expectation mismatch, not proof of an invented finding. Run 3's
AGREE reports the same omission as minor and would understate its severity
under the settled criteria. A3B's clean run 2 blocking finding still contradicts
itself and supports no defect; the fixture omission does not justify that
unrelated reasoning. September's clean revisions cannot be readjudicated from
the summary alone because their raw findings were not retained here.

An explicit `--review-case roman-v2` selects new clean tests with True/False
assertions, the same implementation/spec and the unchanged planted fixture.
The planted side retains its passing visible tests, so the two cells have
different visible test suites. Offline checks prove hidden acceptance still
discriminates and the v2 clean suite fails against the planted implementation.
Run names and recorded case/fixture/spec hashes distinguish v2. All original
fixture bytes, manifest entries and historical results remain unchanged.
See the [driver criteria and usage](../crates/localpilot-mesh/seat-eval/README.md).
No v2 model samples have been collected; compare future v2 trials separately.

## Opt-in diagnostic continuation (2026-10-02)

Three fresh Flash Next reviews used source
`0163a0856fef7b7dfe10c9d86a0d55ef84f96f5e`, rebuilt after committing,
with explicit review-attempt capture (LocalHub#206). They reused **roman-v1**,
the saved LocalBox IQ3_XXS / 256k / native tune, reasoning off, server-discovered
262144-token context, isolated memory, read-only permissions and 900-second cap.
Prompt, retry count, fixtures and progress policy were unchanged.

| Run | Raw outcome | Wall time | Captured attempts |
|---|---|---|---|
| 1 | AGREE | 265.1 s | Initial accepted; runtime stop NoProgress |
| 2 | AGREE | 92.7 s | Initial prose refused; valid repair accepted |
| 3 | AGREE | 106.1 s | Initial accepted; runtime stop Done |

All engines exited zero; no timeouts, final missing verdicts or tool-budget
exhaustion. Four untruncated attempt samples were captured and fixture hashes
remained unchanged. LocalBox was stopped afterward. These are legacy expected
decision matches; the bool-test omission and severity caveat from LocalHub#205
still apply, so they do not establish three adjudicated correct reviews.

Run 2's initial parser input was exactly "Looking at the diff." (20 bytes),
without a JSON object. The engine correctly refused it and repaired successfully.
This proves premature prose completion in that attempt; prior two-attempt
failures lack captured response text, so their exact cause remains unknown.
No two-attempt failure reproduced here; three unseeded samples cannot establish
a general reliability improvement.

LocalHub#207 records a distinct termination question: run 1's refused mutation
attempts led automatic verification to try denied `python`, stopping NoProgress
while its valid verdict still posted. Preserve mutation budgets/permissions
when investigating intended readonly review verification. LocalHub#204 remains
open; no progress-policy changes were made during these trials. New paired v2
reviews and the broader model evaluation remain outstanding.
