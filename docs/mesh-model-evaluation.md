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
