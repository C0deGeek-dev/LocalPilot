# Development Tooling

What to use while building LocalPilot: AI-assistant capabilities already in the
loop, optional MCP servers, cargo tooling worth installing, and repo
conveniences. This is a developer-environment doc — none of it ships in the
product, and none of it is a clean-room source (these are generic dev tools).

Status tags: **present** = already in the repo; **recommended** = install/adopt;
**optional** = nice-to-have.

---

## 1. Assistant capabilities already available

These need no install — they exist in the assistant session driving this repo.
Exact availability differs by assistant and version, so confirm in your session.

### Code navigation: LSP / rust-analyzer (when exposed)

If the assistant exposes an **LSP tool** wired to `rust-analyzer` (Claude Code
does; not every Codex session does), use it instead of grepping for symbols:
go-to-definition, find-references, hover (types/docs), workspace-symbol search,
go-to-implementation, and call hierarchy. It is the fastest way to navigate the
crate graph and to check who implements the provider/tool traits before changing
them.

When no LSP tool is available, fall back to `rg` for symbol search, `cargo
check` / `cargo clippy` for compiler diagnostics, and `cargo doc --open` for API
surface. The trait-impl question is then answered by searching for `impl …
for`.

### Skills

Slash-invoked skills that map cleanly onto this project's workflow:

- **`/code-review`** — review the current diff/branch for bugs and cleanups at a
  chosen effort level; `/code-review ultra` runs a deep multi-agent cloud review.
  Use before every PR. Pairs with the §14 checklist in
  [13-rust-best-practices.md](13-rust-best-practices.md).
- **`/simplify`** — quality-only pass for reuse/simplification/efficiency on the
  changed code. Good after a feature lands and before review.
- **`cleanup-audit`** *(c0degeek plugin skill, not a built-in slash command)* —
  a whole-repo teardown review: finds dead/duplicate/over-engineered/legacy code,
  redundant queries, unused dependencies, and doc/test drift, then emits a triaged,
  report-only cleanup plan. This is the periodic full-repo sweep that `/code-review`
  and `/simplify` (diff-scoped) and Captain Hindsight (per-subject close-out) leave
  uncovered — run it deliberately, not every PR. It should lean on `cargo machete`,
  `clippy`, and `cargo deny`/`audit` (§3) rather than re-deriving what they report,
  and stay clean-room (recommend removal/consolidation only, never importing
  external code).
- **`/security-review`** — security pass on pending branch changes. Run it on
  anything touching `localpilot-sandbox`, `run_shell`, path handling, or secret
  redaction.
- **`/verify`** and **`/run`** — actually launch the CLI and observe behavior
  (e.g. `localpilot doctor`, the Milestone-1 harness commands) rather than
  trusting that tests imply the binary works.
- **`/init`** — (re)generate a `CLAUDE.md` of codebase conventions if/when we add
  one for the assistant.
- **`/fewer-permission-prompts`** — scans transcripts and proposes a project
  allowlist for routine read-only commands. Run once cargo commands become
  repetitive (see §4).
- **`/loop`**, **`/schedule`** — drive long or recurring tasks (e.g. watch a CI
  run, repeat an eval pass). Use sparingly.

The `caveman` family (`/caveman`, `/caveman-commit`, `/caveman-review`,
`/caveman-compress`, `cavecrew` subagents) is for token-efficient
communication and compressed delegation; orthogonal to Rust, useful on long
sessions.

## 2. MCP servers (optional)

Add an MCP server only if it removes real friction, and treat each as untrusted
third-party code that runs on this workspace. Verify the source before enabling.

### Rust cargo-wrapper MCPs — document, do not adopt

Servers like **`rust-analyzer-mcp`**, **`rust-mcp-server`**, and **`cargo-mcp`**
mostly re-expose what the shell and an LSP tool already do (run cargo; query
rust-analyzer). They add a third-party process that runs local commands for
marginal gain. **Do not install them by default.** Reach for one only if the
assistant lacks both shell and LSP access.

### MCPs worth considering (after audit)

- **OpenAI Docs MCP** — read-only access to official OpenAI API docs. Useful when
  implementing the OpenAI provider adapter (ADR-0004 requires official-docs
  provenance). Read-only, low risk.
- **GitHub MCP / app** — PRs, CI, issues. Often unnecessary: a session may
  already have `gh`/GitHub tooling. Add only if yours does not.
- **Context7 / crates-docs MCP** — current crate docs on demand. Optional, and
  only after auditing the server. For now, web search to `docs.rs` / `crates.io`
  is enough.

### Setup (both assistants)

MCP config differs per assistant; keep it project-scoped and reviewable where
possible:

```powershell
# Claude Code — writes to project .mcp.json
claude mcp add --scope project <name> -- <command-to-launch-server>

# Codex — `codex mcp add`, or edit ~/.codex/config.toml ([mcp_servers] table)
codex mcp add <name> -- <command-to-launch-server>
```

Recommendation: **start without MCP servers.** Shell + cargo (+ LSP where
available) cover the workflow. Add the OpenAI Docs MCP when provider work begins;
revisit the rest only when a concrete gap appears.

## 3. Cargo tooling to install

Install per-developer with `cargo install <tool>` (or `cargo binstall` for
prebuilt binaries). Each maps to an existing project need.

| Tool | Status | Why for this repo |
| --- | --- | --- |
| `cargo-nextest` | recommended | Faster, better test UX; matches the test runner referenced in [13](13-rust-best-practices.md) §10. `cargo nextest run --workspace`. |
| `cargo-insta` | recommended | Snapshot tests for CLI help, errors, TUI render, generated prompts — exactly the snapshot layer in [08-testing.md](08-testing.md). `cargo insta review`. |
| `cargo-deny` | present (`deny.toml`) | License/advisory/ban/source gate. `cargo deny check`. |
| `cargo-audit` | recommended | RustSec advisory scan of `Cargo.lock`. Run before release. |
| `cargo-machete` | recommended | Find unused dependencies; keeps the pinned workspace table honest. |
| `cargo-hack` | optional | Feature-matrix and MSRV checks (`--rust-version`), so the 1.82 floor and minimal feature sets stay real. |
| `cargo-llvm-cov` | optional | Coverage for the eval/test suites; track over time per [08-testing.md](08-testing.md). |
| `bacon` | optional | Background `check`/`clippy`/`test` watcher for a tight local loop. |
| `typos-cli` | optional | Catch typos in docs/code/identifiers in CI. |
| `cargo-semver-checks` | later | Catch accidental breaking changes once crates expose stable public APIs. |
| `cargo-mutants` | later | Mutation testing to find weak assertions in the highest-stakes code: rule engine, permission policy, redaction, and parsers. |

## 4. Repo conveniences worth adding

These reduce friction and are **not yet present** — propose in their own small
PRs:

- **Cargo aliases** (`.cargo/config.toml`) for the CI quartet, so `cargo ci`
  runs fmt-check + clippy-deny + test + check locally exactly as CI does. Lowers
  the chance of a green-local / red-CI surprise.
- **`.editorconfig`** — enforce `max_width`-friendly settings, final newline, and
  LF for tracked text files across editors (supports the cross-platform line-ending
  rule in [13](13-rust-best-practices.md) §7).
- **Workspace `[workspace.lints]` table** — centralize the clippy policy from
  [13](13-rust-best-practices.md) §9/§12 (`unwrap_used`, `expect_used`,
  `dbg_macro`, `unsafe_code`) once it stabilizes, instead of per-crate attrs.
- **Git pre-commit hook** (or `cargo-husky`) running `cargo fmt --check` and a
  fast `cargo clippy` so style failures are caught before push, not in CI.
- **`.claude/settings.json` allowlist** for routine `cargo`/`git` read commands,
  generated via `/fewer-permission-prompts`, to cut approval prompts during long
  build sessions.
- **`cargo deny` / `cargo audit` step in CI** — the testing doc lists them as
  optional; promote to a non-blocking (then blocking) CI job before v1 release.

### Supply-chain posture (present)

The CI workflow enforces three guards beyond `cargo deny`/`audit`/`machete`:

- **Lockfile guard** — a pull request that changes `Cargo.lock` without any
  `Cargo.toml` change fails: dependency moves come from manifest decisions,
  never silent drift.
- **Minimum release age** — a newly adopted crate version must be at least 14
  days past its crates.io publish date at adoption (policy in `deny.toml`;
  verified at review on the manifest change the lockfile guard forces).
- **Build-script audit** — a new dependency that introduces a `build.rs` or a
  proc-macro gets its build script read at review before adoption; build
  scripts execute arbitrary code at compile time.
- **Install smoke test** — CI installs the CLI from the locked graph into a
  clean root (`cargo install --path crates/localpilot-cli --locked`) and runs
  the produced binary, so "builds in-tree but does not install" cannot ship.

## 5. Project skills (portable across Claude and Codex)

§1 lists the assistant's *built-in* skills. This section is about **skills we
author into the repo** to encode LocalPilot's own procedures.

**The format is shared.** A skill is a directory with a `SKILL.md` file whose
frontmatter has `name` and `description`; the body (and optional scripts /
reference files) loads on demand only when the description matches the task
(progressive disclosure). Both Claude Code and Codex support this `SKILL.md`
format, so one authored skill body works in both tools — only the discovery
directory differs:

- **Claude Code:** `.claude/skills/<name>/SKILL.md` (project-scoped, committed)
  or `~/.claude/skills/` (personal). Picked up live, no restart.
- **Codex:** `.agents/skills/<name>/SKILL.md` (repo, also searched in parents and
  at repo root), `$HOME/.agents/skills` (user), `/etc/codex/skills` (admin).

Because the discovery dirs differ, **do not put the only copy in one
assistant's folder.** Make `.agents/skills/<name>/SKILL.md` the canonical copy,
then give Claude a tiny `.claude/skills/<name>/SKILL.md` stub that points to the
canonical file. Symlinks work on POSIX but are painful on Windows (ADR-0007
keeps Windows tier-1), so prefer a committed stub over a symlink.

**Trust boundary.** Checked-in skills are executable instructions, and on Claude
project skills can carry an `allowed-tools` list that changes permission behavior
once the repo is trusted. Before trusting checked-in skills, review their
`SKILL.md` and any bundled scripts. Do not grant broad tool allowlists in repo
skills; keep permissions in user-local config unless the rule is narrow,
auditable, and project-specific. Treat a PR that adds or edits a skill (or its
scripts/allowlist) as a security-relevant change and review it as one.

Pick the right lever:

- **Always-on, durable rules** → instruction files, not skills. Codex reads
  `AGENTS.md` (present); Claude reads `CLAUDE.md` (not present yet — worth adding
  a short one that points at `AGENTS.md` and the docs). These load every turn.
- **On-demand procedures** → skills. They cost nothing until their description
  matches, so they suit multi-step recipes used occasionally.

**Do not** wrap generic Rust knowledge in a skill — the model already has it,
and [13-rust-best-practices.md](13-rust-best-practices.md) is its durable home.
Skills earn their place only when they encode *this repo's* non-obvious steps.
Keep the set small; every skill's description competes for the model's
attention.

Recommended skills to author (each maps to an existing spec):

| Skill | Encodes | Why repo-specific |
| --- | --- | --- |
| `clean-room-guard` | [00-clean-room.md](00-clean-room.md), ADR-0004/0005 | How to use the documented read-only reference (see `AGENTS.md`) without copying; when a provenance note is required; what is prohibited (prompts, identifiers, structure, UI copy, branding). Easy to get wrong, unique to this project. |
| `implement-harness-step` | [06-harness-spec.md](06-harness-spec.md), [03-implementation-plan.md](03-implementation-plan.md) | The core product loop: `brief.md` / `PROGRESS.md` contracts, rule verdicts, attempt limits, progress update, commit policy. Central and easy to do ad hoc. |
| `add-tool` | [05-tool-system.md](05-tool-system.md) | Implement the `Tool` trait, generate JSON schema, register, route through the permission engine (never bypass), sandbox policy, required allow/deny tests. |
| `add-provider` | [04-provider-contract.md](04-provider-contract.md) | Where a provider impl lives (`localpilot-llm` module, behind the trait), quota metadata, stream-event model, required tests (text/tool/stream/malformed/quota), provenance note from public API docs. |
| `add-mcp-integration` | [02-architecture.md](02-architecture.md) §`localpilot-mcp` | MCP is v1 scope. Forces MCP tools/resources through the *same* permission and redaction pipeline as builtin tools — not a side channel. |
| `write-golden-eval` | [08-testing.md](08-testing.md) §Golden-Task Evals | Evals are required; this prevents ad hoc benchmark tasks and copied fixtures, and records the per-task scorecard fields. |
| `add-tui-view` *(opt)* | ADR-0006, [02-architecture.md](02-architecture.md) §`localpilot-tui` | Ratatui/crossterm view with `TestBackend` snapshot expectations and cross-platform terminal constraints. |
| `author-adr` *(opt)* | [10-decisions.md](10-decisions.md) | Append an ADR in the exact house format (newest on top, status, reason bullets). |
| `plan-large-task` | §7 below | Tier the planning ceremony: in-session `EnterPlanMode` for small tasks; a bundled multi-slice plan template (`tasks/<Name>-Plan.md`) with decision log, resume-safe checkpoints, and Captain Hindsight for large ones. Present in the repo. |

Keep skill bodies short and link out to the spec rather than restating it. The
skill's job is to route the agent to the contract and list the must-pass tests,
not to duplicate the doc.

Suggested authoring order: `clean-room-guard`, `implement-harness-step`,
`add-tool`, `add-provider`, `add-mcp-integration`, `write-golden-eval`.

## 6. Quick reference

```powershell
# Local gate (mirror CI)
cargo fmt --check
cargo clippy --workspace --all-targets -- -D warnings
cargo nextest run --workspace      # or: cargo test --workspace
cargo check --workspace
cargo build -p localpilot --features tui
cargo clippy -p localpilot --features tui --all-targets -- -D warnings

# Hygiene (pre-release)
cargo deny check
cargo audit
cargo machete

# Snapshot review after a render/prompt change
cargo insta review
```

## 7. Build-process planning (tiered)

How an assistant plans *its own work on this repo* — distinct from the product's
`localpilot harness plan` command, which emits the runtime `brief.md` /
`PROGRESS.md` ([06-harness-spec.md](06-harness-spec.md)). Never name a
build-plan file `PROGRESS.md` or `brief.md`; those names belong to the product
runtime.

The `plan-large-task` skill encodes the trigger and names the template source;
this section is its durable, always-loaded summary.

**Tier S (small)** — 1-2 files, single session, no new crate. Plan in-session
with `EnterPlanMode`, implement, then `/code-review` + `/simplify`. No `tasks/`
files. This is the default; when unsure, it is Tier S.

**Tier L (large)** — use the canonical plan template (the `plan-from-template`
skill in the c0degeek-ai plugin/repo), copied to `tasks/<Name>-Plan.md` with
the repo
[overrides file](../.agents/skills/plan-large-task/plan-template-overrides.md)
spliced in, if **any** hold:

- spans 3+ crates, or needs a new crate;
- likely to outlast one session / survive a context-window reset;
- touches a non-negotiable surface (sandbox, permission engine, secret
  redaction, provider trait, tool trait);
- needs a new ADR in [10-decisions.md](10-decisions.md).

Tier L mechanics: subjects (5-8) with stable box IDs, an append-only decision
log, resume-safe checkpoints (update plan → run gate → commit → push), and a
**Captain Hindsight** review at each subject close (not per box; for Tier S the
`/code-review` + `/simplify` pass is the lighter equivalent).

Gate per checkpoint is the four-command gate in §6 (fmt/clippy/test/check);
hygiene (`machete` on dep change, `deny`/`audit` before a release milestone) is
not per-checkpoint.

**Documentation ships with the change (both tiers).** Every change runs a
*documentation-impact review*: identify which docs it touches — route via the
ownership map in [`README.md`](README.md) and `CLAUDE.md` ("## The specs") —
and update them in the same checkpoint (Tier L) or the same PR (Tier S), or
state `n/a` with a one-line reason. The trigger is broad: behaviour,
architecture, commands, usage, configuration, setup, troubleshooting, or
developer workflow. Scope is every doc a user *or* contributor relies on, not
README alone — the numbered spec set, the topic docs
(`install`/`configuration`/`providers`/`mcp`/`extending`), `README.md`,
`docs/wiki/` guides (Getting Started / How-Tos / Examples / Troubleshooting),
and `CHANGELOG.md` for user-facing changes. Durable architecture decisions
still graduate to an ADR in [`10-decisions.md`](10-decisions.md).

Two rules keep `tasks/` from leaking into the product: the folder is
**disposable** (deleted before v1) so shipped code, commits, and identifiers
must be plan-agnostic — no box/decision IDs or plan filenames; and a decision
that is a durable architecture call is **promoted to an ADR** in
[10-decisions.md](10-decisions.md), since the decision-log row dies with the
folder. Commit `tasks/` while live (resume-safety needs it pushed); the
template and its Captain Hindsight prompt are the author's own original work, so
no third-party provenance attaches — but everything the plan produces stays
clean-room compliant ([00-clean-room.md](00-clean-room.md)).
