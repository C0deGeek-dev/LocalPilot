# Plan-Template.md — Skeleton For A Multi-Slice Build Plan In This Repo

> **Purpose.** Reusable shape for an autonomous-agent-driven, multi-slice
> *build-process* plan for Unshackled. This is developer-process tooling, not a
> shipped product artefact — keep it separate from the product harness's own
> `brief.md` / `PROGRESS.md` (those are runtime files spec'd in
> `docs/06-harness-spec.md`).
>
> Adapted for this repo from the author's own original multi-slice plan
> template; no third-party provenance.
>
> Copy this file to `tasks/<Name>-Plan.md` and replace placeholders.
>
> **Disposable.** The plan and its `tasks/<name>/` folder are deleted (or
> archived out of the repo) before v1. Shipped code, comments, tests,
> identifiers, and commit messages must not depend on or reference them — see
> §6.11. Commit the folder anyway while live, so the plan is resume-safe across
> machines and context resets (§6.14).

> **Name-clash rule.** Never name a build-plan file `PROGRESS.md` or `brief.md`
> — those names are reserved for the product harness runtime. Build-plan
> tracking lives only in subject Progress-log sections under `tasks/`.

> **Terms.**
> - **Subject** — one file under `tasks/<name>/` (`NN-<slug>.md`). 5-8 per plan.
>   Subject `00` is reserved for tooling research and readiness.
> - **Box** — one `[ ]` checklist item inside a subject file, ID
>   `<subject-id>.<box-number>`. Stable; never renumbered.
> - **Slice** — one agent work-session, recorded as one line in a subject's
>   Progress log. A box may span several slices; a slice may tick several boxes.
> - **Checkpoint** — a durable handoff point after one or more boxes complete:
>   plan files updated, verification recorded, commit created, branch pushed.
> - **Collaboration mode** — `solo`, `coordinated`, or `parallel`. All modes are
>   resume-safe; only `parallel` activates branch/owner coordination tables.

> **Placeholder convention.**
> - `<Name>` — PascalCase plan name (e.g. `ProviderAdapters`).
> - `<name>` — lowercase folder name for the plan's subjects.
> - `NN` — two-digit subject number (`01`, `02`), matches the §3 `#`.
> - `<slug>` — short kebab-case subject name, e.g. `permission-engine`.
> - `<box-id>` — shorthand for `<subject-id>.<box-number>`, e.g. `01.3`.
> - `<base>` — merge base / target branch this plan branches from; record in §2.
> - `TBD` — the agent fills during the run.
> - Do not use literal ellipses in filenames.

---

## How to use this template

1. Confirm the work is **Tier L** (see the `plan-large-task` SKILL.md trigger).
   If it is Tier S, do not use this template — use in-session `EnterPlanMode`.
2. Copy this file to `tasks/<Name>-Plan.md`.
3. Create a sibling folder `tasks/<name>/` to hold subject files.
4. Fill §1 (Subject), Collaboration model, §2 (Inputs), §3 (Subject index).
5. Set Collaboration mode to `solo` unless coordination/parallelism is known up
   front.
6. Create `tasks/<name>/00-tooling-research-and-readiness.md` from the "00
   subject-file shape" below before the agent starts.
7. Create each other indexed subject file from the "Subject-file shape" below.
8. Fill §6 (Cross-cutting principles) — short, blocking, non-negotiable.
9. Leave §4 (Decision log) and §5 (Master tracker) starter rows for the agent.
10. Leave §7 (Gate review) as-is; the agent ticks it last.
11. Leave §8 (Acceptance / sign-off) empty for the user or reviewer.
12. Create `tasks/<name>/manual-actions.md` from the start — split agent-owned
    boxes from human-owned boxes.
13. Create `tasks/<name>/lessons.md` empty. Append *during* the run, the moment
    a slice teaches something. Durable lessons migrate to the permanent
    `tasks/lessons.md` at §7.
14. Run the Captain Hindsight checkpoint in each subject file before marking that
    subject `DONE` in §5. A `DO NOT CLOSE` verdict keeps the subject open until
    the required fixes, decisions, or lessons are resolved.

---

## Collaboration model

> One plan shape supports solo, coordinated, and parallel work. Start in `solo`
> unless multiple active workers are expected now. Upgrade by a §4 Decision-log
> row when the need appears; do not fork the plan or switch templates.

| Field | Value |
|---|---|
| Mode | `solo` / `coordinated` / `parallel` |
| Primary owner | TBD |
| Coordinator | same as Primary owner unless Mode is `parallel` |
| Resume safety | required |
| Parallel branches | `no` unless Mode is `parallel` |
| Notes | TBD |

Mode rules:

- `solo` — one active worker owns the plan at a time. Checkpoint commits/pushes
  are still required so another worker (or the same one after a context reset)
  can resume.
- `coordinated` — one primary worker owns the plan, with explicit non-agent
  boxes for release, product, security, or review sign-off. Use
  `manual-actions.md`; no parallel branch table required.
- `parallel` — multiple workers may work concurrently on separate subjects. Each
  active subject records owner, branch, dependencies, conflict-risk files,
  status, and handoff notes before work starts.

If Mode changes, append a §4 Decision-log row with the previous mode, new mode,
rationale, affected subjects, and branch/ownership impact.

### Parallel work tracker

> Fill only when Mode is `parallel`. Otherwise leave the starter row empty or
> mark `n/a`; keep the section so a later upgrade has a known place to land.

| Subject | Owner | Branch | Dependencies | Conflict-risk files | Status | Handoff notes |
|---|---|---|---|---|---|---|
| | | | | | | |

### Checkpoint gate

> A box is not done until the work can be resumed by someone else. A single
> checkpoint may cover several related boxes.

Before considering a box done:

- Update the box, subject Progress log, §5 tracker, and any affected §4
  decisions / lessons / manual actions.
- Run the relevant verification command (see §7 gate), or record the exact
  blocker and reproduction command.
- Commit the coherent checkpoint and push the branch.
- Keep commit messages, branch names, and PR titles/descriptions plan-agnostic:
  no box IDs, decision IDs, plan filenames, `tasks/<name>/`, or `slice`/`slices`
  (see §6.11). Repo commit style still applies.

Example commit message shape: `Add provider retry policy tests`, not
`Complete 03.2`.

---

## 1. Subject

> One paragraph. What is being built. What is explicitly out of scope. The "out
> of scope" half is as important as the "in scope" half — it stops the plan from
> sprawling.

---

## 2. Authoritative inputs

> Table. One row per input doc / branch / spec. Contribution column says exactly
> what each input gives. For this repo, the relevant specs are in `docs/` —
> name the exact doc(s) the plan implements.

| Source | Contribution |
|---|---|
| | |

---

## 3. Subject file index

> One row per subject file. **Aim for 5-8 subjects, not 15.** If you need more,
> the work is probably two plans.

| # | File | Subject |
|---|---|---|
| 00 | `tasks/<name>/00-tooling-research-and-readiness.md` | Tooling research and readiness |
| 01 | `tasks/<name>/01-<slug>.md` | |
| 02 | `tasks/<name>/02-<slug>.md` | |
| TBD | TBD | TBD |

---

## 4. Decision log

> Append-only. Every time the agent (or reviewer) deviates from a spec literal —
> even slightly — a row goes here. **A spec amendment never disappears into a
> slice; it is recorded as a decision with rationale.** This is what makes the
> plan survive a context-window reset.
>
> **Cite the box.** The Refs column names the box ID(s)
> (`<subject-id>.<box-number>`) and/or file(s) the decision touches.
>
> **ADR promotion (this repo).** A decision-log row dies when the plan folder is
> deleted. If a decision is a *durable architecture* call — anything a future
> contributor would need to understand the design — promote it to a real ADR in
> `docs/10-decisions.md` in the house format, and cite the ADR number in the
> Refs column. Transient build-sequencing choices
> stay here; durable architecture choices graduate to docs/10.
>
> **Retiring a subject or box.** If a subject or box proves wrong mid-run, do
> **not** delete it. Mark it done and struck
> (`- [x] ~~<box-id> <box text>~~ ABANDONED; see D###`), mark the subject
> `ABANDONED` in §5, and add a Decision-log row with the rationale.
>
> **Collaboration changes.** Mode changes are decisions. Record previous mode,
> new mode, rationale, affected subjects, and branch/ownership impact.

| ID | Date | Title | Decision | Rationale | Refs (box IDs / files / ADR#) |
|---|---|---|---|---|---|
| D001 | | | | | |

---

## 5. Master progress tracker

> One row per subject file. A subject is `DONE` when **every** box in its file is
> `[x]` and its Hindsight checkpoint verdict is `CLOSE`. Abandoned boxes use the
> struck-and-done format from §4. Mark a retired subject `ABANDONED`; an
> abandoned subject still gets its Done cell `[x]` (abandoned counts as
> resolved). §5 is "fully ticked" when every Done cell is `[x]` and every Status
> is `DONE` or `ABANDONED`.
>
> **Owner labels.** Every box MUST have an owner from the enum: `agent`,
> `release-engineer`, `product-owner`, `tech-lead`, `domain-sme`. In a solo repo
> most boxes are `agent`; reserve human roles for sign-off boxes. The
> Owner-summary column names the specific roles present (e.g. `agent: 4;
> tech-lead: 1`), never a generic `human`.
>
> **Human-owned boxes** are mirrored into `tasks/<name>/manual-actions.md` so
> they don't get lost between agent-owned boxes. Keep the two in sync.

| Done | # | File | Status | Owner summary | Human actions mirrored? |
|---|---|---|---|---|---|
| [ ] | 00 | `tasks/<name>/00-tooling-research-and-readiness.md` | TODO | agent: 8 | n/a |
| [ ] | 01 | `tasks/<name>/01-<slug>.md` | TODO | TBD | TBD |
| [ ] | 02 | `tasks/<name>/02-<slug>.md` | TODO | TBD | TBD |

---

## 6. Cross-cutting principles

> These apply to every subject file. Violations are blockers, not nits. Lift any
> repo-specific rules from `AGENTS.md`, `CLAUDE.md`, and
> `docs/13-rust-best-practices.md`.

1. KISS · YAGNI · CLEAN · SOLID · DRY (in that order when conflicting).
2. **Clean-room provenance is blocking.** All code, prompts, tests, identifiers,
   and UI copy original to this repo; official public APIs or local servers
   only. See the `clean-room-guard` skill and `docs/00-clean-room.md`.
3. **Rust engineering rules hold** (docs/13): MSRV 1.82, exact-pinned workspace
   deps, typed errors per crate, `#![forbid(unsafe_code)]`, no
   `unwrap`/`expect`/`panic!` on library runtime paths, cross-platform
   path/shell discipline.
4. **Tier-1 parity.** Windows, Linux, and macOS are equal tier-1 (ADR-0007). A
   box that only works on one OS is not done.
5. **Keep code modular and locally understandable.** Small, cohesive crates and
   modules with explicit boundaries. Split files before they become dumping
   grounds; split functions when branching/nesting/mixed responsibility make
   them hard to scan. If a file or function must be large, record why and pin
   the behaviour with tests.
6. **Cyclomatic complexity stays low.** Branch-heavy functions are blockers
   unless the branching is inherent to the domain and covered by focused tests.
   Prefer guard clauses, extracted decision helpers, table-driven cases, or enum
   dispatch over deep `if`/`else` or nested `match` on booleans.
7. **Spec at the contract level, not the SDK level.** State requirements as
   testable contracts on observable behaviour. Do not prescribe specific crate
   call shapes — that's the implementer's choice. State *what* must be true, not
   *how* to call it.
8. **Coverage % is a smell-detector, not a goal.** A test pins observable
   behaviour, not a number. If you can't say "this test prevents future-X bug",
   delete it. Coverage gaps provoke "why is this path untested?", not "add a
   test that touches it".
9. **Every plan box has an owner and a stable ID.** Owner from the §5 enum; ID
   `<subject-id>.<box-number>`. See §5.
10. **Lessons land in `tasks/<name>/lessons.md` as they happen**, not at the
    gate — per-plan run-notes that die with the folder. Durable lessons migrate
    to the permanent `tasks/lessons.md` at §7.
11. **Code and public metadata are plan-agnostic.** Comments, test names, commit
    messages, branch names, PR titles, and identifiers must read as permanent
    project artefacts. Never reference the plan, slices, box IDs, decision-log
    entries (`D###`), or this file — they vanish when the plan folder is deleted.
    Put the *why* directly in the comment. (A durable design rationale belongs in
    an ADR, not a plan ref — see §4 ADR promotion.)
12. **Captain Hindsight review before subject close.** Before marking a subject
    `DONE`, run the subject's Hindsight checkpoint (Appendix). Record Keep / Fix
    before closing / Record / Risk / Verdict. A `DO NOT CLOSE` verdict is a
    blocker.
13. **Tooling research before implementation.** Every plan starts with subject
    `00` unless explicitly waived in §4. No implementation subject starts until
    subject 00 records repo context, baseline verification, stack best practices,
    official sources, and any approved skills/MCP/tooling.
14. **All plans are resume-safe.** A checked box is backed by a durable
    checkpoint: plan updates, verification note or blocker, commit, and push. No
    box is ticked merely because work exists in an unpushed workspace.
15. **Parallelism is opt-in.** Start in `solo` unless multiple workers are known
    up front. Changing to `parallel` needs a §4 decision and a filled Parallel
    work tracker before concurrent subject work starts.
16. *(plan-specific principles below)*

---

## 7. Gate review (run last; tick everything)

> Run only when §5 is fully ticked. §7 is the engineering gate the agent ticks;
> §8 is human acceptance that follows it.

- [ ] All §5 subjects done (or explicitly `ABANDONED` with a §4 row)
- [ ] Subject 00 completed, or explicitly waived/abandoned with a §4 row
- [ ] `cargo fmt --check` clean
- [ ] `cargo clippy --workspace --all-targets -- -D warnings` clean
- [ ] `cargo test --workspace` passes (or `cargo nextest run --workspace`)
- [ ] `cargo check --workspace` clean
- [ ] If deps changed: `cargo machete` clean. Before a release milestone:
      `cargo deny check` and `cargo audit` clean
- [ ] Cross-cutting principles from §6 reviewed; any plan-specific tests/rules
      hold
- [ ] Every non-abandoned subject has a recorded Captain Hindsight checkpoint
      with verdict `CLOSE`
- [ ] Every ticked box has a Progress-log entry and is covered by a pushed
      checkpoint commit
- [ ] Durable architecture decisions promoted to ADRs in
      `docs/10-decisions.md` (see §4)
- [ ] Current plan/integration branch is pushed; `git status --short` has no
      uncommitted plan/code changes except explicitly deferred artefacts
- [ ] If Collaboration mode is `parallel`, the Parallel work tracker has no stale
      active owners, unmerged subject branches, or unresolved handoff notes
- [ ] Shipped code/tests/comments/identifiers are plan-agnostic — grep the repo
      **excluding `tasks/`** for box IDs (`\b\d\d\.\d+\b`), decision IDs
      (`\bD\d{3}\b`), the literal `tasks/<name>/`, the plan filename
      `<Name>-Plan.md`, and `\bslices?\b`; zero hits after triaging false
      positives (version strings can match the box-ID pattern)
- [ ] Commit messages are plan-agnostic — `git log <base>..HEAD` mentions no box
      IDs, decision IDs, `tasks/<name>/`, `<Name>-Plan.md`, or `slice`/`slices`
- [ ] Branch names and public PR titles/descriptions are plan-agnostic if used
- [ ] `tasks/<name>/manual-actions.md` — every human-owned box resolved or
      explicitly deferred
- [ ] *(plan-specific gates below)*
- [ ] `tasks/<name>/lessons.md` reconciled; lasting lessons migrated to the
      permanent `tasks/lessons.md` (create it if missing) before the
      `tasks/<name>/` folder is deleted (see Disposable note)
- [ ] Plan handed to reviewer for §8 sign-off

---

## 8. Acceptance / sign-off

> Filled by the user or reviewer after §7 passes. Intentionally separate from §4:
> sign-off is acceptance, not a spec amendment.

| Date | Reviewer | Result | Notes |
|---|---|---|---|
| | | | |

---

## 00 subject-file shape (copy into `tasks/<name>/00-tooling-research-and-readiness.md`)

```markdown
# 00 — Tooling Research And Readiness

## Goal
Research the project stack, current best practices, official documentation,
assistant skills, MCPs, and local tooling before implementation starts. Convert
the findings into concrete plan rules, gates, enabled tools, and references so
later subjects execute with the right context already in place.

## Boxes
> Subject 00 is required unless explicitly waived in the plan's §4 Decision log.
> No implementation subject starts until this subject is `DONE`, `ABANDONED`,
> or waived by decision.

- [ ] **00.1** (agent) Read repo instructions and authoritative docs (AGENTS.md,
      CLAUDE.md, the owning docs/ spec); list applicable constraints,
      clean-room/provenance rules, and existing conventions.
- [ ] **00.2** (agent) Inventory the crate graph, workspace deps, CI, existing
      commands, and the implementation surfaces this plan will touch.
- [ ] **00.3** (agent) Run the baseline gate (fmt/clippy/test/check), or record
      exact blockers and the command output needed to reproduce them.
- [ ] **00.4** (agent) Research current Rust best practices for the surfaces this
      plan touches using official or primary sources; record only findings that
      affect this plan.
- [ ] **00.5** (agent) Research APIs, crates, providers, or standards this plan
      depends on using official or primary sources; record links, versions, and
      date-sensitive notes (provider work needs official-docs provenance).
- [ ] **00.6** (agent) Review applicable repo skills and any candidate MCP
      servers/tools; classify each `adopt`/`defer`/`reject` with rationale,
      trust notes, source URL, permissions, and setup cost.
- [ ] **00.7** (agent) Set up only approved local tooling/config needed before
      coding; keep security-sensitive permissions out of repo config unless
      narrowly justified.
- [ ] **00.8** (agent) Bake adopted findings into the plan: update §6, §7 gates,
      subject boxes, §4 decisions, and `tasks/<name>/lessons.md`. Research that
      changes no plan artefact is not complete; bake in the finding or record why
      it was rejected/deferred. End with an implementation-readiness summary.

## Hindsight checkpoint
> Run after all boxes complete and before marking the subject `DONE` in §5. Use
> the embedded prompt in "Appendix: Captain Hindsight Prompt". Record the result.
> Required sections: Keep; Fix before closing; Record; Risk; Verdict (`CLOSE` or
> `DO NOT CLOSE`). A `DO NOT CLOSE` verdict keeps the subject open.

- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
> One line per slice. Date · slice number · box IDs touched · what shipped · how
> verified · checkpoint commit/push status. Append a `lessons.md` line here too
> whenever the slice taught something.
```

---

## Subject-file shape (copy into each `tasks/<name>/NN-<slug>.md`)

```markdown
# <subject-id> — <Subject Title>

## Goal
> One paragraph. What this subject delivers.

## Boxes
> ID = `<subject-id>.<box-number>`. Stable — never renumber; mark retired boxes
> done and struck with `ABANDONED; see D###` (see §4), don't delete.
> Owner MUST be one of the §5 enum:
> agent · release-engineer · product-owner · tech-lead · domain-sme.

- [ ] **01.1** (agent) Box description — outcome wording. Include the smallest
      verifiable artefact (test name, file path, log line).
- [ ] **01.2** (agent) Next box.
- [ ] **01.3** (tech-lead) Box that needs human action — also mirror into
      `tasks/<name>/manual-actions.md`.

## Hindsight checkpoint
> Run after all boxes complete and before marking the subject `DONE` in §5. Use
> the embedded prompt in "Appendix: Captain Hindsight Prompt". Record the result.
> Required sections: Keep; Fix before closing; Record; Risk; Verdict (`CLOSE` or
> `DO NOT CLOSE`). A `DO NOT CLOSE` verdict keeps the subject open.

- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
> One line per slice. Date · slice number · box IDs touched · what shipped · how
> verified · checkpoint commit/push status. Append a `lessons.md` line here too
> whenever the slice taught something.
```

---

## manual-actions.md shape (`tasks/<name>/manual-actions.md`)

> Mirror of every human-owned box from the subject files (see §5). One row per
> human action; keep in sync with the owning subject file. Status is one of
> `TODO`, `DONE`, `DEFERRED` (a deferral needs a rationale). Owner is a
> non-`agent` role from the §5 enum.

| Box ID | Owner | Action | Source subject | Status | Deferral rationale |
|---|---|---|---|---|---|
| 01.3 | tech-lead | TBD | 01 | TODO | |

---

## Appendix: Captain Hindsight Prompt

> Embedded so copied plans are self-standing and do not depend on machine-local
> prompt files. Run at each subject close (§6.12).

```text
You are now Captain Hindsight.

Review the completed subject, phase, box, or major plan section with hindsight.
Assume the work is already done, then identify what is clearer now than it was
before the work started.

Check specifically for:
- Scope drift or missed requirements.
- Spec deviations that need a Decision-log row (and whether the decision is
  durable enough to promote to an ADR in docs/10).
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
2. Fix before closing: concrete issues, missing tests, spec drift, plan hygiene,
   or design problems.
3. Record: decisions or lessons that must be added to the plan files (note any
   that should become an ADR).
4. Risk: anything still uncertain after verification.
5. Verdict: CLOSE or DO NOT CLOSE.

If the verdict is DO NOT CLOSE, list the smallest concrete actions needed before
the work can be closed.
```
