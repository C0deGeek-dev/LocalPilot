# Unshackled-Plan.md — v1 Implementation Plan

> Multi-slice, autonomous-agent-driven plan to take Unshackled from its
> clean-room scaffold to a public alpha (`v0.1.0-alpha.1`). Built on
> `tasks/plan-template.md`.
>
> **Disposable.** This plan and `tasks/unshackled/` are deleted (or archived out
> of the repo) once v1 alpha ships. Shipped code, comments, tests, identifiers,
> and commit messages MUST NOT reference this plan, subjects, box IDs, or
> decision IDs — see §6.11.

> **Terms.**
> - **Subject** — one file `tasks/unshackled/NN-<slug>.md`. 10 here (00–09).
> - **Box** — one `[ ]` item in a subject, ID `<subject-id>.<box-number>`
>   (e.g. `03.2`). Stable; never renumbered.
> - **Slice** — one agent work-session, one line in a subject's Progress log.

---

## 1. Subject

**In scope.** Implement Unshackled v1 as specified in `docs/` and `README.md`:
a Rust-native, provider-neutral, local-first coding-agent harness with two
operating modes (default conversational **agent mode** and enforced **harness
mode**), running on Windows, Linux, and macOS as equal tier-1 platforms. v1
delivers, end to end: config loading with deterministic precedence; provider
runtime (one local OpenAI-compatible provider + one official hosted provider)
behind one trait with streaming, capabilities, quota metadata, and recovery;
the eight builtin tools through a permission engine with three profiles
(`default`/`relaxed`/`bypass`); the session/agent loop with cancellation,
limits, context compaction, and bad-output recovery; the full harness (brief +
progress parse/render, `init`, `status`, `intake`, `plan`, `feature`, `resume`,
deterministic rule engine, anti-sunk-cost replan loop, per-step commits);
quota wait/resume with safety gates; MCP client; local memory store; skills
incl. generated drafts; a ratatui/crossterm TUI; a golden-task eval suite; and
release hardening (installers, supply-chain gates, clean-room audit, docs) to
the §9 "Public Alpha Criteria".

**Out of scope** (per `docs/01-product-spec.md` Non-Goals / Later / Out of
Scope): cloud sync; remote-execution service; web UI surface; remote agents;
multi-repo orchestration; plugin/skill marketplace; image input; IDE
integration; model training/fine-tuning; voice; hidden telemetry; any private
or undocumented consumer-product endpoint adapter. Anything resembling a vendor
clone, copied prompts, or copied identifiers is prohibited, not merely
out of scope (see `docs/00-clean-room.md`, §6.12 below).

---

## 2. Authoritative inputs

| Source | Contribution |
|---|---|
| `README.md` | Product identity, repo layout, crate roster, first-milestone command surface, design principles. |
| `docs/00-clean-room.md` | Hard provenance rules; allowed inputs; read-only-reference policy; PR provenance-note format; prohibited framing. **Blocking.** |
| `docs/01-product-spec.md` | Product definition, 7 core jobs, two operating modes, interfaces, user-facing files, first-milestone vs v1-committed vs later scope. |
| `docs/02-architecture.md` | System shape, per-crate responsibilities + must-not-own, runtime flows, data model, error handling, observability. Names crates not yet in the workspace (memory, skills, recovery, quota). |
| `docs/03-implementation-plan.md` | Phase 0–15 task lists and "Done when" gates. Primary source for box wording. |
| `docs/04-provider-contract.md` | Provider declaration fields, `ModelRequest`/`ModelEvent`, error taxonomy, provider-differences, quota semantics, required provider tests, config example. |
| `docs/05-tool-system.md` | `Tool` trait, the 8 builtin tools + per-tool rules, permission decisions, result model, safety invariants. |
| `docs/06-harness-spec.md` | Mode/permission flags, `.unshackled.toml`/`brief.md`/`PROGRESS.md` shapes, command I/O, rule triggers/verdicts, baseline rules, anti-sunk-cost loop, commit policy. |
| `docs/07-security-and-privacy.md` | Trust model, workspace trust, redaction, shell classification table, permission profiles, per-platform policy, quota safety gates, supply chain, abuse resistance. |
| `docs/08-testing.md` | Test layers, golden-task eval fields, snapshot/live policy, fixture policy, required MVP tests, CI matrix. |
| `docs/09-release-plan.md` | Versioning, channels, public-alpha criteria, installer targets, release checklist, clean-room scan terms. |
| `docs/10-decisions.md` | ADR-0001..0007 (narrow crates, provider-neutral core, project files = source of truth, no private endpoints, read-only reference, ratatui, tier-1 tri-platform). ADRs win over style rules. |
| `docs/11-implementation-checklist.md` | Flat capability checklist mirrored into boxes; the "Foundation: replace repo URL" item is already done. |
| `docs/12-feature-specs.md` | UI direction (footer stats, thinking panel), bad-output recovery ladder, skill shape/triggers/suggestions, memory store rules + commands, quota modes/config/safety/UI. |
| `docs/13-rust-best-practices.md` | Engineering style guide: MSRV 1.82, exact-pinned workspace deps, type-driven design, `thiserror` per crate, async/Tokio rules, cross-platform path/shell discipline, secret wrapper, lints (`-D warnings`), test toolbox, `#![forbid(unsafe_code)]`. **Blocking review rules.** |
| `docs/14-dev-tooling.md` | Assistant skills, optional MCP servers, cargo tools to install, repo conveniences, the repo project-skills to author and their order. Source for the §00 bootstrap subject. |
| `CONTRIBUTING.md` / `SECURITY.md` | PR requirements + provenance note; security scope and defaults. |
| Existing scaffold (`crates/*`, `Cargo.toml`, `.github/workflows/ci.yml`, `deny.toml`, `rust-toolchain.toml`, `rustfmt.toml`) | Current state to build on, not replace. 10 stub crates, MSRV/edition pinned, CI fmt+clippy+test+check on 3 OSes. |
| Read-only behavior reference `D:\repos\unshackled` (ADR-0005) | High-level workflow/edge-case clarification ONLY when local docs are silent. Never a source of code/prompts/identifiers/tests; requires a PR provenance note when consulted. |

**`<base>`** = `main` at commit `eab7123` (plan branches from here).

---

## 3. Subject file index

> 10 subjects. This exceeds the template's 5–8 guideline; v1 spans 16 spec
> phases and ~170 checklist items, so the work is bundled by architectural seam
> rather than split into multiple plans. Recorded as **D001**.

| # | File | Subject |
|---|---|---|
| 00 | `tasks/unshackled/00-bootstrap-tooling.md` | START: install/setup dev tooling, author repo skills, decide MCP, verify baseline build — before any product code. |
| 01 | `tasks/unshackled/01-foundation.md` | Workspace hardening: add the 4 missing crates, workspace lints, cargo aliases, `.editorconfig`, CI matrix expansion, supply-chain gates, real `doctor`. (Phase 0) |
| 02 | `tasks/unshackled/02-core-config-store.md` | Core domain types, config loading + precedence + redaction, store (transcript persistence, atomic writes, index). (Phases 1, store slices) |
| 03 | `tasks/unshackled/03-provider-runtime.md` | Provider trait, stream model, registry, local + one official provider, capabilities, quota metadata, retry/backoff, error taxonomy, reasoning round-trip. (Phase 2) |
| 04 | `tasks/unshackled/04-tools-and-sandbox.md` | Tool registry + schema gen, 8 builtin tools, path/containment policy, command classification (Win+POSIX), permission engine + 3 profiles, workspace trust. (Phases 3, security) |
| 05 | `tasks/unshackled/05-session-and-recovery.md` | Session runtime / agent-mode loop, cancellation, limits, print mode, context compaction, bad-output recovery engine. (Phases 4, 9) |
| 06 | `tasks/unshackled/06-harness-core.md` | Brief/progress parse+render, `init`/`status`/`intake`/`plan`/`feature`/`resume`, original prompts, rule engine, worker + anti-sunk-cost loop, commit policy. (Phases 5, 6, 7, 8) |
| 07 | `tasks/unshackled/07-extensions.md` | Quota wait/resume + continuous mode, MCP client, skills (+ generated drafts/suggestions), local memory store. (Phases 11, 12, 13, 14) |
| 08 | `tasks/unshackled/08-terminal-ui.md` | ratatui/crossterm TUI: viewport, input, streaming, approval modal, footer stats, thinking panel, slash commands, pickers, narrow collapse, snapshots. (Phase 10) |
| 09 | `tasks/unshackled/09-evals-and-release.md` | Golden-task eval framework + scorecard, MVP-test coverage completion, installers, clean-room audit, docs, alpha tag. (Phases 8-evals, 15) |

---

## 4. Decision log

> Append-only. Every deviation from a spec literal gets a row, with the box
> ID(s) it touches. A spec amendment never disappears into a slice.
>
> **Retiring a subject/box:** mark done+struck
> (`- [x] ~~<box-id> text~~ ABANDONED; see D###`), mark the subject `ABANDONED`
> in §5, add a row here. Never delete — history must survive a context reset.

| ID | Date | Title | Decision | Rationale | Refs (box IDs / files) |
|---|---|---|---|---|---|
| D001 | 2026-06-01 | 10 subjects, not 5–8 | Plan uses 10 subject files. | v1 spans 16 spec phases / ~170 checklist items across hard crate seams; bundling by seam is clearer than two competing plans and keeps each subject reviewable. | §3, all subjects |
| D002 | 2026-06-01 | Subject 07 is a known scope-drag risk | Keep quota+MCP+skills+memory in one subject for now; **split trigger**: if execution stalls (a single box open across 3+ slices, or the subject exceeds ~1.5× the slice budget of its neighbours), split 07 into `07a` quota/MCP and `07b` skills/memory, log the split here, and renumber via new files (07a/07b) without renumbering existing boxes. | Highest-surface subject (4 phases). Documenting the trigger now avoids an ad-hoc mid-run reshuffle. | `tasks/unshackled/07-extensions.md` |
| D003 | 2026-06-02 | MCP posture for the run | **Start without MCP servers.** No cargo-wrapper MCPs. Revisit the read-only OpenAI Docs MCP when provider work (subject 03) begins, and only after auditing the server. | `docs/14` §2 recommendation; shell + cargo + LSP cover the workflow and every added MCP is untrusted third-party code on the workspace. | 00.6 |
| D004 | 2026-06-02 | Dev-tool versions pinned to MSRV 1.82 | Install `cargo-nextest 0.9.92`, `cargo-machete 0.7.0`, `cargo-insta 1.47.2`; not the latest. | Latest `cargo-nextest` needs rustc 1.91 and latest `cargo-machete` needs the `edition2024` cargo feature; both exceed the pinned 1.82 toolchain. `nextest 0.9.97-b.2` (the newest 1.82-compatible) segfaults on Windows, so the last stable `0.9.92` is used. Dev tooling only — nothing ships. | 00.2 |
| D005 | 2026-06-02 | `cargo ci` shipped as per-step aliases | `.cargo/config.toml` defines `ci-fmt`/`ci-lint`/`ci-test`/`ci-check` rather than a single `cargo ci`. | Cargo aliases cannot chain multiple subcommands, and a one-shot runner would need an `xtask`/`cargo-make` crate — adding a 15th member conflicts with box 01.1's "14 crates" gate. CONTRIBUTING documents running the four as the local gate. | 01.4 |
| D006 | 2026-06-02 | `deny.toml` wildcards = warn | Internal `{ path = ... }` members read as wildcard deps; `allow-wildcard-paths` does not exempt publishable crates, so `bans.wildcards` is set to `warn` (not `deny`) for now. | Keeps `cargo deny check` green without prematurely deciding the crates.io publish posture (and `publish = false` on every member). Tighten to `deny` + `publish=false` at release (subject 09). | 01.7 |
| D007 | 2026-06-02 | Pin `tempfile 3.14.0` / `getrandom 0.2.15` | Lock `insta`'s transitive `tempfile` to 3.14.0 and `getrandom` to 0.2.15. | `tempfile` ≥3.16 pulls `getrandom` ≥0.3, whose 0.4.x manifest requires the unstable `edition2024` cargo feature and fails to parse under the pinned 1.82 toolchain. Dev/test-only deps. | 01.9 |
| D008 | 2026-06-02 | `ToolUseId` wraps `String`, not `Uuid` | Box 02.1 lists `ToolUseId` among the UUID newtypes; instead it wraps the opaque provider-assigned correlation string. `SessionId`/`TurnId`/`MessageId` still wrap `Uuid`. | A tool-call id must match the token a provider emits and expects back (`docs/04` `ModelEvent::ToolCall { id: String }`); minting our own UUID would force a brittle string↔uuid map in every adapter. Still a distinct newtype, satisfying 02.1's intent. | 02.1, 02.3 |
| D009 | 2026-06-02 | First official hosted provider = OpenAI API | Ship the official **OpenAI** public API as the first hosted provider (03.7). One OpenAI-compatible adapter serves both the local server and official OpenAI; only base URL, auth, and source type differ. | OpenAI's Chat Completions API is fully public and documented (<https://platform.openai.com/docs/api-reference/chat>), satisfies ADR-0004, and reuses the local adapter. Provenance noted in `openai.rs` and CONTRIBUTING. | 03.7, 03.13 |
| D010 | 2026-06-02 | Pin provider-runtime transitives to MSRV 1.82 | Lock `hyper-rustls 0.27.5`, `idna_adapter 1.2.0`, and the `getrandom@0.3` line to `0.3.1`. | `reqwest`/`wiremock` otherwise resolve `hyper-rustls`≥0.27.9 (rustc 1.85), `idna_adapter` 1.2.2 and `getrandom` 0.3.4→`wasip2`→`wit-bindgen 0.57` (all need `edition2024`), which fail under 1.82 and break `cargo deny` manifest parsing. | 03.6, 03.12 |
| D011 | 2026-06-02 | deny: allow CDLA-Permissive-2.0, ignore RUSTSEC-2025-0134 | Add `CDLA-Permissive-2.0` to the license allow-list and ignore advisory `RUSTSEC-2025-0134`. | CDLA-Permissive-2.0 is `webpki-roots`' CA-data license (needed for TLS to official APIs), a permissive data license. RUSTSEC-2025-0134 marks transitive `rustls-pemfile` *unmaintained* (no vulnerability); we do not control it directly. | 03.12 |
| D012 | 2026-06-02 | windows-gnu `ring` test crash is environmental | Verify locally with per-crate `cargo test -p <crate>`; treat aggregate `cargo test --workspace` / `nextest` as CI's job. | On the local `x86_64-pc-windows-gnu` toolchain, `ring` (via `reqwest` rustls-tls) crashes the CLI bin test binary under the aggregate runner. Every per-crate suite passes; CI runs MSVC where the crash does not occur. Not a code defect. | 03 |

---

## 5. Master progress tracker

> A subject is `DONE` when every box in its file is `[x]`. Abandoned boxes use
> the §4 struck-and-done format; an abandoned subject still gets `[x]` in Done.
> §5 is fully ticked when every Done cell is `[x]` and every Status is `DONE` or
> `ABANDONED`.
>
> **Owners** are one of: `agent`, `release-engineer`, `product-owner`,
> `tech-lead`, `domain-sme`. Owner-summary names the specific roles present
> (never a generic "human"). Human-owned boxes are mirrored into
> `tasks/unshackled/manual-actions.md` and kept in sync.

| Done | # | File | Status | Owner summary | Human actions mirrored? |
|---|---|---|---|---|---|
| [x] | 00 | `tasks/unshackled/00-bootstrap-tooling.md` | DONE | agent: 6; tech-lead: 2; release-engineer: 1 | yes |
| [x] | 01 | `tasks/unshackled/01-foundation.md` | DONE | agent: 10 | n/a |
| [x] | 02 | `tasks/unshackled/02-core-config-store.md` | DONE | agent: 15 | n/a |
| [x] | 03 | `tasks/unshackled/03-provider-runtime.md` | DONE | agent: 12; tech-lead: 1; release-engineer: 1 | yes |
| [x] | 04 | `tasks/unshackled/04-tools-and-sandbox.md` | DONE | agent: 14 | n/a |
| [ ] | 05 | `tasks/unshackled/05-session-and-recovery.md` | TODO | agent: 13 | n/a |
| [ ] | 06 | `tasks/unshackled/06-harness-core.md` | TODO | agent: 17; tech-lead: 1 | yes |
| [ ] | 07 | `tasks/unshackled/07-extensions.md` | TODO | agent: 15; tech-lead: 1 | yes |
| [ ] | 08 | `tasks/unshackled/08-terminal-ui.md` | TODO | agent: 11 | n/a |
| [ ] | 09 | `tasks/unshackled/09-evals-and-release.md` | TODO | agent: 9; release-engineer: 3; tech-lead: 1 | yes |

---

## 6. Cross-cutting principles

> Apply to every subject. Violations are blockers, not nits. 1–11 are tuned to
> this Rust plan; the C#/.NET defaults from the template are dropped.

1. **KISS · YAGNI · CLEAN · SOLID · DRY** (in that order when they conflict).
2. **MSRV 1.82 / edition 2021, inherited from the workspace.** No API or syntax
   newer than 1.82. Raising MSRV is its own commit with a `CHANGELOG.md` note
   (`docs/13` §1).
3. **Every external dependency is exact-pinned (`=x.y.z`) in
   `[workspace.dependencies]`** and referenced by members with
   `dep = { workspace = true }`. No second pin in a member crate. New deps must
   pass `deny.toml` and `cargo audit` before use (`docs/13` §1–2).
4. **No new build warnings.** `cargo fmt --check` and
   `cargo clippy --workspace --all-targets -- -D warnings` are gates. Fix the
   cause; any `#[allow(...)]` carries a one-line reason (`docs/13` §9).
5. **Keep crates narrow and dependency direction one-way** (ADR-0001). `core`
   depends on nothing internal; nothing depends on the CLI; no cycles.
   Provider code only in `unshackled-llm`; local side effects only in
   `unshackled-tools`; permission decisions only in `unshackled-sandbox`
   (`docs/13` §2; `docs/02`).
6. **Type-driven design.** Newtypes for IDs/units; `enum` over struct-with-kind;
   parse-don't-validate at boundaries; `#[non_exhaustive]` on growable public
   enums/errors; generate tool/provider JSON schemas from typed structs
   (`schemars`), never hand-written (`docs/13` §3).
7. **Typed errors at every crate boundary with `thiserror`**
   (`ConfigError`, `ProviderError`, `ToolError`, `PermissionError`,
   `HarnessError`, `StoreError`, …). The CLI MAY use `anyhow` at the top only;
   libraries MUST NOT leak `anyhow` across a public boundary. Every fallible
   public fn has a `# Errors` doc (`docs/13` §4).
8. **No `unwrap`/`expect`/`panic!`/`todo!`/`unimplemented!` on runtime paths**
   in library crates (allowed in `#[cfg(test)]`; `unreachable!` only with a
   proof comment). Each library crate starts `#![forbid(unsafe_code)]`; `unsafe`
   needs an ADR + `// SAFETY:` (`docs/13` §8, §12).
9. **Async discipline.** Never block the executor; never hold a lock across
   `.await`; prefer `std::sync::Mutex` for short sections; make long ops
   cancellable via `CancellationToken`/`select!` and leave state consistent
   (temp-then-rename, no half-written files); object-safe provider/tool traits
   via `async-trait`; await or deliberately detach every `JoinHandle`
   (`docs/13` §5–6).
10. **Cross-platform from the start** (ADR-0007). Windows, Linux, macOS are
    equal tier-1; behavior parity is a release requirement. Use `Path`/`PathBuf`
    + `.join()`; canonicalize + normalized `starts_with` for workspace
    containment (mind Windows `\\?\`, case-insensitivity, 8.3, ADS); per-OS
    shell/command classification; argument lists not shell strings; `#[cfg]`
    branches must be tested on their OS (CI runs all three) (`docs/13` §7).
11. **Security-by-construction.** Secrets live in a wrapper whose
    `Debug`/`Display` prints `***`; raw value only via explicit `expose()`.
    Redact before persistence and before logging, not after. Treat model
    output, tool input, and provider output as untrusted. The model and the
    harness MUST NOT bypass the permission engine. `bypass` profile is never the
    default and is always shown in footer/status (`docs/07`, `docs/13` §8,
    `docs/05` safety invariants).
12. **Clean-room provenance is blocking** (`docs/00`, ADR-0004/0005). All code,
    prompts, tests, identifiers, UI copy original to this repo. Official public
    APIs / local servers only — no private/undocumented endpoint adapters. Cite
    public API docs in a PR provenance note; when the read-only reference at
    `D:\repos\unshackled` is consulted, add the high-level provenance note from
    `docs/00`. No vendor branding as product identity; no prohibited framing.
13. **Project files are the harness source of truth** (ADR-0003). `brief.md`
    and `PROGRESS.md` are authoritative and user-editable; the next run treats
    the edited file as truth. Transcripts are supporting context only.
14. **Spec at the contract level, not the SDK level.** State requirements as
    testable contracts on observable behaviour (`docs/13` §3, template lesson).
    The smallest verifiable artefact (test name / file path / log line) goes in
    each box.
15. **Tests prove contracts; coverage is a smell-detector, not a goal.** Prefer
    hand-written fakes over mocking frameworks; tests deterministic + offline by
    default (`tempfile`, never the real home/config); live tests opt-in behind
    `UNSHACKLED_LIVE_TESTS`; cover at least one failure path; snapshots reviewed
    deliberately (`docs/13` §10, `docs/08`).
16. **Every box has an owner (§5 enum) and a stable ID** `<subject-id>.<box-number>`.
    Lessons land in `tasks/unshackled/lessons.md` as they happen.
17. **Code is plan-agnostic** (§6.11 template). Comments, test names, commit
    messages, identifiers must read as permanent project artefacts — never
    reference this plan, subjects, box IDs, or `D###`. Put the *why* in the
    comment. Commit messages for harness-produced commits follow the spec's
    `harness: <step description>` shape only inside the product's own runtime,
    not for plan commits.

---

## 7. Gate review (run last; tick everything)

> Run only when §5 is fully ticked. §7 is the engineering gate; §8 is human
> acceptance after it.

- [ ] All §5 subjects `DONE` (or explicitly `ABANDONED` with a §4 row).
- [ ] `cargo check --workspace` passes; `cargo build --workspace` passes on all three OSes via CI.
- [ ] `cargo fmt --check` clean; `cargo clippy --workspace --all-targets -- -D warnings` clean.
- [ ] `cargo test --workspace` (or `cargo nextest run --workspace`) green on Windows, Ubuntu, macOS.
- [ ] `cargo deny check` and `cargo audit` pass; `cargo machete` reports no unused deps.
- [ ] All `docs/08` "Required MVP Tests" exist and pass (config, provider, tools, harness, recovery, context, store).
- [ ] Golden-task eval suite runs and reports a task success rate; scorecard fields recorded.
- [ ] §6 cross-cutting principles reviewed; no `unwrap`/`expect`/`panic!` on library runtime paths; no `anyhow` leaking from a library; no secret reachable via `Debug`/logs; no lock held across `.await`.
- [ ] Behavior parity verified across the three tier-1 platforms (ADR-0007); `#[cfg]` branches tested on their OS.
- [ ] **Clean-room audit** (`docs/00`, `docs/09`): text scan for prohibited framing terms ("source-map", "leaked", "free build", "fork of", "private endpoint", vendor names as identity, browser-cookie auth); no private/undocumented endpoint adapter; prompts/tests/identifiers original; provenance notes present where the reference was consulted.
- [ ] **Personal-absolute-path scan** scoped to shipped artifacts: scan `crates/`, install scripts, and public docs for personal absolute paths and assert zero hits. **Exclude** `tasks/` (disposable, deleted at go-live) and the single documented read-only-reference path `D:\repos\unshackled` in `AGENTS.md`, which ADR-0005 / `docs/00` explicitly allow. A hit anywhere a user would receive it (binary, archive, shipped doc) is a blocker; the documented reference mention is not.
- [ ] Shipped artefacts plan-agnostic — grep the repo **excluding `tasks/`** for box IDs (`\b\d\d\.\d+\b`), decision IDs (`\bD\d{3}\b`), `tasks/unshackled/`, `Unshackled-Plan.md`, `\bslices?\b`; zero hits after triage (version strings can match box-ID pattern).
- [ ] Commit messages plan-agnostic — `git log <base>..HEAD` mentions no box IDs, `D###`, `tasks/unshackled/`, `Unshackled-Plan.md`, or `slice`/`slices` (same triage).
- [ ] `tasks/unshackled/manual-actions.md` — every human-owned box resolved or explicitly deferred with rationale.
- [ ] `docs/09` Public-Alpha Criteria met: clean-room audit complete, no private endpoints, no prohibited framing, tests green, TUI usable, harness completes a small repo task, docs explain provider setup, security model documented; installers build; release archives contain license files.
- [ ] `tasks/unshackled/lessons.md` reconciled; lasting lessons migrated to permanent `tasks/lessons.md` (create if missing) before `tasks/unshackled/` is deleted.
- [ ] Plan handed to reviewer for §8 sign-off.

---

## 8. Acceptance / sign-off

> Filled by the user/reviewer after §7 passes. Sign-off is acceptance, not a
> spec amendment.

| Date | Reviewer | Result | Notes |
|---|---|---|---|
| | | | |
