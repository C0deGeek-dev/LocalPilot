# LocalMind integration contract (forward-looking)

> Status: forward integration contract for the next development track. It records
> how Unshackled's learning surfaces evolve toward **LocalMind**, the extracted,
> host-neutral local learning engine. It changes no shipped behavior in this
> alpha and does not reopen completed work; it is the handoff that the next track
> implements.

## Why

Learning (candidate lessons, review queues, memory promotion, retrieval, skill
generation and maintenance, audit, self-improvement) is a coherent capability
that should not be re-implemented inside every coding agent. LocalMind owns that
core as a standalone engine usable by Claude Code, OpenAI Codex, and generic
transcript workflows. **Unshackled is LocalMind's first native host**: the
learning feature is built into the Unshackled binary and its commands — users do
not install LocalMind separately.

## What ships in this alpha (bridge surfaces)

The current crates are deliberately small **alpha bridge surfaces**, not the
final learning system:

- `unshackled-memory` — a flat, redacted, inspectable memory store with
  relevance-ranked retrieval, a token cap/threshold, and `memory` commands.
- `unshackled-skills` — a skill manifest/loader plus disabled suggestion drafts
  with a per-pattern cooldown.

These should be **wrapped or replaced by LocalMind core through an adapter**, not
expanded into a separate rich learning system.

## Ownership boundary

- **LocalMind core is host-neutral and must not depend on Unshackled.** It owns
  session closeout, redaction-on-import, summarization, candidate-lesson
  extraction, the terminal review queue, accepted-lesson persistence
  (Markdown-backed memory with a SQLite audit trail), search, agent-ready context
  export, and `SKILL.md` draft emission.
- **Unshackled owns the native host role**: capturing the evidence below,
  enforcing permissions and redaction at capture time, the TUI/footer/review
  surfaces, and the built-in commands. Unshackled may depend on LocalMind core
  through an adapter crate (a future `unshackled-localmind`); LocalMind core
  never depends back.

## Signal mapping (Unshackled → LocalMind core)

| Unshackled signal (already produced) | Where today | LocalMind core input |
| --- | --- | --- |
| Session transcript bundle | `unshackled-store` JSONL + `export_session` | imported, redacted session for summarization |
| Tool events (name, args, result, error) | `RuntimeEvent::ToolStarted/Finished`, `ContentBlock::ToolUse/ToolResult` | per-session tool trace for lesson extraction |
| Code diffs | per-step `git` commits (harness `resume`) | change evidence for a candidate lesson |
| Test output | `suite_green` / `test_command` result in the completion gate | pass/fail signal attached to a lesson |
| Commits | `harness: <step>` per-step commits, `StepTrace` | durable outcome anchor (commit hash) |
| Recovery events | `unshackled-recovery::RecoveryDiagnostic` | "frequent failure + fix" candidate lessons |
| Memory retrieval | `unshackled-memory::retrieve` | replaced by LocalMind retrieval/export |
| Review queue | (bridge: drafts created disabled) | LocalMind terminal review queue |
| Skill drafts | `unshackled-skills::SkillDraft` (disabled, cooldown) | LocalMind `SKILL.md` draft emission |

All capture stays redacted-before-persistence and inside the permission boundary;
LocalMind never bypasses either.

## How LocalMind is bundled

LocalMind is its own repository, vendored here as a git **submodule** at
`external/localmind` (pinned to a commit). The `unshackled-localmind` adapter
depends on the submodule's `localmind-core` and `localmind-store` crates by path,
so a single `cargo build` compiles everything into the Unshackled binary — no
separate install. LocalMind stays standalone and upstream; bumping the submodule
pointer pulls in new versions.

Working with the submodule:

```sh
git clone --recurse-submodules <repo>      # or, in an existing clone:
git submodule update --init --recursive
```

CI checks out submodules recursively. The adapter is a one-way edge: Unshackled
depends on LocalMind, never the reverse.

## Migration shape

1. **Done — closeout.** `unshackled-localmind::closeout_session` maps an
   Unshackled session transcript into LocalMind, runs summary + candidate-lesson
   extraction, and enqueues candidates for review.
2. **Done — CLI surface.** `unshackled learning` (behind the `learning` feature)
   exposes the loop: `closeout`, `review {list,show,accept,reject,defer,edit}`,
   `promote`, `search`, `skills {generate,list,show,export}`, `audit`. State is
   project-local under `.localmind/`.
3. **Done — retrieval + closeout trigger.** Relevant accepted memory is injected
   as a system message before each turn (REPL, `print`, and each harness step);
   the interactive REPL closes the session out into LocalMind on exit. Both are
   no-ops without the `learning` feature or matching memory.
4. Reduce `unshackled-memory` / `unshackled-skills` to thin shims over the adapter
   (or remove them) once parity is reached — keeping the feature built-in.
5. No separate install: the LocalMind crates are bundled into the binary.

```sh
# Build with the learning subsystem (release builds enable it):
cargo build -p unshackled --features learning
unshackled learning closeout --session <id>   # extract + enqueue lessons
unshackled learning review list               # inspect the queue
unshackled learning review accept <item-id>   # accept / reject / defer / edit
unshackled learning promote <item-id>         # promote to durable memory
unshackled learning search "<query>"          # search accepted memory
```

New rich-learning behavior (closeout, review, promotion, self-improvement) lands
in LocalMind, not by expanding these crates: learning is a host-neutral concern
that LocalMind owns.
