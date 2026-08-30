# Changelog

Notable changes per release. As of 1.0.0 the public CLI/config/provider surface
is SemVer-stable; the configuration schema stability policy is in
[docs/configuration.md](docs/configuration.md).

## Unreleased

## v5.0.0 - 2026-08-30

- Began a new public Git history under PolyForm Noncommercial 1.0.0. Versions
  through v4.0.0 remain available to existing recipients under their original
  MIT grants; commercial use of v5+ requires a separate written license.

## v4.0.0 - 2026-08-29

Coordinated LocalX release.

- **LocalPilot and LocalMind now share one exact embedding-server lease
  contract** (ADR-0176). Ownership is bound to the normalized configured
  endpoint and LocalBox's live server PID under a machine-global lock; legacy
  markers migrate only after that runtime state matches. The process-lifetime
  RAII guard covers errors and cancellation, user-started listeners remain
  untouched, stale client leases are pruned, and a detached LocalPilot-owned
  reaper waits when a standalone LocalMind client outlives the session so the
  final lease still stops only LocalPilot's server.

- **Skill catalogs now have visible entry boundaries without sacrificing the
  descriptions used to choose an install.** `skills available` and the relevant
  matches in `skills research` put identity and full description on separate
  lines and leave a blank line between entries. Terminal output reinforces names
  and install state with the configured semantic palette; pipes, `/skills`
  transcripts, and `NO_COLOR` remain escape-free and fully legible (LocalHub#150).

- **Retired recovery diagnostic kinds remain readable without obsolete loop
  detectors.** Historic `tool_call_loop` and `repeated_transient_error` records
  load through an `unknown` kind fallback while preserving attempts, health,
  and actions. The active repeated-error, no-progress, and stream guards are
  unchanged (LocalHub#137).

- **Swarm member failures use the existing `departed` lifecycle status.** The
  unused `mark_failed` API and `Failed` status have been removed. Historic
  snapshots with `state: "failed"` still load as departed members, including
  backup recovery; new snapshots write `departed` (LocalHub#136).

- **The inert `learning` Cargo feature has been removed.** LocalMind-backed
  learning remains part of the default binary; source installs, CI, nightly,
  and release builds no longer pass a no-op feature. Source-build commands that
  still name `--features tui,learning` must use `--features tui` instead
  (LocalHub#134).

- **Normal sessions now persist the provider metadata promised by the store and
  incognito contract.** Session open and provider switching cache a versioned,
  redacted snapshot of the provider's typed declaration under
  `.localpilot/providers/`, reusing an unchanged snapshot on the next run.
  Incognito sessions use the same path in memory and leave no provider file on
  disk (ADR-0160; LocalHub#115).

- **Folder-ingest previews and model-call budgets are now truthful.** A preview
  estimates the configured primary chunk embedding calls instead of serializing
  a permanent zero. `[ingest] max_model_calls = 0` now explicitly means
  unlimited; a nonzero value stops before call `limit + 1`, reports the used
  budget, and leaves the incomplete path retryable. Capped runs keep Markdown
  available in the LocalMind Docs index without letting that secondary embedding
  pass escape the cap (LocalHub#114).

- **A full context window is no longer misreported as an output cap.** Provider
  prompt usage now calibrates the bytes/4 estimate used by automatic compaction,
  adds a 5% content-mix cushion, and corrects the host's context gauge to the
  reported count. A length stop whose prompt plus output fills the declared
  window while output is far below its cap reports the actual numbers, compacts,
  and retries once without appending the chunked-write steer. If history is
  already minimal it stops honestly and suggests a new session or split task;
  genuine output-cap stops keep their existing smaller-write guidance
  (ADR-0161; LocalHub#83).

- **The LocalMind Graph panel now has a real, cancellable reindex action.** Its
  empty state no longer points at the nonexistent `localpilot learning graph
  reindex` command. Press `r` on Graph to run a permission-gated manual index in
  bounded background batches; progress and cancellation report exact remaining
  work, and the open panel refreshes in place. Missing-store, learning-disabled,
  and incognito sessions remain non-writing (ADR-0174).

- **Memory retrieval no longer loses the answer to the question's own phrasing.**
  Measured against a frozen human-labelled judgment set: **lexical recall 0.00 →
  0.75**, cross-cutting 0.13 → 0.30, paraphrase 0.74 → 0.80, with no class
  regressing.

  Two defects were masking each other, and neither alone was sufficient. The two
  memory stores were merged **by store** rather than by relevance — every global
  hit appended after every project hit — so with the caller capping at five, a
  global memory was unreachable whenever five project memories matched at all.
  And the term-coverage gate counted query terms equally, so a question's
  scaffolding (*know*, *about*) outvoted its subject (`process_dir`).

  Stores now merge by relevance, normalised per store because bm25 is not
  comparable across two indexes, with project precedence kept where it means
  something — a project memory still wins a duplicate. The gate now admits a body
  containing the query's **rarest** term regardless of count: that term carries
  the question's specificity, and rank still decides afterwards. Coverage also
  counts tokens rather than substrings, so `changed` is no longer credited for
  appearing inside *unchanged*.

  A stopword list was rejected: `use`, `type`, `match` and `test` are ordinary
  English and load-bearing in a technical corpus.

- **Memory injection is measured, and it works — bounded by retrieval quality**
  (ADR-0167). On 18 knowledge-dependent, exactly-gradeable tasks against a local
  model, injecting retrieved memory raised task success from **0.056 to 0.556**.

  The attribution is the finding rather than the headline. Where retrieval
  delivered a memory containing the answer, injection lifted success from **0.083
  to 0.833**; where it did not, injection changed **nothing at all**. The overall
  gain is carried entirely by the tasks retrieval got right, so **retrieval
  quality is the binding constraint on memory's whole value** — which makes the
  measured lexical recall of 0.00 expensive rather than academic.

  Narrow by construction: one store, one machine, one model, single-turn factual
  recall. A prior A/B on multi-step tool discipline saw injected context *lengthen*
  trajectories, a cost this task shape cannot observe.

- **Memory retrieval is measured against a frozen, human-labelled judgment set.**
  The scoring surface is judged-set aware: an id the retriever returns that nobody
  judged is `UNJUDGED` rather than wrong, precision is reported as bounds with
  judgment coverage beside it, and a judgment set is addressed by the SHA-256 of
  its qrels file so a set that changed under a measurement is detectable rather
  than assumed.

  `context_hits` is now exported — it was the only unexported half of a pair whose
  result type (`SearchHit`) was already public, and it is the function a
  measurement has to target because it is what decides what the model receives.

- **`localpilot learning keep <ID>...` — the reviewer's "no, this one is fine".**
  `freshness --apply` flags memory for review; until now nothing shipped could
  lift a flag. That was not merely a missing convenience: a flagged memory is
  **excluded from the queryless context primer**, so a false positive silently
  stopped being offered as background context, permanently, with keyword search
  unaffected the whole time.

  Ids that were not flagged are **reported, not counted as success** — an id that
  does not exist or was never flagged looks exactly like a win if it is only
  tallied.

  Found while judging a real freshness run: of 76 flags, a reviewer agreed with 7.
  Sixty-nine memories had dropped out of the primer, including much of a curated
  knowledge seed, and there was no supported way to put them back.

- **Lexical and dense span retrieval are compared with numbers, offline.** The
  dense arm reads precomputed vectors from a committed fixture, so the comparison
  needs no model server and cannot drift between runs — a comparison that depends
  on a live endpoint is one that silently stops happening. It is **evaluation
  scaffolding, not a shipped retriever**.

  On the frozen query set, the two are complementary rather than competing.
  Lexical answers exact identifiers at rank one and correctly returns nothing for
  queries with no answer. Dense answers paraphrase queries that lexical scores
  **zero** on, and lifts cross-session recall from 0.67 to 1.00 — but returns a
  full result set for every query, including the ones whose right answer is
  nothing.

  That last point is measured rather than assumed to be fatal: the worst
  similarity among answerable queries (0.59) sits clear of the best among
  unanswerable ones (0.48), so a similarity floor is viable on this corpus. What
  the floor should be is not answered here — calibrating a threshold on eight
  queries would be overfitting, and saying so is the finding.

- **Session-span search no longer matches on stopwords, and there is now a
  retrieval-quality measurement that says whether it returns the right spans.**

  Everything measured before this was coverage — span counts, index size, build
  time. None of it said whether a query returns what it should. The measurement
  reports recall, precision and MRR per query class, and it found a defect in the
  shipped path immediately: query terms were not filtered, so `"the"` alone
  matched most of a corpus and a paraphrase query scored **perfect recall while
  matching only on `one`, `the` and `of`**. The metric was reporting success for
  retrieval that was doing nothing.

  With common terms filtered, redaction-affected precision goes from 0.20 to
  1.00, lexical results are unchanged, and paraphrase recall reads its true value
  of **0.00** — which is what a keyword index does with words the span does not
  contain. That number is now pinned as the baseline any future dense retriever
  has to beat.

  Reports carry metrics and locators, never span text: evaluation artefacts are
  corpus-derived, and a report that quotes what it found is a transcript excerpt
  wearing the name of a test result.

- **The session span index has a published lifecycle contract.** Every
  transition — a session arriving, growing, forking, being deleted, a transcript
  rewritten or re-redacted, a chunking-contract change, an interrupted build,
  concurrent indexers — has a defined behaviour, an idempotent recovery, and
  telemetry that makes it visible. See `docs/localmind-integration.md`.

  Three properties are load-bearing. The index **never retains what its
  transcript no longer contains**, so re-redacting or rewriting a transcript
  removes the superseded spans rather than orphaning them. Indexing is
  **single-writer**, enforced rather than assumed, so two indexers converge
  instead of double-inserting. And the index is **rebuildable to identical
  locators**, not merely to an equal count — chunking is deterministic, so
  throwing the index away and rebuilding it does not rot locators anyone stored.

  **Nothing ages out:** there is no time-based expiry for sessions or spans.

- **Session transcript spans participate in `knowledge_search`, and resolve
  through `knowledge_fetch`.** A search now reaches every past session rather
  than the newest session's summary, and each span carries a locator that
  `knowledge_fetch` resolves back to its text.

  Spans are labelled **`session transcript`**, never presented as project facts —
  they are what was said and done, unreviewed. They carry **no budget reserve**:
  a guaranteed share is a claim that a source earns its space, and nothing has
  measured whether these do, so they compete in the shared pool where any
  stronger candidate outranks them.

  A locator carries its chunking-contract version and is checked against the
  content hash recorded at indexing. A locator from an older contract, into a
  deleted session, or into a transcript edited since indexing is reported as
  unresolvable **with the reason** rather than answered with different text — a
  wrong answer wearing a correct-looking id is worse than no answer.

- **Session transcripts are indexed for search across every session.** A
  contentless FTS5 index at `.localmind/sessions/spans.sqlite` holds locators for
  transcript spans — never their text — so a query reaches the whole session
  history rather than the newest session's summary. On a real 294 MiB store it
  builds in ~7 s, occupies 0.17x the source bytes, answers in single-digit
  milliseconds, and re-indexing an unchanged corpus is a no-op. Deleting a
  session removes its spans: the index never outlives its source.

  A locator carries the chunking-contract version, so one issued under an older
  contract resolves to nothing rather than to whatever now occupies that ordinal.
  The index refuses to open a store written by a newer build, and refuses to be
  created outside the project it derives from.

  **Library-only for now, deliberately** — the search and fetch tools that expose
  it are the next change.

- **Session transcripts can be read into retrievable spans.** A transcript
  reader (`localpilot-localmind::read_transcript`) understands the three record
  schemas that occur in a real session store — Claude Code JSONL, Codex JSONL,
  and LocalPilot's line-oriented rendering — and splits records into bounded,
  non-overlapping, kind-labelled spans under a versioned contract
  (`SPAN_CHUNKING_VERSION`). What it cannot use is reported by *reason*:
  corruption, an unknown record type, and a deliberate exclusion are counted
  separately, because one combined "skipped" number is how a reader that ignores
  a quarter of a corpus still looks healthy.

  **Library-only for now, deliberately.** Nothing calls it outside its tests yet;
  the index that consumes it is the next change. Disclosed here rather than left
  for a reader to infer a caller that does not exist.

- **LocalMind's `local_only` now constrains inference egress, not only storage
  scope** (ADR-0164). A configuration reading `local_only = true` previously
  accepted an inference or embedding endpoint on any reachable host and sent
  prompt, document and memory text there. Endpoints are now restricted to
  literal loopback destinations, checked **at the request** rather than only at
  configuration load. **Breaking, deliberately:** inference pointed at another
  machine is refused, with an error naming the host; such a configuration was
  already outside what `local_only` claimed, and it was silent about it. No name
  resolution is performed, so a hostname that would resolve to loopback is
  refused too.

- **Memory relevance now scans memory vectors, not everything.** The injection
  relevance gate took a nearest-neighbour window across the whole `vector_index`
  and filtered for memories afterwards, but the index also holds ingested
  documentation chunks and a real store is lopsided toward them. The window was
  spent before the filter ran. Measured live against realistic prompts on a store
  with 8,079 doc and 200 memory vectors, a 64-wide shared window kept a median of
  1.5 memory candidates and none at all for a quarter of prompts; kind-scoped, the
  same prompts get the full window. Keyword search remains the candidate floor and
  a project without an embedding endpoint is byte-identical (ADR-0163).
- **The embedding server now runs only while LocalPilot does.** A session-bearing
  command starts it when the project configured an embedding endpoint, and
  session end stops it. Nothing starts at boot and no OS service is installed —
  a 640 MB model server appearing at login is not something a local-first tool
  should put on a machine, and it should not keep using one when nothing needs
  it. LocalPilot stops only a server **it** started: one you brought up with
  `localbox embed-serve` survives a run ending. Concurrent runs hold leases so
  the last one out stops it, and a crashed run's lease is pruned rather than
  pinning the server up.
- **Pinned end to end: a dead embedding endpoint degrades retrieval, never fails
  it.** The diagnosed scan, the cosine gate and the fused ordering each tolerate
  the failure individually; nothing asserted the whole chain did. A regression in
  any one of them now surfaces as a test failure rather than as a quiet
  degradation, which is the shape the original outage took.
- **An ingest chunk embedded under a different model is now re-embedded.** The
  skip compared only the content fingerprint, so a model change left chunks
  carrying vectors from a model the search side no longer uses — the text was
  unchanged, so the skip fired. Re-running ingest is the repair path for the
  chunk store, and it now actually repairs this case.
- **The memory relevance path no longer swallows why it found nothing.** It used
  to collapse an unconfigured endpoint, a dead endpoint, an unembedded index and
  an index embedded under a different model into a single silent `None`.
  Retrieval still degrades to keyword in every one of those cases — that contract
  is unchanged — but the state is now nameable, and `localmind status` reports it.
- **`[retrieval] rerank_window` governs again.** It was read and discarded, so the
  whole candidate list was reordered. It now bounds movement as documented: fusion
  scores every candidate, only the leading `rerank_window` hits may be permuted,
  and the tail keeps its keyword order (ADR-0163).

- **An older copy earlier on `PATH` is now reported instead of silently winning.**
  Anyone who installed from source before the one-command installer existed has a
  `cargo install` binary in `~/.cargo/bin`, which is usually ahead of the managed
  directory on `PATH`. The installer wrote a correct binary, reported success, and
  the old one kept running — nothing looked wrong except the version. The install
  report now names every train tool that resolves from an earlier entry, the exact
  path that wins, and the two ways out (reorder `PATH`, or remove the old copies;
  `cargo uninstall` is named only when the path really is cargo's). Being on
  `PATH` was never the same as winning it, and the old notice only checked the
  former. Release channel only — a `--prerelease` install builds with
  `cargo install`, so a copy in cargo's bin directory is the install itself.

- **Optional PowerShell `llm*` shortcuts now ship with `localx`.** Install them
  alongside the stack with `localx install --powershell-shortcuts`, or later
  with `localx install powershell-shortcuts`. The installer follows Chris Titus
  Tech's separate `profile.ps1` customization convention, creates an all-hosts
  user profile only when none exists, and leaves every other custom profile
  untouched while printing the exact load line to add. The compatibility set
  includes the restored `llm-add` model-registration shortcut.

## v3.3.2 - 2026-08-20

Coordinated LocalX release.

- **Fixed self-updates from the prerelease channel.** A tagless `localx`
  source build now keeps recognizing itself even when its version stamp is not
  SemVer, so it takes the safe staging-and-replace path instead of trying to
  overwrite the running executable. The installers use the same staged
  replacement with rollback, and tagless builds now carry a parseable
  `<crate-version>-g<sha>` stamp as defense in depth (ADR-0162, LocalHub#79).

## v3.3.1 - 2026-08-20

Coordinated LocalX release.

- **Automatic compaction now sends the model its real digest and budgets for the
  reply.** When context is trimmed mid-run, the conversation receives the
  finalized semantic digest (goal, decisions, per-file operations, command
  outcomes), shrunk to fit and ordered by recency — not the four-bullet
  placeholder it used to get, which left the rich digest in the event log only.
  The session budget now reserves the provider's real output cap (its
  `max_tokens`, or the adapter default) instead of a flat 4,096, so a history the
  local estimate believed fit is no longer rejected by the provider and pushed
  into the destructive overflow path; a pasted image is charged its real token
  cost rather than zero; and an automatic or overflow compaction posts a
  host-visible notice, so a dropping context gauge is explained rather than
  mysterious (ADR-0161, LocalHub#78).

## v3.3.0 - 2026-08-19

Coordinated LocalX release.

## v3.2.0 - 2026-08-18

Coordinated LocalX release.

- **Incognito sessions (`localpilot chat --incognito`, `/incognito`).** An
  incognito session persists nothing of its own — its store is in-memory, prompt
  history is off, and it runs no closeout, knowledge index, or code-graph
  reindex — and every file it creates is gated behind an explicit approval
  (headless denies), a floor that survives a `/bypass`. Persistent slash
  commands (research, ingest, context build, LocalBox adopt/serve, the
  self-improvement loop, skill installs, and LocalMind review decisions) are
  refused, naming what they would have written. A persistent footer indicator
  and empty-composer hint show the live state, and each file-creation approval
  repeats the durability boundary. When the session ends —
  `/incognito off` or exit — it reports every file it created: workspace files
  (a full snapshot diff with no ignore filtering, so `target/` counts; `.git/`
  collapsed to a count), files a tool wrote outside the workspace, and the
  shell/background command attempts presented to the permission gate verbatim,
  with the stated limit that files those created outside the workspace are not
  tracked (ADR-0160).

- **`localx` can update itself.** On Windows, `localx install --prerelease` /
  `localx update --prerelease` ended with `Access is denied (os error 5)` on the
  last step — cargo's final move onto the running `localx.exe` — and the advice
  to re-run or elevate could never work. The running tool is now built into a
  staging directory and swapped in with the same rename-then-copy the release
  channel uses; a refused swap keeps the build and says to copy it over after
  exit. On the release channel a source-bootstrapped `localx` earlier on `PATH`
  is refreshed too, so the command the shell resolves is the version just
  installed, and `localx status` reports the running version and flags a
  shadowing copy. `localpilot update --source` takes the same self route
  (ADR-0159).

- **A tool call cut off by the provider's output cap is reported as what it is
  and retried in pieces.** On the Anthropic wire protocol (including local
  servers that speak it) a `write_file` payload truncated at `max_tokens`
  surfaced as `stream decode error: tool input: EOF while parsing a string at
  line 1 column 41806`, and the chunked-write recovery built for oversized
  writes could never fire on that adapter. The decoder now waits for the stop
  reason before judging an unparseable tool block: a `max_tokens` truncation is
  discarded (never half-applied) and named on the output-limit warning with its
  byte count; a complete-but-invalid payload surfaces as the typed
  malformed-arguments error that steers the model to write in smaller pieces.
  Both decoders report every truncated call, and when one is a file write the
  turn retries once with the chunked-write instruction instead of stopping —
  without moving the recovery ladder. The stop message now says the output cap
  is shared by prose and tool arguments (ADR-0158).

- **A long multi-line paste on Windows stays one prompt.** The legacy
  key-record paste fallback judged the first Enter of a run by how fast the
  terminal loop had *processed* the preceding characters, so any first line
  longer than a few dozen characters was submitted on its own and each later
  paragraph became a separate steer. Classification now uses only the input
  queue and the continuation window, and flushing staged text no longer ends
  the burst, so chunked pastes are classified once (ADR-0157). A bunched typed
  line still submits on its Enter.

- **Intermediate progress no longer competes with the final answer.** Assistant
  prose keeps the filled `●` answer cue unless a later tool start proves that
  exact stable segment was intermediate; then it switches in place to a quieter
  hollow `○` progress cue. The final post-tool answer remains filled. Screen
  readers announce `Progress update:`, while no-color, search, copy, export,
  wrapping, and viewport anchors retain their existing source text and geometry.

- **Successful tool runs can collapse without changing the transcript.** The
  opt-in `[terminal] group_successful_tools = true` preference summarizes three
  or more consecutive successful tools behind one expandable row with count,
  known aggregate duration, retained-detail, and terminal-truncation disclosure.
  F7/F8, Enter, Escape, prefix clicks, search, anchors, no-color, and
  screen-reader modes use the same interaction model as individual tools.
  Copying across a collapsed group inserts a counted omission marker, while
  persistence and exit/export output keep every original tool. The default is
  off.

- **Failed tools surface the useful diagnostic tail without flooding chat.** A
  collapsed failure keeps two leading command/context rows plus the six newest
  retained wrapped rows. An ellipsis gutter, screen-reader sentence, and
  width-specific disclosure count identify the skipped middle; a selection
  crossing that jump copies an explicit omission marker rather than silently
  joining noncontiguous output. Expansion restores the complete retained item.
  Running, successful, and cancelled previews keep their four-row ceiling.

- **Tool rows now have one keyboard-and-pointer focus model.** F7/F8 move a
  stable focus through tool activity, Enter expands or collapses the focused
  row, Escape returns to the composer, and clicking the disclosure prefix uses
  the same geometry and viewport-anchor path. Expansion keeps the headline on
  its screen row except for one deterministic bottom clamp. The new
  `[terminal] density` preference defaults to `compact`, preserving the current
  layout; `comfortable` adds breathing room only between adjacent tool rows,
  while the separator before assistant or reasoning text remains in both.

- **Tool results disclose what is hidden.** Expandable full-screen tool rows now
  show a chevron and the number of collapsed visual rows, distinguish that
  temporary omission from a permanently bounded terminal view, and align
  completion metadata when a wide terminal can do so without wrapping. Narrow,
  no-color, colorblind, screen-reader, search, selection, and copy behavior keep
  the same retained timeline authority.

## v3.1.0 - 2026-08-11

Coordinated LocalX release.

- **`localx`, one command for the whole stack.** A new umbrella binary installs
  and updates the entire stack in one step: `localx update` refreshes every
  release-train tool (localpilot, localmind, localbox, localbench) *and* the
  llama.cpp engine; `localx install [all|<tool>|engine]` provisions them;
  `localx status` reports what is installed; and `localx <tool> …` runs any stack
  tool. `localx update --prerelease` (or `localx install --prerelease`) builds
  each app from its repository's latest `main` instead of the newest release —
  the developer channel for testing pushed-but-uncut work (needs a Rust
  toolchain; app tools only). `localx` self-updates as part of the stack. The
  one-command installer now bootstraps `localx` and lets it install the rest;
  `localpilot update --all` still works and shares the same install core.

Coordinated LocalX release.

- **Tool activity is compact without being opaque.** Full-screen chat now shows
  a descriptive, target-aware action while a tool runs, then keeps up to three
  indented result rows under the outcome, line count, and elapsed time. The
  complete bounded result remains searchable and expands in place from the
  status glyph; ordinary output is subdued, while trustworthy unified-diff
  additions and deletions retain semantic emphasis. Screen-reader labels,
  no-color status cues, stable selection, and bounded exit transcript output
  remain intact.

- **The legacy inline chat host is retired.** `localpilot chat` now resolves only
  to the full-screen terminal application; the `LOCALPILOT_CHAT_UI` selector, the
  inline driver, and the `localpilot-tui` crate are removed (ADR-0154). The
  slash catalog's phantom inline host is gone too. Full-screen and pair behaviour
  is unchanged, and non-interactive/plain output is unaffected.

- **LocalMind is now inspectable and reviewable inside full-screen chat.** The
  top bar now exposes real Session and LocalMind product tabs; `/localmind` and
  selecting the LocalMind tab both open its Docs, Graph, Memory, Review, Skills,
  and Audit sections with `Tab`/`Shift+Tab` navigation and bounded viewport
  rendering. Switching tabs preserves LocalMind section, selection, and reviewer
  state, while transient overlays return to the tab beneath them. Opening it
  without a project store is read-only and creates no project state;
  Skills reports LocalPilot proposals without changing them. Review requires an
  explicit session-local reviewer identity, limits actions to valid candidate
  states, and sends Accept, Reject, and Promote through the active permission
  profile and approval dialog before LocalMind writes anything.

- **The full self-improvement loop is available inside chat.** Full-screen chat
  exposes `/selfimprove`, `status`, `start [finding-rank]`,
  `next`, `approve <reviewer>`, and `reset` through the existing persisted
  orchestrator. Multiple findings are shown as a bounded numbered report and
  require an explicit selection; proposals and build output are bounded in the
  UI. `next` stops at Proposed, while approval displays the exact persisted diff
  and requires a reviewer-bound confirmation before the sole `ApprovalToken`
  path can promote. Build remains responsive and reaches a durable boundary;
  confirmed reload exits and restores the terminal before invoking the existing
  self-dev process swap.

- **Injected credential stores no longer touch the host OS keychain.** Stores
  constructed with an explicit fallback path are now fail-closed and file-only,
  so all-feature tests and dependency-injected callers cannot read, overwrite,
  or delete the process user's ambient provider and MCP credentials. The
  production user store remains the sole keychain opt-in.

- **LocalBox models and direct serving are first-class chat commands.**
  `/localbox models` reads LocalBox's versioned catalog contract and lists the
  exact launch name first, accepted aliases, model/quant identity, required
  engine, tuned/default profile state, and the active model when detectable.
  `/localbox serve <model>` now means exactly start that model, wait for
  readiness, adopt its provider config, and switch the current session; the old
  `/localbox adopt --serve <model>` spelling remains a compatibility alias.
  LocalPilot preflights LocalBox's own run-profile result before launching. If
  tuned settings are unavailable it shows the actionable warning and starts
  nothing until the user explicitly retries with `--allow-untuned`; only that
  approved retry passes LocalBox's one-shot fallback flag. Older LocalBox builds
  degrade with update guidance rather than parsing prose or launching blindly.

- **Interactive research now becomes part of the conversation.** A completed or
  cleanly interrupted `/research` run adds one assistant-style, redacted result
  to the active session, so the next turn and a resumed session can refer to its
  numbered findings, sources, and open questions. The injected index is capped
  at 4 KiB, labels evidence as untrusted, preserves source/fetch provenance and
  a pointer to the complete report on disk, and is recorded exactly once; failed
  runs add no synthetic result.
- **Long `ask_user` answers remain reviewable before confirmation.** The
  full-screen Other editor wraps and grows with the answer, then keeps the
  caret visible in a bounded vertical viewport with a proportional scrollbar.
  Home/End exposes either end after overflow or resize, Unicode editing stays
  grapheme-safe, and confirmation still returns the complete stored answer.
- **Full-screen input stays responsive while LocalBox is busy, and long work is
  visibly alive.** Active operations now wake from one dedicated terminal-event
  reader independently of a 20 Hz render cadence, and ordinary-key paste
  detection never waits on every keystroke. Once a modern terminal emits a real
  bracketed paste, the legacy key-burst heuristic retires for that session; its
  fallback also requires a dense prefix before treating Enter as paste content.
  A bunched typed follow-up therefore submits on its first Enter even when
  inference and the TUI share a machine. Working chrome animates and shows
  monotonic elapsed time; manual compaction is labelled `Compacting` without
  inventing internal phases or percentages.
- **Live slash controls remain available during full-screen turns.** The shared
  command policy keeps permission profiles, `/bg`, `/effort`, and `/think`
  live while work runs. Profiles
  apply from the next tool call and effort from the next provider request, even
  within the same turn. Ctrl+Q slash submissions use the same dispatcher as
  Enter, unsupported active-turn commands name the live choices, and operations
  without live handles refuse explicitly instead of dropping input.
- **Ctrl+C protects a typed full-screen draft before cancelling work.** With no
  selection, the first press atomically stashes and clears a nonempty composer;
  the next empty-composer press cancels active work and a following consecutive
  press exits. Idle drafts use the same clear-first rung, Ctrl+S restores the
  stash, other input resets exit arming, and footer/help copy tracks the current
  behavior. Selection-copy precedence is unchanged.
- **Assistant and reasoning rows no longer open with a blank glyph-only line.**
  Leading CR/LF provider framing is removed only when a new streamed segment is
  created, whitespace-only openers are dropped, and later deltas retain their
  exact whitespace. Raw stream-byte accounting is unchanged.

## v2.8.1 - 2026-08-07

Coordinated LocalX release.

- **Self-update now survives a transient release-asset connection failure and
  its source fallback selects the right workspace package.** Manifest and
  archive downloads retry bounded transport/body failures while still failing
  immediately on definitive HTTP errors. If binary installation remains
  unavailable, `cargo install --git` explicitly selects `localpilot`, avoiding
  Cargo's ambiguous-workspace error from the `localpilot-fuzz` and `xtask`
  binaries.

## v2.8.0 - 2026-08-07

Coordinated LocalX release.

- **The no-progress guard no longer kills a turn that recovers, and its stop
  names the signal that fired.** A turn that trips the degenerate-loop detector
  now gets its one strategy-change hint plus exactly one grace call, whose
  observation recomputes progress — the turn continues only when that
  recomputation makes the signal inactive, and otherwise stops. Repeat detection
  counts only within a sliding window of recent successful calls (so calls far
  apart no longer add up to a false loop); a user
  steering message resets the progress breakers (not the cost/tool-call budget or
  deadline), while system/background notices do not; and every `NoProgress` stop
  now records which signal fired (stuck repeat / novelty decay / consecutive
  failures) alongside the unchanged coarse reason. Compaction no longer mistakes
  the guard's own stop notice for the session goal — it keeps the latest real
  request as the goal. (Explicit operator budgets are unchanged.)

- **`localpilot doctor` and `/settings` now surface skill-discovery state.**
  Doctor reports a `skills:` block — whether `[skills] autonomous_discovery` is
  on or off, how many discoverable packages are readable and how many user-only
  packages are hidden. When the project overlay is not included it distinguishes a
  confidently untrusted workspace (`workspace untrusted`) from one whose trust
  could not be evaluated (`workspace trust could not be evaluated`) — matching the
  report's own trust line — and in both cases counts only the user-global baseline
  without reading any project manifest. A catalog that cannot be scanned is
  reported as `unreadable` (distinct from a real empty `0`), and malformed entries
  are surfaced as a count (`package entries skipped as unreadable: N`) so an empty
  `0` is never mistaken for a clean catalog. The full-screen `/settings` view adds
  a static `Installed package discovery: on/off` row that, when off, points at
  `/skills list` and the config switch — with no catalog scan on render.

- **Accepting the workspace-trust dialog now trusts the live session
  immediately.** Live session trust is derived from the trusted-folders store
  (not the permission profile) and updated in place when you accept — so the
  session's tools see the trusted project overlay (e.g. project skills) at once,
  and a resume reads the same value as a live turn, with no relaunch. Trusting
  "for this session only" stays in-memory (nothing persisted); "trust and
  remember" still writes the store. If installed skill packages become readable
  by the grant while discovery is off, the "disabled, not empty" cue is added
  once. In a paired session the grant applies to both peers or fails cleanly,
  never leaving one peer trusted and the other not.

- **Skill tools now clearly separate the two "skills" lanes, and say when
  discovery is merely off.** The installed-package tools
  (`skill_list`/`skill_search`/`skill_load`) and LocalMind's
  `active_skills`/`skill_drafts` now describe themselves as distinct lanes and
  cross-reference each other, so an empty result from one no longer implies the
  other is empty. When skill packages are available in the session's readable
  catalog but model discovery is off (`[skills] autonomous_discovery = false`),
  the agent is told that discovery is *disabled, not that no skills exist*, and
  pointed at `/skills list` in chat (or `localpilot skills list` outside chat) and
  the config switch — without ever injecting package names, descriptions, or
  counts. Interactive sessions compute that state once at launch, trust-safely.

- **New `skill_list` tool: page the whole installed skill catalog.** When
  `[skills] autonomous_discovery = true`, the model can list every discoverable
  skill package (name, one-line summary, and origin scope) in name order,
  paginated (default 50, max 100 per page, with a `next offset` when more remain)
  — so it can pick from the full catalog instead of guessing search terms. It is
  package-only and read-only: user-only skills contribute only an omitted count
  (never a name or body), an untrusted workspace shows only the user-global
  baseline, and `skill_search`'s "no strong match" and overflow results now point
  at `skill_list`. For LocalMind-derived skills the model still uses
  `active_skills`/`skill_drafts`.

- **`skill_search` matching is more forgiving and honest.** It now matches a
  skill's name, description, and command triggers with one shared,
  punctuation-insensitive signal, so a query like `threejs` finds a `Three.js`
  skill and a run-together name like `reactthreefiber` finds `react-three-fiber`.
  The inclusion gate and the ranking use that one signal (every returned skill
  scores at least 1). When nothing matches strongly, the result reports how many
  discoverable skills exist instead of implying there are none, and a search with
  more matches than the page cap says so rather than silently dropping the rest.
  Unrelated queries still honestly return no match — search never invents one.
  User-only skills remain excluded from search and from that count.

- **`/compact` and the long-running ingest runs now work in full-screen chat.**
  `/compact` (and `/compact force`) summarize the conversation on the same live
  pump a turn uses — a single Ctrl+C cancels the compaction and returns to the
  chat, leaving the conversation unchanged, without leaving full-screen. `/ingest
  run`, `/ingest refresh`, and `/ingest resume` run the workspace walk in
  full-screen with live progress; a single Ctrl+C pauses a run (resume it with
  `/ingest resume`).

- **`/research` now works in full-screen chat.** `/research <topic>` runs a
  one-shot research pass on the same live pump, and bare `/research` enters a
  persistent research mode where each plain prompt becomes a topic (the footer,
  settings, and composer show the mode; `/agent` exits). The egress disclosure is
  shown and drawn **before** any web or model request, a single Ctrl+C ends a run
  with a partial report rather than losing it, and results open as a scrollable,
  copyable report. Research is text-only: a prompt submitted with image
  attachments while in research mode is declined with a notice and your draft and
  attachments are preserved untouched. `[research].enabled = false` disables it;
  `[research.web].enabled = false` still runs local-only research with a truthful
  "web disabled" disclosure.
- **`/harness-resume` and `/wait-resume` now work in full-screen chat.** Both run on
  the same live pump against an inner runtime, entering Harness mode (the footer shows
  it; `/agent` exits). They use the live model, provider, permission profile, and
  workspace-trust grant as of when you invoke them — a `/model` or profile switch is
  honored, and `/wait-resume` blocks if the provider changed during the wait. Tool
  approvals surface in the normal dialog; a single Ctrl+C ends a run gracefully; and the
  result opens as a bounded, scrollable report instead of flooding the transcript.

- **The full-screen slash surface is complete.** `/agent` and `/harness` now appear in
  the full-screen picker as mode switches — `/harness` enters harness mode (a plain
  prompt runs an ordinary turn, exactly as inline; the footer and settings show the mode
  and `/agent` switches back). Every full-screen command now reaches a real route, so the
  "not available in full-screen chat yet" message is gone. `/compact force` stays the way
  to force a compaction (there is no separate `compact_force` picker entry).

- **Synchronous commands work in full-screen chat with bounded output.** `/tree`,
  `/knowledge`, `/context`, `/agents`, `/skills`, and `/bg` now run in the
  full-screen session. Short output is shown inline; long output opens a
  scrollable report you can copy with `Ctrl+C` and close with `Esc`, instead of
  flooding the conversation with dozens of lines. The fast `/ingest` subcommands
  (status, preview, pause, and the rest) run the same way; the long-running ingest
  runs are covered in the entry above.

- **`/think` hides or shows reasoning in full-screen chat.** Toggling reasoning
  off removes reasoning items from the timeline — render, scrolling, search, and
  copy all skip them — while the underlying reasoning is retained (streaming
  continues in the background and reappears when you show it again). It works
  while a turn is running. The `/exit print` transcript already omits reasoning
  and is unaffected.

- **Permission profiles and reasoning effort are switchable in full-screen chat.**
  `/default`, `/relaxed`, `/bypass`, and `/unrestricted` now change the permission
  profile from inside the full-screen session — the enforcement engine and the
  footer/settings update together, so the displayed profile always matches the one
  in force. `/effort <level>` sets the reasoning effort for subsequent turns and is
  shown in settings ("provider default" until set). Modes (`/agent`/`/harness`)
  remain launch-time — the full-screen host has no distinct mode loop yet, so they
  are not wired rather than shown falsely.

- **Full-screen and pair chat now handle the takeover commands with arguments.**
  `/help`, `/theme`, `/settings`, `/diff`, and `/search` are parsed as real
  commands in the full-screen and pair hosts instead of being intercepted as bare
  tokens: `/help me` reports "this command does not take arguments" rather than
  "unknown"; `/theme <name>` applies a theme directly (an unknown name warns and
  leaves the picker closed); `/settings <query>` pre-fills the settings filter;
  `/diff <path>` filters the diff to matching paths; `/search <query>` seeds the
  search. `/help`, `/theme`, and `/search` still work while a turn is running.
  The inline composer is unchanged. Submitting `/search` with an attached image
  now shows the same "remove image attachments" notice as any other slash command
  (previously it opened search silently), keeping every attachment path
  non-silent.

- **Opt-in two-agent collaboration: `localpilot pair <task>`.** Two ordinary,
  independent Agent-mode sessions can now work one task in a shared workspace
  through a typed, bounded propose/revise/agree protocol. The driver runs only
  one model turn at a time and exits successfully only after both peers agree on
  the current revision and digest; round and slot bounds, `/abort`, and Ctrl+C
  stop non-converging work. The full-screen host shows labelled peer panes at
  wide widths and an active-only, F6-switchable pane below 61 columns while
  preserving per-peer scroll, search, selection, and attributed approvals and
  questions. Both peers use the selected permission profile through independent
  engines, and the command never applies or commits automatically. Startup now
  discloses that two resident histories can consume more tokens/provider quota
  without claiming a fixed multiplier. The swarm API's public `MemberRole` enum
  is deliberately `#[non_exhaustive]` as it gains the exact-two `Peer` role. See
  [docs/configuration.md](docs/configuration.md#pair-collaboration).

- **Context hygiene: `localpilot doctor --hygiene`.** `doctor` gains an opt-in
  `--hygiene` flag that inspects the authored context a session assembles from
  the current directory — the `CLAUDE.md`/`AGENTS.md` instruction files and the
  skills visible to the project — and reports each layer's token weight plus
  advisory findings: a directive stated in more than one layer (redundancy),
  directives that disagree (conflict), and layers large enough to be worth
  right-sizing. It reads and reports only — never edits — and every quoted
  snippet passes the same redactor as the rest of `doctor`, so a secret in an
  instruction file is not echoed. Without the flag, `doctor` output is unchanged.
  Rides the existing `--format human|json`. See
  [docs/install.md](docs/install.md) and ADR-0140.

- **Self-improvement loop: `localpilot selfimprove status` / `next`.** The four
  existing self-improvement stages — read-only review, human-gated patch
  proposal, and the self-dev build/reload — are now wired into one loop by a thin
  orchestrator. `status` shows the current stage; `next` advances exactly one
  step: review → propose → **[human approval]** → build → reload. Past the human
  gate, `next` refuses to promote without an explicit `--approve --reviewer`, and
  it builds the approved, merged tree — never the proposal worktree. This does
  **not** enable any autonomous loop: every step is explicit and the unattended
  self-editing loop stays deferred. See
  [docs/02-architecture.md](docs/02-architecture.md) and ADR-0138.

- **Run a task plan as a swarm, each agent on a chosen model.** The new
  `localpilot swarm run <plan>` reads a JSON plan (objective, mode, and nodes —
  each with an optional per-node `model`) and runs it to completion: a worker per
  ready task, refilling as workers finish, each built on the model its node asked
  for (or the run default). A multi-provider config lets different nodes target
  models served by different providers. A node whose model no configured provider
  advertises is **refused before the worker is built** — the run fails that node
  loudly rather than silently falling back to the default, and a worker that
  would have been built on the wrong model is refused too. The fan-out is bounded
  (`--concurrency`, `--max-agents`) so N agents on N models cannot exhaust the
  machine. This turns the swarm substrate into a runnable capability. See
  [docs/configuration.md](docs/configuration.md) §Swarm model selection and
  [docs/02-architecture.md](docs/02-architecture.md).
- **LocalBox integration: detect a local model server and adopt it.** When no
  usable model is configured, startup, the `/model` command, and
  `localpilot models` now point at a detected LocalBox server (or an
  installed-but-stopped LocalBox) instead of only erroring; when no LocalBox is
  present the messages are unchanged. The new `localpilot localbox adopt` and
  in-session `/localbox adopt` write a `[providers.local]` block for a running
  LocalBox — a permission-gated config write that upserts only the local
  provider, preserving your other providers, MCP tables, and comments.
  `localbox adopt --serve <model>` also starts a server first (gated) if none is
  running. In a chat, `/localbox adopt --serve <model>` performs that launch and
  adoption from the terminal UI, rebuilds the provider registry, and switches
  the current idle conversation to the local model without losing its
  transcript. Cancelling the wait does not stop the LocalBox-owned server, whose
  startup may continue in the background. See [docs/providers.md](docs/providers.md).

## v2.7.0 - 2026-08-02

Coordinated LocalX release.

- **The full-screen terminal-chat shell and timeline are now the interactive default.**
  Bare `localpilot chat` selects the alternate-buffer host backed by the
  backend-neutral `localpilot-terminal-ui` crate. `LOCALPILOT_CHAT_UI=inline`
  remains only as a temporary legacy rollback while the deferred physical and
  cross-terminal acceptance matrix remains open. The foundation
  carries stable content IDs, virtualized content-anchored history, framed
  prompts, compact activity, semantic themes, responsive status/composer/footer
  regions, a screenshot-measured true-color palette with a dark application
  canvas and joined prompt/composer surfaces, restrained frame chrome,
  grapheme/display-width editor geometry, a provider-neutral runtime-
  event adapter, transactional terminal restore, and contextual Ctrl+C handling:
  copy selected text, cancel active work when no text is selected, and exit only
  on a consecutive second press. The full-screen host now draws before workspace
  projections, restores the workspace-trust gate and durable prompt history,
  submits through the existing `SessionRuntime`, streams stable timeline items,
  shows a live response-byte counter, denies tool approvals safely, and keeps an
  ordered visible pending-operation queue. During an active model turn, Escape
  promotes the leading plain-text prompts into urgent, ordered steering: the
  incomplete provider response remains visible but does not enter model history,
  and the same turn restarts with the new direction. A queued shell or image is
  an ordering barrier, so Escape hard-cancels the current turn and preserves the
  original follow-up order instead of skipping ahead; Ctrl+C remains the direct
  hard-cancel path. Collapsed tool rows now include a
  truthful output-line count before their expandable detail, and the theme
  picker uses a readable refactor sample for its semantic color preview. The
  full-screen host also supports
  application-owned drag selection/copy, wheel and page navigation, draggable
  scrollbar navigation, precise composer clicks, contextual right-click copy in
  the timeline and atomic text paste in the composer, boundary-aware history,
  reverse and timeline search, fuzzy slash/file completion, atomic compact
  multiline paste, and clipboard-image placeholders whose bytes stay out of
  prompt history. The shared `ask_user` tool now pauses a model turn for up to
  four ordered questions; the full-screen host presents numbered single- or
  multi-select choices with descriptions and an automatic free-text Other row,
  resolves each timeline row with the answer, and dismisses explicitly without
  guessing when the user presses Escape or the host closes. Workspace trust now
  uses the same full-width, numbered, keyboard/mouse-focusable timeline treatment, with
  distinct session-only, remember, and deny choices plus explicit screen-reader
  selection text; session-only trust does not write the trusted-folder list.
  Ctrl+G temporarily restores
  the ordinary terminal, opens the
  draft in a foreground external editor, then rebuilds the full-screen frame
  without losing its timeline or opaque attachment identity. Detailed
  conversation surfaces, accessibility hardening and the remaining terminal
  matrix remain follow-up work; the temporary selector is removed only after
  feature parity is accepted.
- **Added: `localpilot selfdev` — build, vet, publish, and reload LocalPilot from
  its own source.** `selfdev build` fingerprints the working tree and builds it;
  `selfdev publish` runs the build through the publish gauntlet and, only if it
  passes, installs the binary immutably and points a channel at it, then reclaims
  old versions beyond the most recent few (a version a channel points at is never
  reclaimed); `selfdev gc` runs that reclaim on demand; `selfdev status` shows
  what is installed, what each channel points at, and the auto-reload breaker's
  state; and `selfdev reload -- <args>` builds, vets, promotes, and then swaps this
  process onto the new binary running `<args>`. Everything here is the manual
  capability — a developer or a CI job drives it explicitly (`reload` swaps the
  process only because you asked it to, and carries no session continuation). The
  autonomous in-session loop, where the model builds and reloads itself
  mid-session, is a separate opt-in this build does not ship (ADR-0128).

- **Added: reload is safe to fail — a rollback token, a no-phantom version
  comparison, and a circuit breaker.** Before a channel is pointed at a new build,
  what it pointed at before is recorded; if the new build does not come up, the
  channel is rolled back to the previous version. An auto-reload triggers only when
  the candidate is *provably* newer — both timestamps readable and the candidate
  strictly newer — so an unreadable timestamp is treated as "no update", never as
  "newer forever". And a durable counter bounds how many times auto-reload may be
  attempted, incremented before each relaunch so a relaunch that never returns
  still counts and a looping process cannot reset it by restarting. There is
  deliberately no crash-detect-and-revert loop (see ADR-0128).

- **Added: the in-place reload primitives — swap onto a freshly built binary and
  continue the session on the other side.** Before the swap, everything durable is
  written first — the new binary is installed immutably, the channel is pointed at
  it, and a continuation intent naming the session and its in-flight task is
  recorded — because a process replacement runs no destructors. The swap itself is
  one small step that differs by platform (replace the process in place on Unix,
  spawn the successor and exit on Windows) behind a single seam, launching the
  concrete immutable binary so a later build can never overwrite what is running.
  On the far side, the resumed session reads the intent and continues on its own
  with a hidden "reload succeeded; carry on" prompt. The intent is durable, read
  without being consumed, and marked delivered only once the continuation
  completes, so a restart that dies mid-continuation retries and one that succeeds
  is never replayed. These are the building blocks; the command that drives them
  is an opt-in developer surface, off by default.

- **Added: graceful shutdown for a running turn.** A host can now ask a turn to
  *wind down* instead of cancelling it. Where cancelling discards — aborting the
  in-flight tool and throwing the turn away — a graceful shutdown finishes safely:
  it stops at the next boundary, answers every pending tool call so the transcript
  stays valid and resumable, and flushes the session first. A tool whose whole job
  is to wait is answered with a non-error result that carries its exact original
  input, so the model can re-issue the identical call after the process returns;
  any tool that changes something is answered as interrupted, because repeating it
  would repeat the effect. This is the safe-stop primitive an in-place update or
  reload needs, since a process replacement runs no destructors.

- **Added: a publish gauntlet that refuses to promote a stale or broken build,
  and `localpilot version --json`.** The new flag prints this binary's own build
  identity — version, commit hash, and source fingerprint — as one JSON line. The
  gauntlet reads it and holds a candidate to three checks before any channel may
  point at it: its embedded hash *and* fingerprint must match the source it was
  built from (so a rebuild of different bytes at the same commit is caught, not
  just a different commit); the source tree must not have changed while the build
  ran; and the candidate must complete a real RPC handshake within a deadline —
  proof it can boot its config, provider, tools, and session and answer on the
  wire, not merely print a version. A candidate that hangs is killed at the
  deadline rather than waited on.

- **Added: an immutable store for self-built versions, and marker-file channel
  pointers.** Each self-dev build lands in its own directory named by its source
  label and is never written to again; a rebuild of the same source is a no-op,
  and a rebuild of different source is a different directory. Which build runs is
  decided by a *channel pointer* — a small marker file naming a label — swapped
  atomically by rename. So switching versions never overwrites a file a running
  process was launched from, and the previous build stays intact and usable
  behind it. The pointer is a plain file on every platform rather than a symlink,
  so Windows behaves exactly like Linux and macOS and needs no elevated
  privilege. Builds are copied into the store rather than hard-linked, on purpose:
  the source is a live build-output path a later build rewrites, and a shared
  inode would let that later build reach into a version already in use.

- **Added: a source fingerprint, and a build that knows what it built.** A commit
  hash answers "which commit", not "which bytes" — an uncommitted edit, a staged
  hunk, and a stray new file all produce a different binary from the same `HEAD`.
  LocalPilot can now reduce a working tree to one stable label: the short commit
  hash when the tree is clean, and the hash plus a fingerprint over the status,
  the diff, and the *contents* of untracked files when it is not. Returning a
  tree to its earlier bytes returns its earlier label. A binary built from that
  tree carries the identity with it, so a later step can refuse to ship a binary
  that no longer matches the source it claims. The build that produces it keeps
  to its own target directory and its own profile, and leaves a core free,
  because the session that asked for the build is still running. The build script
  now watches the repository only when it actually read something from it — a
  caller that supplies the identity no longer pays for a full rebuild on every
  commit.

- **Added: `run_plan` — the swarm plan driver, and the prompts that go with it.**
  A coordinator can now hand a whole task graph to the driver: it dispatches what
  is ready, spawns a worker per task, and refills on each completion rather than
  waiting for a whole wave. Each task carries an assignment contract in front of
  its input, because a worker inherits none of the coordinator's prompt — so
  "do this task and nothing else", "report what you established rather than what
  you did", and, in depth mode, "say what you did not check" have to travel with
  the work. A session that joins a swarm gets orchestration guidance appended to
  its own prompt at that moment and not before. A worker that returns nothing or
  times out is treated as gone and its task salvaged, rather than marked done on
  the strength of silence. The run report says how much of the concurrency the
  plan actually used, and explains it when the answer is "hardly any" — a chain
  runs one worker at a time however large the budget, which is not a fault but is
  worth saying out loud.

- **Added: the swarm failure lifecycle.** A worker that dies holding an
  assignment no longer strands the plan. Members heartbeat, and staleness is
  measured from the last beat rather than from admission — a member that has
  never beaten has not had the chance, and reaping it would reap every worker at
  birth. A departed member's unfinished tasks return to the plan, bounded by a
  per-task reclaim counter; past the budget the task is failed loudly, because a
  task that keeps outliving its workers is failing rather than unlucky. Its
  children are reparented onto the nearest surviving ancestor, and a departed
  coordinator is replaced by the lowest surviving member id — deterministic, so
  every observer elects the same successor without coordinating. A salvage report
  naming each task and its fate reaches whoever now owns the work. A reaper
  releases the hosting of finished members while keeping what they reported.
  Durable per-swarm snapshots hold the plan and membership in their own stream,
  so recovering a plan never requires replaying a transcript; writes are atomic,
  keep a backup, and refuse to go backwards.

- **Added: advisory file-conflict alerts.** When several agents share one
  working tree, an agent that changes a file another agent is working in now
  hears about it mid-turn. Every file-mutating builtin, and `read_file`, report
  what they touched as typed data — path, operation, and the line range, computed
  from the content that actually changed rather than from what the tool intended,
  so `multi_edit` and `apply_patch` are covered as exactly as `write_file`. The
  server keeps a short-lived index of who touched what and tells the peers a
  change affects, over the same soft-interrupt path the rest of the swarm uses.
  Two agents editing different parts of one file are left alone; two editing the
  same lines are both told, and a prior *reader* is told its knowledge went
  stale — in different words, because a reader has not lost work. The guarantee
  is advisory and stated plainly: nothing is locked, nothing is blocked, and
  nothing is rolled back. Both edits land, and git remains the merge substrate.

- **Added: `swarm` — agent-to-agent messaging.** Sessions collaborating on one
  repository can now message each other: `send` to one peer by name or id,
  `broadcast` to the agents you spawned (the whole swarm only if you are the
  coordinator), and `roster` to see who is here. Scope is the spawn tree, so one
  worker cannot cost every other worker a turn. Delivery rides the same
  soft-interrupt substrate as the user's own steering, in three modes —
  `notify`, `interrupt`, and `wake` (which starts a turn on an idle recipient,
  since there is nothing to interrupt). A long message requires a one-line
  summary, because the recipient is mid-task and has to decide whether to break
  off before reading the rest. Action verbs and field names are normalised, so a
  model writing `dm`/`tell`/`msg` with the body under `text` or `content` is
  understood rather than made to retry. The tool declares no effects and is
  gated by a host capability instead: a session that is not in a swarm — nearly
  every session — is told so and carries on.

- **Added: swarm state and parallel headless workers for the opt-in server.** A
  server can now host several sessions in one repository working on one plan.
  Swarms are scoped by *repository* rather than by path — every git worktree of
  one repo resolves to one swarm, read from git's own on-disk layout with no
  `git` subprocess — so a worker spawned into a worktree joins the coordinator
  instead of founding an invisible second swarm. Membership stores exactly one
  structural edge (who a member reports back to) and derives children, ancestry,
  and subtrees from it. Fan-out is bounded by two caps enforced as a
  *reservation* taken before the expensive part: a lifetime member cap and a
  concurrency budget, with idempotency keys so a retried spawn is answered rather
  than starting a second worker. A worker is an ordinary hosted session, so
  cancel, steer, event fanout, and reaping all work on it unchanged; when its
  turn ends its answer is bounded, recorded, and injected into its spawner as a
  labelled background message. A spawn that names a model is refused if the built
  session is on a different one. Still strictly opt-in: nothing on the
  single-agent path changed.

- **Added: `localpilot-taskgraph`, a pure task-graph engine.** A new leaf crate
  holding the plan several workers can share and the rules that keep it coherent
  while they mutate it: validated `seed` / `expand` / `complete` /
  `inject-from-gate` mutations, ownership and acyclicity checks, typed handoff
  artifacts with a lenient confidence parser, derived (never stored) readiness,
  and a deterministic simulator that runs a whole plan to completion with no live
  agents. The crate has no LocalPilot dependencies and does no I/O, so it is
  useful on its own — a single agent can decompose work into a graph, hydrate
  each step with what earlier steps established, and have gates refuse a
  completion that does not say what it left unchecked. Nothing is wired into the
  session runtime yet.

- **Added: multi-session resource pooling + session reaping for the opt-in
  server.** A `serve` process now shares one provider stack and one MCP
  connection pool across every hosted session — the MCP server subprocesses are
  spawned once at start-up, and each session projects a fresh tool registry that
  only *references* that one pool rather than re-spawning it, so N concurrent
  sessions speak to one set of MCP servers, not N. Only the mutable per-session
  state (the `SessionRuntime`, its transcript/compaction/config, a fresh
  approver) is built per session, and it stays isolated between sessions. A new
  periodic **reaper** keeps the resident set bounded: it removes a session once
  its last client has been detached beyond a grace period, or it has gone idle
  past a timeout, persisting the session's event log first
  (`SessionRuntime::close`) and **never** touching a session with an in-flight
  turn (it only closes what it can `try_lock`). Clean shutdown now stops the
  reaper and persists every remaining session before releasing the endpoint.
  Measured per-session resident-memory cost stays on the order of tens of KiB
  (an `#[ignore]`d RAM soak records the numbers). Still strictly opt-in (D003):
  the default in-process path is unchanged.

- **Added: `localpilot serve` + `localpilot connect` — the opt-in local-IPC
  server.** A new `serve` command hosts this workspace's sessions in one
  long-lived process over the local transport (a Unix domain socket or a Windows
  named pipe — never a network server), and `connect` is a thin plain-text
  client that attaches over stdin/stdout (stdin lines become prompts, session
  events stream to stdout; a permission ask is answered with `/allow`/`/deny`,
  `Ctrl-C` cancels a turn). Several `connect` clients can attach to the **same**
  session at once (`connect --resume <id|name>`): every client sees the same
  event stream and any of them can drive, steer, cancel, or read `status`, with
  fanout handled by the per-session host's broadcast. `serve` acquires a
  single-owner lock first (a second `serve` for the same workspace reports the
  running one and exits 0); `connect --server` starts a server first if none is
  running. The wire is the existing `attach` handshake and the RPC event
  vocabulary. **Strictly opt-in (D003):** none of this runs unless you invoke
  `serve`/`connect`, and the default in-process `chat`/`ask`/`print`/`harness`
  path is byte-for-byte unchanged. Internally, the stdio `rpc` command and the
  server factory now build sessions through **one** shared recipe
  (`SessionSetup::build`), so their runtime construction can never drift; the
  `RuntimeEvent`→`ServerEvent` projection (`localpilot-rpc::map_event`) is now
  public and shared by both. See
  [docs/embedding.md](docs/embedding.md#running-the-opt-in-server-serve--connect)
  and [docs/install.md](docs/install.md#running-the-optional-server).

- **Internal/Added: connection-scoped session attach handshake + additive
  protocol evolution.** The shared RPC envelope (`localpilot-rpc`) gains, purely
  additively, a `ClientCommand::Attach { target }` command — where `target` is
  `open_new` / `resume_id { session_id }` / `resume_name { name }` — and a
  `ServerEvent::Attached { session_id, server_version }` confirmation, plus a
  `SERVER_VERSION` constant. The server is one connection = one session: a
  connection names its session once and is bound to it, rather than multiplexing
  many sessions per connection. `localpilot-server` gains an `attach` dispatch
  (`attach(target, &registry, &factory, &store)`) that routes each target to the
  registry and returns the bound `SessionId`; an unknown id or name is a typed
  `AttachError`, never a panic (resume-by-id is guarded so a never-seen id cannot
  mint an empty ghost session). New fields follow an additive-evolution
  discipline — `#[serde(default)]` and skipped-when-empty — so a payload from a
  peer predating a field still deserializes without a second version handshake;
  the existing `RPC_PROTOCOL_VERSION` negotiation is unchanged and coexists with
  it. The existing single-session stdio RPC/ACP/MCP path is byte-for-byte
  unchanged: a client that never sends `attach` behaves exactly as before. No
  user-facing change — there is still no `serve`/`connect` command; this is the
  protocol groundwork. See [docs/embedding.md](docs/embedding.md).

- **Internal/Added: per-session multi-client fanout and lock-free out-of-band
  control.** The `localpilot-server` crate gains a `host` module (`SessionHost`)
  layered over a registry session handle. Several client connections can attach
  to one session and all receive its `RuntimeEvent` stream through a
  session-lifetime `broadcast` channel (a client attaching mid-turn still sees
  subsequent events; a dropped client prunes itself without erroring the driver).
  Cancel and steer reach an in-flight turn *without* taking the session's async
  mutex that the running turn holds: `drive` publishes the turn's
  `CancellationToken` into a short slot before awaiting it, so `cancel()`,
  `steer()`, and `is_busy()`/`status()` operate on that slot and the steer queue
  alone. A small `control(Control::{Cancel, Steer, Status})` dispatch maps a
  decoded control request onto these methods. Built from safe `std` + `tokio`
  only. No user-facing change: there is still no `serve`/`connect` command and no
  wire protocol over the transport — this is the host-side substrate. See
  [docs/embedding.md](docs/embedding.md).

- **Added: the agent can ask you a question (ADR-0121).** A new `ask_user` tool
  puts one to four multiple-choice questions to you inline in the TUI, driven
  with the arrow keys like the slash and file pickers — Space toggles on a
  multi-select question, Enter confirms, Esc skips, and a final row always
  accepts free text. The system prompt carries the threshold with it (ask when
  different readings would lead to materially different work, or before
  something hard to undo; otherwise pick the obvious option and state the
  assumption), so it does not turn into a permission prompt for everything.
  Where no human is reachable — a piped run, a CI run, a subagent — the tool
  says so and the model proceeds on its own judgment instead of stalling, and a
  dismissed question hands the decision back rather than failing. The intake
  guidance gate now asks through the same widget on a terminal, with the stdin
  prompt unchanged everywhere else.

- **Improved: configured documentation tools actually get used (ADR-0120).**
  The agent prompt now carries a version-sensitive documentation policy — when a
  task depends on current behaviour of an external library, framework, SDK, API,
  CLI, or cloud service (upgrade errors, migrations, deprecated APIs, changed
  config shapes), consult a documentation tool rather than prior knowledge —
  in two forms: direct use when the tool set is fully advertised, and
  `tool_search` → `tool_load` → call when the broker is on. It appears only when
  a documentation tool is actually reachable, stays bounded (stable local
  questions trigger no lookup), and names no vendor. Broker resolution also
  gained a fallback for tools its own description never matched: the MCP server
  name and schema property names/descriptions are indexed, and a generic
  capability vocabulary bridges `upgrade`/`migration`/`version` to
  `docs`/`documentation`/`reference` and `dependency` to `package`/`library`, so
  a need like "`<library>` version upgrade problem" can reach a generically
  named documentation tool. Tools that already matched rank exactly as before,
  and each search hit now says why it matched.

- **Added: path-scoped instruction files (ADR-0119).**
  `.github/instructions/*.instructions.md` files are now discovered, and their
  `applyTo` frontmatter glob (one glob, a comma-separated list, or a YAML list)
  narrows a rule to the files it is about. A scoped file is injected only on
  turns where a matching file is in play — the workspace files the session has
  touched, plus any workspace file the prompt names outright — so a monorepo can
  keep its Rust and web instructions apart instead of injecting both everywhere.
  A scoped file without `applyTo` applies project-wide, and unscoped instruction
  files (`Navigator.md`, `CLAUDE.md`, `AGENTS.md`,
  `.github/copilot-instructions.md`) are never filtered.

- **Fixed: a failing command no longer accuses `run_shell` of being stuck.** A
  tool result now carries a three-state outcome (ADR-0116): a completed command
  that exits non-zero, a non-2xx `fetch`, a background process dying in its
  grace period, a refused delegation, or an MCP response with `isError: true`
  is the *work* reporting failure, while only a tool that could not run at all
  (spawn error, timeout, denial) counts as a malfunction. The ordinary
  edit/test debugging loop no longer emits false `ToolStuck` warnings, the
  repeated-failure nudge now says to change what produced the failing output
  instead of suggesting the tool is broken, memory no longer learns "run_shell
  failed N times" from red test runs, and the stuck-threshold message no longer
  claims calls are being stopped when they are not. Old transcripts and event
  logs keep parsing: the wire format is a strict superset.
- **Fixed: `delegate` no longer reports success for a delegation that never
  ran, and a remote MCP failure no longer arrives as `status: success`.** Both
  now carry the reported-failure outcome with their text intact.
- **Fixed: a tool error now takes the same redaction and output bounding as a
  success (ADR-0117).** Error text — including synthesized denials and gate
  blocks — is redacted and bounded at the dispatch chokepoint, so no result the
  model sees bypasses the safety invariants.
- **Improved: `print` mode now reports failures and survives event-stream lag
  (ADR-0118).** The `handoff:` line gains `tool_failures`,
  `reported_failures`, and `stuck_tools`; failing tool calls, warnings, and
  stuck signals appear as bounded one-line stderr diagnostics while stdout
  stays the answer alone; a printer that falls behind the event stream skips
  the dropped events and keeps printing instead of silently truncating the
  answer; and stderr writes are checked (a closed stderr silences diagnostics
  without cancelling the turn).

- **Internal/Added: opt-in local-IPC server transport groundwork.** A new
  `localpilot-server` crate provides a cross-platform framed local transport
  (Unix domain socket / Windows named pipe, reusing the existing LF-delimited
  JSON framing) and daemon lifecycle (detached spawn, a retry-connect ready
  handshake, and single-owner exclusivity with stale-endpoint reaping). Built
  from safe `std` + `tokio` only — no `unsafe`, no `libc`/`nix`. No user-facing
  change: there is no `serve`/`connect` command yet and no session hosting over
  it; this is transport/lifecycle groundwork alongside the unchanged stdio
  drive. (`localpilot-rpc` widened `JsonRecordReader` to `pub` so the new crate
  reuses its framing instead of duplicating it.)

- **Internal: the provider layer is split into isolated crates.**
  `localpilot-llm` is now a thin umbrella over `localpilot-llm-core` (the shared
  provider trait, stream events, errors, auth, headers, request shapes) and one
  crate per adapter (`localpilot-llm-openai`, `localpilot-llm-anthropic`). Editing
  an adapter re-checks only its ~1.5k-line crate instead of the whole provider
  layer. No user-facing change — the public API, CLI, config, and provider
  behaviour are byte-for-byte identical.

- **Improved: `harness wait-resume` now actually waits and escalates.** On a
  provider quota/rate limit the paused-run marker now records the real provider
  id and a pause-attempt count that grows the backoff window across repeated
  pauses (instead of a fixed window). `wait-resume` now waits out the pause
  window — re-checking the safety gates and cancellation on a bounded poll,
  honouring `quota.max_wait_minutes` — and then resumes, instead of printing an
  ETA and exiting. Cancellation (Ctrl-C) ends the wait; an explicit `--provider`
  that differs from the paused run is treated as a provider change.

- **Added: `localpilot import claude-code`.** Import a Claude Code session
  (`~/.claude/projects/.../<id>.jsonl`) as a resumable LocalPilot session. The
  history is text-flattened — tool calls and results become plain-text markers
  and provider-specific reasoning is dropped — so it resumes safely under any
  provider; it is redacted on write like any session. Resume it by name with
  `localpilot --resume imported_cc_<id>`; it shows a `[cc-import]` badge in
  `session list`. A re-import never overwrites an existing session or steals its
  name (use `--force` to import again under a new name).

- **Added: already-seen read elision (opt-in).** With `[tools] elide_seen_reads`
  on, a `read_file` that returns a file+range already read this session and
  unchanged since (same mtime and length) is replaced with a compact stub
  pointing at the earlier read, instead of re-spending the context on an
  identical body — a real saving on read-heavy loops. Conservative and off by
  default: a changed file (or any doubt) always returns full content, never a
  stale stub, and the elided read still records as a successful `read_file`, so
  require-prior-read and the scorecards are unaffected.

- **Improved: mid-turn steering is now a typed soft-interrupt substrate.** User
  input steered into a running turn (already admitted at a safe boundary) is now
  one case of a typed soft interrupt that also carries a source (user / system /
  background task) and an urgency flag. A non-user message is labelled so it does
  not read as user-typed input; every injection is recorded as a durable
  `SoftInterruptInjected` event for replay. A steer that arrives as a turn would
  otherwise finish now keeps the turn going so the model sees it (Point B), and an
  urgent interrupt is admitted between tool calls, skipping the rest of the batch
  while keeping the tool_use/tool_result contract valid (Point C). The
  system/background-task producer path is available as a library surface for
  later work; only user steering produces interrupts today.

- **Improved: memory retrieval now fuses keyword and semantic rankings.** When
  the stored-vector rerank is enabled (`[retrieval] rerank` + an embedding
  endpoint), memory injection now blends the keyword (bm25) ranking with the
  dense (cosine) ranking via Reciprocal Rank Fusion, so a memory both retrievers
  agree on rises instead of letting a single noisy cosine dominate. Keyword search
  stays the candidate floor and the default (rerank off / no embeddings) path is
  byte-identical. New `[memory] injection_dedup_ttl_turns` (default `0`, off)
  suppresses re-injecting a memory already shown within the last N turns so a
  persistently-relevant lesson doesn't crowd out the rest. The audit still equals
  the injection.

- **Added: Anthropic prompt caching (opt-in per provider).** Set
  `prompt_caching = true` on an `anthropic` provider to place an ephemeral
  `cache_control` breakpoint on the stable prefix (tools + the stable system
  prompt), so it is cached across turns and re-served at ~0.1× cost instead of
  re-sent in full — a large cost/latency cut on a multi-turn session. The
  per-turn volatile context (memory, project instructions) stays after the
  breakpoint. Off by default. Cache tokens are now accounted (`cache_creation` /
  `cache_read`) and served-from-cache tokens show as `cached:N` in the footer;
  OpenAI's automatic `cached_tokens` are accounted the same way.

- **Cleanup: removed dead surfaces and corrected drifted docs.** Deleted unused
  internal code (an unwired provider retry policy, an unpopulated `cost_usd`
  footer estimate with no pricing source, and the unwired `write_loop_lesson`
  writeback) and the dead `--mode` CLI-override plumbing. Corrected the docs that
  advertised unbuilt surfaces: `--mode` and `--replan` are not flags (mode is
  selected by subcommand; re-running `harness plan` regenerates the plan), and
  the loop-outcome writeback + `[memory] outcome_downweight` are documented as
  reserved/not-yet-wired. Reserved config keys (`[harness] mode`,
  `[memory] outcome_downweight`) and the `ManualPin` pack source are unchanged
  (kept for SemVer-stable config + future use). The skills research report now
  says a skill is *eligible* for `skill_load` rather than claiming it was loaded.

- **Security: writes to a secret-like path now prompt even in a trusted
  workspace.** A trusted in-workspace write was auto-allowed with no regard for
  the target, so overwriting `.env`, an SSH key, or another credential file was a
  silent Allow. Secret-like writes now prompt (and deny non-interactively) under
  `default` and `relaxed`, matching how secret-like *reads* are already gated —
  the allowlist can no longer relax them either. Ordinary in-workspace writes are
  unchanged.

- **Fixed: large tool output is no longer lost past 64 KiB.** A tool result
  larger than the old per-tool 64 KiB cap was truncated *before* it reached the
  retention store, so `read_tool_output` could only page back the first 64 KiB.
  Tools now hand their full output to the single dispatch-seam bound, which keeps
  a head/tail view in context and retains the complete output — so an oversized
  `search_text` or `run_shell` result is recoverable in full.

- **Fixed: phase-cadence quality-gate checks now run.** A ratified check with
  `cadence = "phase"` (a whole-suite test, dependency check, or `audit`) is
  evaluated at the plan boundary — when a completed step leaves no incomplete
  step — instead of never running. A blocking phase finding (e.g. a failing
  audit) stops `harness resume` with its reason rather than reporting a clean
  completion. Step-cadence checks are unchanged.

## v2.6.0 - 2026-07-27

Coordinated LocalX release.

- **Security: updated `tar` to 0.4.45** (RUSTSEC-2026-0067, RUSTSEC-2026-0068).
  This is the crate that unpacks downloaded release archives, so both advisories
  sit directly on the update path: one lets `unpack_in` chmod arbitrary
  directories by following symlinks, the other mis-handles PAX size headers.

- **One-line install, no toolchain.** `install/install.sh` and
  `install/install.ps1` now install prebuilt binaries when run standalone:

  ```sh
  curl -fsSL https://raw.githubusercontent.com/C0deGeek-dev/LocalPilot/main/install/install.sh | sh
  ```

  The script downloads the archive for your platform, checks it against the
  published `SHA256SUMS` **before unpacking**, and then hands off to the binary to
  install the rest of the stack — `localmind`, `localbox`, and `localbench` — at
  the same version. The tools are cut as a set and only tested together, so they
  are installed as a set; `localpilot update --all` re-runs it.

  Run inside a checkout the scripts still build that working tree, because a
  developer running the installer in their own clone means their code. `--binary`
  and `--from-source` force either mode.

- **Installed versions are now reachable from your shell.** Every train tool's
  executable is published into one directory (`~/.local/share/localx/bin`, or
  `%LOCALAPPDATA%\localx\bin`), which is the single entry to add to `PATH`.
  `update`, `version pin`, and `version rollback` refresh it, so switching
  versions takes effect where you type rather than only in `version list`.

- **`localpilot update` no longer needs a Rust toolchain.** It downloads the
  archive published for your platform, verifies it against the checksum in the
  release manifest, and only then unpacks it. Building from source stays
  available with `--from-source`, and is the automatic fallback on a platform
  with no published build.

  Each version installs into its own directory, so the running binary is never
  overwritten and an interrupted update leaves the previous version working. New
  `localpilot version list|pin|rollback` inspect and choose between installed
  versions — rollback is a rename, not a download.

  Releases now also carry **build provenance**, signed keylessly through Sigstore,
  so you can verify an archive came from this repository's build:
  `gh attestation verify <archive> --repo C0deGeek-dev/LocalPilot`. There is no
  signing key to trust. This is not OS-level code signing — macOS and Windows may
  still warn about an unidentified developer.

## v2.5.0 - 2026-07-27

Coordinated LocalX release.

- **Releases now ship a wider set of prebuilt binaries, verified.** Alongside the
  existing Linux x86-64, macOS Apple Silicon, and Windows x86-64 archives, a
  release now carries a **static musl** build that runs on any Linux regardless
  of glibc version, and a **Linux arm64** build. Each archive ships a SHA-256
  beside it and the release carries a `manifest.json` indexing what exists.

  Publishing happens once, only when every platform built, and refuses to run if
  one is missing — previously each platform attached its own archive, so a broken
  build produced a release that looked complete and was quietly missing a
  platform. The checksums prove an archive was not corrupted in transit; they do
  not prove who produced it, which needs signing.

- **Subagents: delegate a bounded task to an agent you declare in a file.** A
  `*.agent.yaml` under `.localpilot/agents/` (project) or `~/.localpilot/agents/`
  (everywhere) declares a name, a model, the tools the agent may use, which parts
  of the system prompt it wants, and its instructions. The model reaches them
  through a new `delegate` tool; `localpilot agents list|show` inspects them.

  An agent's tools are always a **subset of the calling session's** — the child's
  registry is filtered from the caller's own and its permission engine carries the
  caller's profile, so delegation changes who asks, never what is allowed. Agents
  nest one level deep, a child returns a bounded summary rather than its
  transcript, and every refusal is readable output that says what to do instead.
  A permission ask raised inside an agent is forwarded to you with the agent
  named, so you are never asked to approve a command you cannot place.

- **New `search_definitions` tool: find declarations, not lines.** Asking "where
  is this defined" through `search_text` returned every call site as a separate
  line and usually needed a follow-up read to find the one declaration among
  them. `search_definitions` resolves each match to its enclosing function, type,
  module, or test and returns that declaration's symbol path, signature, and
  location, with optional `language` and `kind` filters. Repeated matches inside
  one declaration collapse to a single hit with a count. Measured across this
  workspace a broad query returns 3–6× less output; a query that is already
  narrow does not improve, and the tool says so — `search_text` remains the right
  tool for prose, configuration, and "show me every matching line".

  It keeps no index or cache, so there is nothing to ingest and nothing to go
  stale, and it honours the same ignore files, workspace scoping, and permission
  decisions as the existing search tools.

- **An MCP server can be given its own environment, including credentials.** A
  server entry accepted only `command` and `args`, so a server needing a setting
  or an API key could only be configured by exporting the variable before
  LocalPilot started — outside the config file, outside the credential store, and
  invisible to `doctor`. `[mcp.servers.<name>.env]` now takes three deliberately
  distinguishable forms: a plain string for ordinary values,
  `{ credential = "alias" }` naming an entry in the credential store (the
  recommended path — the config holds only the alias), and `{ value = "..." }`
  for a credential written literally into a project-local, git-ignored file.
  Configure nothing and inheritance behaves exactly as before.

  New `localpilot credential set|list|delete` stores those named values beside
  provider keys but in a separate namespace, so `credential set openai` cannot
  overwrite what `localpilot login openai` stored — the separation is structural
  rather than a naming convention. Values are read from stdin, never taken as
  command-line arguments, never printed in full, and there is no command to
  reveal, export, or copy one afterwards. `login` / `logout` are unchanged, and
  credential files written by earlier versions still resolve.

  A referenced credential that is not stored now prevents that server from
  starting at all, so the failure stays a configuration error naming the variable
  and the alias rather than an obscure fault from a server that came up without
  the value it needed. `localpilot doctor` distinguishes command-unavailable,
  credential-missing, startup-failure, and connected, and reports the configured
  variable *names* — never values, in either its human or JSON output.

  Session tool discovery, designated research search, and the `doctor` probe now
  share one resolver and one spawn instead of each launching servers themselves.

  Finally, anything an MCP server sends back is stripped of the credentials that
  server was given. A server can read its own environment, so a credential could
  previously be returned through the handshake, the tool descriptions it
  advertises, a tool result, or a protocol error — and only tool output was
  covered, by pattern-based redaction that cannot match a value issued from the
  credential store. Filtering now happens at the transport, covering all four,
  before anything reaches the model, a transcript, stored output, or a log.
  Values shorter than 8 characters are left to pattern redaction, since matching
  a short string verbatim would corrupt ordinary text. This is defence in depth,
  not a containment boundary: a server that returns a credential encoded or split
  across fields defeats byte-for-byte matching, and the permission engine remains
  the actual boundary. Adding an environment grants no new tool permission.
  (ADR-0101, LocalHub#43.)

## v2.4.0 - 2026-07-26

Coordinated LocalX release.

- **A timed-out or stopped command no longer leaves its work running.** On Linux
  the process-tree reap was a silent no-op: `kill` was invoked as
  `kill -KILL -<group>`, which procps parses as two option words — it exits
  reporting success and signals nothing. So every `run_shell` that hit its
  timeout left the whole tree alive, still holding its ports, pipes, and memory
  for the rest of the session. The call now uses the explicit
  `kill -s KILL -- -<group>` form that actually reaches the process group.
  Separately, `run_background`'s `stop`, `/bg stop`, and session close only
  signalled the child they spawned, which for a shell-wrapped command is the
  wrapper: stopping a dev server killed the shell and orphaned the server. They
  now reap the whole group, *before* killing the child — descendants are found by
  walking links from the parent, so killing the parent first orphans them and the
  walk finds nothing. A regression test drives a real grandchild that outlives its
  parent and fails if the workload survives being stopped.
- **Research resolves search-provider redirect wrappers instead of discarding
  them** (ADR-0100, LocalHub#42). A 3xx no longer ends a candidate URL. The HTTP
  client's automatic redirect following stays off — no hop may bypass the
  allowlist or the audit log — and LocalPilot resolves each hop itself: a
  `Location` is required and resolved against the URL that produced it (so a
  relative target works), only `http`/`https` destinations are accepted, and each
  destination is re-gated by the same decision the first hop passed, through the
  same check the browser renderer uses. A destination needing confirmation stays
  blocked, and a cross-host hop to loopback, link-local, a private range, or an
  unspecified address is refused ahead of the allowlist so an open-web reach
  cannot become an SSRF channel (a host redirecting within itself inherits the
  permission it already had). Chains stop at five hops and cycles are detected;
  cooldown, pacing, timeouts, body bounds, redaction, and admission apply per
  hop. This is what makes an MCP search tool that returns attribution or grounding
  wrapper URLs useful — previously it could find the right source and still
  contribute no evidence. Evidence now records the **final** URL as its locator,
  keeping the proposed URL as provenance, and the retrieval account separates
  redirects *followed* from candidates that ended at one, with a distinct audit
  decision per outcome (`redirect-followed`, `redirect-blocked`,
  `redirect-malformed`, `redirect-cycle`, `redirect-depth-exceeded`) in place of
  the old single `redirect-not-followed`. No vendor host is special-cased.
- **Review-only skill discovery** (ADR-0099, LocalHub#41). `skills research [-g]
  <query>` (and `/skills research …`) discovers relevant skills — installed,
  available in a registered source, or in a newly found public GitHub repository —
  ranks them, and saves a review proposal to `.localpilot/skill-proposals.toml`
  without registering a source or installing a skill. `/research <topic>` runs the
  same lane automatically and adds a separate `Relevant skills` report section. Web
  discovery reuses the `[research.web]` egress (allowlist/disallowlist/audit/
  `--no-web`) with the official public GitHub repository-search API as the
  fresh-install fallback; a rate limit yields a partial result. Skill
  recommendations are never research findings or memory candidates; LocalMind's
  Skills tab reviews the proposals and delegates any mutation back to `skills …`.
- **Skill source repositories and managed installs** (ADR-0098, LocalHub#40).
  A curated way to pull advisory skills from public **HTTPS** Git repositories:
  `skills repo add|refresh|list|delete`, `skills available [query]`,
  `skills install [--repo <id>] <name>` / `--all`, and `skills delete <name>`,
  each available as both `localpilot skills …` and `/skills …` with identical
  behaviour. Sources are explicit cached commit snapshots (adding installs
  nothing; refresh is the only network update and is atomic); a source exposes one
  catalog root (`.localpilot/skills` › `.agents/skills` › `.claude/skills` ›
  `skills` › a root `SKILL.md`) and is rejected as a whole on an invalid or
  duplicate-named manifest. Managed installs copy the full package into
  `.localpilot/skills` (effective through the normal resolver), record provenance,
  never overwrite a same-scope skill, run nothing, and grant nothing; `--all` is
  all-or-nothing. `skills delete` removes only LocalPilot-installed skills and
  refuses hand-authored content. Management is user-only (never a model tool):
  project mutations need a trusted workspace, global mutations add a global-impact
  disclosure, and every network/write/delete discloses its impact and needs an
  interactive confirmation or `--yes`. `-g` selects the user-global scope.
- **A reusable skill can live once in your home directory** (ADR-0097,
  LocalHub#39). Skill discovery now reads a per-user global baseline
  (`~/.localpilot/skills`, `~/.agents/skills`) overlaid by the active project
  (`<project>/.localpilot/skills`, `<project>/.agents/skills`), resolved by the
  manifest `name` into one effective skill per name — project `.localpilot` ›
  project `.agents` › global `.localpilot` › global `.agents`. A winning
  definition replaces the shadowed one whole (never a field-level merge), so
  removing a project override reveals the unchanged global skill again with no
  reinstall. Resolution is enumeration-independent (precedence comes from the
  scope, not `read_dir` order). Workspace trust gates the *project* overlay only:
  global skills are user-controlled content and load regardless, while an
  untrusted project cannot shadow one with checked-in instructions. `skills
  list`/`show` report each effective skill's origin scope, and
  `skill_search`/`skill_load` load the same effective definition; loading stays
  read-only and grants nothing.
- **A UTF-8 BOM in a `SKILL.md` no longer hides every project skill**
  (ADR-0096, LocalHub#38). The manifest parser now strips one optional leading
  byte-order mark before checking for the `---` frontmatter delimiter (all other
  validation stays strict), so a file saved as "UTF-8 with BOM" loads normally.
  Discovery is also resilient: a malformed skill is skipped and reported (by
  path) as a warning rather than aborting the whole set, so one bad file never
  hides the valid ones. `localpilot skills list`/`show` surface the skipped
  entries. The repository's own `.agents/skills` files are normalised to UTF-8
  without a BOM.
- **Research can render JavaScript-only documentation in a headless system
  browser, inside the egress boundary** (LocalHub#37). Built under the optional
  `render-browser` feature, the renderer drives a discovered system
  Chromium/Chrome/Edge over the Chrome DevTools Protocol (no browser is bundled
  or downloaded) to recover a page's post-JavaScript content when a render
  signal fires. It stays strictly within the research boundary: every browser
  request — navigation, redirect, subresource, frame — is gated through the same
  `[research.web]` allowlist before it leaves the machine, http/https only,
  with an unconditional SSRF block on `localhost`/loopback/link-local/private
  addresses; the browser context is ephemeral and cookie-less; the render is
  time-bounded; and every request/block is audited content-free. Without the
  feature or a browser, research records an explicit "renderer unavailable"
  outcome and falls back to iframe recovery.
- **Research detects pages that need browser rendering and stops silently
  returning shell-only evidence** (LocalHub#37, first slice). A fetched page
  whose real content is missing from its initial HTML — an empty single-page-app
  mount, hydration-only markup, an iframe-only body, or a `Loading…` placeholder
  — is now recognised. Research recovers an allowlisted iframe's document through
  the ordinary gated fetch path (with the frame URL as its provenance), and when
  nothing can be recovered it records an explicit "needed rendering" outcome in
  the retrieval accounting and egress audit instead of counting the shell as
  complete. A new `[research.render].mode` (`auto` default, `off` kill switch,
  `always`) governs the behaviour; server-rendered pages are unaffected and
  never trigger detection. The JavaScript-executing browser renderer itself
  lands in the following slices.
- **Research keeps the topic as a contract through decomposition, search, and
  admission, and separates evidence relevance from candidate trust** (ADR-0094,
  LocalHub#36). The original topic is now passed to the relevance classifier
  alongside the sub-question, so a page that answers a generic sub-question but
  is about a different framework/engine is rejected unless the topic asks for a
  comparison. Decomposition is instructed to keep the topic's load-bearing
  constraints in every sub-question; a sub-question that drops them is re-scoped
  with the topic before its redacted query leaves the machine (and before the
  deterministic term-overlap fallback scores it), so a generic sub-question
  cannot silently become a generic web search. Review candidates now name both
  numbers — "evidence relevance 0.85, candidate trust 0.30" — so admitted
  evidence scoring 0.75–0.95 stays distinguishable instead of all collapsing to
  the low unreviewed-trust ceiling; the classifier's short reason is preserved
  in the retrieval accounting.
- Prism-served responses are no longer misreported as truncated streams: the
  Anthropic-dialect stream normalizer synthesizes a missing
  `content_block_start` and flushes held-back visible text before
  `content_block_stop`, while genuine EOFs still surface as truncations.
- **Research evidence passes an admission gate, and promotion can no longer
  write source dumps into memory** (ADR-0087, LocalHub#30/#24). Fetched web
  content is classified for relevance by an already-configured model
  (LocalMind `[inference]` chat first, the default provider second — no new
  model setting) right after reduction; a rejected page is recorded in the
  egress audit and never becomes a finding. Without a usable model, the
  coverage floor now also gates findings and candidates (withheld counts are
  disclosed as a retrieval note), and local knowledge hits are normalized
  relative to their query's best hit. Research candidates carry their
  concise statement and their full bounded source as separate fields: review
  still shows the complete evidence, but promotion writes only a
  reviewer-approved standalone lesson — an unedited excerpt (or a
  navigation-chrome statement) is refused with an actionable error until
  distilled. Legacy fused-body candidates promote unchanged.
- **`knowledge_search` results are structured locators the follow-up tools
  accept** (LocalHub#23). Every result now carries its id, source kind, path
  (with line range for file chunks), normalized relevance, snippet,
  approximate token cost, and an explicit fetchability marker — closing the
  layered-retrieval gap where `knowledge_expand`/`knowledge_fetch` demanded
  ids the search never emitted. Only ingest chunk ids are fetchable;
  accepted-memory, recent-session, and code-graph results say `not
  fetchable`, and passing one to the follow-up tools returns the reason
  instead of a silent miss.
- **The context pack ranks on one normalized relevance scale** (ADR-0086,
  LocalHub#22/#25/#26). Cross-source ranking no longer sums raw scores from
  incompatible scales: every candidate carries a bounded unit relevance
  (lexical sources normalized relative to their query's best hit, session
  facts scored by task overlap, fixed moderate values for graph rows), so
  source-quality/file-match/recency/confidence bonuses have measurable,
  bounded effects for every source. Reserves now require relevance — a
  below-floor candidate cannot claim its source's guaranteed budget (it
  still competes in the shared pool), and an unrelated recent session
  contributes zero entries instead of consuming 15% of the budget. The
  `knowledge_search` window is relevance-ordered over the selected pack and
  withholds below-floor entries rather than padding to `max_hits` (manual
  pins always render; zero results is an honest answer). Raw scores stay in
  the per-entry signal breakdown as diagnostics.
- **`doctor` and `memory status` explain the research-docs pipeline**
  (LocalHub#28). `localpilot doctor` gains a `research docs` line (and a
  `research_docs` JSON object) reporting how many research reports sit on
  disk, whether `[research] ingest_report` is enabled, and the LocalMind doc
  index's chunk/vector counts — so "report exists but ingestion is disabled",
  "ingestion enabled but nothing indexed", and "indexed without embeddings"
  are distinguishable states instead of one silent empty search.
  `localpilot memory status` reports the same doc chunk/vector counts beside
  the memory entry count.
- **Colliding MCP tool names remain usable without invalid provider requests.**
  Builtins and earlier registrations keep their names; a colliding MCP tool is
  advertised as `<server>_<tool>` while calls to its server retain the original
  remote name. If that prefixed name is also occupied, the later tool is skipped
  with a warning so duplicate function declarations never reach a provider.
- **Folder ingest now feeds the LocalMind UI Docs tab** (ADR-0082, LocalHub#18).
  `localpilot ingest run`/`refresh` (and the session-open background build)
  bridge the workspace's Markdown files into LocalMind's documentation index
  (`doc_chunk`), redacted like every persisted chunk, so `localmind ui` can
  browse and semantically search project docs without a separate
  `localmind ingest docs` invocation. Unchanged files are a no-op via a hash
  ledger, vanished files leave the index, and a doc-index failure never fails
  the run. Opt out with `[ingest] docs_index = false`.
- **Harness intake can gate on a guidance score** (ADR-0081, opt-in).
  With `[harness.guidance] enabled = true` (or `--guidance` per run),
  `localpilot harness intake` first has the model enumerate the idea's
  decision axes — resolved (quoting the idea) or not specified — and computes
  a deterministic score (resolved ÷ total). Below the configurable threshold
  intake pauses instead of writing a brief that encodes guesses: on a
  terminal it asks the open questions on stdin (empty answer delegates that
  axis; answers fold into the idea as explicit user decisions), on a
  non-terminal it emits a structured `needs_guidance` JSON report and writes
  no brief; `--assume-judgment` proceeds with the delegation recorded. Axes,
  score, questions, answers, and delegation land in
  `.localpilot/intake.jsonl`. The score is an inspectable signal — an axis
  the model never lists cannot count against it — never proof the idea is
  fully specified.
- **Provider request timeouts now bound silence, not total duration**
  (ADR-0080). `request_timeout_secs` is a stall window — the longest
  tolerated quiet spell while a response is open (to the first byte, then
  between stream chunks) — so a slow-but-streaming local server is never cut
  off mid-response at a hard deadline that then read as a server crash. A
  genuinely silent server now stops the turn immediately with guidance
  (check GPU offload, or raise `request_timeout_secs`) instead of burning
  retries that restarted prompt processing from zero. Bound total turn time
  with `[harness] turn_timeout_secs`.
- **Chat `/research` copy reports the real egress state**. Entering research
  mode no longer claims "web off": the notice reflects the configured
  `[research.web]` state (ADR-0076 disclosure), and the TUI mode/picker
  descriptions match.
- **Research depth is configurable and progress is visible**. New `[research]`
  keys — `max_rounds` (default 3), `per_source_evidence` (5),
  `max_total_evidence` (120), `time_budget_secs` (unset) — feed the retrieval
  loop's bounds, with per-run flag overrides on the subcommand: `--rounds`,
  `--max-questions`, `--time-budget`, and `--quick` (single-pass). Round
  summaries now stream live as each round completes instead of printing at
  the end, the report gains a per-question **Coverage** table (verdict,
  evidence, corroborations, origins), and interactive `/research` Ctrl+C now
  asks the loop to stop at the next boundary and posts the partial report —
  coverage-so-far instead of nothing.
- **Research evidence is deduplicated, diversity-capped, and honestly
  scored** (ADR-0079). Near-duplicate snippets fold into one (the
  duplicate's provenance is kept on the survivor and still counts as an
  independent origin), no single origin can saturate a question once others
  are answering (soft cap, 3 per question per origin), and web evidence is
  scored by content-term overlap with the sub-question instead of a flat
  constant — an off-topic page can no longer read as relevant. Every fold,
  drop, cap, or early stop is reported in a new "Retrieval notes" section
  instead of happening silently. Web fetching is also polite now: repeat
  visits to a host are paced by its own response time, and a 429/5xx cools
  that host down for the rest of the run (audited as `host-cooldown`).
- **Research is now multi-round and coverage-driven** (ADR-0078). Instead of
  one gather per sub-question, the loop scores per-question coverage
  deterministically (relevance floor + distinct-origin independence) and
  re-queries uncovered questions across rounds — retrying the original
  question, adding a drift-guarded pseudo-relevance reformulation, and
  widening retrieval depth — until everything is covered, a round finds
  nothing new, or the round/evidence/time budget is hit. Reports and both
  research surfaces now show per-round progress lines and a coverage summary
  (covered/weak/open), and "open questions" are only the questions that
  stayed empty after follow-up retrieval actually tried.
- **Research can use real web search via designated MCP tools** (ADR-0077).
  Name `(server, tool)` pairs under `[research.mcp] tools` (e.g.
  `tools = [{ server = "search", tool = "search" }]` referencing
  `[mcp.servers.search]`) and web research calls them per sub-question as
  candidate-URL proposers — replacing model-guessed URLs with search results.
  Proposals are leads only: extracted URLs pass the same
  allowlist/disallowlist gate, bounded no-redirect fetch, and audit as
  before, each search call is itself audited with the redacted query, and a
  tool that errors, times out, or rate-limits is skipped without failing the
  run. Nothing is consulted unless explicitly designated. Search works with
  or without a chat model configured.
- **Web research is now on by default** (ADR-0076). Research cannot rely on a
  small local model's parametric memory, so `[research.web].enabled` defaults
  to `true` with open-web reach (an unset allowlist now means `["*"]`), and
  the interactive `/research` surface runs the same web-enabled path as the
  subcommand. Every web-active run still prints the egress disclosure first,
  audits every request, sends only the redacted sub-question off-machine,
  and never follows redirects; `disallowlist` still beats the allowlist.
  New `--no-web` flag skips web for one run; `[research.web].enabled = false`
  remains the absolute kill switch no flag can override; `--web` is now a
  compatibility no-op. **Migration**: an explicitly written `allowlist = []`
  keeps its old meaning (nothing is fetched); users who relied on the old
  default-off posture should set `enabled = false` or pass `--no-web`.
- Web research findings read as prose, not raw HTML. A fetched page used to
  become evidence as its raw markup: a naive tag strip left inline
  `<script>`/`<style>` bodies behind as "junk", and the length budget was
  spent on chrome, so both the finding and its evidence block showed truncated
  page source instead of content. Fetched HTML is now reduced to readable text
  at gather time — whole non-content elements (`script`, `style`, `head`,
  `nav`, `footer`, …) are dropped body-and-all, block tags become line breaks,
  remaining tags are stripped, and common entities are decoded. Gated on the
  response `Content-Type` (with a marker sniff when the server sends none), so
  plain-text, Markdown, and JSON bodies are still kept verbatim. The same
  reducer now backs the excerpt/`Sources:` sanitize pass, so a code/HTML blob
  from any source distils cleanly. Extends ADR-0067.
- `localpilot research` ranks and scores results honestly. A near-empty
  project could surface an unrelated file (e.g. `.idea/modules.xml`) as a
  "finding" purely because one incidental word prefix-matched in a big OR
  query, labelled with a confidence that was actually a hardcoded flat prior
  rather than a measure of match quality. Fixed: `.idea`, `.vscode`, `.vs`,
  `.settings`, and `.fleet` are now excluded from ingestion by default
  (existing indexes need `localpilot ingest rebuild` to drop stale chunks);
  a term-coverage floor in LocalMind's shared search path now requires a
  multi-term query to actually match several of its terms, not just one;
  and research finding/candidate confidence is now derived from each
  evidence's own relevance instead of a flat constant. `--web` also now
  prints an explicit note when it silently contributed no evidence, instead
  of leaving a spurious local match as the only visible result.
- `localpilot research` can index its report into LocalMind. A new opt-in
  `[research] ingest_report` (default off) also ingests the written report into
  LocalMind's documentation index (`doc_chunk`), so research output is
  semantically searchable and shows up in the LocalMind UI — reusing the
  `localmind ingest docs` chunker in-process. Best-effort and idempotent; the
  manual `localmind ingest docs .localpilot/research` remains available.
- `/research` findings reach the review queue again. A recent change made
  research candidates that were reduced to a source excerpt "report-only", but
  because research synthesis is heuristic (every finding is a gathered excerpt)
  that silently enqueued zero candidates for the common case, so nothing showed
  up in LocalMind's review UI. A backed research finding is now enqueued as a
  review-gated candidate carrying its distilled one-line statement (the raw
  source blob still stays in the written report only). See ADR-0072.
- **Out-of-workspace reads are grantable instead of a dead end** (ADR-0070).
  The `bypass` profile now asks for an out-of-workspace path interactively
  instead of hard-denying (it was weaker than `default`); a new
  `[permissions] extra_read_roots` gives standing read-only grants honored in
  every profile and non-interactively (writes keep the hard workspace
  boundary, secret-like reads keep their gate, bad entries are reported and
  skipped); and a new `unrestricted` profile (`--permission unrestricted`,
  `/unrestricted`, or config) approves everything with no prompts — never the
  default, surfaced in the footer in the strongest warning style, redaction
  and logging stay on. An out-of-workspace denial now names the target and
  all three remedies in the model-visible error.
- Permission profile slash commands (`/default`, `/relaxed`, `/bypass`,
  `/unrestricted`) now apply mid-turn (ADR-0071). The permission engine sits
  behind a shared, swappable handle snapshotted per tool call, so a switch
  takes effect from the running turn's next tool call instead of waiting for
  the turn to finish; the idle path writes through the same handle so the two
  paths cannot diverge.
- Quitting no longer looks like a hang: each slow close-out learning stage
  (model-backed lesson extraction, then the bounded code-graph reindex) runs
  under a progress line on stderr — spinner-animated on a TTY and cleared so
  the stage summary prints on a clean row, printed once on a non-TTY so
  captured logs stay clean — instead of silence until it finishes.
- Permission prompts describe the actual risk. An in-workspace read or write
  that asks only because the session is untrusted (the default-profile floor)
  now reads "read a file" / "write a file" instead of falsely claiming
  "read outside the workspace"; out-of-workspace writes now say so. One
  shared label serves the TUI and every wire adapter.
- A driven session learns from its driver: corrections made over
  `localpilot mcp serve` — steers, cancellations, permission denials — are
  recorded as `driver_intervention` events in the session event log (named
  after the client from the MCP handshake) and offered on disconnect as
  review-gated lesson candidates labelled `driver-intervention`, so a
  frontier coach's redirects can become promoted memory after human review.
  Approvals stay event-log-only; candidates are capped per session. See
  [docs/localmind-integration.md](docs/localmind-integration.md#driver-interventions-ride-the-same-bridge).
- Harness scorecards carry the shared contract's new `interventions` field:
  the process block counts the `driver_intervention` events an `mcp serve`
  coach recorded on the session log, so a coached run's scorecard reports how
  much external steering it took (an undriven run reports zero).
- New `localpilot mcp serve`: serve the session runtime as an MCP server
  (protocol 2025-06-18) on stdio, so an MCP client — an agent host like
  Claude Code or Codex — can drive and steer a session through tools:
  `prompt` (with mid-turn `steer`/`follow_up` dispositions), `cancel`,
  `status`, `transcript`, a cursor-paged `events` feed with a bounded wait,
  and `reply_permission`. Permission decisions stay in the engine; an
  unanswered ask is denied, and `--no-approvals` withholds the reply tool for
  watch-and-steer coaching. Supports `--continue`/`--resume` like `rpc`. See
  [docs/embedding.md](docs/embedding.md#mcp-over-stdio).
- RPC: `localpilot rpc` accepts `--continue` (most recent session in the
  workspace) and `--resume <id-or-name>`, matching `chat`, so a headless
  driver can pick an earlier session back up across process restarts. The
  `hello` reply reports the resumed session's id; the current permission
  profile and trust state apply, never the resumed log's. See
  [docs/embedding.md](docs/embedding.md#rpc-over-stdio).
- Install: `install.ps1`/`install.sh` keep LocalMind pinned to its tested
  release commit for a release build of LocalPilot, but a dev build (working
  tree not exactly on a clean version tag) now fetches and checks out
  LocalMind's latest `main` instead, so iterating on both repos together
  doesn't get stuck on a stale pinned snapshot. See
  [docs/localmind-integration.md](docs/localmind-integration.md#pin-policy-pinned-for-releases-floating-for-dev-builds).
- Chat: Ctrl+C is now staged like a shell. With a prompt typed (or a slash /
  `@`-mention autocomplete open), the first Ctrl+C clears the composer and
  dismisses the overlay; a second Ctrl+C on an empty composer quits. On an empty
  composer it quits right away. (Esc still quits immediately.)
- Sessions can be named and resumed by name. In `chat`, `/name <text>` (alias
  `/rename <text>`) names the current conversation; the name shows in the header,
  the status line, and beside the id in `/sessions` and `session list`. Resume by
  that name anywhere an id is accepted — `chat --resume <name>` (with a new
  `--continue` for the most recent session), `print --resume <name>`, and
  `session resume <name>` — no flag needed to tell a name from an id, since an id
  is a UUID. `session name <id|name> <new-name>` names or renames from the shell.
  Names are unique per workspace and stored in the session index, not the
  transcript.
- Chat: pasting an image works more reliably. An explicit paste re-resolves the
  provider's vision capability (config > probe) before refusing — catching a
  vision server that came up after startup — and a clipboard read that fails for
  any reason other than "no image present" now always shows a notice instead of
  doing nothing silently. When the model still isn't known to accept images, the
  notice names both ways to enable it (`supports_vision` or `[discovery]
  vision_probe`).
- The agent now avoids dumping a whole project into one giant file: the always-on
  prompt steers it to split a large implementation into smaller modular files,
  and `write_file` refuses a single payload over a soft 64 KiB limit — steering
  to split or to use `append_file` — so an oversized call that would be truncated
  in transit is prevented, not just recovered from (complements ADR-0038).
- Research findings are now concise claims, not pasted source chunks. A finding
  whose text is a code/HTML blob (or is over-long) is reduced to a short,
  single-line excerpt and its raw text is carried separately as evidence,
  rendered in a fenced block that can't break the report layout. This also stops
  raw blobs from leaking into enqueued memory candidates.
- Research web egress: `[research.web].allowlist` now accepts `*` (all hosts)
  and `*.example.com` (domain + subdomains), and a new `disallowlist` blocks
  specific domains even when the allowlist would permit them (disallow is
  checked first and wins). Lets you allow broad access while carving out
  specific domains. Fail-closed defaults are unchanged.
- `learning review list` is now readable: each row leads with a bracketed id
  and category and the body is shown as a single-line snippet (long bodies are
  truncated); `review show <id>` still prints the full entry.
- Chat: `/research` now appears in the slash-command autocomplete list.
- Chat: Ctrl+C exits the app even while a slash command is being typed; the
  autocomplete overlay no longer captures the global quit key (the `@`-mention
  picker is fixed the same way).
- The embedded LocalMind engine advanced to current main, bringing the
  documentation index and semantic doc search, the cross-device sync
  foundation (`sync_meta`), the store-level embed flag the folder-ingest
  bridge relies on, and the UI/store walk-up resolution into the bundled
  copy.
- Fixed a `--features tui` build break (a borrow conflict in the research
  prompt output capture) that plain `cargo check` missed.

## v2.3.0 - 2026-07-07

Coordinated LocalX release.

- The harness spec's discard/reset recovery rung is implemented (ADR-0066):
  a rule set to the new `discard` severity (`[harness.rules]`, e.g.
  `quality_gate = "discard"`) abandons a failed attempt and restores the
  working tree to committed state before the fresh attempt, instead of
  iterating in place. Off by default; the Retry-only ladder is unchanged
  without the config.
- `verify_before_done` (and `verify_command`) are honored in interactive chat
  and the rpc wire client, not only `session`/`eval` — a parity test now pins
  the harness config keys across all three entry points.
- `login` no longer stores a key the provider actively rejected; the error
  names the `--no-verify` override for gateways and offline setups (a
  network/validation failure still stores with a warning, unchanged).
- A quota pause-marker write failure is logged instead of silently swallowed
  (a later `resume` cannot see a pause window that was never persisted).
- Docs: SECURITY.md and the historical release plan no longer name moving
  version literals; the architecture doc gains the missing
  `localpilot-selfreview` and `localpilot-verify` crate sections; the README
  session verb list matches the CLI (`prune`, not `fork`).
- Memory-injection retrieval honors LocalMind's `[retrieval] rerank` /
  `rerank_window` keys: with rerank opted in and an embedding endpoint
  configured, the top keyword candidates are reordered by the same
  stored-vector cosines the relevance gate already computes (ADR-0065,
  engine contract D-LM-0026). Default off; the injected order is
  byte-identical when off.
- The embedded LocalMind engine was advanced, bringing its SQLite
  concurrency pragmas (WAL + busy timeout at every open — the session and
  the standalone CLI share one database file) and `status`/`eval` CLI
  fixes into the bundled copy.

## v2.2.0 - 2026-07-06

Coordinated LocalX release.

- Interactive-session hardening: child processes and the terminal are now
  isolated from each other so a session can no longer freeze, lose Ctrl+C, or
  have its display corrupted.
  - **Child processes never take the interactive stdin or console.** Every child
    spawned while the TUI may own the terminal — `run_shell` and its subtree,
    background processes, MCP servers, and the stream-editor — gets a null stdin
    (a child that reads stdin was consuming the TUI's keystrokes, including the
    Ctrl+C key event raw mode depends on) and is detached from the console: on
    Windows its own invisible console via `CREATE_NO_WINDOW` (a shared console
    let any child or grandchild read `CONIN$` or re-cook the console mode), on
    Unix a non-foreground process group so a direct `/dev/tty` read gets
    SIGTTIN. Pinned by `spawn_invariants` source tests.
  - **All UI text is scrubbed of terminal-control bytes before it can render.**
    A degenerating local model's deltas, colored tool output, an ANSI-laden
    notice, or a hostile update tag could previously reach the terminal raw and
    flip its charset/wrap/keyboard-protocol modes out from under the TUI. Every
    `UiEvent` text payload is now stripped of C0/C1 controls and whole ANSI
    CSI/OSC sequences; the streaming path carries an incomplete trailing escape
    across deltas (bounded) so a sequence split over two deltas is still
    swallowed whole. Pastes route through the same scrub.
  - The `chat` TUI no longer installs the default terminal log subscriber while
    it owns the terminal, so a mid-session tracing event can't print raw lines
    into the inline viewport (file logging via `LOCALPILOT_LOG` is unaffected).
  - A panic under the event loop now restores the terminal (raw mode, keyboard
    enhancement flags, bracketed paste) before the panic message prints, and a
    launch-banner failure falls through to terminal teardown instead of leaving
    the shell raw.
  - Resuming a session replays the tail of its conversation into the transcript
    instead of showing a blank screen; `/compact` and `/research` (and
    research-mode prompts) run through the event pump so the UI stays live and
    Ctrl+C cancels them; session close-out now learns from the session the user
    actually ended in after `/new`, `/continue`, or `/fork`.
  - Bumped `localx-eval-core` to the revision that isolates gated-check children
    from the host terminal (the same stdin-inheritance class of bug in the eval
    `CheckRunner`).

- Removed three harness rules that were declared and unit-tested but never
  evaluated on any live path: `workspace_boundary`, `secret_file_guard`, and
  `test_first_when_configured`. Workspace containment and secret-file protection
  are enforced solely by the permission engine at the tool-dispatch choke-point
  (`dispatch_gated → PermissionEngine::decide`) on every profile including
  `bypass` — the rules mirrored that boundary without ever firing, so two of them
  carried a misleading `critical` flag. Their now-orphaned `RuleContext` fields
  and the unused `pre_edit` trigger were removed with them. No enforcement
  changes: the permission engine is unchanged. The harness spec's Runtime-status
  note now states plainly that these properties are the permission engine's, not
  the rule engine's.

## v2.1.5 - 2026-07-04

Coordinated LocalX release.

## v2.1.4 - 2026-07-04

Coordinated LocalX release.

## v2.1.3 - 2026-07-03

Coordinated LocalX release.

- The harness step-completion gate no longer dead-locks its own commit. The
  `progress_updated` rule was made runtime-active but kept its `block` default,
  so once the gate re-read `PROGRESS.md` it refused to commit any step the model
  had not already ticked — even though the harness ticks `PROGRESS.md` itself as
  it commits. The rule is now advisory (`warn`) by default (still configurable to
  `block`), restoring the commit-and-tick flow.

## v2.1.2 - 2026-07-03

Coordinated LocalX release.

- The interactive TUI build compiles again. `localpilot-cli`'s `tui`-gated
  modules call `tracing`, but only `tracing-subscriber` was declared, so the
  release build (`--features tui,learning`) had failed since 2.1.0 — the v2.1.0
  and v2.1.1 LocalPilot release artifacts never built. Declaring the `tracing`
  dependency restores it (the trust-gate fix below now reaches a published
  release).

## v2.1.1 - 2026-07-03

Coordinated LocalX release.

- The first-run "Trust this folder?" prompt is no longer clipped: the inline
  live region now grows to fit a modal gate (the trust prompt or a tool
  approval) so its `[y]/[n]` choice line is always visible, instead of falling
  below a fixed-height band. Streaming keeps the fixed band, so a per-token
  redraw never resizes the viewport.

## v2.1.0 - 2026-07-03

Coordinated LocalX release.

- Research web egress no longer follows HTTP redirects: a 3xx is treated as a
  miss and audited, so an allowlisted host cannot bounce a fetch to an
  off-allowlist destination.
- The headless completion gate now evaluates the progress rule against the real
  `PROGRESS.md` — it flags a step claimed done but not ticked, instead of always
  passing.
- Silent failures on the live session path now warn: a failed workspace-trust
  persist (which would otherwise re-prompt every session) and a failed
  background project-knowledge index build (which would otherwise make
  `knowledge_search` return nothing/stale results with no cause).
- Docs: the harness spec now states which baseline rules are runtime-active vs
  declared-only (workspace containment and secret-file reads are enforced by the
  permission engine); alpha-era wording removed from the user docs.
- Internal: removed dead public API with no callers.

## v2.0.2 - 2026-07-02

Coordinated LocalX release.

- **Exiting the REPL no longer waits for background work.** The
  first-session knowledge ingest runs detached; the runtime previously
  waited for it on shutdown, so quitting hung after the closeout line
  until the walk finished. Interrupted ingests resume on the next
  session open.
- **CI integration suites exec the prebuilt test binary** instead of
  `cargo run` per invocation (a nested-cargo build-lock hang killed the
  Linux test job on every run since June 27; Windows intermittently
  failed replacing the running executable).
- **Supply-chain gate healed**: the first-party `localx-llama` git tier
  is allow-listed with its lockstep-pin justification, `anyhow` is
  pinned to the patched 1.0.103 (RUSTSEC-2026-0190), and `quinn-proto`
  to 0.11.15 (RUSTSEC-2026-0185).

## v2.0.1 - 2026-07-02

Coordinated LocalX release.

## v2.0.0 - 2026-07-02

Coordinated LocalX release.

- **The eval primitives moved to the shared `localx-eval-core` crate.** The
  capability-scorecard wire contract, discipline metrics, blinded judge core,
  ablation, gate-mediated check runner, and verify-command detection now live
  in the public `localx-llama` repository (consumed as a rev-pinned git
  dependency) so LocalBench can grade against the same contract. LocalPilot's
  public API is unchanged — host-bound adapters re-export the shared names.
  Recorded as ADR-0062; ADR-0063 and ADR-0064 record the ecosystem's
  in-process no-think filter and native TUI doctrine.

## v1.2.1 - 2026-07-01

Coordinated LocalX release.

- **The default REPL honours the configured `[permissions] profile`.** `localpilot`
  with no subcommand (the interactive REPL) previously always ran with the
  `Default` profile, ignoring `[permissions] profile = "bypass"` in config. It now
  resolves the profile from config, so a project (or LocalBox's bypass opt-in) that
  asked for bypass actually runs bypassed instead of prompting per action.

## v1.2.0 - 2026-06-30

Coordinated LocalX release.

- **Vision (image input) is a resolved per-provider capability (ADR-0061).**
  LocalPilot no longer assumes every local OpenAI-compatible server is text-only.
  A model's vision support resolves in precedence **config > probe > false**: a new
  per-provider `supports_vision` flag (user-set, or auto-written by LocalBox when it
  loads a multimodal projector) wins; otherwise a best-effort, **read-only** probe
  of a local llama.cpp server's documented `GET /props` `modalities.vision` (no
  model inference; toggleable via `[discovery] vision_probe`, default on; an
  unreachable/signal-less server is treated as unknown, never a false claim);
  otherwise text-only. The OpenAI adapter's image-input gate becomes "official API
  **or** vision resolved true", so an undeclared provider is byte-identical to
  before. `doctor` reports the declared capability and `localpilot models` the full
  resolved capability and its source; the interactive image-attach preflight now
  refuses with actionable guidance (how to declare `supports_vision`) instead of
  sending an image blind. No `GET /v1/models` augmentation and no active trial-image
  probe. See `docs/04-provider-contract.md` §Vision and `docs/configuration.md`.

- **New `/research` mode and `localpilot research` subcommand (ADR-0060).** A
  bounded research loop decomposes a topic into sub-questions, gathers evidence
  across local sources (ingested knowledge + accepted memory), cross-checks each
  finding against its evidence, and produces both a redacted Markdown report and
  **review-gated** memory candidates (never written to accepted memory). It is
  reachable interactively (`/research <topic>` one-shot; bare `/research` enters a
  persistent research mode) and headlessly (`localpilot research <topic>`, with
  `--no-report`/`--no-memory`). When a provider and model are configured the model
  decomposes the topic; synthesis stays grounded in gathered evidence so a finding
  is always backed. The loop lives in a new host-neutral `localpilot-research`
  crate. **Web research is off by default** and reachable only via the headless
  `localpilot research --web` opt-in, which prints an egress disclosure, fetches
  only allowlisted domains (others are skipped and logged), sends only the redacted
  sub-question, and audits every request; `[research.web] enabled = false` is the
  kill switch. Configure under `[research]`; see `docs/configuration.md` and
  `docs/07-security-and-privacy.md`.

- **Outcome-aware down-weight wired to the uplift eval (ADR-0046/ADR-0059).** The
  engine's reasoned route-to-review flag was built but never wired to an outcome
  signal. It is now wired to the uplift A/B eval (not a live turn — one turn is too
  weak a signal): when an arm that injected a set of lessons under-performs its
  control, those lessons are routed to review (never deleted) for a human to
  re-judge, joined by the per-turn `memories_used` audit. Off by default
  (`[memory] outcome_downweight`); only `memory`-layer ids are eligible; reversible.

- **Semantic relevance gate at memory injection (ADR-0059).** Accepted-memory
  injection was gated only by keyword bm25 score (unnormalized, not portably
  tightenable), so a same-language but off-topic lesson could inject into an
  unrelated task and mislead the model (the negative transfer seen in the v1.1.0
  sweep). The injection layer now embeds the prompt once per turn and scores each
  keyword candidate by normalized cosine over the stored vectors, gating any hit
  below `[memory] injection_min_cosine` (default `0.6`; `0.0` disables). Because
  cosine is normalized it ships **default-on**, but it is **best-effort**: with no
  embedding endpoint (or an unembedded lesson) the hit carries no cosine and is
  injected exactly as on the keyword path — a no-embed run is byte-identical. The
  keyword search stays the candidate floor; cosine only re-filters. Reuses the
  engine's `embed_query` + global-aware `vector_search`. See
  `docs/configuration.md`.

## v1.1.0 - 2026-06-29

Coordinated LocalX release.

- **`localpilot eval` verifies the build before finishing, by default.** The
  verify-before-done gate is now **on by default for `eval`** (opt out with
  `eval --no-verify`, which reproduces the prior behaviour byte-for-byte), so a
  benchmark measures compiled+tested solves instead of code the model never
  built. Interactive and `print` turns are unchanged (the `[harness]
  verify_before_done` config default stays `false`). Stack detection gains a
  C++ branch: a workspace with C++ sources at the root (a CMake project or a
  bare exercism layout) is compile-checked with an artifact-free
  `g++ -std=c++17 -I. -fsyntax-only <sources>` — catching "it never compiled"
  without writing build artifacts into the captured diff. When the gate is on
  but no target is detected, a warning makes the un-verified finalize visible.
  The gate runs in the workspace's de-verbatim cwd (see above), so its build
  command no longer ran in a fallback directory on Windows. The legacy
  `--verify` flag is accepted but redundant.
- **Edit tools tolerate indentation drift and guide a failed edit.** `edit_file`,
  `multi_edit`, and `apply_patch` now share one anchored matcher: an exact unique
  match first, then a single leading-indentation-tolerant rung that applies only
  on a *unique* block whose indentation differs by one consistent whitespace
  prefix (re-indenting the replacement to the file), then a guiding error — the
  match count for an ambiguous edit, or the nearest existing line plus a re-read
  hint for a not-found one — instead of a bare "old_text was not found". An empty
  or identical-to-`new_text` `old_text` is rejected. Matching stays anchored,
  never fuzzy (no best-guess location); CRLF handling and `multi_edit`/
  `apply_patch` atomicity are unchanged. This cuts the "model gives up and
  rewrites the whole file" failure when its `old_text` indentation is slightly off.
- **The Windows shell prefers PowerShell 7 (`pwsh`), so `&&` chains work.** A
  `run_shell` `command` string runs through `pwsh` when it is on PATH, falling
  back to `powershell.exe` (Windows PowerShell 5.1) otherwise. `pwsh` supports
  the `&&`/`||` chain operators that 5.1 lacks, so a chained command
  (`cargo build && cargo test`) runs as written instead of erroring — which is
  what taught the learning corpus junk "PowerShell doesn't support `&&`" lessons.
  Detection is cached; it is *prefer*, not *require* (a host without `pwsh` still
  works with `;`). A timed-out command's whole process tree is killed
  (`taskkill /T /F`; a process-group `kill` on Unix), confirmed by test, so a
  hung build's grandchildren (`make`→`cc1`, `gradle`→daemon) never orphan.
- **Child processes run in the workspace on Windows, not a fallback directory.**
  The sandbox canonicalizes the workspace root to a verbatim extended-length path
  (`\\?\…`); handed to a child process as its working directory, a launched shell
  could not use it (cmd fell back to `C:\Windows`, PowerShell resolved relative
  paths against a broken `$PWD`), so every model-issued build/test command ran
  *outside* the workspace and failed. The shell, git, background, and
  verify-before-done child processes now spawn in a de-verbatim equivalent of the
  same directory (`Workspace::process_dir`, via `dunce`), while the verbatim
  containment root and its `starts_with` boundary are unchanged. Windows/Linux/
  macOS parity; no behaviour change off Windows.
- **Ingest keyword retrieval ranks by FTS bm25, and short query terms match whole
  tokens.** `knowledge_search`'s keyword tier now ranks by the FTS index's own
  **bm25** score (IDF-weighted, so a common token like `and` ranks far below a
  rare one), with the file-path column weighted above the body — replacing the old
  flat term-count + substring path bonus. Query terms of 3+ characters still match
  as prefixes (`pars` → `parser`); shorter terms match a whole token exactly, so
  `an` no longer matches `and` (and `do` no longer matches `docker`). This is a
  deliberate ranking change (ADR-0057, refining ADR-0025) — it reorders some
  results by design; the hybrid keyword-floor/vector blend shape is unchanged.

- **`knowledge_search` is hybrid keyword+vector retrieval when embeddings are
  configured.** With an embedding model set (and reachable), the query is embedded
  and the cosine-nearest chunk vectors are blended into the keyword results, so a
  semantically-relevant chunk the keyword query missed is recalled. Keyword
  (term-match) hits stay the **floor** — a keyword hit always ranks above a
  vector-only hit, so a strong keyword hit always surfaces; cosine only
  sub-orders. With no embedding model, or when the endpoint is unreachable, the
  result is **byte-identical** to the prior keyword-only ranking (a bounded vector
  window keeps the pass cheap).

- **Ingested chunks are embedded on ingest (best-effort, opt-in) into a chunk
  vector index.** When an embedding model is configured (the same
  `[inference]` embedding gate accepted-memory embedding uses — the local CPU
  embed server), each ingested chunk is embedded into a new rebuildable
  `ingest_chunk_vectors` table (schema v4, mirroring the accepted-memory
  `vector_index` shape). It is **best-effort**: an unchanged chunk is not
  re-embedded (content-fingerprinted), a down/unconfigured endpoint writes no
  vectors and never fails ingest, and chunk vectors are dropped with their chunks.
  With no embedding model configured this is a no-op, so ingest stays exactly the
  keyword path. `ingest run`/`refresh` report `embedded: N of M chunks` when
  embeddings are active. New `[ingest] embed_chunks` (default `true`) opts out of
  the per-chunk ingest embedding cost while keeping accepted-memory embeddings.

- **Ingested folder knowledge is language-tagged and `knowledge_search` filters
  to the workspace language.** Each ingested chunk now records its file's
  programming language (reusing LocalMind's `language_for_extension` map — the
  same one accepted-memory tagging uses), and `knowledge_search` filters hits to
  the workspace's dominant language (via `detect_workspace_language`), excluding
  off-language chunks while keeping language-neutral (`NULL`-tagged, e.g. docs)
  chunks eligible. A docs-only or mixed workspace detects no dominant language and
  applies no filter, so keyword retrieval stays byte-identical to before. The
  chunk store migrates additively (schema v3, nullable `language` column;
  pre-existing chunks read as untagged until re-ingested).

- **Accepted memory now has a proactive lifecycle: usage tracking + a freshness
  pass + an operator surface.** A memory's hit count is bumped when it is injected
  into a turn (best-effort, post-turn, off the retrieval path), so dead weight and
  high-value lessons are both visible. New `localpilot learning freshness` flags
  stale / never-retrieved / version-sensitive accepted memory **for review** — by
  age, never-retrieved-after-a-grace, and a version-sensitive heuristic, across the
  project and global stores (`--scope project|global|both`); it is **dry-run by
  default** (`--apply` writes), bounded by a per-run cap, and **never deletes** — a
  flagged lesson is resolved through the existing `learning review` / `memory
  delete` path. `localpilot learning lifecycle` lists the queues (flagged,
  never-retrieved, most-used, contradicted). Both honour `--format human|json`.

- **Optional source re-validation (`localpilot learning revalidate`, opt-in,
  default-off).** Asks the configured local model whether version-sensitive
  accepted lessons are still current and flags "no longer true" ones **for
  review** — never deletes. It is **network-touching and disclosed**: a preview
  (no `--apply`) counts candidates **offline and contacts nothing**; only
  `--apply` contacts the model (egress is disclosed on stderr). The offline
  `learning freshness` pass needs no model and stays the default; this deeper
  check is opportunistic.

- **`edit_file`/`multi_edit`/`apply_patch` match across CRLF/LF line endings.**
  The edit tools matched `old_text` against the raw file bytes, so a model that
  emits `old_text` with `\n` could not edit a CRLF-stored file — every attempt
  failed "old_text was not found", pushing the model to give up and rewrite the
  whole file (and to keep re-learning that workaround as a lesson). Matching now
  runs on a line-ending-normalized form; the file's original CRLF/LF style is
  preserved on write.

- **Injected memory's language filter now also catches idiom-named lessons.** A
  lesson learned in a language but named only by idiom (a Go `sort.Strings`
  pattern) is tagged with the session's language at promotion (LocalMind), so the
  workspace-language injection filter excludes it from other languages instead of
  leaking it as noise.

- **Injected memory is filtered by the workspace language.** The session's
  dominant language (a bounded, cached scan at session start) is pushed into
  accepted-memory retrieval, so a lesson clearly about another language is
  excluded inside LocalMind's query (schema v7) rather than retrieved and
  dropped afterward — a Python idiom no longer lands in a Rust task and wastes
  the injection budget. A lesson that names no single language stays eligible
  everywhere. Opt out with `[memory] injection_language_filter = false`. The
  extension→language table now lives in LocalMind, shared with the stored lesson
  tag, so the workspace signal and the tag cannot drift.

- **Learning is on by default (`localpilot eval` stays clean-room).** LocalMind
  learning now defaults **on** (D-LM-0019), so interactive and agentic runs
  accumulate reviewed, machine-wide memory out of the box — `local_only`, review-
  gated (candidates, never auto-active), opt out with `[learning] enabled = false`.
  Capability measurement is unaffected: **`localpilot eval` neither reads nor
  writes accumulated memory by default** (clean-room), and a new **`eval --learn`**
  flag opts a run into closing the session out into LocalMind (review-gated lesson
  candidates, scope-routed to the global store) — for turning a benchmark or
  scripted run into a learning corpus without contaminating a measurement arm.

- **Portable signed knowledge bundles (`learning export` / `learning import`).**
  Accepted memory can be exported to a portable, signed bundle and imported on
  another machine or from someone else. `learning export --out pack.json [--scope
  project|global|both]` writes a deterministic, re-redacted, Ed25519-signed pack;
  `learning import pack.json [--apply]` verifies it **fail-closed** (a tampered or
  unknown-version pack is rejected and never stored), classifies trust
  (trusted/untrusted by signing key), and is **review-gated** — a dry run by
  default, `--apply` enqueues entries as review candidates with import provenance,
  never straight into active memory. The CLI states plainly that *a verified
  author is not verified content*. Trust is local (a keypair + manual trust list,
  no PKI). The round-trip lives under `learning` because `memory export` is the
  code-graph snapshot. See `docs/localmind-integration.md`.

- **Machine-wide global memory (on by default, via LocalMind).** A **global**
  store shared across every project on the machine is now on by default, so
  cross-project knowledge (tool-use patterns, debugging recipes, durable user
  preferences) accumulates and "the more you use it the smarter it gets" fires
  across projects. The store lives under `~/.localmind/memory` (overridable by
  `global_memory_root` or `LOCALMIND_GLOBAL_ROOT`); a conservative classifier
  routes only clearly cross-project lessons there, promotion stays review-gated,
  and retrieval merges project + global with project precedence. `local_only`
  (same-machine, never remote). A project that wants project-only memory sets
  `allowed_scopes = ["project"]`. See
  [docs/localmind-integration.md](docs/localmind-integration.md) and LocalMind
  D-LM-0017.

- **Project instruction files are injected directly, every turn (default-on).**
  `CLAUDE.md`/`AGENTS.md` previously reached the model only through the
  review-gated learning store, so a fresh checkout's instructions might never be
  seen. LocalPilot now injects the merged instruction document **directly into
  the turn context every turn** — ungated and independent of learning — bounded by
  `[context] instruction_char_budget` (8000 chars, truncate-with-marker over
  budget) and redacted first. Discovery gains two conventions: a first-class
  **`Navigator.md`** (LocalPilot's own, highest precedence) and
  **`.github/copilot-instructions.md`** (lowest), alongside `CLAUDE.md`/`AGENTS.md`;
  within a tier they order by kind (`Navigator` > `CLAUDE` > `AGENTS` >
  copilot). Opt out with `[context] inject_instructions = false`. The ingest path
  is unchanged (still review-gated). See ADR-0056.

- **Built-in loop safety rails — default-behaviour change.** A fresh project with
  no `[harness]` budget/timeout used to run an **unbounded** loop that a weak
  model could spin to an external SIGKILL with no scorecard. The loop now applies
  a conservative built-in bound when the config leaves a rail unset (an explicit
  `[harness]` value always wins): a headless run (`eval`/`print`/`harness` step)
  self-bounds to **200** tool calls and **600 s**; an interactive session bounds a
  runaway at **500** tool calls with no default wall-clock (a long interactive
  turn is legitimate and cancellable). This is a safety default, not a feature
  lever — an unbounded loop is a defect — so it ships on; tune or lift it with
  explicit `tool_call_budget`/`turn_timeout_secs`. The verify gate now also stops
  a turn with `NoProgress` (not a clean `Done`) when its build never goes green
  within the re-entry cap, tying the no-progress signal to the build result. The
  built-in default fills only the hard ceiling (no soft start), so the cost
  controller's no-progress branch is inert under it; the always-on degenerate-loop
  guard (ADR-0052: repeated/cyclic calls or a run of consecutive failures) now
  stays active for the built-in default and only defers to the controller when an
  operator sets an **explicit** budget — so a spinning or failing loop stops early
  on `NoProgress` instead of burning the whole ceiling. See
  [docs/06-harness-spec.md](docs/06-harness-spec.md) §Built-In Safety Rails and
  ADR-0055 (refining ADR-0029/0052).

- **Verify-before-done gate (`[harness] verify_before_done`, default-off).** A
  solve loop ends when the model stops calling tools, which let a turn "finish"
  code it never built — the largest avoidable cause of compiled-language losses.
  When enabled, a turn that would finalize with no tool call first runs a
  build/test verification; on failure the diagnostics are fed back and the loop
  continues instead of declaring success. The command is detected from the
  workspace stack (`cargo test`, `go test ./...`, `npm test`, `python -m pytest`,
  `mvn`/`gradle test`, `make`) or set explicitly with `[harness] verify_command`.
  It reuses the permission-gated quality-gate runner (no second command engine or
  retry loop) and is bounded by the budget/timeout rails plus a fixed re-entry
  cap. `localpilot eval --verify` / `--verify-command <cmd>` enables it for one
  run so a benchmark arm can measure its lift. Off by default (a feature lever);
  see [docs/06-harness-spec.md](docs/06-harness-spec.md) §Verify-Before-Done Gate
  and ADR-0054.

## v1.0.0 - 2026-06-24

Coordinated LocalX 1.0 release. First stable: the CLI, configuration, and
provider contract are now under SemVer. Validated on real local models,
including a cross-model sweep (lesson-injection uplift holds on a second model;
the grammar tool-call lever ships opt-in, default-off — no validity headroom on
either model measured).

- **Google Cloud Vertex AI Gemini via ADC.** Added `kind = "google-vertex-openai"`
  with `auth = "google_adc"` for projects that require Application Default
  Credentials instead of API keys. LocalPilot derives the documented Vertex
  OpenAI-compatible base URL from `google_project` + `google_location`, reads a
  gcloud `authorized_user` ADC file (`google_adc_path`, `GOOGLE_APPLICATION_CREDENTIALS`,
  or the gcloud default), mints short-lived OAuth bearer tokens in-process, and
  uses the same auth path for chat, `localpilot models`, and `/model`.
  `doctor` reports only `google_adc` / `google_adc_file`, never ADC JSON or
  minted tokens.
  Gemini tool calls now also preserve and replay the OpenAI-compatible
  `extra_content.google.thought_signature` metadata, avoiding Vertex/Gemini
  `Function call is missing a thought_signature` errors on multi-step tool use.

- **Outward self-improvement drafts (`self-review propose-issue`/`propose-pr`/
  `emit-draft`, default-off).** The self-improvement loop can now author a **draft**
  issue/PR from a ranked self-review finding and — only with an explicit human
  `--approve` — publish it as a **draft** to an allowlisted repo via the `gh` CLI.
  It is human-gated by construction: the same value-typed approval token that
  promotes a patch is required to publish, and the autonomous loop can never mint
  one (it can propose but not publish). The surface is off by default
  (`[self_improvement] enabled` + an `outward_targets` allowlist, both required and
  fail-closed); publication is draft-only (never ready/merge), dry-run by default
  (`emit-draft` without `--approve` prints the `gh` plan and publishes nothing),
  redacted, and writes drafts to the git-ignored `.localpilot/outward/` store for
  inspection before any publish. `drafts list`/`show`/`discard` inspect them.
  (ADR-0053, extends ADR-0034.)

- **`fetch` fails fast on a stalled connect.** The network tool now sets a connect
  timeout (bounded under the request timeout) so a hung TCP/TLS connect errors
  quickly instead of blocking the agent loop for the full request window.

- **Always-on degenerate-loop guard.** A turn can no longer spin unbounded when the
  tool-call budget is off. Even with the budget disabled, the loop now stops with
  `NoProgress` if the no-progress detector trips (a repeated or cyclic successful
  call set) or a run of consecutive *failing* calls exceeds a fixed conservative
  limit — the denied/failing spin the detector never saw (it is fed only by
  successful calls), which had let a weak local model loop for thousands of
  messages. A productive turn is never cut, and when the budget is configured the
  existing controller still owns the no-progress stop. "Budget off" still means no
  *cost* ceiling. (ADR-0052.)

- **Opt-in argument-repair feedback to LocalMind (`[tools] repair_learning`, default
  off).** At session close, the session's argument-repair patterns are offered to
  LocalMind's existing review-gated queue as aggregate, redacted candidates (which
  model needed which repair on which tool). Reuse-only: it stores no raw
  inputs/paths/content, writes no accepted memory, and adds no new store — a human
  promotes a candidate or it expires in review. A repair signal is never auto-promoted
  to an always-on rule cue.

- **Opt-in, conservative tool-argument repair (`[tools] repair`, default off).** A
  validator-first stage that, when enabled, repairs a *shape-invalid* tool call
  (a bare string where an array of strings is expected, a stringified array/object
  of the right item type, or a markdown autolink in a path field) on **only** the
  fields the validator flagged, re-validates, and either runs the repaired call —
  with a model-visible note saying what changed — or falls back to the readable
  error. It is gated by the tool's safety contract: a destructive, external-write,
  irreversible, or MCP tool, and any content/command field, is **never** repaired
  (`run_shell`, `apply_patch`, `git_commit`, `git_restore`, `fetch` get a readable
  error, never a silent rewrite). Repair changes arguments, never authority — the
  permission engine runs on the repaired input. `warn` applies and logs each repair
  loudly; `off` (the default) reproduces the prior behaviour exactly. Every repair
  and every high-risk refusal is a redacted session event. (The git contracts
  `git_restore`/`git_commit`/`apply_patch` are reclassified to their honest
  side-effect class so this gate is provable from the contract alone; this is
  advisory metadata only — the permission path and prompts are unchanged.)

- **Schema-aware tool-input validation errors and a dormant validity metric, lit up.**
  When a tool call's arguments are well-formed JSON but do not match the tool's
  schema, the model now receives a concise, schema-aware message — the offending
  field, the expected shape, and a valid example drawn from the tool's contract —
  instead of the raw deserializer string, so it can self-correct on the next turn
  (the validator-first / retry-with-error pattern). On by default; set
  `[tools] readable_errors = false` to restore the raw message (the rollback). The
  raw detail is always retained in the logs/telemetry. Independently, the
  previously dormant tool-input validity metric is now lit up: each tool call is
  validated against its schema and recorded as a redacted `tool_input_valid` /
  `tool_input_invalid` session event (classified by malformed-argument shape, never
  carrying a raw value), and the `eval` scorecard reports `schema_valid_rate`. This
  is measurement plus a message improvement — dispatch behaviour is unchanged.

- **`doctor` and `models` are agent-consumable (ADR-0048 `--format`, extended).**
  `doctor` gains `--format human|json` (`--json` alias; JSON by default off a
  terminal): the JSON adds the resolved **binary path**, the `git describe`
  **version**, the **provider kind/base URL/model/context window**, the **memory
  store root**, and a list of **capability tokens** — enough for a wrapper to
  detect a stale PATH binary vs the repo build (drift detection is the caller's
  job) and to feature-detect a surface (e.g. the `--workspace` flag) instead of
  guessing from the version. `models` no longer prompts then silently skips
  non-interactively: it gains `--format human|json`, a `--yes` flag, and a clear
  terminal state — under no-TTY (or `--yes`) it never blocks on a prompt, reports
  `approval_required` rather than skipping, and **exits non-zero** when an endpoint
  is unreachable or approval was required without `--yes`. The credential is still
  reported as a source label only, never the value.

- **`print` survives a closed reader and bounds a long turn.** A dogfood `print
  --allow-writes` run hung for minutes, then panicked with `failed printing to
  stdout: The pipe is being closed` when its reader closed stdout. Two fixes: the
  streamed-answer writes are now checked — a closed reader (`BrokenPipe`, or the
  Windows `ERROR_BROKEN_PIPE`/`ERROR_NO_DATA` codes) is a clean stop that cancels
  the turn and exits `141` (the SIGPIPE convention) instead of the process panic;
  and a new optional `[harness] turn_timeout_secs` bounds a turn by wall-clock, so
  a long or stuck turn stops with a terminal state rather than hanging. Either way
  `print` now emits a one-line, machine-readable `handoff:` summary on stderr —
  stop reason, tool calls, files changed, and whether memory was written — so a
  non-interactive caller always reads a terminal state. The timeout is unset by
  default (no behaviour change); set it to opt a turn into the bound.

- **Code-authoring guardrails in `seed-packs/coding-lessons.json` + an opt-in
  `print --self-review`.** The curated coding pack gains six general, model-actionable
  lessons distilled from a dogfood run where the local author wrote compilable code that
  skipped unspecified rigor: propagate a subprocess child's exit code (and surface its
  stderr); drain child stdout/stderr concurrently (and don't claim concurrency you didn't
  write); pass process args as a list, not a quoted string; guard a process launch like a
  missing argument; factor duplicated parse/format logic into one helper; and don't claim
  a build or tests pass before running them. Because one-shot `localpilot print` *reads*
  accepted memory (it injects lessons; it just never closes out), seeding these reaches the
  author with no new wiring. `print --self-review` adds an opt-in, read-only repo-health
  pass after a run (advisory, on stderr; never edits or commits), and `print --help` now
  states the reads-memory-but-does-not-learn contract.

- **Discoverable structured output for `learning search` / `memory search` (ADR-0048).**
  Adding `--json` was not enough — a dogfood run showed both the operator and the local
  model missed it and tab-parsed the human table. Now the format is resolved from context:
  when stdout is **not a terminal** (piped or redirected) the commands emit a JSON array by
  default; a real terminal still gets the human table plus a one-line stderr hint pointing
  at the structured form. A uniform `--format human|json` overrides either way (`--json`
  kept as an alias) — `--format human` forces the table even when piped. `memory search`
  gains the same JSON output as `learning search`. Stdout stays script-stable; the hint and
  diagnostics ride on stderr.

- **Workspace-aware LocalMind store resolution.** `localpilot learning` and
  `localpilot memory` now resolve the store like `git` resolves its repo root —
  walking up from the current directory to the nearest ancestor holding
  `.localmind` — so running from a project subdirectory answers from the project's
  store instead of silently using or creating a different, empty one. The resolved
  root is logged to stderr. A new `--workspace <path>` flag pins the root
  explicitly (skipping the walk-up). `learning search` / `memory search` are now
  read-only (a search never creates a store) and distinguish three empty outcomes
  on stderr — no store found, an empty store, and a non-empty store the query
  missed — so a bare `no matches` is no longer ambiguous. Stdout stays
  script-stable (an empty `--json` result is still a valid empty array).

- **`learning search --json`.** Accepted-memory search can emit a JSON array (id, score,
  path, snippet, category) for agent consumption, alongside the default human-readable
  text. Empty results are a valid empty array.

- **`doctor` reports a truthful version after a same-branch rebuild.** The embedded
  `git describe` version is captured by `build.rs`, which previously only re-ran when
  `.git/HEAD` changed — but a commit on the current branch advances the branch ref, not
  HEAD, so the reported version went stale after a pull + rebuild. The build script now
  also retriggers on the resolved branch ref and `packed-refs`.
- **`localpilot init` no longer writes a dangling default provider.** The starter
  `.localpilot.toml` shipped `default = "local"` with `[providers.local]` commented out,
  so the first `ask`/`print`/`chat` failed to resolve a provider. The `default` line is
  now commented alongside the provider block, with guidance to uncomment both once a
  provider is configured.
- **`localpilot models` explains an empty result.** When the only configured providers
  speak a protocol with no `GET /models` listing (e.g. `anthropic`), the command names
  them and explains the served model is whatever the local server has loaded, rather than
  printing a blanket "no providers ... configured".

- **`learning seed` now records an audit row per lesson.** Seeding writes accepted
  memory directly (the human gate moves to authoring time), but previously left no
  trace in `learning audit`. Each seeded lesson now writes an audit event (actor
  `seed`, subject = memory id, metadata naming the source and category), so a seeded
  memory has the same provenance trail as a promoted one. A dry run still writes
  nothing.

- **Advisory whole-repo teardown sweep at completion.** When `[harness]
  teardown_sweep` is enabled, the harness runs a read-only cleanup-audit pass at
  the completion seam alongside the retrospective — surfacing dead/abandoned code,
  duplicate/parallel logic, over-engineering, redundant data access, and doc/test
  drift as ranked advisory findings (each with a category, confidence, risk,
  recommended action, and the hidden-usage channels ruled out). It extends the
  existing `localpilot-selfreview` scanner (no second scanner), leans on
  `cargo machete`/`clippy`/`cargo deny` for tool-owned categories rather than
  re-deriving them, and is advisory by construction: it never blocks completion,
  edits code, or commits. Off by default; the same pass is available on demand via
  `localpilot self-review --cleanup`. See ADR-0047 and docs/06-harness-spec.md.

- **Promote a curated lesson to an always-on rule cue.** A seed lesson tagged
  `rule-cue` is injected every turn as terse, always-present guidance (independent
  of prompt relevance) — a weak model acts on a short always-on rule better than
  on a retrieved paragraph. Advisory, not an enforced harness rule (ADR-0027); the
  cue is excluded from the relevance block so it is never injected twice. Opt-in;
  default unchanged. See ADR-0046.
- **Outcome-aware down-weighting routes a lesson to review.** `flag_unhelpful_lesson`
  flags a lesson the uplift eval found unhelpful for human re-review (it stays
  active and is never auto-deleted), reusing the engine's reasoned route-to-review
  flag. See ADR-0046.
- **Accepted-memory injection tuning (`[memory]`).** A new config section makes
  always-on memory injection earn its context cost, with every default preserving
  the prior behaviour: `injection_min_score` (gate out weak matches so they don't
  fill the per-turn budget), `injection_context_aware` (scale the injected char
  budget toward the model's context window — a small model gets less),
  `injection_char_budget` (the budget / ceiling), and `injection_skip_categories`
  (skip a category a rule already enforces, so injection adds signal not
  redundancy). Additive and opt-in; default-off pending the uplift eval. See
  ADR-0045 and docs/configuration.md.
- **Selectable constraint encoding (`constraint_mode`).** A provider can now
  choose how a tool-call constraint is encoded: `response_format` (default — the
  OpenAI structured-output wrapper, unchanged) or `json_schema` (a documented
  llama.cpp server extension that sends the schema as a top-level `json_schema`
  field the server compiles to a grammar). Use `json_schema` for a local server,
  such as a turboquant `llama-server` build, that rejects the `response_format`
  wrapper — so the constraint engages the server's grammar instead of falling
  back to native tool-calling. Opt-in per provider (`[providers.<id>.options]
  constraint_mode = "json_schema"`); default and fallback are unchanged. See
  ADR-0044 and docs/04-provider-contract.md. **Live finding (2026-06-22):** on a
  turboquant `q3635ba3bapex` server the `json_schema` field still `400`s on the
  model's `<think>` prefix (same as `response_format`); only a raw GBNF `grammar`
  field engages there — so a third encoding, `constraint_mode = "grammar"`, was
  added: it emits a top-level GBNF `grammar` (a valid-tool-call grammar built from
  the tool names, JSON sub-grammar authored from the JSON spec). Live-verified to
  engage (`200`, valid constrained tool call after `<think>`). Per-argument schema
  constraint (a json-schema→GBNF converter) remains a follow-up. All three
  encodings are opt-in, default `response_format`; default-off pending a
  discipline eval.
- **Constrained decoding is disabled after a server rejects it.** A local
  OpenAI-compatible server that declares constrained decoding but returns a
  client error on the schema-constrained request now has the constraint dropped
  for the rest of the session after the first rejection, instead of re-sending
  it (and logging a fallback warning) every turn. Native tool-calling is the
  fallback, unchanged.
- **Curated best-practice seed packs.** `seed-packs/` ships opt-in coding and
  research lesson packs plus long-form references; seed them with `localpilot
  learning seed --file` or `localpilot ingest run`. Nothing is auto-loaded.
- **Seed curated lessons + re-enable memory injection.** `localpilot learning
  seed --file <pack.json>` writes a curated, author-reviewed set of best-practice
  lessons straight into LocalMind accepted memory (idempotent — re-seeding skips
  lessons already present; `--dry-run` validates without writing). `localpilot
  memory enable` clears the injection-disable flag that `memory disable` sets, so
  a lesson-on/off comparison is scriptable. See ADR-0043.
- **Switch provider/model mid-conversation with `/model`.** In the `chat` REPL,
  `/model` lists the configured providers and their models; `/model <provider>`
  or `/model <provider> <model>` re-points the active session — for example start
  on a local model and continue the same conversation on Anthropic or OpenAI. The
  switch selects an already-built provider (no rebuild, no re-auth), takes effect
  at the next turn boundary, and keeps the full transcript. Listing reuses the
  `GET /models` discovery and degrades gracefully offline. See ADR-0041.
- **Store API keys with `localpilot login` (bring-your-own-key).** `localpilot
  login anthropic|openai` deep-links to the provider's key page, takes a pasted
  key, validates it with one minimal request (`--no-verify` skips), and stores it
  in the OS keychain (Windows Credential Manager) or a `0600` per-user file
  (macOS/Linux); `localpilot logout <provider>` removes it. A stored key needs no
  environment variable: resolution is keychain → file → `api_key_env` → config.
  `localpilot doctor` now reports each provider's credential *source* (keychain /
  file / env / not set), never the secret. Bring-your-own-key only — no "sign in
  with Claude/ChatGPT" and no subscription credentials (ADR-0042). The keychain
  backend is the opt-in `keychain` build feature.
- **Prompt history survives a restart, scoped to the project.** The `chat`
  composer's Up/Down recall is now seeded from a durable store, so a new session
  starts with your past prompts instead of an empty history. The store is one
  global append-only file (`prompt-history.jsonl`) under the per-user directory
  beside `config.toml`, with each prompt tagged by the directory it was typed in;
  recall shows **only the current project's** prompts by default, and **Ctrl-T**
  toggles a view of every project's. It is on by default and fully opt-out via
  `[history] persistence = "none"` (no read, no write). Prompts are stored raw so
  recall is faithful, protected by mode `0600` on unix (the per-user directory ACL
  on Windows) and a bounded size; see ADR-0040 and
  `docs/07-security-and-privacy.md` (§Prompt History At Rest).
- **Gated `self-review propose-patch` write loop.** The write half of the
  self-improvement loop (ADR-0034) is now wired: `localpilot self-review
  propose-patch --finding <rank> --model <model>` asks a model to author a minimal,
  scope-confined fix for a ranked finding into an isolated git worktree and stops;
  `localpilot self-review promote --id <id> --reviewer <you> --approve` applies it
  to the main branch (the `--approve` flag is the explicit human act that mints the
  approval token — without it promotion is refused; fast-forward only, never
  pushes); `localpilot self-review discard --id <id>` drops the proposal. A proposal
  persists across invocations, so review can happen between propose and promote. The
  agent never mints the token, never merges, and never pushes — the gate is structural.
- **Scroll-up history no longer loses the start of a conversation.** In the
  `chat` REPL the inline live region used to be torn down and re-created every time
  its height changed (composer, activity tail, pickers). Early in a session that
  dropped freshly committed transcript blocks before they had scrolled into the
  terminal's native scrollback, leaving a hole in scroll-up history — the
  conversation's start gone while pre-launch shell output survived. The live region
  is now a fixed-height band, re-initialised only on a terminal resize, so every
  committed block stays in scrollback. Trade-off: a small constant gap above the
  composer when idle (tunable via `LIVE_REGION_HEIGHT`). See ADR-0039.
- **A large file write no longer degrades the session.** When a local model
  cannot emit a big file-write tool call as one well-formed payload, the harness
  used to re-prompt blindly and degrade without ever writing the file. It now
  detects the failed write specifically (a typed `MalformedToolArguments`
  provider signal carrying the tool name) and steers the model to write the file
  in pieces — the first section with `write_file`, each remaining section with a
  new **`append_file`** builtin (atomic, newline-preserving, binary-refusing) —
  recovering the write within the existing repair budget. The recovery ladder's
  input-shrink actions, previously computed but never applied, now compact
  history on a repeated bad turn. See ADR-0038 and `docs/06-harness-spec.md`
  ("Bad-output recovery").
- **Ingestion shows a live progress loader.** In the `chat` REPL the walking
  ingest actions (`/ingest run`, `/ingest refresh`, `/ingest resume`) no longer
  block silently: a working spinner runs while stage notices report discovering,
  files-to-parse, parsed *N*/*total* (throttled), indexing, and writing, ending
  in an `ingestion completed: … file(s), … chunk(s)` summary. `Ctrl-C` pauses an
  in-flight run — the chunks already written are kept, so `/ingest resume`
  continues instead of restarting — and failures surface as a notice rather than
  leaving the UI stuck. The non-interactive `localpilot ingest run`/`refresh`
  also print stage banners. Backed by a new `ingest_run_with_progress` engine
  entry point (the old `run` is a no-op-callback shim, so behaviour is
  unchanged). Docs corrected to match: `docs/01-product-spec.md` drops the
  never-shipped `/search` command and fixes `/resume` (it reopens the previous
  session; the harness workflows are `/harness-resume` / `/wait-resume`), and the
  wiki How-To/Troubleshooting pages show real `ingest`/`knowledge` subcommands.
- **Plan mode carries planning judgment.** The planner now prefers steps that
  extend or reuse the existing code named in the repository summary over adding
  parallel code, and must cover every acceptance criterion in the brief. `brief.md`
  gains an optional `## Risks & Rollback` section (absent in older briefs,
  round-trips losslessly), and the per-step worker prompt asks the model to update
  the matching documentation in the same step as a behaviour change. When a run
  finishes its last step, an **advisory** completion retrospective reviews the work
  against the brief (unmet criteria, scope drift, test-quality) and appends durable
  lessons to a new root `LESSONS.md`; it reports only — it never blocks completion,
  edits code, or commits. See [docs/06-harness-spec.md](docs/06-harness-spec.md)
  §Completion Retrospective and ADR-0035.
- **Completion-retrospective lessons are offered to review.** Each lesson the
  completion retrospective records is now *also* offered to LocalMind's review-gated
  queue as a candidate, so a human can promote it to memory instead of it living only
  in the un-gated `LESSONS.md` (which stays the human-editable mirror). Advisory and
  non-blocking — a failed enqueue never breaks a finished run — and a candidate
  reaches memory only after human review. See
  [docs/localmind-integration.md](docs/localmind-integration.md) and ADR-0037.
- **Measured session-friction findings (self-review).** `localpilot self-review`
  gained a third, deterministic findings source: a captured run's capability
  scorecard `process` block is projected into the same ranked findings stream with
  no model in the loop (`--process-file <scorecard.json>`). Redundant tool calls, a
  budget-exceeded/no-progress stop, an edit before any observation, a done-claim
  with no test run, and a mid-task failure each surface as a friction finding; a
  clean run yields none. This is the auto-captured counterpart to the existing
  model-reported audit-prompt friction. See
  [docs/12-feature-specs.md](docs/12-feature-specs.md) §Self-Review.
- **Loop-outcome lesson writeback (self-improvement loop learning arc).** When a
  human accepts or rejects a patch proposal, the outcome is written back as a
  durable lesson through the existing review-gated LocalMind path (no new store):
  an accepted outcome becomes a process lesson, a rejected one a first-class
  negative-signal anti-pattern ("Avoid (rejected): …") carrying the
  change-provenance reference. Once accepted, the lesson is retrieved by
  `localpilot self-review` on the next run, so the loop stops repeating a mistake;
  a bad lesson is curated through the existing `memory delete`/review-reject
  paths. See [docs/localmind-integration.md](docs/localmind-integration.md)
  §Loop-Outcome Lesson Writeback (LocalMind decision D-LM-0014).
- **Human-gated patch generation (self-improvement loop write half).** A new
  crate turns an approved finding into a minimal change inside an isolated git
  worktree on its own branch (never the main working tree), scope-bound to the
  files the finding named, carrying a change-provenance record
  (prompt/model/tools/test-evidence/rationale/risks/rollback/lessons). The only
  operation that writes outside the worktree — promoting the change onto the main
  branch — requires an approval token a human-confirmation path mints; the agent
  never self-merges, promotion fast-forwards only and never pushes, and rollback
  is to drop the worktree. The git surface runs fixed subcommands as argv (no
  shell, no network). See [docs/12-feature-specs.md](docs/12-feature-specs.md)
  §Human-Approved Patch Generation and
  [docs/07-security-and-privacy.md](docs/07-security-and-privacy.md).
- **`localpilot self-review` (read-only repo-health scan).** A new subcommand
  walks the workspace and emits a ranked, advisory findings report — leftover
  `TODO`/`FIXME` markers, a decision index (registry) lagging the decision log,
  incomplete plan rows, broken doc links, and an opt-in missing-test heuristic —
  plus model-emitted harness-friction findings (`--audit-prompt` /
  `--friction-file`). Findings rank by severity × confidence; prior accepted
  lessons inform the scan. It writes nothing. `--json` emits the machine-readable
  report (`localpilot-selfreview-v1`). See
  [docs/12-feature-specs.md](docs/12-feature-specs.md) §Self-Review.
- **Project context files (`CLAUDE.md` / `AGENTS.md`).** LocalPilot now discovers
  project instruction files at the workspace root, in nested directories, and at
  a per-user global location (`~/.localpilot/`), resolves their `@`-import
  directives (cycle-detected and depth-bounded), and merges them by precedence
  (repo-root > nested > global) into one ordered context document. Folder
  ingestion captures the merged document as first-class derived knowledge under a
  synthetic `<project-context>` path, so `knowledge_search` can surface project
  conventions and constraints on demand. See
  [docs/configuration.md](docs/configuration.md) §Project context files.
- **Background processes.** A new `run_background` tool runs a long-running
  command — a dev server like `npm run dev` or `bun run index.ts`, or a watcher —
  detached from the turn: it confirms the process stayed up past a short grace
  period, captures its startup output, and tracks it so later turns can `list`,
  read `logs`, or `stop` it. The registry is session-scoped and in-memory; every
  child is killed when the session closes (no cross-invocation daemons).
  `run_shell` now recognizes a dev-server/watcher command and points at
  `run_background` instead of blocking until its timeout, and `bun`/`deno` are
  recognized by the command classifier. The interactive UI pins a running-process
  indicator to the bottom-right status corner, and a new `/bg` command lists them
  (`/bg`), stops one (`/bg stop <id>`), or stops all (`/bg stop all`).
- **Capability scorecard.** The golden-task evals now emit a machine-readable
  JSON scorecard per task run, widening the previous pass/fail line into three
  measured layers — `results` (pass/fail, regression-safety, partial credit),
  `quality` (diff size, vs-gold ratio, format/lint/type-check clean, complexity,
  tests-added), and `process` (tool-call count, redundant calls,
  reproduce-before-fix, test-before-done, retrieval utilization, exit reason,
  recovery) — read deterministically from the captured diff and the session event
  trace. A reported `speed` block (wall time, tokens) is a guardrail, never the
  headline. The one-line discipline scorecard is unchanged. See
  [docs/08-testing.md](docs/08-testing.md) §Golden-Task Evals.
- **Per-turn tool-call budget is now opt-in (behavior change).** The
  `[harness] tool_call_budget` / `tool_call_budget_max` keys default to **unset**,
  so a turn runs unbounded unless an operator configures a budget — previously
  both defaulted to a fixed `50`. Setting either key enables enforcement (a single
  configured bound serves as both the soft start and the hard ceiling); with the
  budget off, neither the cost ceiling nor the no-progress stop fires.
- **First-party capability corpus.** Added an original, clean-room corpus of
  small buggy tasks (each with its own failing→passing test) under
  `crates/localpilot-harness/tests/corpus/`, plus an in-repo runner that drives
  the harness loop headless against each task, emits the scorecard, and grades by
  building and running the task's own test in isolation. Includes a git-history
  extraction helper that surfaces fix-commit candidates as reviewable fixture
  stubs. Offline-deterministic by default; a live model path is gated behind
  `LOCALPILOT_LIVE_TESTS`.
- **LLM-as-judge quality rubric.** Added an original, blinded, calibrated
  LLM-as-judge that scores the quality dimensions static signals cannot see
  (readability, idiomatic style, abstraction fit, latent-bug risk) into the
  scorecard's optional `judge` block. Single-solution scoring is blind by
  construction; comparative judging randomizes solution order and maps the verdict
  back; a prompt-addressed cache makes scoring offline-deterministic; and
  `cohens_kappa` reports agreement against a human-labelled sample. See
  [docs/08-testing.md](docs/08-testing.md) §LLM-as-judge quality rubric.
- **Judge ranking self-test.** Added a cheap, per-run trust gate complementing
  calibration: the judge must score each authored `better` fixture strictly above
  its `worse` pair (`ranking_selftest_offline`, `RANKING_FIXTURES`) or scoring is
  refused (`score_offline_gated` → `JudgeError::Untrustworthy`, naming the failed
  fixture) rather than emitting a believed-but-wrong number. Runs offline with no
  model (the CI gate); `ranking_selftest_live` is the opportunistic live variant.
- **Ablation, attribution, and composite scoring.** Added an ablation arm matrix
  (`baseline`, `full`, and one arm per harness feature turned off, model pinned),
  per-feature attribution that maps each feature to the process signal it should
  move and flags a feature that is on but inert, and a composite score where
  correctness gates first and passers rank by quality + process + regression-safety
  (speed stays a reported guardrail). All deterministic and offline-testable, with
  an original clean-room set of adversarial tasks.
- **`localpilot eval` command.** A new headless subcommand runs the agent on one
  problem in the workspace and emits the capability scorecard (JSON) to stdout —
  the solver entry point an external benchmark runner drives. It runs the same
  harness a real session uses, captures the produced diff + the session trace,
  optionally grades with `--test <cmd>` (or leaves `results` for an external
  grader), and records `--arm`/`--task`/`--gold-diff` on the card. Only the JSON
  reaches stdout, so the line is pipe-safe.

## v0.3.0-beta.3 - 2026-06-18

Coordinated LocalX beta release.

- **Release hygiene.** Stamped every crate's `Cargo.toml` package version at
  `0.3.0-beta.3` and advanced the `external/localmind` submodule pin to the
  matching beta.3 LocalMind commit. The coordinated cut had moved the top-level
  `VERSION` but left the Rust packages and the embedded LocalMind a train behind.
- **RPC robustness.** The stdio line framer now caps an unterminated record
  (default 16 MiB) and returns a framing error instead of buffering without
  bound, so a peer that never sends a newline cannot exhaust memory.
- **Memory inspector accuracy.** The per-turn "memories used" record (shown by
  `localpilot memory inspect`) is now derived from the same single retrieval that
  builds the injected context, so it lists exactly what was injected — no longer
  over-reporting memories ranked past the injected cap, and now including the
  repository primer (`primer` layer) and push-mode ingested chunks (`ingest`
  layer) that were previously omitted. Each turn does one memory search instead of
  two.
- **Security (command classification).** An inline Windows shell command —
  `cmd /c …`, `powershell`/`pwsh -Command …`, `-EncodedCommand`, `-File` — is now
  treated as opaque and classified `unknown` (gated), exactly like `bash -c`,
  instead of being substring-classified. This closes a path where
  `cmd /c "echo data > file"` was auto-allowed as a read while the shell performed
  the write. Independently, an argument with an output redirection (`>`/`>>`) can
  no longer be classified `read-only`. The classifier fails toward a prompt, never
  a silent allow (ADR-0032).
- **Security (shell secret reads).** A read-only shell command (`cat`/`type`/
  `head`) whose path argument is secret-like (`.env`, `*.pem`, `~/.ssh/…`,
  `.aws/credentials`, …) or resolves outside the workspace now prompts, instead of
  being auto-allowed to read the file into model context. Ordinary in-workspace
  reads are unaffected (ADR-0032).
- The **no-unsupported-claim gate** is now reachable through configuration:
  `[harness] claim_gate = "warn"` (default `"off"`) flags a completed-action
  claim in the final reply that no verified tool call this turn supports. Matching
  is now **per claim** — a verified action no longer excuses a different,
  unverified one — and a verified shell command (opaque) backs any category while
  the structured file tools match by kind. The expanded lexicon recognizes more
  completions (added, implemented, generated, ran, pushed, merged, …) while
  present-tense and plan phrasing stay untouched. An offline false-positive/recall
  benchmark scores the gate without a live model (ADR-0023).
- Added a **pull-based tool surface** (ADR-0031), off by default. With `[tools]
  broker = true`, each turn advertises only a small working-set of tool *schemas*
  (a configurable core plus the broker's own tools plus what has been revealed)
  instead of every tool's schema; tool names are still listed cheaply. Two
  read-only tools, `tool_search` and `tool_load`, let the model find and reveal a
  tool on demand. A call to a tool that is not advertised (unknown,
  out-of-working-set, or retired) no longer returns a bare `unknown tool` error —
  the broker resolves it to the closest available tool, reveals it, and asks the
  model to retry, without running the attempted call. An opt-in `[tools] marker`
  lets the model write a `NEED: <capability>` line to request a tool proactively.
  **Reveal-never-grant:** revealing changes visibility only; a revealed
  write/network tool still passes the full permission gate. The broker searches a
  live, fingerprinted catalog of the registry (MCP tools attributed to their
  server; a retired tool drops out, with an optional old→replacement overlay since
  MCP carries no deprecation field). With `[tools] learning = true` the broker
  re-ranks tools by past success, graduates frequently-revealed tools into the
  always-advertised set (persisted across sessions), and records redacted
  `tool_resolution` telemetry. All `[tools]` defaults reproduce prior behaviour.
- Added a **look-before-launch** discipline (ADR-0030). The agent is now nudged to
  inspect a named target before standing up its own competing server. A new
  always-on system-prompt convention states it, and a deterministic
  `check_before_launch` rule enforces it: when the task prompt named a local
  serveable target (a loopback host, or any `host:port` with an explicit port) that
  has not been probed this session, an attempt to launch a local HTTP server
  (`python -m http.server`, `npx serve`, `php -S`, `vite`, …) or scaffold a
  competing `index.html` surfaces a model-visible verdict — *probe it first; only
  launch your own server if the probe fails*. The probe state is read from the
  session evidence ledger (a successful `fetch`, or a `curl`/`Invoke-WebRequest`
  probe command), never the model's claim. It is advisory and tighten-only: default
  `warn` (the call still runs), tunable via `[harness.rules] check_before_launch` to
  `block` (refuses the launch) or `off`. Auto-extracted targets ignore external
  reference URLs without a port.
- The per-turn tool-call ceiling is now **progress-aware** (ADR-0029). A turn that
  keeps making forward progress runs up to a hard cost ceiling instead of stopping
  at a single fixed count; a turn that spins on the same successful calls gets a
  strategy-change nudge and then stops on a distinct `no_progress` reason at the
  soft start, rather than wasting the rest of the budget. The hard ceiling always
  stops the loop, so a turn can never run unbounded. Two new `[harness]` keys —
  `tool_call_budget` (soft start) and `tool_call_budget_max` (hard ceiling) — both
  default to `50`, so behaviour is unchanged until an operator raises the maximum.
- Added a cross-context **handoff**: `localpilot handoff` writes a redacted,
  git-ignored snapshot (`.localpilot/handoffs/<id>.md`) of the latest session's
  durable state — a machine-checkable header plus a body separating confirmed facts
  from assumptions, referencing `brief.md`/`PROGRESS.md`/`DECISIONS.md` by path rather
  than copying them. `localpilot handoff resume <id>` runs a deterministic check
  (branch, commit, dirty-state, referenced paths/session) and surfaces mismatches as
  warnings before a fresh agent acts. A handoff is an execution record — never
  committed and never promoted into LocalMind memory.
- Project-local skills (advisory prompt modules under `.localpilot/skills/` or
  `.agents/skills/`) are now a live, pull-based surface. `localpilot skills list`
  and `localpilot skills show <name>` read them deterministically; with `[skills]
  autonomous_discovery = true` (off by default) the model can also discover them on
  demand via the read-only `skill_search` / `skill_load` tools. The loader now
  respects a skill's `disable-model-invocation` flag — a user-only skill is reached
  only by exact name, never auto-surfaced by search. Loading a skill runs nothing;
  declared permissions are surfaced, not granted, and the workspace trust gate and
  permission engine still apply.

## 2026-06-17 - Retrieval and learning

- Ingested chunks are now prefixed with offline document context (front matter
  or leading line) before indexing, so a chunk split mid-thought still matches
  its document's subject. Opt-in model-written prefixes are gated and audited.
- Added a layered retrieval contract — `knowledge_expand` and `knowledge_fetch`
  tools alongside `knowledge_search` — so a turn spends a bounded number of
  tokens to locate the right knowledge before paying for full bodies.
- Off-machine learning extraction is now gated: model-backed extraction runs
  against a loopback endpoint by default, and an off-machine endpoint is reached
  only with the `LOCALPILOT_LEARNING_ALLOW_REMOTE` opt-in (audited); otherwise
  close-out falls back to the deterministic extractor and the transcript stays
  local.
- Added a local "memories used this turn" inspector: a `memory used` CLI
  subcommand and a TUI panel showing each used memory's provenance, confidence,
  epistemic status, contradictions, and staleness. Fully offline.
- Fixed a TUI-only build break in the `ingest resume` path.

## 2026-06-17 - Documentation

- README now documents the `ingest` and `knowledge` commands and the
  `localpilot-verify` crate, which had shipped without a README entry.
- Added an in-repo wiki source (`docs/wiki/`) one-way CI-synced to the GitHub
  Wiki, a `docs/README.md` doc-ownership index, and an offline link check over
  the docs.

## 0.3.0-beta.2 - 2026-06-15

Coordinated LocalX beta release. The learning loop now closes end to end.

- The learning adapter selects the model-backed extractor when an `[inference]`
  endpoint is configured (with graceful deterministic fallback), instead of
  always running deterministic. See ADR-0019.
- New learning projects auto-wire `[inference]` to the host's own loopback
  provider endpoint, so local models do the learning jobs with no manual config;
  a remote provider is never wired automatically (remote-egress policy).
- Added a read-only `active_skills` tool: active skills are advisory prompt
  modules surfaced with provenance, never installed or executed. See ADR-0020.
- Committed an end-to-end learning-loop regression fixture (closeout → promote →
  durable memory + audit + retrieval).
- Extracted the `run_shell` builtin into its own module.
- Docs: scoped `context-intelligence-vision.md` against LocalMind's vision; added
  the extractor-selection and skill-consumption contracts to the integration doc.

## 0.3.0-beta.1 - 2026-06-12

- Fixed interactive input editing: the caret is visible, and Left/Right,
  Home/End, Backspace, Delete, newlines, and pastes edit at the cursor. Provider
  streams that disconnect before a completion marker now recover instead of
  persisting a visibly truncated response as complete.
- Made the session context budget configurable with `[harness]
  context_token_limit` (default 24000) so a model's full context window is used
  for compaction instead of a fixed default.
- Reworked the REPL input box: it grows with multi-line content up to a cap and
  then scrolls; newlines now work across terminals (a trailing `\` before Enter,
  plus Ctrl+J / Shift+Enter where the terminal reports enhanced keys); large
  pastes collapse to a `[pasted #n · N lines]` placeholder and expand to full
  text on submit.
- Added a first-run trust gate: the REPL shows the workspace folder and asks
  whether to trust it before acting, remembering the answer per folder (skipped
  under `--bypass`).
- Added the Anthropic Messages API provider (`kind = "anthropic"`), a second,
  protocol-distinct adapter implemented clean-room from the public API:
  top-level `system`, `tool_use`/`tool_result` blocks, required `max_tokens`,
  `x-api-key` + `anthropic-version`, and a typed SSE stream (ADR-0008).
- Added `localpilot update [--check]`: checks the repository for a newer release
  tag and, on confirmation, reinstalls from source with the same feature set
  (MSVC toolchain on Windows for the TUI). The REPL and bare launch also do a
  cached, once-a-day check; disable with `LOCALPILOT_NO_UPDATE_CHECK`. The
  binary now embeds a real version via `build.rs`.
- Fixed the installers to build `--features tui,learning`, initialize the
  LocalMind submodule, and prefer the MSVC toolchain on Windows for the TUI.
- Documented the configuration reference and stability policy
  (`docs/configuration.md`) and consolidated the extension points into
  `docs/extending.md`.
- Updated the vendored LocalMind engine to the coordinated LocalX
  `v0.3.0-beta.1` release train and exposed active LocalMind skills through
  the adapter.

## 0.1.0-alpha.6

- Fixed the interactive REPL: drain buffered events so a fast response is shown
  (not dropped) and surface provider/stream errors instead of failing silently;
  handle only key *press* events (Windows no longer doubles typed characters);
  add a working spinner + elapsed timer; support bracketed paste and Alt+Enter
  for a newline.
- Added a task checklist panel driven by an `update_plan` tool.
- Retry transient provider connection failures (network/5xx) with exponential
  backoff and a notice; rate-limit/quota errors still pause.

## 0.1.0-alpha.5

- Integrated the LocalMind learning engine (vendored as a git submodule) behind
  the opt-in `learning` feature: session closeout, the review queue, memory
  promotion and search, skill drafts, an audit log, retrieved-context injection
  before turns, and automatic closeout on REPL exit — one-way edge, bundled into
  the binary, all state local under `.localmind/`. New `localpilot learning`
  commands.

## 0.1.0-alpha.4

- Added interactive tool-approval prompts in the REPL (the approval interface is
  now asynchronous); default-profile sessions can perform approved actions
  without `--bypass`.
- Connected MCP servers and exposed their tools to the session through the same
  permission engine and redaction.
- Sized quota pauses from provider rate-limit metadata; show live tokens/sec and
  a quota reset timer in the footer.

## 0.1.0-alpha.3

- Added `localpilot harness wait-resume` to continue a run paused on a provider
  quota/rate limit once it is safe.

## 0.1.0-alpha.2

- Made the `chat` REPL launchable and bundled the `tui` feature into release
  builds; the bare `localpilot` command launches the REPL when a provider and
  model are configured.

## 0.1.0-alpha.1

- Created the clean-room Rust workspace and the product/architecture/harness/
  provider/security/testing/release specifications, with two operating modes
  (agent and enforced harness) and configurable permission profiles.
- Added the full crate roster (`localpilot-memory`, `-skills`, `-recovery`,
  `-quota`, and the rest) and centralized the lint policy in `[workspace.lints]`
  (`unsafe_code` forbidden; `unwrap`/`expect`/`todo`/`dbg!` denied on library
  runtime paths, relaxed in tests).
- Added real `doctor` diagnostics: version, platform, config search paths,
  provider credential presence (never values), tool availability, trust state.
- Added the provider runtime: an object-safe provider trait with typed
  capabilities, a stable error taxonomy, and quota metadata behind one streaming
  contract. The OpenAI-compatible adapter serves local servers and the official
  OpenAI API, with streaming, tool calls, reasoning round-trip, and a
  config-driven registry. Added `localpilot ask`.
- Added the sandbox: a workspace path boundary, per-OS command risk
  classification, and a permission engine with `default`/`relaxed`/`bypass`
  profiles, a secret-file guard, and a workspace-trust floor.
- Added the tool system: a permission-gated registry and the builtin tools
  (`read_file`, `write_file`, `edit_file`, `list_files`, `search_text`,
  `run_shell`, `git_status`, `git_commit`) with generated schemas, atomic writes,
  and output redaction on every profile.
- Added the shared agent-mode session runtime (cancellable streaming loop, tool
  execution, transcript persistence, context compaction, loop limits) with
  bad-output detection and a budgeted recovery ladder, plus `localpilot print`
  and the `chat` REPL behind the opt-in `tui` feature.
- Added the harness core: lossless `brief.md` / `PROGRESS.md` documents; the
  `init`, `harness status`, `intake`, `plan`, `feature`, and `resume` commands;
  original intake/planner prompts; a deterministic rule engine with protected
  critical rules; and an anti-sunk-cost worker that commits one step at a time.
- Added the v1 extensions: quota wait/resume with safety gates, a local redacted
  memory store with ranked retrieval and `memory` commands, the skill
  manifest/loading/suggestion system, and an MCP client.
- Added the terminal UI: a dense ratatui view (header, transcript with live
  streaming, always-visible footer, optional thinking panel, approval modal,
  slash commands, model/provider picker, transcript search, responsive collapse)
  snapshot-tested with a test backend.
- Updated pinned dependencies for security (`tokio` → 1.44.2,
  `tracing-subscriber` → 0.3.20); no MSRV change. Added editor/CI tooling and an
  opt-in pre-commit gate; CI runs tests under `cargo nextest` plus a
  supply-chain job (`cargo deny`, `cargo audit`).
