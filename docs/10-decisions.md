# Architecture Decision Records

This file starts the decision log. Add new records at the top.

## ADR-0034: The Developer-Process Self-Improvement Loop Is Human-Gated By Construction — Read-Only Up To "Propose", Never Self-Merges

Status: accepted. Builds on ADR-0010 (the runtime validates and controls — every
side effect passes a typed permission engine), ADR-0011 (store split:
`.localpilot/` is the execution record, `.localmind/` is memory), ADR-0023
(deterministic-first verification), ADR-0028 (handoff is a checked execution
record, never memory), and ADR-0033 (external corpora never enter the clean-room
tree). Cross-engine half recorded as LocalMind `D-LM-0014`. Source consulted
clean-room: a comparison of a self-styled "self-evolving" agent fork in LocalHub
research — its *premise* (an agent that observes its own friction and proposes
improvements) is adapted; **no code, prompt, identifier, or branding is ported**,
and its stated anti-goal (autonomy → human-oversight → zero) is explicitly
rejected.

LocalPilot grows a developer-process self-improvement capability: it can scan a
repository for drift, observe its own harness friction during real work, propose
a minimal fix, gate that fix on offline evals, and learn from the outcome. The
hazard a capability like this carries is **autonomy creep** — each convenience
quietly erodes the point at which a human must say yes, until an agent is editing,
committing, and merging its own changes. This record fixes the invariant that
makes the loop safe to build, so every later layer composes against fixed terms.

**The loop and its one-way boundary.** The stages are
`observe → retrieve → detect → propose → evaluate → patch → human-approve → merge → lesson-writeback`.
A single boundary cuts the loop in two:

- **Up to and including `propose`, every stage is read-only** and the agent may
  run it autonomously. `observe` (repo scan + harness-friction findings),
  `retrieve` (prior lessons from LocalMind, read-only), `detect` (rank findings),
  and `propose` (emit a ranked, advisory findings report) perform **no workspace
  mutation** — their only effect is a workspace read (`Effect::ReadPath`), exactly
  like `knowledge_search`/`skill_load`.
- **From `patch` onward, every stage that can change code, push, or merge is
  hard-gated on explicit human approval.** Patch generation writes only inside an
  **isolated git worktree**, never to `main`; the agent stops at "proposed patch +
  provenance + eval result" and cannot apply, commit, push, or merge it without an
  explicit human approval token. The gate is enforced **by construction** — the
  apply path requires the token as a parameter and there is no code path that
  reaches a write to `main` without one — **not by prompt convention.**

**No self-merge, ever.** The agent never merges its own patch to `main` and never
auto-pushes. Merge is a human action outside the loop. Rollback for any proposed
change is to drop the worktree/branch; nothing durable was mutated.

**The eval gate is necessary, not sufficient.** A LocalBench offline eval gate
(reusing the ADR-0033 capability scorecard) scores a proposed patch and can
*block* it from reaching the human queue, but a green gate **never** substitutes
for human approval — it only filters out obviously-bad patches before a human
spends attention. Offline benchmarks are the accepted bar (ecosystem
validation-evidence policy / D008); a live local-model run is opportunistic.

**Learning carries provenance and negative signals.** Accepted and rejected
outcomes are written back as durable LocalMind lessons through the existing
review-gated memory path (ADR-0011) — a rejected patch writes a *negative-signal*
lesson — so the next run retrieves prior outcomes and stops repeating a mistake.
Lessons carry provenance and outcome; a bad lesson is curated/superseded, never
silently trusted.

**Outward publication is the highest-risk tail and is defer-by-default.** Emitting
a finding or patch as a GitHub/Azure DevOps issue or PR is an irreversible outward
action: it is **draft-only**, confirm-gated, never auto-merged, and ships only
after the read-only and gated layers are proven.

Reason:

- the invariant is **structural, not aspirational**: "read-only ≤ propose; every
  write/push/merge is human-gated; no self-merge" is enforced by the permission
  engine and an approval-token-typed apply path, so a confused or prompt-injected
  model cannot reach a mutation the human did not authorize — the same posture
  ADR-0010 fixed for tools and ADR-0027/0031 fixed for skills/tools (reach injects
  content the agent reads; it grants no effect);
- keeping the autonomous half **read-only** means the agent can run the expensive,
  useful part (observe → propose) unattended without ever being one bug away from
  an unintended write;
- composing existing mechanisms (worktree isolation, the permission engine, the
  LocalMind review-gated memory path, the LocalBench scorecard) rather than
  building a new engine keeps the safety guarantees the stack already proved, and
  the loop adds a *bound*, not a second control plane;
- defer-by-default outward automation means the irreversible surface is built last
  and behind a separate human sign-off, so the loop is useful long before it can
  publish anything.

Supersedes nothing. Auto-instrumenting the harness to capture per-tool-call
friction (beyond the audit-prompt friction source), a model-judged eval critic,
and any move toward reducing the human gate are explicit non-goals here; each
would need its own decision and, for anything touching the gate, a fresh security
review against this invariant.

## ADR-0033: External Benchmark Corpora Never Enter The Clean-Room Tree

Status: accepted. Builds on `docs/00-clean-room.md` (clean-room provenance) and
the golden-task eval scorecard in `docs/08-testing.md`.

Measuring this harness against public coding benchmarks (SWE-bench, the Aider
polyglot set) is valuable, but those corpora are authored elsewhere and their task
instances, fixtures, and prompts are exactly the kind of external material the
clean-room policy forbids from entering this repository.

Decision: a public benchmark corpus is **never** vendored into this repository or
materialized under any checkout of it. Instead, an external runner (owned by the
benchmarking tool, not this repo) drives the `localpilot` binary as the
solver-under-test against workspaces materialized in a user-local, git-ignored
cache **outside** this tree, and consumes the same machine-readable capability
scorecard. The runner refuses to write task data under a path that contains this
project's checkout. The first-party corpus mined from this repository's own git
history (original, uncontaminated) stays in-repo and is the trusted bar.

Reason:

- keeps clean-room provenance intact — no copied corpus, fixture, or prompt enters
  the tree, even for measurement
- still lets the harness be graded against public benchmarks, reported as deltas
  between harness arms (public absolute numbers are contamination-suspect)
- the in-repo first-party corpus remains the contamination-proof, trusted measure
- the boundary is enforced in code (a path guard), not by convention

## ADR-0032: Inline Shell Commands And Redirections Are Opaque To The Command Classifier

Status: accepted. Builds on ADR-0007 (tri-platform tier-1) and the permission
engine's command-class table.

The `run_shell` permission decision rests on classifying a command into a risk
class. The classifier reads the program and its arguments; it must never trust a
substring of a command it cannot actually parse. Two Windows-specific gaps let a
write masquerade as an auto-allowed read:

- `cmd`/`powershell`/`pwsh` were routed to substring classifiers *before* the
  opaque-wrapper check, so `cmd /c "echo data > file"` matched the `echo`
  keyword and classified `read-only` — auto-allowed — while the shell honoured
  the `>` and wrote the file (anywhere, since a command carries no contained
  path). POSIX `bash -c` was already opaque; the Windows shells were not.
- The substring classifiers ignored output redirection entirely.

Decision: an invocation of `cmd`/`powershell`/`pwsh` that carries an inline
command or script — `/c`, `/k`, `-Command` (and its prefix abbreviations),
`-EncodedCommand`, `-File` — is **opaque**, exactly like `bash -c`, and
classifies `unknown` (gated: ask interactive, deny non-interactive). Separately,
any argument containing a redirection (`>`/`>>`) lifts a `read-only` verdict to
at least `project-write`. The classifier always fails toward a prompt, never
toward a silent allow.

A command also carries no contained path, so a `read-only` command
(`cat`/`type`/`head`) could read a secret-bearing or out-of-workspace file and
pull it into model context unprompted — the redaction stack runs at persistence,
not on the live request. Each non-flag path argument of a read-only command is
therefore inspected against the same secret-path table the file tools use and
the workspace boundary; a secret-like or out-of-workspace argument adds an
explicit read effect, so the command faces the same prompt the `read_file` tool
would. Best-effort and conservative: ordinary in-workspace reads add no prompt.

Reason:

- a permission boundary must hold against a confused or prompt-injected model;
  a false prompt costs a keystroke, a misclassified write costs a file
- substring parsing of an opaque inline command is unreliable in both
  directions (it missed `echo >` as a write and only caught `del` as destructive
  by coincidence); treating the whole inline command as opaque is the honest,
  parser-free position, identical to the long-standing `bash -c` rule
- `unknown` and `destructive` share the same gate (`ask`/`deny`), so reclassifying
  an inline destructive command as `unknown` changes the label, not the
  protection — verified by the boundary tests and a proptest invariant that no
  inline or redirected `cmd`/`powershell` argv is ever `read-only`

## ADR-0031: The Tool Surface Is Pull-Based — A Per-Session Working Set, A Broker That Reveals, Reveal-Never-Grant

Status: accepted. The tool-surface sibling of ADR-0027 (the skill model:
pull-based discovery via `skill_search`/`skill_load`); applies ADR-0016 (project
knowledge is pulled on demand, not pushed every turn) and ADR-0017 (retrieval
context is a request-time projection) to *tools*; builds on ADR-0010 (the runtime
validates and controls). Source consulted clean-room: the change-aware-invalidation
and layered-retrieval findings in the LocalHub comparison research — concepts
reimplemented, nothing vendored.

Every registered tool's full schema was advertised to the model on every turn.
That is the tool-surface analogue of the always-loaded-skill-description model
ADR-0027 rejected: it taxes every turn's context and hurts small local models,
and it grows linearly as MCP servers add tools. This record makes the tool surface
**pull-based**, the same shape skills and knowledge already use.

The model holds a small per-session **working set** — the bounded subset of tools
whose specs are advertised this turn, seeded from a core default plus the broker's
own tools. When the model needs a capability the working set does not contain it
**signals** a need, and a **broker** resolves that need to the best tool(s) over a
**live, fingerprinted catalog** of the current registry, then **reveals** the
resolved tool: it adds the tool to the working set and returns the tool's exact
current schema plus a one-line usage example. The model then calls the tool
normally.

**Reveal changes visibility only — reveal-never-grant.** Revealing a tool mutates
the advertised set and nothing else. Dispatch is unchanged: the permission engine
(`Allow`/`Ask`/`Deny`) runs first on every call, then the tighten-only gate chain,
exactly as before. A freshly revealed write or network tool therefore hits the
*same* `Ask`/`Deny` it would have hit had it always been advertised. The broker's
own surface (`tool_search`, `tool_load`) is read-only (`Effect::ReadPath`), like
`skill_search`/`skill_load`: searching and revealing inject *content the model
reads*; they enable nothing.

**Two triggers feed one broker core.** *Failure-driven* (always built, needs no new
model behaviour): a call to a tool the working set does not contain — unknown,
out-of-working-set, or retired (an MCP tool that vanished from `tools/list`) —
returns a re-resolution ("closest available: Y — schema, example; now available,
retry") instead of a bare `unknown tool` error, reveals Y, and lets the model
retry. The attempted call does **not** execute. *Loose NL marker* (secondary,
config-gated **off by default**): the model writes a short marker (`NEED:
<capability>`) and the harness parses assistant output, resolves, and reveals
proactively. The marker needs new model behaviour, so it ships off until a live
small-model reliability run validates it; failure-driven carries the feature
meanwhile.

**The catalog is live, fingerprinted, and change-aware.** It is a projection over
the registry (`registry.specs()`), rebuilt on the registry-change signal
(registration / MCP (re)connect), never a second source of truth. Each entry
carries a content fingerprint — a stable hash of (name + description + schema +
source version) — so adds, removals, and schema bumps produce an index delta with
no manual upkeep. MCP is the volatile edge: a server's advertised list is
authoritative for its entries on each enumeration, and a tool absent from the new
list is removed. MCP carries no deprecation field (spec rev 2025-06-18), so
deprecation is an **overlay only** — an optional hand-maintained old→replacement
map that annotates and de-ranks an entry; it grants and removes nothing.

**Ranking is deterministic-first.** Need→tool resolution uses an in-process
word-overlap scorer (the `skill_search` primitive applied to catalog entries), so
the change set stays LocalPilot-only and the path stays fast and offline. A
model/LocalMind ranker is a future drop-in behind the same `resolve()` seam.

**Defaults reproduce today's behaviour.** The broker is config-gated and **off by
default**: with it off, the full registry is advertised exactly as before — the
rollback path. Cross-session persistence of the live working set is out of scope;
only graduation-derived core defaults persist (a separate, opt-in learned-freshness
tier). Resolve-and-run is explicitly out of scope: the broker reveals and the model
retries; it never translates the model's args and executes a tool the model did not
itself call.

Reason:

- **structural local-model-first posture:** tool guidance is fetched when relevant
  rather than taxing every turn, mirroring `knowledge_search` and ADR-0027 — the
  same proven pull pattern, now over tools;
- **reveal-never-grant keeps the safety floor intact:** the permission engine and
  tighten-only gates remain the sole execution authority; reveal is a visibility
  hint and dispatch is truth, so a stale revealed schema costs at most one
  correction round-trip and can never execute with wrong params;
- **change-aware by construction:** a metadata fingerprint computed on the
  registry-change signal (not polled, not a filesystem walk) tracks a surface that
  MCP servers mutate, so LocalPilot evolves as the surface evolves;
- **failure-driven needs zero new model behaviour:** the model already attempts
  tool calls; the re-resolution only makes the miss helpful, so the feature pays
  off even on a small model that never learns the marker convention.

Supersedes nothing. A model-judged relevance scorer, a hard MCP rename-continuity
protocol, and resolve-and-run are explicit non-goals here; each is a future drop-in
behind the same seam and would need its own decision (and, for any move toward
auto-execution, a fresh security review against reveal-never-grant).

As shipped: a `[tools]` config block (`broker`, `core`, `working_set_cap`,
`score_floor`, `marker`, `learning`, `graduation_threshold`) gates the feature,
all defaults reproducing prior behaviour. The catalog/broker live in
`localpilot-tools` (the registry projects a fingerprinted catalog; the broker
holds the working set and the `tool_search`/`tool_load` read-only tools); the
session owns the advertise lever and the failure-driven/marker triggers; learning
records a redacted `ToolResolution` session event and persists graduated tools in
the disposable project store across sessions.

## ADR-0030: Inspect A Named Target Before Launching Your Own, Enforced As An Evidence-Grounded Rule

Status: accepted. Builds on ADR-0010 (the runtime validates and controls) and the
`RequiresPriorRead` precondition lineage (a side effect grounded in current
evidence, not the model's memory).

A task that names an existing target the agent can reach — a local URL, a running
service, a `host:port` — should be *inspected* before the agent assumes it must
stand up its own competing server or scaffold a competing entry page. Prompt
guidance alone did not hold: a model would ignore an explicit "test it at this
URL" and launch its own server anyway. That is a model-behaviour drift the
deterministic harness layer is meant to catch, exactly like an unread overwrite.

Two complementary mechanisms ship. A system-prompt convention (the always-on
nudge) states the look-before-launch discipline. A deterministic
`check_before_launch` rule enforces it: when a local serveable target was named in
the task prompt and **not** probed this session, an attempt to launch a local HTTP
server or scaffold a competing entry file surfaces a model-visible verdict —
*probe it first; only launch your own server if the probe fails*. The probe state
is read from the session evidence ledger (a real prior `fetch`, or a probe shell
command such as `curl`/`Invoke-WebRequest` whose arguments hit the target), never
from the model's claim that it "already checked" — the same doctrine as
`RequiresPriorRead`. Named targets are auto-extracted from the prompt (loopback
hosts, or any `host:port` with an explicit port); a bare external reference URL is
not a serveable target and is ignored.

Reason:

- the rule is **evidence-grounded**, not memory-grounded: a satisfied probe in the
  ledger clears it, exactly as a prior `read_file` clears `RequiresPriorRead`
- it is **tighten-only and advisory**: non-critical, default `Warn` (the call
  still runs, the nudge reaches the model), tunable to `Block` (refuses the launch
  before it runs, like a precondition) or `off`. It never grants a side effect the
  permission engine would deny; the permission engine stays the authority
- the trigger is scoped to **local serveable targets** so an external reference URL
  never nags, and the offline false-positive rate (0/3 over the negative set) is
  measured before any move from the `Warn` default to a harder one — a control
  signal is tightened on evidence, never shipped on faith, honouring the
  reliability contract
- launch and probe matching is a curated, **extensible, best-effort pattern set**
  over Windows/Linux/macOS variants; an unrecognised launcher is a documented miss,
  not a guarantee of completeness — the docs say so plainly

Supersedes nothing. Config-declared target lists and auto-probe injection are
explicit non-goals here: the rule *requires* a probe, never injects one, and a
`[harness]` target list is a future drop-in behind the same signal.

## ADR-0029: The Per-Turn Tool-Call Ceiling Is Progress-Aware, With A Hard Cost Contract

Status: accepted. Builds on ADR-0010 (the runtime validates and controls) and
ADR-0023 (deterministic-first verification).

The per-turn tool-call ceiling was a single fixed count: every turn stopped at
the same number of calls. That number is a blunt proxy — it cuts a legitimately
long turn (a large refactor that genuinely needs many calls) at the same point
it would stop a runaway, and it is slow to catch the loop the failure breakers
miss: *successful* calls that make no forward progress (re-reading the same file,
re-running the same search) where every call returns success.

The ceiling is now progress-aware. A deterministic detector flags no forward
progress from two signals — an identical `(call signature, output)` succeeding
repeatedly, and novelty decay (the share of distinct call signatures over a
sliding window falling below a floor). A budget controller turns the ceiling into
a bound with two numbers: a **soft start** and a **hard maximum**. A turn that
keeps making progress runs up to the hard maximum; a turn the detector flags
stops at the soft start; the hard maximum **always** stops the loop. When the
detector first fires, a one-shot strategy-change hint is appended to the tool
result, nudging the model to break out before any stop. The no-progress stop is a
distinct `StopReason` from the cost-ceiling stop, so the two are diagnosable.

Defaults are parity: the soft start and hard maximum both default to the previous
fixed value, so absent or pre-existing configuration reproduces the old stop
behaviour exactly. Raising the hard maximum above the soft start opts a deployment
into the adaptive extension.

Reason:

- the hard maximum is an unconditional cost contract: a turn can never loop
  unbounded regardless of any heuristic's confidence — the bound holds even if the
  progress signal is wrong, which is what makes raising it safe
- progress is judged by deterministic, offline-testable signals (no model in the
  hot path), mirroring ADR-0023; a model-critic progress judge is a future
  drop-in, not a dependency
- the detector composes the existing per-turn breakers' philosophy rather than
  duplicating their counters; it lives beside them in `localpilot-recovery`, and
  the controller is a pure decision unit, so the loop gains a bound, not a second
  control plane
- shipping at parity and measuring the false-positive rate before tightening the
  default honours the reliability contract: a control bound is tightened or
  relaxed, never a permission or safety outcome

## ADR-0028: The Handoff Is A Redacted, Git-Ignored Execution Record, Checked Deterministically, Never Memory

Status: accepted. Builds on ADR-0011 (store split: `.localpilot/` is the execution
record, `.localmind/` is memory), ADR-0003 (project files are the harness source of
truth), and ADR-0012 (`.localpilot/` is local, disposable, never committed). Related
to ADR-0027 (skills; a handoff suggests skills for the next session).

A session that ends mid-task leaves no first-class way for a fresh agent to pick it
up: the transcript is long and unredacted-for-sharing, and the harness documents
describe the plan but not "where we are right now." A **handoff** fills that gap.

Shape:

- **A small machine-checkable header + a human-readable Markdown body.** The header
  carries every field the resume check needs — schema, id, repo, branch, commit,
  dirty, session, references, suggested skills, confidence, created — so the check
  reads structured fields, not prose (the "query-time fields live in the header, not
  the source body" lesson from the retrieval work). The body separates **confirmed
  facts** (what the event log and git actually record) from **assumptions** (the
  inferred objective and next action).
- **Reference, don't duplicate.** The handoff points at `brief.md` / `PROGRESS.md` /
  `DECISIONS.md` by path and tells the reader to read them, rather than copying their
  contents — they stay the source of truth (ADR-0003).
- **Written from durable state, not the raw transcript.** The writer reads the session
  event log (committed steps) and the harness documents — the facts LocalPilot already
  recorded — never the conversation buffer.
- **Redacted through the canonical host redactor** (ADR-0011) over the *whole*
  artifact before it touches disk.

Storage and boundary:

- It lives at `.localpilot/handoffs/<id>.md` — an **execution record**, git-ignored
  and never committed (ADR-0012), distinct in name and location from the harness
  `brief.md` / `PROGRESS.md` runtime files (which live at the repo root and are
  committed plan state).
- It is **never promoted to LocalMind accepted memory.** Session close-out reads the
  transcript, never the handoff file, so a handoff body cannot become a review
  candidate or accepted memory. Close-out may still extract durable *lessons* from the
  session itself as evidence; the full handoff stays transient.

Resume check:

- `handoff resume <id>` runs a **deterministic** check before a fresh agent acts:
  branch identity, whether the recorded commit still exists, dirty-state match,
  referenced paths present, referenced session present. No model judges the prose.
- A mismatch is a **flag to re-verify, not a hard failure** — stale facts are surfaced
  as warnings, never silently dropped (the *flag-don't-drop* precedent from the
  change-aware staleness work).

Reason:

- the cross-context win is a small, honest, *checkable* snapshot the next agent
  verifies against the live repo — not a large unverified context dump or a second
  memory store;
- keeping the handoff an execution record (git-ignored, redacted, never memory) means
  it inherits the store split's privacy and disposability guarantees and adds no new
  long-term-storage surface;
- a deterministic, warning-not-failure resume check matches the local-first posture: no
  model in the verification path, and a moved repo degrades to "re-verify," never to a
  false "all good." Rollback is to stop writing handoffs; nothing else depends on them.

The runtime shape is documented in `docs/06-harness-spec.md` (§Handoff).

## ADR-0027: The Skill Model — Invocation × Authority, Two Artifact Types, Pull-Based Discovery

Status: accepted. Generalizes ADR-0020 (skills are read-only advisory prompt
modules), and applies ADR-0016 (knowledge is pulled on demand, not pushed every turn)
and ADR-0017 (retrieval context is a request-time projection) to skills; related to
ADR-0028 (the handoff artifact). Source consulted clean-room: the `mattpocock/skills`
comparison in LocalHub research (§5, §16) — concepts reimplemented, no file vendored.

"Skill" had drifted into an overloaded word across the stack. This record names the
model so the later runtime work (frontmatter invocation parsing, loader wiring,
handoff) builds on fixed terms. A skill-shaped artifact is placed on **two
independent axes**:

- **Invocation — *who can reach it*:** **user-only** (reached only by a human typing
  its name) or **discoverable** (the model can also reach it on its own — *and* the
  human can still type its name; discoverable always includes user reach). Invocation
  is carried in a `SKILL.md` by the `disable-model-invocation` flag (present ⇒
  user-only; absent ⇒ discoverable).
- **Authority — *what reaching it does*:** **advisory** (the artifact is *content the
  agent reads* — guidance it may apply or ignore; reaching it performs no effect
  beyond a workspace read) or **enforced** (the artifact is a *rule the runtime
  applies* — it can block or gate an action).

**Two artifact types** occupy this space (an earlier draft proposed a third,
"user-invoked command"; it was dropped as redundant — a typed, user-only invocation is
just a *user-invoked skill*):

1. **Harness rule / quality-gate check** — *authority: enforced*, invocation:
   runtime-triggered (cadence/event, not a human or model name). Owned by the rule
   engine and the discovered quality gate (ADR-0009); it can refuse or gate an effect
   and never bypasses the permission engine.
2. **Advisory skill** — *authority: advisory*, invocation: user-only or discoverable.
   The read-only prompt module of ADR-0020 (LocalMind-distilled or project-local
   `SKILL.md`), surfaced as content. Reading one never installs, enables, disables, or
   runs anything. A *user-invoked* skill is simply this with invocation set to
   user-only.

Discovery is **pull-based, not push-based**: a discoverable skill is
**not** loaded into the turn context just because it exists. The model finds skills the
same way it finds knowledge — an on-demand **search**: a `skill_search` surface returns
lean ranked locators (name + one-line summary + score), and only the **chosen** skill
body is loaded into context. This applies ADR-0016/0017 to skills: a discoverable skill
costs ~no standing context (at most a small fixed cue that skills exist and can be
searched), and the always-loaded-description model is explicitly rejected as the
default — it taxes every turn and hurts small local models.

Load-bearing rules:

- **User invocation is deterministic and needs no model judgement.** A human typing a
  skill's name loads that skill's body directly — no search, no ranking, no autonomy.
  This works for *every* skill regardless of its invocation flag.
- **Model discovery is search-on-demand and opt-in.** The model reaches a discoverable
  skill only by searching for it and then loading the chosen body; **autonomous**
  (model-initiated) search-and-load is config-gated and **off by default**, so a small
  local model never auto-injects a skill unless the project opts in. The candidate set
  is the *discoverable* skills only; user-only skills are never returned by search.
- **No-silent-execution is reaffirmed, not weakened.** Nothing here executes, installs,
  enables, or auto-fires a skill without an explicit human step or a disclosed config
  opt-in. Enabling/disabling/retiring stay deliberate human steps (ADR-0020). Loading a
  skill injects *content* the agent reads; any script/asset a skill declares still runs
  only through the permission engine (never a side channel).

Reason:

- one durable vocabulary (two axes, two types) stops "skill" from meaning a harness
  rule and an advisory module interchangeably across LocalPilot and LocalMind — the
  later subjects parse, wire, and document against fixed terms;
- **pull-based discovery** keeps the local-model-first posture structural: skill
  guidance is fetched when relevant rather than taxing every turn, reusing the proven
  `knowledge_search`→`knowledge_fetch` pattern (the ranking primitive already exists as
  `SkillSet::relevant`);
- both types share a safety floor (no silent execution; permission engine never
  bypassed), so naming them adds clarity without adding a new risk surface.

The cross-engine half of this decision (LocalMind skills stay advisory/read-only) is
recorded as LocalMind `D-LM-0013`, which points here as the single source of truth.

## ADR-0026: The Cold-Start Repo Primer Is A Review-Gated, Always-On Context Block

Status: accepted. Builds on ADR-0013 (disposable project-local artifacts) and the
LocalMind engine decision D-LM-0009 (deterministic, review-gated, supersedable
repo primer).

A session starting on an unfamiliar repository should orient without spending its
context window reading files. The engine distils a deterministic **repo primer**
from the code-graph architecture overview (languages, packages, entry points,
call hotspots) — no model in the path. The host's role is *when* and *whether* to
surface it:

- **Distillation** runs at session close-out, right after the code-graph reindex,
  once the graph is fully current (`remaining == 0`). It reuses that existing
  trigger — no new watcher — and is gated by the project's learning flag. It only
  enqueues a review candidate; it never writes accepted memory.
- **Injection** is the pre-turn context hook. The *accepted* primer (an active
  `Project` memory whose id carries the `repo-primer-` marker) is contributed as
  an always-on, token-bounded block — orientation, not prompt-relevance — ahead of
  the relevance-filtered memory and any pushed ingest chunks. An unaccepted or
  stale (superseded) primer is not active, so it is never injected.
- **Staleness** rides the engine's content hash over the overview shape: a drifted
  repo distils a primer with a new id the reviewer accepts as a supersede of the
  prior one, retiring it.

Reason: the cold-start win is an off-context, queryable index plus a small
reviewed orientation — not a larger prompt. Keeping the primer review-gated and
honestly heuristic (confidence < 1.0, `repo@commit` provenance) means the agent is
never handed unverified "truth," and the host adds no graph logic of its own
(it discovers, gates, drives, and injects).

## ADR-0025: Ingested Chunks Live In An Indexed SQLite Store

Status: accepted. Builds on ADR-0013 (folder ingestion uses disposable
project-local artifacts) and ADR-0017 (retrieval context is a request-time
projection).

Folder ingestion persisted every derived chunk in a single `chunks.json` under
`.localmind/ingest/`. Every search and every refresh deserialized the whole file
into memory and scanned it linearly, so a large repo paid a full-RAM load and an
O(n) scan on each query — the opposite of the "lean on modest machines" goal.

Derived chunks now live in an embedded SQLite store at
`.localmind/ingest/chunks.sqlite` with an FTS5 virtual table, versioned by a
`PRAGMA user_version` stepper — the same pattern the accepted-memory store uses.
Search narrows to the matching rows through the FTS index (bounded by a
relevance-ordered limit), then recomputes the existing term-count +
path-name-boost score over just those rows, so ranking is unchanged while the
whole index is never loaded. Refresh updates only the paths that changed:
unchanged files are reused by `path:content_hash`, a changed file's prior rows
are kept as stale tombstones pointing at the new hash, and a vanished file's rows
are tombstoned with no successor. An existing `chunks.json` migrates into the
database on first open and is then removed; `ingest rebuild` recreates the store
from source. Only the large chunk index moved — the small manifest/job/review/
last-pack files stay JSON.

Reason:

- the persisted index exists to keep retrieval lean on modest machines; a
  full-RAM load plus linear scan on every query defeats that, and an indexed
  store fixes it without changing the chunk model or the ranking contract
- SQLite + FTS5 is already in the dependency tree and proven by the
  accepted-memory store, so the chunk store reuses a known-good, offline,
  extension-free pattern (rusqlite `bundled`) rather than inventing storage
- the store is derived and disposable (ADR-0013): migration is one-way and
  rebuild is always a valid fallback, so the change carries no durable-data risk

## ADR-0024: Session Store Has A Conservative Default Retention

Status: accepted. Builds on ADR-0011 (store convergence: the execution record)
and ADR-0012 (`.localpilot` is local, disposable, never committed).

The project-local `.localpilot/` state grew without bound: one transcript and
event-log pair per session, and one `tool-output/<id>.txt` snapshot per tool
call, none of it ever removed. A `RetentionPolicy` (`max_sessions`,
`max_age_days`; `0` = unbounded on that axis) now governs cleanup. `Store::prune`
removes the sessions outside the policy, trims the index, and sweeps any
tool-output snapshot no surviving session still references (a mark-and-sweep over
survivors' tool-call ids plus their `recovery-<id>` snapshot — no mtime
heuristics). It is exposed as `localpilot session prune [--keep] [--older-than]
[--dry-run]` and run best-effort at interactive chat startup.

A conservative cap is **on by default** (`[storage]`: 100 sessions, 90 days,
`auto_prune = true`) so the directory cannot grow forever without anyone opting
in. Both limits and the auto-prune are configurable, and `0`/`false` disable
them.

Reason:

- unbounded growth is a real disk and inspectability problem; a default cap fixes
  it for users who never touch config, the common case
- retention is the store's concern, so the policy and the mark-and-sweep live in
  `localpilot-store` behind one `prune` entry point rather than scattered deletes
- deletion of user history is sensitive (the privacy model treats inspect/delete
  as user controls), so cleanup is best-effort, silent, fully configurable, and
  has an explicit `--dry-run`; cache and provider metadata stay out of scope

## ADR-0023: Deterministic Result Verification In A Thin `localpilot-verify` Crate

Status: accepted. Builds on ADR-0010 (the runtime validates and controls) and
ADR-0001 (narrow crates, one-way dependencies).

The permission engine controls *whether* a tool may run; it does not check
*whether the call did what it claimed*. A separate stage closes that gap: after
a call executes, a `Verifier` judges it against its tool contract and returns a
`Verdict` of `Verified`, `Unverified`, or `Failed`. An effect a contract marks
`Unverifiable`, or one with no checkable postcondition, is `Unverified` — never
silently a success. The verdict is recorded durably (a `ToolVerified` event in
the execution log), and an opt-in gate refuses a final reply that claims an
action completed without a `Verified` call to support it.

This lives in a thin new crate, `localpilot-verify`, depending only on `core`,
`tools`, and `sandbox` — not on the harness — so verification is a stage the
harness composes, not a parallel control loop. A model-critic verifier is a
future drop-in behind the same `Verifier` trait; the deterministic verifier is
the default.

Reason:

- "no success claim without verified evidence" becomes a structural property of
  the loop, not a prompt convention — the gap ADR-0010 left between *controlling*
  an action and *confirming* it
- a deterministic-first stage keeps verification offline, testable, and free of a
  model in the hot path, with the model critic gated behind the same seam
- a narrow crate with a one-way dependency keeps the reliability contract from
  drifting into a second control plane; dropping the crate dependency returns the
  loop to its prior behaviour

Update (later): the opt-in gate is reachable through configuration —
`[harness] claim_gate = "off" | "warn"`, default `off` — so its false-positive
rate can be measured in real use without recompiling, the precondition for any
future default-on decision. The gate matches **per claim**: a completed-action
claim is supported only by a verified call *capable of that effect* (a shell
command is opaque and backs any category; the structured file tools are matched
by kind), so one verified action no longer excuses a different, unverified one.
An offline false-positive/recall benchmark scores the gate against a labelled
corpus so a regression is caught without a live model (validation-evidence
policy).

## ADR-0022: The Final Alternate-Screen TUI Is Preserved As An Annotated Tag

Status: accepted. Supports ADR-0021.

Before the move to inline rendering, the last full alternate-screen terminal UI
— with mouse capture and the mouse-mode toggle — was frozen as an annotated,
immutable git tag, `legacy-altscreen-tui`, on the pristine pre-change release
commit. It is a keep-for-posterity restore point only and is not maintained
further.

To restore and run it from a clean checkout, the bundled LocalMind submodule
must be initialised first:

```text
git checkout legacy-altscreen-tui
git submodule update --init --recursive
cargo run -p localpilot --features tui -- chat
```

Reason:

- a clearly named, immutable, zero-maintenance restore point lets anyone recover
  the previous interface in one step without keeping dead code on the main line
- recording the exact restore command — including the submodule step, which a
  fresh checkout or worktree otherwise misses — makes the rollback reproducible

## ADR-0021: Inline Terminal Rendering, No Alternate Screen Or Mouse Capture

Status: accepted. Refines ADR-0006; the committed ratatui + crossterm stack is
unchanged and this record fixes how that stack is driven.

The interactive REPL renders inline in the terminal's main screen buffer rather
than taking over an alternate screen. Finished transcript items — user messages,
assistant turns, tool results, system notices — are written once into the
terminal's native scrollback with ratatui's `Terminal::insert_before`, and a
small bottom region (a `Viewport::Inline`) holds the only redrawn surface: the
in-progress activity, the composer, and the status line. The mouse is never
captured.

Consequences:

- Native scrollback, text selection, copy/paste, scrollwheel, and the terminal's
  own search work again, because the app neither switches screen buffers nor
  captures the mouse. The previous mouse-mode toggle is removed.
- History is append-only: a finished block is emitted once and never redrawn;
  only the bottom region repaints each frame.
- The inline region's height tracks the composer. Because the framework has no
  in-place inline-height setter, the terminal is re-initialised when the height
  changes. The `scrolling-regions` capability is enabled so inserting history
  uses the terminal's scroll regions instead of clearing the region each commit.
- Arbitrary full-screen layout — a sticky top bar, or split panes that survive
  scrolling — is given up. For a header-once, stream-output, input-at-bottom
  agent REPL this loses nothing that matters.

Reason:

- the alternate-screen renderer was large and unstable and it disabled the
  terminal features users expect; inline rendering is less code and restores them
- the target API is ratatui's public `Viewport::Inline` / `Terminal::insert_before`;
  behaviour was cross-checked against a local read-only behaviour reference, while
  the implementation, prompts, and tests are original to this repository
  (clean-room, ADR-0005)

## ADR-0020: Skills Are Read-Only Advisory Prompt Modules

Status: accepted. Builds on ADR-0011 (review-gating) and ADR-0013.

A "skill" surfaced to the host is a reviewable advisory prompt module, never an
executable workflow. The host exposes skills through read-only, model-callable
tools only — `skill_drafts` (disabled candidate workflows) and `active_skills`
(human-enabled skills, surfaced as guidance with provenance). Each tool's only
effect is a workspace read; reading a skill never installs, enables, disables, or
runs anything, and active skills are not auto-injected into always-on context.

Reason:

- a wrong or stale skill is then at worst irrelevant guidance the agent ignores,
  never an unintended action;
- enabling/disabling/retiring stay deliberate, review-gated human steps;
- it keeps the local-first, no-surprise posture and is safe to automate later.

The consumption contract is documented in `docs/localmind-integration.md`.

## ADR-0019: The Host Selects The Extractor From Inference Config, Defaulting To A Local Endpoint

Status: accepted. Realizes the learning loop's model path; complements ADR-0018.

Session closeout selects the extractor from the project's `.localmind.toml`: the
model-backed extractor when `[inference].features.extraction` is set, otherwise
the deterministic extractor. The model path falls back to deterministic when the
endpoint is unreachable or returns malformed output. On first use, when the
project's default provider points at a loopback endpoint, the adapter writes an
`[inference]` block targeting that same local endpoint (stripping the `/v1`
suffix LocalMind appends itself), so "local models do the learning jobs" needs no
manual plumbing.

Reason:

- the default learning experience may depend on a local model, with deterministic
  as a graceful, always-available fallback;
- detection is project-scoped, so behaviour does not depend on the host machine;
- a remote provider is never wired automatically — pointing inference at a
  non-loopback endpoint is an explicit, disclosed opt-in (ecosystem remote-egress
  policy). LocalMind stays host-neutral: it only ever sees a generic local
  endpoint (LocalMind decision D-LM-0002).

## ADR-0018: The Learning Write-Path Closes On Every Opted-In Session, Keyed On Structured Signals

Status: accepted. Complements ADR-0011 (the store split and review-gating) and
ADR-0016/0017 (the read path); this record fixes the *write/learn* path.

LocalMind was first-class on the read path but second-class on the learn path:
close-out ran only in the interactive REPL, and it flattened the transcript to
text and re-parsed prose. This record closes the loop.

- **Close-out runs on every deliberate, opted-in session-end path** — the
  interactive REPL, each headless harness step, and the RPC/ACP serve loop —
  through one shared best-effort, non-fatal helper. It skips an empty session, so
  opening and closing one leaves no artifacts. One-shot `localpilot print` is
  excluded, so a bare prompt never creates project files. The headless harness
  builds a fresh runtime per step, so per-step close-out is the natural granularity
  and captures step-level failure/fix/commit.
- **The import is keyed on structured signals, not re-parsed prose.** Close-out
  builds the import from the redacted transcript and then appends compact lines
  from the session **event log** — failed tools, recovery diagnostics, committed
  steps — so the deterministic extractor sees the fact LocalPilot already recorded.
  Only names, statuses, and short commit hashes are appended; never raw payloads.
  The deterministic text path stays the baseline: when the event log has nothing
  notable, the import is the transcript alone, unchanged. LocalMind-core's adapter
  contract is metadata-thin and is **not** changed.
- **In-session surfaces never bypass review-gating.** The `remember` tool lets the
  agent propose a durable lesson — it enqueues a review candidate (permission-gated,
  project-local write) and never writes accepted memory. The read-only
  `skill_drafts` tool lists or inspects generated drafts without enabling them; the
  disabled flag stays authoritative and activation stays a human step. Each tool
  has a tool-gated system-prompt cue that appears only when the tool is registered.

Consequences:

- Autonomous runs learn, not just the REPL: a headless or RPC session produces
  reviewable candidates enriched with execution outcomes.
- Close-out cannot regress the autonomous critical path: it is best-effort,
  non-blocking, off the turn path, and gated on the existing opt-in.
- Review-gating (ADR-0011) holds end to end: nothing on the write path writes
  accepted memory or activates a skill automatically; everything new produces a
  review candidate or a read-only suggestion.
- The change is a contained host-edge change (call sites plus the adapter import
  text); LocalMind-core stays host-neutral and unchanged.

Reason:

- The structured truth LocalPilot already records in the event log is the
  high-value, low-risk lever: enriching the import text the extractor consumes
  needs no engine change, while flattening to prose threw that truth away.
- Per-step close-out matches how the harness already builds sessions, so it adds
  no new lifecycle.
- All prompt, tool, and behavior text remains original to this repository
  (clean-room, ADR-0005 / docs/00-clean-room.md).

## ADR-0017: Retrieval Context Is A Request-Time Projection

Status: accepted. Refines ADR-0016 and ADR-0014 (pull-over-push and
runtime-only projection still hold; this record fixes *how* the per-turn seed
reaches the model).

Per-turn context-hook output — lean accepted project memory, plus ingest chunks
only under `[ingest] mode = "push"` — is computed once per turn and injected into
the outgoing `ModelRequest` adjacent to the leading system prompt. It is **never**
appended to `self.messages`, the durable transcript, or the event log. Its token
estimate is reserved from the compaction budget so the request still fits the
limit. The ingest knowledge base is reached on demand through the read-only
`knowledge_search` tool, which returns a ranked cross-source pack (ingest,
accepted memory, recent-session facts, code graph) via a compute-only path that
performs no write. On an interactive REPL the index is built in the background on
first use (trust-gated, off the turn path).

Consequences:

- `self.messages` equals the authored history equals the stored transcript again:
  the synthetic-message persistence invariant ("a resumed session reconstructs
  exactly the history the model received") holds without a retrieval exception.
- Re-derived retrieval cannot accumulate across turns, and folding it into the
  leading system run means it rides the wire as top-level `system`, not as a
  resent user message (on Anthropic, a non-leading system message maps to user).
- Because the injected block is no longer part of the compacted history, the
  compaction budget explicitly reserves its token estimate; reported context
  usage is the real request total.
- The evict-on-replace seed path (and its synthetic marker) are deleted — less
  code, fewer states.
- A present-but-unreadable ingest index is reported distinctly from a missing
  one, so corruption is visible rather than masked as "no knowledge"; a turn
  never breaks on a knowledge miss.

Reason:

- The interim ephemeral-but-in-`messages` seed (ADR-0016) softened the
  synthetic-persistence invariant and required eviction bookkeeping and a
  compaction cache that already counted it. Treating retrieval as a request-time
  projection — what it always was conceptually — is the correct model and removes
  that machinery.
- Keeping the pull tool read-only (compute-only pack) means a model can pull
  ranked project knowledge with no write or heavy side effect.
- All behavior, tool, and prompt text remain original to this repository
  (clean-room, ADR-0005 / docs/00-clean-room.md).

## ADR-0016: Project Knowledge Is Pulled On Demand, Not Pushed Every Turn

Status: accepted. Refines ADR-0014 and ADR-0015 (the runtime-only projection and
the ranked budget still hold; this record changes how the *ingest* source is
delivered).

Ingested folder knowledge is reached on demand through a read-only
`knowledge_search` tool, not auto-seeded into every turn. The only always-on
retrieval seed is accepted, review-gated memory, and it is contributed leanly:
bounded in size, re-derived each turn, and **replaced** rather than accumulated.
A new `[ingest] mode` selects behavior — `pull` (default) or `push` (legacy
auto-injection of ingest chunks), the latter kept only as an escape hatch.

The per-turn retrieval block is marked synthetic and is **not** written to the
durable transcript or event log: it is re-computable context, not authored
history.

Consequences:

- Retrieval no longer grows the context window with turn count. Previously the
  pre-turn context hook appended a fresh system message every turn, so
  re-derived retrieval accumulated; on the Anthropic wire a non-leading system
  message also maps to a resent user message, compounding the growth.
- The event-log → transcript projection and session close-out lesson extraction
  stay clean, because the ephemeral retrieval seed never enters them.
- Ingested knowledge is still ranked and budgeted when pulled (the tool wraps the
  deterministic read-only ingest search; the ADR-0015 allocator remains available
  via the pack path).
- A fresh project's knowledge base is empty until `localpilot ingest` runs; the
  tool returns a useful "not indexed yet" result rather than an error. The
  first-use auto-ingest was removed from the turn path so a heavy walk/chunk no
  longer stalls the first turn.
- `push` mode restores the prior always-on ingest injection without a rebuild.

Reason:

- Auto-seeding the lowest-trust, highest-volume source (ingest) on every turn was
  the dominant cause of context filling quickly, and it duplicated content the
  model rarely needed standing by.
- Pull keeps high-trust accepted memory passively available and lean while making
  bulk project knowledge reachable on demand at no standing context cost — the
  retrieval analogue of reading a file only when relevant.
- The behavior, tool, config, and prompt cue are original to this repository
  (clean-room, ADR-0005 / docs/00-clean-room.md).

## ADR-0015: Derived Context Sources Compete Under One Ranked Budget

Status: accepted

Derived knowledge that can be injected into a turn — accepted memory anchors,
recent session facts, ingest hits, code-graph neighbors, and explicit manual
pins — competes for one token budget instead of each source getting a fixed
slice. Selection is a deterministic two-phase allocation: a per-source reserve
phase (filled highest-precedence source first) guarantees a high-value entry
survives a flood from a noisier source, then a shared pool fills the remainder by
a composite rank.

The rank is composed from explicit, inspectable signals: raw relevance, a
source-quality weight (manual pin > accepted memory > recent session > ingest >
code graph), recency, a stale penalty, and a redundancy penalty that demotes the
second and later hits from the same file. Every candidate is recorded as either
selected or skipped with its reason and full signal breakdown.

Consequences:

- A context pack is auditable end to end: a reader can see why each entry was
  included and why a high-ranking near-miss was dropped.
- Runtime conversation context (the kept raw suffix and the compaction digest)
  is owned by the compaction layer (ADR-0014); the ranked budget governs the
  derived-knowledge layer. The two compose by precedence — system context and
  the current turn are hard, then the recent suffix and digest, then the ranked
  derived sources.
- Manual pins and accepted memory are protected by reserves so a lexical ingest
  flood cannot crowd out review-gated or user-chosen context.

Reason:

- Fixed per-source slices either waste budget or starve a strong signal; one
  ranked competition spends the budget where it is most useful while keeping
  trusted sources protected.
- OpenCode and Pi informed the layered-precedence and budget concepts; the
  ranking, signal set, reserve math, and data shapes are original to this
  repository (clean-room, ADR-0005 / docs/00-clean-room.md). No reference code
  or prompt text was copied.

## ADR-0014: Context Projection Is Runtime-Only And Audit-First

Status: accepted

Runtime compaction, derived ingest packs, accepted memory retrieval, and code
graph facts all contribute to the active model request, but they keep distinct
ownership and lifetime boundaries. Compaction rewrites only the active runtime
projection; it may persist source-grounded summary and attempt metadata in the
session event log, but it does not write accepted memory, skill drafts, review
items, or ingestion artifacts.

Consequences:

- Compaction cutover is completed-only: a candidate projection must pass
  pairing, budget, and digest validation before it becomes active.
- The deterministic compactor is the correctness baseline. Smart modes must
  report fallback reasons and leave a valid deterministic projection.
- Compaction audit events store mode, fallback reason, counts, estimates, and
  truncation metadata without raw dropped transcript dumps.
- Ingestion remains rebuildable `.localmind/ingest/` state, and accepted memory
  remains LocalMind review-gated state.

Reason:

- Treating runtime context as memory would silently teach LocalMind unreviewed
  facts and weaken ADR-0011.
- Provider output-limit and partial tool-call failures require atomic request
  projection, not in-place mutation of transcript history.
- Shared source hints and budget metadata make context decisions inspectable
  without leaking private plan state or raw oversized content.

## ADR-0013: Folder Ingestion Uses Disposable Project-Local Artifacts

Status: accepted

Project folder ingestion writes derived state under `.localmind/ingest/`:
manifests, redacted chunks, job state, skipped-file reports, review candidates,
and context packs. These artifacts are rebuildable from the trusted project
folder and may be deleted without touching accepted memory.

Accepted memory remains owned by LocalMind's reviewed memory path. Ingestion may
enqueue review candidates through LocalMind, but it must not write accepted
memory directly.

Consequences:

- `.localmind/ingest/` is disposable derived state. Rebuild and forget commands
  remove only ingestion artifacts.
- Persisted ingestion content is redacted by the LocalPilot redaction stack
  before it is written.
- The first implementation keeps deterministic JSON artifacts and Rust-side
  ranking. SQLite-backed search can be added later if the derived corpus needs
  FTS behavior, but that would remain rebuildable ingestion state.
- Context packs are persisted as the latest derived pack for inspection and
  staleness handling; they are not durable memory.

Reason:

- ADR-0011 already reserves `.localpilot/` for execution records and LocalMind
  for memory/learning. Folder ingestion is broad mechanical project knowledge,
  so it belongs beside LocalMind state but outside accepted memory.
- Keeping the v1 artifacts rebuildable avoids migration risk while the schema is
  still young.
- Review-queue promotion preserves the curated-memory boundary and gives users
  an explicit approval point before broad file observations become durable
  knowledge.

## ADR-0012: Project `.localpilot.toml` Is Local-Only, Never Committed

Status: accepted. Amends the "committed `.localpilot.toml`" wording in
ADR-0009.

The project-local `.localpilot.toml` is a machine-local file: it is listed in
`.gitignore` and is not committed. External launchers generate provider
config into it in the project directory (base URL, model, key env-var name),
and those values are inherently machine-local. The ratified quality gate
(`[[harness.checks]]`, ADR-0009) lives in the same file and is therefore also
local-only.

Consequences:

- The ratification trust boundary is the explicit user action that writes
  checks into the local file — not version control. Wording in
  [`docs/06`](06-harness-spec.md) and [`docs/07`](07-security-and-privacy.md)
  says "ratified into the project's local `.localpilot.toml`" rather than
  "committed".
- A fresh clone has no ratified gate; `gate propose` / `gate ratify` is the
  supported way to re-establish one. A team that wants a shared, reviewed
  gate definition can keep one in its own committed docs and ratify from it,
  but the harness never reads checks from a committed file.

Reason:

- committing the file would leak machine-local endpoints and invite config
  drift between what a launcher generates and what the repo pins
- one file with one clear lifecycle (generated/edited locally, ignored) beats
  splitting harness config across a committed and an ignored file
- ratification was always defined as the user's explicit act; tying trust to
  VCS state added nothing and contradicted the launcher workflow

## ADR-0011: Store Convergence — Execution Record vs Memory

Status: accepted

LocalPilot persists state in two stacks, which were growing toward overlap.
This record fixes the ownership boundary:

- **The LocalPilot store (`.localpilot/`) is the execution record, and only
  that**: transcripts, the durable session event log (tree-shaped, format-
  versioned), caches, tool-output snapshots, provider metadata, and recovery
  diagnostics. It never grows memory, lesson, retrieval, or review features.
- **LocalMind (`.localmind/`) is the only memory and learning backend**:
  session closeout, candidate lessons, the review queue, accepted memory,
  retrieval/context injection, skill drafts, and audit. New rich-learning
  behavior lands in LocalMind, never as a host-local memory implementation.
- **One redaction authority at the host boundary.** LocalPilot's redaction
  stack (`localpilot-config::redact`) is the canonical redactor: everything
  the host persists or hands to LocalMind is redacted by it first. LocalMind's
  import-time redaction remains as engine-internal defense in depth, not a
  second authority — divergence between the two pattern sets is resolved by
  updating the host stack.

Reason:

- two stores with drifting responsibilities and two redaction pattern sets is
  how secrets leak and how features get implemented twice
- the event log needs a single unambiguous home (the execution record) before
  later features (headless drive, hooks, subagents) build on it
- LocalMind is host-neutral and reusable; baking memory into the LocalPilot
  store would fork that capability

## ADR-0010: Reliability Contract for Unattended Operation

Status: accepted

LocalPilot's differentiator is unattended multi-step execution. That claim is
made testable by an explicit **reliability contract**: a small set of named
invariants the runtime guarantees on every exit path, each pinned by a named
test, split across the owning specs:

- Session-loop invariants (tool-result pairing on every exit path, no partial
  replies persisted, transcript fidelity) —
  [`docs/06`](06-harness-spec.md) §Reliability Contract.
- Permission invariants (no `run_shell` path weaker than the equivalent
  builtin, floor-aware allowlists that never lift destructive/privileged/
  unknown gating, wrapper commands never auto-allowed, approval prompts that
  state their target) — [`docs/07`](07-security-and-privacy.md) §Reliability
  Contract.

A change that breaks a contract-pinning test is a contract change: it requires
a superseding ADR, not a test edit. The bypass profile's scope is part of the
contract: bypass keeps the workspace boundary for path-bearing effects only;
shell commands are not path-contained, and the docs state this rather than
implying containment that does not exist.

Reason:

- the product's central claim ("every side effect passes a typed permission
  engine"; "safe to run unsupervised") was previously aspiration enforced
  only by convention — line-level review found exit paths and classification
  gaps that falsified it
- invariants stated in the spec and enforced by property tests survive
  refactors; workflow descriptions do not
- naming the tests in the spec makes the contract auditable: a reader can run
  the contract

## ADR-0009: Discovered Project Quality Gate

Status: accepted

The harness's single `test_command` is generalized into a quality gate: a set of
language-specific inspection checks — format, lint, test, dependency hygiene,
advisory audit, static analysis — drawn from the project's own toolchain rather
than hardcoded into the engine. Built-in toolchain profiles per stack declare
the default checks, how to interpret a check's findings, and which findings are
safely auto-fixable; a discovery step detects the stack, probes which tools are
actually available, and proposes a gate the user ratifies into committed
`.localpilot.toml`. The rule engine runs checks at a per-check cadence (fast
checks each step, full checks at phase boundaries) and acts on findings: safe
deterministic fixers are applied and re-run, remaining failures feed the
anti-sunk-cost loop (retry, bounded, then replan recorded in `DECISIONS.md`), and
dependency/audit findings block for a human decision. Discovered commands are
untrusted — discovery proposes, the user ratifies, and every check runs through
the same permission engine and sandbox as any other shell command.

Reason:

- replaces a single test hook with real per-language cleanup and inspection
  without baking tool lists into the engine
- keeps the engine stack-neutral: the abstraction is built in, the instances are
  discovered (the spirit of ADR-0002)
- makes findings actionable inside the loop instead of advisory, with bounded
  auto-fix and replan rather than runaway churn
- preserves the security model: discovered commands are ratified once and always
  mediated by the permission engine ([`docs/07`](07-security-and-privacy.md)),
  never auto-trusted
- per-check cadence keeps fast per-step feedback without paying full-suite cost
  on every step

## ADR-0008: Anthropic Messages API as the Second Provider

Status: accepted

A second, protocol-distinct provider adapter is added alongside the
OpenAI-compatible one: the Anthropic Messages API. It is implemented clean-room
from the public API reference, talks only to the documented official endpoint,
and exercises the provider trait's generality (top-level `system`,
`tool_use`/`tool_result` content blocks, a required `max_tokens`, and a typed
SSE stream).

Reason:

- satisfies the Stable requirement of at least two provider implementations
  ([`docs/09`](09-release-plan.md))
- proves the provider abstraction is not OpenAI-shaped by construction
- adds a major hosted model family without coupling the core to it (ADR-0002)

## ADR-0007: Windows, Linux, and macOS Are All Tier-1

Status: accepted

LocalPilot targets Windows, Linux, and macOS as equal first-class platforms. No
platform is a second-class port. Behavior parity is a release requirement, CI
builds and tests on all three, and installers ship for all three.

Reason:

- the target users run on all three platforms
- shell/filesystem security policy must be correct per-platform, not POSIX-only
- treating one OS as primary causes silent breakage on the others
- forces explicit Windows and POSIX command/path handling from the start

## ADR-0006: Ratatui as the TUI Framework

Status: accepted

The terminal UI is built on `ratatui` with the `crossterm` backend and
`tui-textarea` for input. This is a committed choice, not a recommendation.

Reason:

- `ratatui` is actively maintained and the de facto Rust TUI framework
- `crossterm` provides one terminal backend across Windows, Linux, and macOS,
  supporting the tier-1 platform commitment (ADR-0007)
- a single committed stack keeps rendering, layout, and snapshot tests uniform
- alternatives are out of scope unless a future ADR supersedes this one

## ADR-0005: Read-Only Local Behavior Reference

Status: accepted

A local working implementation may be inspected as a read-only behavior
reference while planning and implementing this Rust project.

The reference may be used to clarify expected workflows, command behavior,
configuration shape, user-facing edge cases, and high-level product
requirements. It must not be used as source material for copied, translated, or
mechanically ported code, prompts, tests, private endpoint behavior,
implementation structure, identifiers, UI copy, branding, or other prohibited
material.

Reason:

- preserves momentum while the Rust specs are still incomplete
- gives implementers a working behavior baseline for ambiguous flows
- keeps this repository independently authored and clean-room auditable
- makes provenance expectations explicit in planning and review

## ADR-0004: No Private Endpoint Adapters

Status: accepted

LocalPilot will not implement adapters for private, undocumented, or
consumer-product endpoints. Provider integrations must use official APIs, local
servers, or explicit user-owned custom endpoints.

Reason:

- reduces legal and account risk
- keeps provider contracts stable
- avoids brittle reverse-engineered behavior
- preserves trust in the project

## ADR-0003: Project Files Are Harness Source of Truth

Status: accepted

The harness treats `brief.md` and `PROGRESS.md` as authoritative. Transcripts
are helpful context but not authoritative state.

Reason:

- users can inspect and edit plans
- sessions can resume after crashes
- implementation remains auditable

## ADR-0002: Provider-Neutral Core

Status: accepted

The core crate must not depend on provider-specific APIs or payload shapes.

Reason:

- avoids coupling the product to one vendor
- makes local models first-class
- keeps tests independent of network access

## ADR-0001: Rust Workspace with Narrow Crates

Status: accepted

LocalPilot is split into narrow crates rather than one large binary crate.

Reason:

- clearer boundaries
- easier clean-room review
- smaller test surfaces
- easier future embedding
