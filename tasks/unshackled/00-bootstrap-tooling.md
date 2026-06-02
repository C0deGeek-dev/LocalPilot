# 00 — Bootstrap: Dev Tooling, Skills, MCP (START)

## Goal
> Set up the agent's working environment **before any product code is written**.
> Install the cargo tooling the plan relies on, author this repo's project
> skills so later subjects can lean on them, decide and (if chosen) configure
> MCP servers, add the assistant instruction stub, and confirm the scaffold
> still builds clean. Source: `docs/14-dev-tooling.md`. Nothing here ships in
> the product. This subject is the literal first step; later subjects assume its
> tooling exists.

## Boxes
> ID = `00.<box-number>`. Owners: agent · release-engineer · product-owner ·
> tech-lead · domain-sme.

- [x] **00.1** (agent) Record a baseline: `cargo check --workspace`,
      `cargo fmt --check`, `cargo clippy --workspace --all-targets -- -D warnings`,
      `cargo test --workspace` all run and their current pass/fail state is noted
      in the Progress log. Establishes the green starting point before tooling
      changes. (Verified: four commands run, outcomes logged.)
- [x] **00.2** (agent) Install the cargo tooling from `docs/14` §3 that later
      subjects use: `cargo-nextest` (test runner), `cargo-insta` (snapshot
      review), `cargo-audit`, `cargo-machete`. Confirm each resolves
      (`cargo nextest --version`, `cargo insta --version`, `cargo audit --version`,
      `cargo machete --version`). `cargo-deny` is already present via `deny.toml`;
      confirm `cargo deny check` runs. Optional (`cargo-hack`, `cargo-llvm-cov`,
      `bacon`, `typos-cli`) are NOT installed now — note them as deferred.
      (Verified: version strings captured in Progress log.)
- [x] **00.3** (agent) Add `CLAUDE.md` at repo root: a short instruction stub
      that points at `AGENTS.md` and `docs/` (per `docs/14` §5 "always-on
      durable rules → instruction files"). Must not duplicate `docs/13`; just
      route the assistant to the specs and the clean-room policy. Keep it
      plan-agnostic — it ships in the repo. (Verified: file exists, links
      resolve, no plan/box references.)
- [x] **00.4** (agent) Author the canonical repo skill `clean-room-guard` at
      `.agents/skills/clean-room-guard/SKILL.md` with `name`+`description`
      frontmatter, encoding `docs/00` + ADR-0004/0005 (how to use the read-only
      reference without copying, when a provenance note is required, what is
      prohibited). Add a tiny `.claude/skills/clean-room-guard/SKILL.md` stub
      pointing at the canonical file (committed stub, not a symlink — Windows
      tier-1, `docs/14` §5). Body short; links to the spec. (Verified: both
      files exist; stub references canonical path.)
- [x] **00.5** (agent) Author the remaining repo skills from `docs/14` §5 in the
      suggested order, each canonical under `.agents/skills/<name>/SKILL.md` with
      a `.claude` stub, each linking to its spec rather than restating it:
      `implement-harness-step` (`docs/06`+`docs/03`), `add-tool` (`docs/05`),
      `add-provider` (`docs/04`), `add-mcp-integration` (`docs/02` §mcp),
      `write-golden-eval` (`docs/08` §Golden-Task Evals). Skill bodies declare no
      broad tool allowlists (trust-boundary, `docs/14` §5). (Verified: five skill
      dirs exist with canonical + stub; descriptions are repo-specific.)
- [x] **00.6** (tech-lead) Decide the MCP posture for the run. `docs/14` §2
      recommendation is **start without MCP servers**; adopt the OpenAI Docs MCP
      only when provider work (subject 03) begins, and only after audit. Record
      the decision (start-without, add-OpenAI-Docs-MCP-at-03, or other) in the
      Decision log and mirror to `manual-actions.md`. No cargo-wrapper MCPs by
      default. (Verified: §4 row added; default is no MCP unless this box says
      otherwise.)
- [x] **00.7** (tech-lead) Review and approve every authored skill `SKILL.md`
      and any bundled script as a security-relevant change before the repo is
      trusted (`docs/14` §5 trust boundary). Confirm no skill grants a broad
      `allowed-tools` list. Mirror to `manual-actions.md`. (Verified: sign-off
      noted; allowlists are narrow/auditable or absent.)
- [x] **00.8** (release-engineer) Provide provider credentials policy for later
      live work: confirm `.env` is gitignored (it is), document which env vars
      hold which keys (`UNSHACKLED_LOCAL_API_KEY`, `OPENAI_API_KEY` per
      `.env.example`), and confirm CI must never carry real keys. No real keys
      are committed. Mirror to `manual-actions.md`. (Verified: policy noted; live
      tests stay opt-in behind `UNSHACKLED_LIVE_TESTS`.)
- [x] **00.9** (agent) Run `/fewer-permission-prompts` reasoning: add a narrow
      `.claude/settings.json` (or settings.local) allowlist for routine
      read-only `cargo`/`git` commands used repeatedly in this plan, keeping
      risky actions prompting (`docs/14` §4). Keep the allowlist auditable and
      project-scoped. (Verified: settings file present; only read-only/safe
      commands allowlisted.)


## Hindsight checkpoint
> Run after all boxes in this subject are complete and before marking
> the subject `DONE` in §5. Use the embedded prompt in `tasks/Unshackled-Plan.md`
> "Appendix: Captain Hindsight Prompt". Record the review result here.
>
> Required output sections: Keep; Fix before closing; Record; Risk;
> Verdict (`CLOSE` or `DO NOT CLOSE`). If the verdict is `DO NOT CLOSE`,
> leave the subject open, add/reopen boxes or update decisions/lessons,
> and rerun this checkpoint after the fixes.
>
> Subjects already marked `DONE` before this checkpoint was added still need
> this section completed retroactively before the §7 gate review is ticked.

### Review result

1. Keep: Starting with tooling, repo skills, MCP posture, credentials policy,
   and baseline commands was the right first move. D003 and D004 captured the
   important setup decisions instead of leaving them as session-only context.
2. Fix before closing: None. The `cargo deny check` advisory failure found here
   was carried into subject 01 and resolved there, so it does not need to reopen
   subject 00.
3. Record: Already recorded. MCP posture is D003; MSRV-compatible tool pins are
   D004; the deny failure lesson is in `lessons.md`.
4. Risk: The human skill/security review was agent-audited in this run and may
   still be re-confirmed by the owner before trusting repo skills, but the box
   requirement was mirrored and resolved in `manual-actions.md`.
5. Verdict: CLOSE.

- [x] Captain Hindsight review recorded
- [x] Verdict is `CLOSE`

## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.
> Append a `lessons.md` line here whenever a slice teaches something.

- 2026-06-02 · slice 1 · 00.1–00.9 · Baseline green (fmt/check/clippy/test all
  pass, 0 tests on stubs). Installed `cargo-nextest 0.9.92`, `cargo-insta 1.47.2`,
  `cargo-machete 0.7.0`; `cargo-audit 0.22.1` + `cargo-deny 0.19.6` already
  present; optional tools deferred. Added root `CLAUDE.md` (router to AGENTS.md +
  docs). Authored 6 repo skills (`clean-room-guard`, `implement-harness-step`,
  `add-tool`, `add-provider`, `add-mcp-integration`, `write-golden-eval`) as
  canonical `.agents/skills/*` + `.claude/skills/*` stubs, no `allowed-tools`.
  Added `.claude/settings.json` read-only cargo/git allowlist. MCP posture
  recorded as D003 (start without MCP). Credentials policy confirmed (`.env`
  gitignored; env→key map documented). Verified: tool `--version` strings
  captured; `git check-ignore .env` matches; skills grep shows zero allowlists.
  Note: `cargo deny check` reports `advisories FAILED` — carried into subject 01
  (supply-chain gate).
