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
No v2 model samples had been collected at this calibration checkpoint; the
later v2 trials below remain separate from historical v1 samples.

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
when investigating intended readonly review verification. No progress-policy
changes were made during these trials. New paired v2 reviews and the broader
model evaluation remain outstanding.

### Search-stall investigation

LocalHub#204's offline replay establishes a limit of the current heuristic,
rather than a broken cost bound. Successful calls use tool name plus arguments
as their novelty signature; stuck-repeat counts identical signature/output
pairs. Over 12 successful calls the novelty floor is 0.34; an identical pair
trips at three occurrences, followed by one grace call on the default rail.
Different queries can therefore return identical evidence without tripping.
Successful searches also reset the consecutive-failure streak and same-error
breaker; initial read refusals do not classify later successful searches as
failures. Automatic WorkProfile bounds individual reads/output and mutation
scope, not cumulative search novelty. The review brief already supplies the
task, diff, anchors and required JSON shape; it does not restrict discovery
to the changed lines.

Three fake-provider scenarios run real readonly dispatch with the automatic
262,144-token envelope and default 200-call ceiling, then the production
review parser/validator/repair boundary:

- Two legitimate oversized whole-read refusals, then equivalent regex variants
  and spaced exact repeats return one unchanged search result. The turn stops
  at 200 calls with BudgetExceeded and no progress nudge. A malformed repair
  leads to explicit ESCALATE, with no fabricated verdict.
- The same initial refusals followed by 185 distinct searches reveal 185 new
  source lines. All 187 calls finish, and a provider-authored valid AGREE posts.
- Identical successful searches trip the existing detector: three occurrences
  plus one grace search, six total calls including the refusals. NoProgress
  ends the initial turn; a provider-authored valid repair still posts VERDICT.

These are controlled mechanism replays, not exact reproductions of the old
model stream: historical logs retained query summaries, not complete arguments,
results or response text. No model reliability or verdict-quality improvement
is claimed. No runtime policy change is warranted by this evidence: identical
results can validate distinct boundary hypotheses, and negative searches can
establish absence. Output deduplication or query-count limits would cut such
legitimate investigation without proving the model has stalled. Keep the hard
bound and safe escalation, and retain missing-verdict, incorrect-verdict and
wall-timeout outcomes separately. The current model limitation remains part
of the broader evaluation; denied automatic verification is separate work.

### Readonly verification correction

LocalHub#207's offline replay reproduced NoProgress after refused mutation/test
attempts with unchanged source hashes and a valid review verdict. The completion
gate had treated attempted-operation cost as implementation requiring tests.
It now distinguishes refused attempts from authorized invocations that may have
written. An unchanged readonly review can finish Done, while attempts still
spend their budget. Authorized successful/partial writes, observed changes and
harness checkpoints retain required verification, including after permission
downgrade. Optional checks still obey permissions. Stop tags and protocol
validation are unchanged; earlier live captures stay historical evidence.
This deterministic runtime correction does not establish a live model-quality
improvement or resolve missing JSON. See [the gate contract](06-harness-spec.md#verify-before-done-gate)
and [offline verification tests](08-testing.md#review-search-progress-and-safe-completion).

## Calibrated Flash Next v2 reviews (2026-10-02)

Six fresh, serial `roman-v2` reviews used the rebuilt LocalHub#207 runtime at
`c9232de952d4ebee5781b022d8a76720b8404d94`. Binary identity remained unchanged
throughout; a subsequent landing-page documentation commit did not affect it.
Settings were the saved LocalBox IQ3_XXS / 256k / native tune, reasoning off,
OpenAI-compatible local provider with server-discovered 262144-token context,
project-isolated memory, readonly permissions and opt-in attempt diagnostics.
Prompts, retries, grants and progress policy were unchanged during the batch.

| Cell | Run | Final outcome | Wall time | Attempt outcomes |
|---|---|---|---|---|
| Planted | 1 | ESCALATE, no verdict | 82.8 s | Two prose parse failures |
| Clean | 1 | AGREE | 666.6 s | Initial TimedOut; valid repair |
| Planted | 2 | REVISE | 89.4 s | Initial accepted |
| Clean | 2 | AGREE | 103.2 s | Initial accepted |
| Planted | 3 | REVISE | 153.4 s | Initial prose parse failure; valid repair |
| Clean | 3 | AGREE | 143.6 s | Initial accepted |

The calibrated clean controls received **3/3 AGREE**; planted defects received
**2/3 delivered REVISE**, with one missing-verdict escalation. That failed
review's repair prose identifies the bool defect but never supplies JSON, so
it does not count as a delivered correct verdict. The two accepted revisions
identify the supported True-as-int violation and missing bool regression tests.
The clean agreements have no supported blocking/important finding under the
settled criteria. Secondary claims still need scrutiny: error-message wording
is unspecified, one indexing complaint assumes behavior outside the stateless
declaration-search contract, and purported exhaustive checking "in my head"
is not execution evidence. Clean run 3 explicitly distinguishes static review
from the author's test report; its optional subTest suggestion is minor.

There were **zero 900-second driver wall kills**, but **one initial runtime
turn timeout at the built-in 600-second limit**, recovered by repair. The
900-second driver cap bounds the whole review and does not extend each model
turn. LocalHub#208 records that result rows omit the effective turn deadline
and timeout count; `killed=false` alone cannot support "no timeouts".
Clean run 1 made 195 calls, including 179 searches (161 distinct logged query
summaries); varying queries remain the known LocalHub#204 model limitation.
It timed out before the 200-call ceiling rather than exhausting that budget.

All six engines exited zero. Nine attempts were retained: five accepted,
three prose parse failures and one unavailable response after timeout; all
available samples were untruncated. Fixture hashes remained unchanged and
all 13 successful reads had no read errors. LocalBox was stopped afterward.
Fixture discrimination and driver regression checks passed (12 fixtures,
13 tests). The integrated runtime previously passed the workspace gate
(3646 tests passed, 0 failed, 7 ignored); this checkpoint changes documentation
only. These six unseeded trials establish neither a causal improvement nor
general model reliability. Owner tasks, 27B, larger samples, additional planted
defects and the single-agreement decision remain within open LocalHub#202.

## Expanded fixed-runtime evaluation (2026-10-02)

Thirty-nine new trials use runtime source `65a7d01add59c31d20373abbcb5abc973fe1e8cd`
and one unchanged binary (SHA-256
`589f68b1a51c0767ae85f2893358e6e2aee9871a84558534ab2a98a62be88fb0`).
LocalHub#208 supplies actual per-turn deadline and stop metadata. Existing saved
LocalBox tunes, local OpenAI-compatible provider, server-discovered 262144-token
context and project-isolated memory are retained. Flash uses IQ3_XXS/native;
27B uses i1-Q4_K_M/turboquant; A3B uses apex-i-quality/turboquant. Reviews are
readonly, with two existing attempts and opt-in bounded diagnostics. Owners use
the existing session lease and Bypass profile. No prompt defaults, grants,
parser, progress policy, model downloads or global configuration changed.

Reviews have a 900-second driver limit and a built-in 600-second limit per turn.
Owner loop budget is 2700 seconds plus the existing exit grace (LocalHub#209),
so it is not a strict whole-process wall limit. Runtime timeouts, driver kills,
missing verdicts and incorrect delivered verdicts are distinct outcomes.
All completed trials and failures are retained, with fixture hashes and final
participant journals checked. LocalBox is stopped between model phases.

### Flash output-format comparison

| Brief | Planted REVISE | Clean AGREE | Initial accepted | Final accepted | Initial literal JSON |
|---|---|---|---|---|---|
| Baseline | 2/3 | 3/3 | 4/6 | 5/6 | 0/6 |
| JSON reminder | 3/3 | 2/3 | 4/6 | 5/6 | 3/6 |

The reminder is inserted before fingerprints; fixtures and review criteria stay
fixed. Three unseeded paired repeats alternate arm order. Paths visible to the
model differ and include arm names, so this is an exploratory comparison, not
causal evidence. The parser accepts prose before valid JSON; the reminder is
not schema or grammar enforcement. Both arms deliver five expected verdicts
out of six, with no observed delivery gain. No production prompt change follows.

Baseline's missing planted verdict is correct initial prose without JSON, then
NoProgress on repeated searches during repair. Reminder's missing clean verdict
is initial prose, then an untruncated JSON candidate lacking its final closing
brace. No output-cap warning was observed; provider termination reason was not
captured. Another reminder clean review recovers a 600-second initial timeout.
All ten delivered primary decisions are supported, but supplementary claims
include a wrong manual Roman trace, a wrong table count and diagnostic-wording
requirements absent from the task. Static analysis is not test execution.

### Reviews by model and fixture

| Model | Case / brief | Planted delivered REVISE | Clean delivered AGREE | Missing verdicts | Supported primary decisions |
|---|---|---|---|---|---|
| Flash Next | roman-cm-v1 / baseline | 1/1 | 1/1 | 0 | 2/2 |
| Flash Next | roman-upper-v1 / baseline | 1/1 | 1/1 | 0 | 2/2 |
| Flash Next | roman-v2 / baseline | 2/3 | 3/3 | 1 | 5/5 |
| Flash Next | roman-v2 / json-reminder | 3/3 | 2/3 | 1 | 5/5 |
| Qwen 27B | roman-cm-v1 / baseline | 0/1 | 1/1 | 1 | 1/1 |
| Qwen 27B | roman-upper-v1 / baseline | 0/1 | 1/1 | 1 | 1/1 |
| Qwen 27B | roman-v2 / baseline | 0/3 | 0/3 | 5 | 0/1 |
| Qwen A3B | roman-cm-v1 / baseline | 1/1 | 0/1 | 0 | 1/2 |
| Qwen A3B | roman-upper-v1 / baseline | 1/1 | 0/1 | 0 | 1/2 |

27B's six roman-v2 trials deliver no correct verdict: two planted reviews and
all three clean controls escalate without verdicts; the remaining planted review
delivers false AGREE despite True returning I. Schema errors, invented paths,
tool repeats and long in-flight generation appear in the retained evidence.
Both supplemental clean controls deliver correct AGREE after repair, but the
upper-bound answer has wrong examples and a circular proof, while CM's body
says only "reviewed". Upper-bound planted escalates on repeated tools; CM planted
times out initially, then its repair hits the 900-second driver limit without
a completed answer. That killed repair has no final ESCALATE or completed
diagnostic attempt. These observations do not identify an inherent model or
quantization root cause.

A3B delivers all four supplemental verdicts: both planted REVISE decisions are
supported, while both clean controls are false REVISE. The clean claims invent
missing CM mapping, subTest swallowing failures, and a greedy-divmod bug despite
the updated remainder. No review tool calls occur; the supplied brief includes
the frozen diff and anchors. A matching anchor authenticates source bytes, not
the finding's reasoning. Upper-bound repair attaches whole-file hashes to
single-line ranges; native verdict correctly marks those anchors stale. That
answer also retains a retracted claim as blocking. CM's correct core diagnosis
comes with confused traces, invented missing tests and unsuitable fix advice.
Thus two correct catches do not imply consistently supported finding quality.

The original frozen product fixtures remain unchanged. Supplemental upper-bound
1999 and CM-to-MC cases are original separately frozen cases; planted visible
suites omit directly failing examples while matching clean suites cover them.
Keep these one-repeat cases separate from roman-v2 and historical v1 results.
An automatic integrity REVISE without a model turn is not a model defect catch
(LocalHub#210). No such refusal is scored as a model outcome in this fresh batch;
an earlier instrumentation attempt was excluded and retained separately.

### Owner acceptance and submission

| Model | Task | Review requested | Hidden acceptance | Measure | Native stops |
|---|---|---|---|---|---|
| Flash Next | slug | no | pass | fallback final tree | NoProgress |
| Flash Next | roman | yes | pass | first submitted tree | TimedOut, Done |
| Flash Next | duration | no | fail | fallback final tree | TimedOut, NoProgress |
| Qwen 27B | slug | no | fail | fallback final tree | NoProgress, NoProgress |
| Qwen 27B | roman | no | fail | fallback final tree | NoProgress, NoProgress |
| Qwen 27B | duration | no | fail | fallback final tree | NoProgress, NoProgress |
| Qwen A3B | slug | no | pass | fallback final tree | NoProgress, NoProgress |
| Qwen A3B | roman | yes | pass | first submitted tree | NoProgress, Done |
| Qwen A3B | duration | no | fail | fallback final tree | NoProgress, NoProgress |

Hidden acceptance is independent of the driver's scripted reviewer agreement.
A passing fallback source check does not satisfy required visible tests or
submission. Final journals are authoritative: LocalHub#212 tracks owner rows
that miss a final escalation when the process exits between polling intervals.
Original rows are preserved rather than rewritten.

Fresh unknown-reliability work profiles allow one file/region. Source plus tests
can encounter this allowance without a supported bounded checkpoint sequence;
test-triggered repairs can also be refused after the allowance is spent
(LocalHub#211). This integration issue qualifies owner comparisons. Slug passes
Flash's fallback source check but has no visible tests or submission. Roman
recovers on retry, submits with eight unrelated scratch files, and passes hidden
acceptance; its independent post-run visible suite has 44 passes. Duration fails
hidden acceptance, with int(None) for omitted units; its independent visible
suite has three failures and eight passes, including one wrong expected sum.
These independent checks do not imply model-executed verification.

27B submits none of its three owner tasks and all fallback hidden checks fail:
Slug only lowercases with no functioning test suite, Roman creates neither
required file, and Duration lacks the required parse_duration function. A3B
Roman submits and passes hidden acceptance; its independent visible suite has
35 passes. A3B Slug passes fallback hidden acceptance without submitting, but
its visible suite has 18 passes and one incorrect assertion: its zero-length
test omits max_len=0. A3B Duration does not submit and fails hidden acceptance;
its visible suite has six failures and 24 passes. Regex restrictions reject
required forms and hours-only reaches an uninitialized result. Both models'
incomplete work and the shared owner integration barrier remain visible.

An isolated native fake-provider replay also shows parseable REVIEW_REQUEST
posting after a failed required verification gate and NoProgress, then closing
on scripted agreement (LocalHub#213). It is mechanism evidence, excluded from
model scores. Owner submission must preserve typed verification outcomes;
readonly review verdicts have a different contract. LocalHub#214 records that
concise tool traces often show only a wrapper header, hiding the error reason.

A separate native control confirms that valid Python unittest commands can be
refused by Bypass's intentional permission floor for opaque file targets, while
an echo control succeeds. Bypass does not grant arbitrary shell access. The
required owner verification route needs explicit integration (#211); the full
tool message already explains these refusals, but the concise trace hides it
(#214). This control is excluded from model scores.

### Runtime outcomes

| Phase | Trials | Runtime turn timeouts | Driver kills | Nonzero engine exits |
|---|---|---|---|---|
| Flash Next | 19 | 4 | 0 | 0 |
| Qwen 27B | 13 | 3 | 1 | 1 |
| Qwen A3B | 7 | 0 | 0 | 0 |

Flash's server was reloaded before supplemental reviews after its original
runner exited following Duration. Same binary/driver/runner/tune and all completed
trials were preserved. The first post-reload turn timed out during sampled slow
prompt processing, then repair recovered at 840.4 seconds. Timing observations
are retained without claiming a root cause. Historical and fresh batches stay
separate. These small samples establish neither a general model ranking nor
readiness to adopt single-agreement acceptance; broader LocalHub#202 stays open.
