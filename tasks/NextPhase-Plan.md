# NextPhase-Plan.md — LocalPilot Next Phase: Reliability Gate, Durable Sessions, Catalog, Headless Drive

> **Template provenance.** Built from the canonical plan template
> (`D:\repos\c0degeek-ai\templates\plan-template.md`, 2026-06-09 revision) via
> the `plan-from-template` workflow, plus this repo's binding adaptations from
> `.agents/skills/plan-large-task/SKILL.md` and repo docs: the cargo gate, ADR
> promotion to `docs/10-decisions.md`, clean-room provenance, and the
> name-clash rule. The repo's bundled template copy predates the 2026-06-09
> revision; see manual action M3.
>
> **Disposable.** This file and `tasks/next-phase/` are deleted (or archived
> out of the repo) before v1. Shipped code, comments, tests, identifiers, and
> commit messages must not depend on or reference them — see §6.11.

> **Name-clash rule.** No build-plan file here is named `PROGRESS.md` or
> `brief.md` — those names are reserved for the product harness runtime
> (`docs/06-harness-spec.md`).

> **Terms.**
> - **Subject** — one file under `tasks/next-phase/` (`NN-<slug>.md`).
>   Subject `00` is reserved for tooling research and readiness.
> - **Box** — one `[ ]` checklist item inside a subject file, ID
>   `<subject-id>.<box-number>`. Stable; never renumbered.
> - **Slice** — one agent work-session, recorded as one line in a subject's
>   Progress log. Slice numbers count per subject, starting at 1.
> - **Checkpoint** — a durable handoff point: plan files updated, verification
>   recorded, commit created, branch pushed.
> - **Collaboration mode** — `solo`, `coordinated`, or `parallel`.

---

## How this plan was derived

This plan merges three inputs (see §2) into one ordered build effort:

1. `tasks/localpilot-next-phase-research.md` (2026-06-08) — the authoritative
   feature source. Its §0 identity contract ("accountable autonomy") and §9
   workstreams W0–W6 define *what* the next phase builds and the filter every
   capability must pass.
2. `tasks/opencode-vs-localpilot.md` (2026-06-07) — background breadth survey,
   superseded by (1) where they overlap; still cited for roadmap detail
   (event-sourced store shape, catalog fields, acceptance-criteria patterns).
3. `review-technical.2026-06-09.md` — line-level correctness review of the
   load-bearing runtime code. Found five flat-out-wrong defects (§1) and a
   cluster of permission-model inconsistencies (§2) that falsify, in current
   code, the identity contract's central claim ("every side effect passes a
   typed permission engine, enforced even under bypass").

**The structural decision this plan adds over its inputs:** the review findings
are a *blocking Phase 0 gate*, not a parallel track. Subjects 01 and 02 fix the
wire-contract, streaming, and permission-engine defects and turn the identity
contract into a *testable reliability contract* (spec text + property tests).
No later subject starts before 01 and 02 are `DONE` (D001). Building the W1–W6
features on the current foundation would bake the defects into a larger
surface — e.g. making the permission engine "the first always-on hook" (W1/W5)
while it still auto-approves destructive commands under an allowlist
(review §1.5) spreads the break.

Resulting order: **reliability gate (01–02) → durable sessions (03) → catalog +
budget honesty (04) → headless drive (05) → hook fabric + lifecycle UX (06) →
tools/skills/supply-chain (07)**.

---

## Collaboration model

| Field | Value |
|---|---|
| Mode | `solo` |
| Primary owner | David |
| Coordinator | same as Primary owner |
| Resume safety | required |
| Parallel branches | `no` |
| Notes | Single active worker; checkpoint commits/pushes still required per box so any worker (or the same one after a context reset) can resume. |

Mode rules:

- `solo` — one active worker owns the plan at a time. Checkpoint
  commits/pushes are still required for resume safety.
- `coordinated` — one primary worker plus explicit non-agent sign-off boxes
  tracked in `manual-actions.md`.
- `parallel` — multiple workers on separate subjects; requires the Parallel
  work tracker filled before concurrent work starts.

If Mode changes, append a §4 Decision-log row with the previous mode, new
mode, rationale, affected subjects, and branch/ownership impact.

### Parallel work tracker

> n/a — Mode is `solo`. Kept so a later upgrade has a known place to land.

| Subject | Owner | Branch | Dependencies | Conflict-risk files | Status | Handoff notes |
|---|---|---|---|---|---|---|
| n/a | | | | | | |

### Checkpoint gate

> A box is not truly done until the work can be resumed by someone else.
> A single checkpoint may cover several related boxes.

Before considering a box done:

- Update the box, subject Progress log, §5 tracker, and any affected §4
  decisions / lessons / manual actions.
- Run the relevant verification command from the §2 Verification-commands
  table, or record the exact blocker and reproduction command.
- Commit the coherent checkpoint and push the branch. If the repo has no
  remote, record that once in the Collaboration model Notes and treat the
  local commit as the checkpoint. (This repo has a remote; push.)
- Keep commit messages, branch names, and PR titles/descriptions
  plan-agnostic: no box IDs, decision IDs, plan filenames,
  `tasks/next-phase/`, or `slice`/`slices`.

Checkpoint names describe durable project work, not plan administration:
`Add provider retry policy tests`, not `Complete 03.2`.

---

## 1. Subject

Bring LocalPilot from "architecturally right with critical runtime defects" to
"reliable enough to run unsupervised, with the durable-session, catalog, and
headless-drive foundations the next phase needs." In scope: (a) every Critical/
High finding and the permission-model inconsistencies from
`review-technical.2026-06-09.md`, plus a written, property-tested reliability
contract; (b) the event-sourced session tree with format versioning (research
W1 core); (c) the generated provider/model catalog with per-model context
limits and reasoning-effort control (W3); (d) RPC-over-stdio and an ACP adapter
(W2); (e) the internal hook fabric with the permission engine as its first
hook, and the session lifecycle UX (rest of W1, hook half of W5); (f) tool/
skills upgrades and supply-chain posture (W4 + W6 subset).

**Out of scope** (recorded; not oversights): background/concurrent subagents
and manifest-packaged plugins (W5 second half — follow-on plan once the event
tree, hook fabric, and hardened permission engine exist; the research doc's
§5.13 auditable-concurrency design remains the binding design input for that
plan); the HTTP/axum server (W2 second step — only when a sanctioned local
client needs it); remote MCP/OAuth, LSP, `webfetch`/`websearch` (NEUTRAL tier —
add when convenient, not here); and everything in research §1a (web client,
desktop app, SDK-as-product, cloud share, hosted services) — hard-deferred,
must not re-enter via any box (D002).

### Risks and rollback

| Risk | Impact | Mitigation / rollback |
|---|---|---|
| Permission tightening (subject 02) breaks existing relaxed-profile allowlist workflows | Users who allowlisted `run_shell` start seeing prompts for destructive/privileged commands | Intentional behavior change; document in docs/07 and release notes; the floor only affects Destructive/Privileged/Unknown classes. Rollback: `git revert` of the sandbox commits — no data or config migration involved. |
| Streaming-decoder rework (subject 01) regresses provider decode paths | Corrupted or failed turns against live providers | Fixture-driven tests (multibyte split, split tags, cross-delta blocks) before refactor; adapters live in one crate, changes are per-commit revertible; live-provider validation remains the release gate per `docs/09-release-plan.md`. |
| Event-log schema (subject 03) locks in a wrong shape | Costly migrations for every later session artifact | Format version + migrate-on-load contract from the first release (§6.22); final-message persistence stays canonical until the derivation test (03.4) proves event-rebuilt transcript == stored transcript. Rollback: keep the legacy persistence path; the event log is additive until cutover. |
| models.dev vendoring (subject 04) has a license/attribution problem | Clean-room/provenance violation shipped in-repo | License verified in 00.5 and product-owner sign-off (04.2) before any snapshot lands. Rollback: delete the snapshot + xtask; the small static catalog remains functional. |
| RPC/ACP surface (subject 05) exposes permission flow to a misbehaving client | Unattended approvals or hung sessions | A non-responding client degrades exactly like non-interactive mode: asks denied and recorded. Protocol is versioned; feature is an opt-in subcommand. Rollback: remove the subcommand; runtime is unchanged. |
| Hook re-routing (subject 06) changes recovery/quota/gate behavior | Regression in the recovery ladder or quality gate | Re-route is structural-only, pinned by the existing behavior tests before and after. Rollback: per-commit revert; hooks wrap existing call sites rather than replacing logic. |

---

## 2. Authoritative inputs

| Source | Contribution |
|---|---|
| `tasks/review-technical.2026-06-09.md` | Defect inventory for subjects 01–02 (§1 critical/high, §2 permission inconsistencies, §3 lesser defects, §5 missing functionality); the reliability-contract idea (§6.1); the two-memory-systems problem (§6.2). Moved under `tasks/` and committed per M2 (2026-06-10). |
| `tasks/localpilot-next-phase-research.md` | Identity contract (§0) and decision filter; workstreams W0–W6 (§9); Pi/OpenCode mechanics to adapt (§5: event bus, RPC framing, session tree, compaction, catalog, thinking levels, steering, skills standard, supply chain, lifecycle); clean-room cautions (§10). |
| `tasks/opencode-vs-localpilot.md` | Background survey; event-log event inventory (Priority 2), catalog field list (Priority 4), tool-upgrade ordering (Priority 6), phase acceptance-criteria patterns. Superseded by the research doc where they conflict (D003). |
| `docs/06-harness-spec.md` | Owns the session loop / harness behavior subjects 01, 03, 06 change; receives the loop half of the reliability-contract spec text. |
| `docs/07-security-and-privacy.md` | Owns the permission/sandbox behavior subject 02 changes; receives the permission half of the reliability contract. |
| `docs/04-provider-contract.md` | Owns the provider contract subjects 01, 04 extend (capability flags, reasoning effort, per-model limits, late-system-message semantics). |
| `docs/05-tool-system.md` | Owns the tool registry/approval behavior subjects 02, 07 change. |
| `docs/10-decisions.md` | ADR home; this plan promotes at least three durable decisions (reliability contract, memory convergence, bypass-boundary scope). |
| `docs/00-clean-room.md` + `clean-room-guard` skill | Provenance rules; OpenCode and Pi are behavior references only (research §10). |
| `docs/13-rust-best-practices.md` | Engineering rules binding on every box. |
| `docs/09-release-plan.md` | Live-provider validation as release gate (bears on subject 01 adapter fixes). |

### Verification commands

> Single source for "the relevant verification command" used by the Checkpoint
> gate and the §7 gates. Subject 00.3 confirms or corrects these rows against
> the real repo; 00.8 bakes in any additions.

| Purpose | Command | Notes |
|---|---|---|
| Build | `cargo check --workspace` | |
| Test | `cargo test --workspace` | |
| Lint/format | `cargo fmt --check` then `cargo clippy --workspace --all-targets -- -D warnings` | both must be clean |
| Dep hygiene | `cargo machete` | on dependency change only |
| Release hygiene | `cargo deny check` and `cargo audit` | before a release milestone only |
| Plan-specific gate (tool pairing) | `cargo test -p localpilot-harness --test pairing` | pairing invariant: scenario tests + 48-case property run (01.2) |
| Plan-specific gate (permissions) | `cargo test -p localpilot-sandbox` and `cargo test -p localpilot-tools --test tools` | allowlist floor, wrapper classification, destructive git flags, approval detail, binary overwrite (02.2/02.3/02.4/02.7) |

---

## 3. Subject file index

> **Depends-on orders subjects in every mode.** Solo runs pick the next subject
> by this column. `—` means no dependency beyond subject 00. Graph is acyclic.

| # | File | Subject | Depends on |
|---|---|---|---|
| 00 | `tasks/next-phase/00-tooling-research-and-readiness.md` | Tooling research and readiness (absorbs research W0) | — |
| 01 | `tasks/next-phase/01-wire-contract-and-streaming.md` | Reliability gate A: wire-contract and streaming correctness | 00 |
| 02 | `tasks/next-phase/02-permission-engine-hardening.md` | Reliability gate B: permission engine hardening + reliability contract | 00 |
| 03 | `tasks/next-phase/03-durable-session-events.md` | Durable sessions: store convergence + event-log tree | 01, 02 |
| 04 | `tasks/next-phase/04-model-catalog-and-budgets.md` | Provider/model catalog, reasoning effort, honest budgets | 01, 02, 03 |
| 05 | `tasks/next-phase/05-headless-drive-rpc-acp.md` | Headless drive: RPC over stdio + ACP adapter | 03 |
| 06 | `tasks/next-phase/06-hook-fabric-and-lifecycle.md` | Hook fabric + session lifecycle UX | 03 |
| 07 | `tasks/next-phase/07-tools-skills-supply-chain.md` | Tool/skills upgrades + supply-chain posture | 02 |

---

## 4. Decision log

> Append-only. Every deviation from a spec literal gets a row. Cite box IDs /
> files in Refs. **ADR promotion (repo rule):** a decision-log row dies with
> the plan folder; durable architecture calls graduate to a real ADR in
> `docs/10-decisions.md` and cite the ADR number here. **Retiring a box:** mark
> it done and struck (`- [x] ~~<box-id> …~~ ABANDONED; see D###`), never
> delete.

| ID | Date | Title | Decision | Rationale | Refs (box IDs / files / ADR#) |
|---|---|---|---|---|---|
| D001 | 2026-06-09 | Reliability gate blocks feature work | Subjects 01 and 02 must be `DONE` before any box in subjects 03–07 starts. | The research doc's identity-contract claims ("every side effect passes a typed permission engine, enforced even under bypass"; unattended multi-step execution) are falsified in current code by review §1.1/§1.5/§2.1–2.3. Building W1–W6 on top spreads the defects into a larger surface (hooks, RPC, subagents all inherit the permission engine and session loop). | Subjects 01, 02; review-technical §1–§2, §6.1 |
| D002 | 2026-06-09 | Scope cut for this plan | Subagents + manifest plugins, HTTP server, remote MCP/OAuth, LSP, webfetch/websearch are out of this plan (follow-on); research §1a items (web/desktop/SDK/cloud) are hard-deferred and may not re-enter via any box. | Subagents/plugins depend on the event tree (03), hook fabric (06), and a hardened permission engine (02) — planning them now is speculative. §1a items contradict the identity contract per research. Template caps plans at 8 subjects; this plan is full. | §1; research §1a, §5.13, §9 |
| D003 | 2026-06-09 | Research doc supersedes survey doc | `tasks/localpilot-next-phase-research.md` is the authoritative feature source; `tasks/opencode-vs-localpilot.md` is background, cited only where the research doc lacks detail. | The research doc post-dates and explicitly supersedes the survey (adds Pi, identity contract, deferred list). Two authorities invite drift. | §2 |
| D004 | 2026-06-09 | Compaction work split across subjects | Compaction *correctness* fixes (oversized-exchange truncation pass, incremental flood check, cached result) land in subject 03; the *window-relative* trigger and iterative summary land in subject 04. | Window-relative math needs real per-model context windows, which only exist after the catalog (04). Sequencing them together would stall the correctness fixes behind the catalog. | 03.6, 04.6; review-technical §3.2–§3.4; research §5.4 |
| D005 | 2026-06-10 | Blank-id tool calls are filtered, not synthesized | 01.1's box literal says "synthesize an error tool_result per unexecuted call". A call whose id is blank can never be validly answered (a tool_result must reference a non-empty tool_use id), so the loop validates *before* persisting: blank-id `tool_use` blocks never enter history, and the model is corrected via a persisted user message instead; calls with usable ids get the synthesized rejection result as specified. | Pairing the unpairable would re-violate the wire contract the box exists to protect. Filtering keeps the invariant trivially true for the degenerate case and synthesizes results for every answerable call. | 01.1, 01.2; `tasks/next-phase/01-wire-contract-and-streaming.md` |
| D006 | 2026-06-10 | Late system messages are positional on every wire | A mid-conversation system message keeps its position: OpenAI-style wires send it in place as `role:"system"`; the Anthropic-style wire hoists only the *leading* system run and delivers later system content as user-role blocks at its original position. Spec text in docs/04 §Late System Messages. | The previous behavior silently time-traveled late system content to the front on one wire only — same history, different model-visible conversation per provider. Positional delivery is the only semantics that is uniform and preserves author intent. | 01.8; docs/04 |
| D007 | 2026-06-10 | Bypass scope documented, not implemented as containment | The bypass workspace-boundary claim is scoped honestly: bypass keeps the boundary for path-bearing (file-tool) effects only; shell commands carry no path information and are auto-allowed without containment. Code comment, docs/07, and behavior now agree; no command path-containment story is built in this plan. | Command path containment requires parsing arbitrary argv for path-bearing arguments — heuristic, bypassable, and a false promise. An honest scope statement ("treat bypass as full shell access") protects users better than implied containment that does not exist. Captured in ADR-0010 (proposed); pending product-owner approval (02.8). | 02.5; docs/07; docs/10 ADR-0010 |
| D008 | 2026-06-10 | Allowlist floor preserves the headless gate allowance | The floor-aware allowlist auto-approves relaxable (low-risk) effects in *any* interactivity rather than only relaxing interactive `Ask`: lifting the non-interactive deny for ReadOnly/ProjectWrite/Network is exactly the mechanism the ratified quality gate (ADR-0009) uses to run headless. Destructive/Privileged/Unknown/ExternalWrite commands, secret reads, and out-of-workspace paths are never relaxed in any mode. | First implementation relaxed only `Ask` and broke `ratification_allowance_lets_the_gate_run_headless_but_grants_nothing_else`; the ratification allowance is a documented dependency of the relaxed profile. | 02.2; docs/07; ADR-0009 |
| D010 | 2026-06-10 | Session-picker commands renamed to avoid the harness `/resume` | The lifecycle box specified REPL `/resume` as a session picker, but `/resume` already triggers the harness step-resume flow (documented behavior). The session lifecycle shipped as `/sessions` (list) + `/session <id>` (switch), with `/new`, `/fork`, `/clone`, `/tree` as specified. CLI surface unaffected (`session list/export/resume`, print `--continue`/`--resume`). | Overloading one command with two unrelated flows (run the next plan step vs. switch conversations) invites destructive mistakes; the harness meaning is older and load-bearing. | 06.4 |
| D009 | 2026-06-10 | Subject 04 slimmed to local-first scope | The first instance targets the user's local model. The vendored models.dev snapshot + xtask generation (04.1) and its license sign-off (04.2) are ABANDONED for this plan; model metadata comes from provider config and dynamic local `/v1/models` discovery (04.4) instead. Kept, re-sourced from config/discovery rather than the catalog: per-model context limits (04.3), reasoning-effort control with a local no-op clamp (04.5, minus catalog-aware clamping), window-relative compaction (04.6), and doctor/TUI surfacing minus cost metadata (04.7). A hosted-model catalog can land in a follow-on plan; 00.5's license research stays recorded for it. | Product-owner direction 2026-06-10: local model first. Vendoring a hosted-model dataset serves breadth the first instance does not need; discovery + config covers the local case end-to-end with less supply-chain surface. | 04.1, 04.2, 04.3–04.7; manual-actions M-row 04.2 |

---

## 5. Master progress tracker

> A subject is `DONE` when every box is `[x]` and its Hindsight verdict is
> `CLOSE`. Owner labels from the enum: `agent`, `release-engineer`,
> `product-owner`, `tech-lead`, `domain-sme`. Human-owned boxes are mirrored
> into `tasks/next-phase/manual-actions.md`; keep the two in sync.

| Done | # | File | Status | Owner summary | Human actions mirrored? |
|---|---|---|---|---|---|
| [x] | 00 | `tasks/next-phase/00-tooling-research-and-readiness.md` | DONE | agent: 8 | n/a |
| [x] | 01 | `tasks/next-phase/01-wire-contract-and-streaming.md` | DONE | agent: 8 | n/a |
| [x] | 02 | `tasks/next-phase/02-permission-engine-hardening.md` | DONE | agent: 7; product-owner: 1 | yes |
| [x] | 03 | `tasks/next-phase/03-durable-session-events.md` | DONE | agent: 6; product-owner: 1 | yes |
| [x] | 04 | `tasks/next-phase/04-model-catalog-and-budgets.md` | DONE (slimmed per D009) | agent: 5; abandoned: 2 | yes |
| [x] | 05 | `tasks/next-phase/05-headless-drive-rpc-acp.md` | DONE | agent: 5 | n/a |
| [x] | 06 | `tasks/next-phase/06-hook-fabric-and-lifecycle.md` | DONE | agent: 5 | n/a |
| [x] | 07 | `tasks/next-phase/07-tools-skills-supply-chain.md` | DONE | agent: 6 | n/a |

---

## 6. Cross-cutting principles

> These apply to every subject file. Violations are blockers, not nits.

1. KISS · YAGNI · CLEAN · SOLID · DRY (in that order when conflicting).
2. **Clean-room provenance is blocking** (repo non-negotiable). All code,
   prompts, tests, identifiers, and UI copy original to this repo; official
   public APIs or local servers only. See `clean-room-guard` and
   `docs/00-clean-room.md`.
3. **Rust engineering rules hold** (`docs/13-rust-best-practices.md`): MSRV
   1.82, exact-pinned workspace deps, typed errors per crate,
   `#![forbid(unsafe_code)]`, no `unwrap`/`expect`/`panic!` on library runtime
   paths, cross-platform path/shell discipline.
4. **Tier-1 parity.** Windows, Linux, and macOS are equal tier-1 (ADR-0007).
   A box that only works on one OS is not done.
5. **Keep code modular and locally understandable.** Split files before they
   become dumping grounds; if a file or function must be large, record why and
   pin the behavior with tests.
6. **Cyclomatic complexity stays low.** Prefer guard clauses, extracted
   decision helpers, table-driven cases, or enum dispatch over deep nesting.
7. **Spec at the contract level, not the SDK level.** State *what* must be
   true, not *how* to call it.
8. **Coverage % is a smell-detector, not a goal.** A test pins observable
   behavior; if you can't say "this prevents future-X bug", delete it.
9. **Every plan box has an owner and a stable ID.**
10. **Lessons land in `tasks/next-phase/lessons.md` as they happen**, not at
    the gate. Durable lessons migrate to the permanent `tasks/lessons.md` at §7.
11. **Code and public metadata are plan-agnostic.** Comments, test names,
    commit messages, branch names, PR titles, and identifiers never reference
    the plan, slices, box IDs, or decision IDs — they vanish when the plan
    folder is deleted. Put the *why* directly in the comment; durable design
    rationale belongs in an ADR.
12. **Captain Hindsight review before subject close** (Appendix). A
    `DO NOT CLOSE` verdict is a blocker.
13. **Tooling research before implementation.** Subject 00 first unless waived
    by a §4 decision.
14. **All plans are resume-safe.** No box is ticked merely because work exists
    in an unpushed workspace.
15. **Parallelism is opt-in.** Mode change requires a §4 decision and a filled
    Parallel work tracker first.

Plan-specific principles:

16. **Reliability gate ordering (D001).** No box in subjects 03–07 starts
    until subjects 01 and 02 are `DONE`. Only subject 00 research boxes may
    run any time.
17. **Identity filter on every box** (research §0): a box must make the agent
    *more trustworthy to run unsupervised*, or be explicitly tagged NEUTRAL
    table stakes. A box that only adds platform breadth is rejected or moved
    to the follow-on plan.
18. **Fix = spec + test, not just code.** Every defect fixed in subjects 01–02
    lands with (a) a regression or property test pinning the invariant and
    (b) where the invariant is durable, contract text in the owning `docs/`
    spec. A code-only fix is an unticked box.
19. **Behavior-reference discipline** (research §10): OpenCode and Pi are
    read-only behavior references. Do not copy code, prompts, identifiers, UI
    copy, event names, JSON shapes, or package structure. Re-derive from the
    public concept; name things in LocalPilot's vocabulary. models.dev,
    agentskills.io, ACP, and `/v1/models` are public specs/datasets —
    implementing against them is fine; copying another agent's parser or
    transcribing its generated tables is not.
20. **Deferred list is binding** (research §1a, D002): web client, desktop
    app, SDK-as-product, cloud sharing, hosted services must not appear in any
    box, decision, or "while we're here" change.
21. **New wire formats version from day one.** The session event log (03) and
    the RPC protocol (05) each carry an explicit format/protocol version and a
    migrate-on-load (or version-negotiation) contract from their first release.

---

## 7. Gate review (run last; tick everything)

> Run only when §5 is fully ticked. §7 is the engineering gate the agent
> ticks; §8 is human acceptance that follows it.

- [ ] All §5 subjects done (or explicitly `ABANDONED` with a §4 row)
- [ ] Subject 00 completed, or explicitly waived/abandoned with a §4 row
- [ ] Build command from the §2 Verification-commands table passes with 0
      errors and 0 new warnings
- [ ] Test command from the §2 Verification-commands table passes
- [ ] Remaining §2 Verification-commands rows (lint/format, dep/release
      hygiene, plan-specific gates) pass or are recorded `n/a`
- [ ] §1 Risks-and-rollback table reviewed; rollback steps still accurate for
      what actually shipped
- [ ] Cross-cutting principles from §6 reviewed; plan-specific rules hold
- [ ] Every non-abandoned subject has a recorded Captain Hindsight checkpoint
      with verdict `CLOSE`
- [ ] Every ticked box has a Progress-log entry and is covered by a pushed
      checkpoint commit
- [ ] Durable architecture decisions promoted to ADRs in
      `docs/10-decisions.md` (repo rule; see §4)
- [ ] Current plan/integration branch is pushed; `git status --short` has no
      uncommitted plan/code changes except explicitly deferred artifacts
- [ ] If Collaboration mode is `parallel`, the Parallel work tracker has no
      stale active owners, unmerged subject branches, or unresolved handoff
      notes
- [ ] Shipped code/tests/comments/identifiers are plan-agnostic — grep the
      repo **excluding `tasks/`** for box IDs (`\b\d\d\.\d+\b`), decision IDs
      (`\bD\d{3}\b`), the literal `tasks/next-phase/`, the plan filename
      `NextPhase-Plan.md`, and `\bslices?\b`; zero hits after triaging false
      positives (version strings can match the box-ID pattern)
- [ ] Commit messages are plan-agnostic — `git log <base>..HEAD` mentions no
      box IDs, decision IDs, `tasks/next-phase/`, `NextPhase-Plan.md`, or
      `slice`/`slices` (same false-positive triage; file grep does not cover
      commit messages)
- [ ] Branch names and public PR titles/descriptions are plan-agnostic if used
- [ ] `tasks/next-phase/manual-actions.md` — every human-owned box resolved or
      explicitly deferred
- [ ] **Plan-specific:** tool-pairing invariant property test exists and
      passes (after any turn, every `tool_use` id in history has a matching
      `tool_result` id)
- [ ] **Plan-specific:** permission regression tests pass — allowlist can
      never auto-allow Destructive/Privileged/Unknown; shell-wrapper commands
      never classify below Unknown on any platform; destructive git flags
      classify Destructive
- [ ] **Plan-specific:** reliability-contract text landed in the owning docs/
      spec(s) and its ADR is merged
- [ ] **Plan-specific:** session event-log roundtrip + format-migration tests
      pass; a transcript rebuilt from events equals the transcript the model
      saw (including synthetic messages)
- [ ] `tasks/next-phase/lessons.md` reconciled; lasting lessons migrated to
      the permanent `tasks/lessons.md` (create it if missing) before the
      `tasks/next-phase/` folder is deleted (see Disposable note)
- [ ] Plan handed to reviewer for §8 sign-off

---

## 8. Acceptance / sign-off

> Filled by the user or reviewer after §7 passes. Intentionally separate from
> §4: sign-off is acceptance, not a spec amendment.

| Date | Reviewer | Result | Notes |
|---|---|---|---|
| | | | |

---

## Appendix: Captain Hindsight Prompt

> Embedded so this plan is self-standing and does not depend on machine-local
> prompt files. Run at each subject close (§6.12). The clean-room,
> cross-platform, and ADR lines are repo-specific additions to the canonical
> prompt.

```text
You are now Captain Hindsight.

Review the completed subject, phase, box, or major plan section with hindsight.
Assume the work is already done, then identify what is clearer now than it was
before the work started.

Check specifically for:
- Scope drift or missed requirements.
- Spec deviations that need a Decision-log row (and whether the decision is
  durable enough to promote to an ADR in docs/10-decisions.md).
- Lessons that should be recorded before context is lost.
- Tests that pin implementation details instead of observable behavior.
- Complexity, duplication, brittle design, or awkward naming that should be
  fixed now.
- Human-owned actions that need to be mirrored or resolved.
- Plan references that leaked into shipped code, tests, comments, identifiers,
  or commit messages.
- Clean-room provenance: any copied prompt/identifier/UI copy, or any
  private/undocumented endpoint use.
- Cross-platform parity (Windows/Linux/macOS) for anything OS-specific.

Return exactly these sections:

1. Keep: what was correct and should remain.
2. Fix before closing: concrete issues, missing tests, spec drift, plan
   hygiene, or design problems.
3. Record: decisions or lessons that must be added to the plan files (note any
   that should become an ADR).
4. Risk: anything still uncertain after verification.
5. Verdict: CLOSE or DO NOT CLOSE.

If the verdict is DO NOT CLOSE, list the smallest concrete actions needed
before the work can be closed.
```
