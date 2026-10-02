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
