# Architecture Decision Records

This file starts the decision log. Add new records at the top.

## ADR-0095: Research Renders JavaScript-Only Pages In A Headless Browser, Inside The Egress Boundary

Status: accepted. Closes LocalHub#37. Builds on the research egress boundary
(ADR-0060/0076) and the topic-scope admission contract (ADR-0094).

Static HTTP extraction cannot recover content that only appears after JavaScript
runs: a single-page-app shell, a hydration-only document, or an iframe-embedded
page reduces to empty/shell-only text, and admitting that shell as complete
evidence is a silent failure. The fix is a bounded browser-rendering fallback —
but rendering executes public-page JavaScript and loads subresources/frames, a
wider action than one GET, so it must stay strictly inside the research egress
boundary rather than be delegated to a permission-gated-but-host-agnostic
browser MCP.

1. **Detection is dependency-free and always on.** `localpilot-research` gains a
   `render_signal` heuristic (empty framework mount, hydration markers,
   iframe-only body, `Loading…` placeholder, script-heavy thin content) run over
   the fetched HTML after reduction. A `[research.render].mode`
   (`auto` default / `off` kill switch / `always`) governs the fallback.
2. **The render contract is host-neutral; the browser is optional.** The
   `Renderer`/`RenderGate` traits and render value/outcome types live in
   `localpilot-research`. The concrete `ChromiumRenderer` is a separate crate
   `localpilot-render`, pulled in by the cli only under the `render-browser`
   feature. The default binary links no browser stack; without the feature or a
   browser, research records `renderer unavailable` and falls back to iframe
   recovery.
3. **The mechanism is an original CDP client over the system browser.** A
   dependency-light Chrome DevTools Protocol client over `tokio-tungstenite`
   drives a discovered system Chromium/Chrome/Edge (none bundled or downloaded).
   CDP is an official documented protocol; keeping the client in-repo keeps the
   security-critical `Fetch`-domain interception in reviewed code rather than a
   black-boxed dependency.
4. **One egress boundary, enforced per browser request.** Every browser request
   — navigation, redirect, subresource, frame — is gated through the same
   `[research.web]` allowlist (a `WebAccessGate` over `WebAccess`) before it
   leaves the machine, http/https only, with an **unconditional** SSRF block on
   `localhost`/loopback/link-local/private addresses ahead of the allowlist
   (a rendered page can reference arbitrary hosts). Redirects are re-gated as new
   destinations. Every request/block is audited content-free.
5. **Bounded, ephemeral, and honest.** The browser runs with a throwaway
   cookie-less profile removed after the run; the render is time-bounded (no
   indefinite network-idle wait). Rendered main-document and same-origin/`srcdoc`
   frame content is reduced and admitted through the *same* topic-scope admission
   path (ADR-0094) as static content, with the frame's URL (or a `srcdoc`
   locator) as provenance; cross-origin frame documents are recovered via the
   gated HTTP path. A page that renders empty records `no substantive rendered
   content`; a page that needed rendering but could not get it records an
   explicit render-required outcome — never fabricated evidence.
6. **Documented limitations** (kept honest, never presented as prose): frames
   nested more than one level deep via the browser, a cross-origin frame whose
   content is itself JavaScript-rendered, and non-HTML (PDF) frame extraction are
   out of scope — such frames yield no extractable content and are skipped.

## ADR-0094: The Research Topic Is A Contract Through Decomposition And Admission; Evidence Relevance And Candidate Trust Are Separate

Status: accepted. Closes LocalHub#36. Follows ADR-0088's model-backed
admission (which judged only the generated sub-question) and ADR-0090's
full-evidence contract.

Model-backed admission (ADR-0088) judged a fetched page or local chunk against
the *generated sub-question* alone. A decomposed sub-question can silently drop
the topic's load-bearing constraints — "three.js procedural materials" produced
"How are parametric controls exposed to users in real-time?", which named no
framework — so the search returned Unreal/Unity/Substance pages and admission
scored them ~0.85 against the generic question. Separately, every review
candidate read `0.30` because candidate creation caps confidence at the
unreviewed-trust ceiling; admitted evidence scoring 0.75–0.95 all collapsed to
that one number, conflating *evidence relevance* with *candidate trust*.

1. **The original topic is threaded to admission.** Both the web source and the
   local knowledge source carry the topic and pass it to the classifier
   alongside the sub-question. The classifier is instructed that the topic's
   load-bearing constraints — framework, library, language, runtime, platform,
   version — always apply, and to reject a page that answers the sub-question
   but is about a different framework/engine unless the topic itself asks for a
   comparison or transferable techniques.
2. **Decomposition is constrained.** The decompose prompt requires every
   sub-question to stay within the topic's scope and keep its named constraints,
   rather than broadening into a question another tool could answer.
3. **Search queries are re-scoped to the topic.** When a sub-question no longer
   carries the topic's significant terms, the topic is prefixed before the
   redacted query leaves the machine (`scope_to_topic`), so a generic
   sub-question cannot silently become a generic web search. The same scoped
   text feeds the deterministic term-overlap fallback, making the no-model path
   topic-aware: a cross-framework page missing the topic's terms floors below
   the admission floor instead of matching the generic sub-question.
4. **Evidence relevance and candidate trust are distinct, truthfully named.**
   `CandidateSpec` preserves the finding's uncapped `evidence_relevance` beside
   the capped `confidence` (candidate trust). The enqueued candidate names both
   ("evidence relevance 0.85, candidate trust 0.30"), so a reviewer distinguishes
   strong from weak evidence without the trust cap being raised — unreviewed
   research stays low-trust and review-gated (ADR-0011).
5. **The classifier's reason is preserved.** Admission parses and carries the
   model's short rationale into the admission trail and the report's retrieval
   accounting, so "admitted at 0.85" is auditable, content-free.
6. **The deterministic fallback stays topic-aware, never dropping constraints
   silently.** Losing the admission model degrades to topic-scoped term overlap,
   not the previous constraint-blind sub-question overlap.

## ADR-0093: A Catalog Model May Name Its Drafter; One Speculation Engine Per Launch

Status: accepted. LocalBox/shared-tier decision (series home per ADR-0062).

Model repos increasingly ship a small companion drafter GGUF for classic
speculative decoding (the Bonsai repos ship DSpark Q4_1 drafters with a
claimed ~1.34× CUDA decode uplift). The launcher had an MTP spec-type path
but no way to use a drafter file; catalog prose honestly said "not wired".

1. **`DraftModule` is a first-class catalog field**, the drafter sibling of
   `VisionModule`: a filename in the model's repo, resolved to the model's
   folder, opt-in per launch (`--draft`), shown in dry-run as
   present/will-download, downloaded on demand, and a failed download stops
   the launch rather than silently degrading. Configured-only — a drafter is
   never auto-detected from disk, because an arbitrary neighbouring GGUF is
   not a safe drafter guess (unlike the `mmproj-*` vision convention).
2. **The argv contract lives in the shared tier**: a resolved drafter path
   emits `--spec-type draft-simple --spec-draft-model <path>` (with
   `--spec-draft-n-max` riding along when set) from the same builder every
   consumer shares.
3. **One speculation engine per launch.** A drafter combined with any
   explicit spec-type other than `draft-simple` (notably the MTP family) is
   a typed error at plan time, not ambiguous server behaviour. A
   tokenizer-mismatched drafter remains the server's own startup refusal,
   surfaced honestly.
4. **The tuner does not search the drafter.** It is a catalog-driven launch
   lever; benchmark arms that want it set it explicitly, keeping tuned
   profiles comparable.

## ADR-0092: Shipped Config Layers Refresh; The User Catalog Merges Additively

Status: accepted. LocalBox-tier decision (shared-series home per ADR-0062);
the config-file half of ADR-0091's anti-staleness posture.

LocalBox reads its three config layers from disk
(`defaults.json` < `llm-models.json` < `settings.json`), but first-run
seeding wrote each file once and never touched it again. The consequence:
upgrading the binary changed nothing an existing install actually read —
new engine pins, new repos, and newly shipped catalog models silently never
arrived. The layers have different ownership, so they get different rules:

1. **Shipped layers refresh.** `defaults.json` and `llm-models.example.json`
   are the binary's property: seeding now rewrites them whenever they differ
   from the embedded copies, so an existing install always reads the pins and
   shipped-model set of the binary it runs. This is safe by layer precedence
   — user overrides belong in `settings.json`, which always wins; editing
   `defaults.json` directly is documented as unsupported.
2. **The user catalog is never rewritten.** `llm-models.json` is the user's
   file: seeded when absent, then additive-only. `localbox update` reports
   shipped models the catalog predates; `localbox update --merge-models`
   adds exactly the missing model keys from the embedded shipped catalog —
   an existing entry is never modified, `CommandAliases` and every other
   top-level key survive, and `--check` previews the keys before any write.
3. **The embedded catalog is the merge source**, not the on-disk example —
   the binary knows what it ships; the on-disk example is a human-readable
   mirror of the same bytes.
4. **A source checkout keeps its dev override**: inside the repo,
   `local-llm/` remains the live tree, so development exercises the shipped
   files directly.

## ADR-0091: Engine Pins Age Loudly And Advance Deliberately — Freshness Reporting Plus A Digest-Verified Pin Refresh

Status: accepted. LocalBox-tier decision (the shared-series home per ADR-0062's
repo split); extends the verified-download posture of the launcher stack.

LocalBox pins every downloadable `llama-server` engine (mainline, turboquant,
PrismML) to a release tag plus SHA-256 asset pins. Pinning is correct — but it
aged silently: nothing ever said "your pin is N releases behind", and advancing
a pin was a hand-ceremony of editing tags and copying hashes. In practice pins
lagged for months without anyone deciding that.

1. **Staleness is reported, never acted on.** `localbox update --check` now
   reports, per pinned mode, whether the pinned tag is the latest upstream
   release. The report is informational; no path auto-installs a newer
   release. A pinned build must never re-download or advance because upstream
   moved (the `.build-stamp` freshness contract keyed to the *resolved* tag is
   unchanged).
2. **Advancing is one deliberate command.** `localbox update --mode <m>
   --refresh-pins` resolves the *latest* release for that mode, selects this
   host's assets through the existing per-mode selectors, and installs them.
   The flag requires an explicit `--mode`; there is no refresh-everything
   sweep. `--refresh-pins --check` previews the tag and asset set without
   downloading.
3. **A freshly recorded pin is digest-verified, not trust-on-first-use.** For
   an asset with no local pin, the upstream release API's `sha256` digest is
   the integrity check: computed bytes that do not match the published digest
   refuse to install or be recorded. Only well-formed `sha256:<hex>` digests
   count; other algorithms are ignored rather than mistrusted. Where upstream
   publishes no digest, the computed hash is recorded and disclosed — the
   pre-existing unpinned behaviour.
4. **Refreshed pins land in the settings layer.** The refresh writes the
   mode's pinned-tag key and the asset hashes into `~/.local-llm/settings.json`
   (upserting only those keys, preserving everything else). Settings win layer
   precedence and survive upgrades; the shipped `defaults.json` stays the
   fresh-install baseline.
5. **The local pin table remains the install-time authority.** Upstream
   digests are a cross-check at pin-recording time only; a normal pinned
   install verifies against the locally recorded hash exactly as before, and
   a mismatch still deletes the file and refuses.

## ADR-0090: Local Research Evidence Carries The Full Bounded Chunk; A Snippet Never Poses As Full Source

Status: accepted. Completes ADR-0087's full-evidence review contract for the
local source (LocalHub#34).

The research adapter used to map a local hit into evidence as only its
`KnowledgeHit::snippet` — an ~320-character match window — while the review
surface labelled that field "Full source evidence." For web evidence the same
field holds the complete bounded fetched page; the cross-source label was not
truthful, and the fetchable `chunk_id` was discarded before candidate
creation.

1. **After a local hit passes admission, its complete bounded chunk is
   fetched** through the existing read-only fetch layer (`fetch_layer`, one
   batch call per gather) and carried as the evidence's `full_source`. The
   compact match-centred snippet stays as the finding preview/claim; the full
   chunk becomes the finding's review-only `evidence` — the same
   statement-vs-evidence split fetched pages get, so promotion still writes
   only the reviewer-distilled lesson (D-LM-0029), never the expanded source.
2. **Local evidence is explicitly bounded** (64 KiB, matching the web fetch
   bound and sitting under the renderer's 100 k safety net) and any cut is
   disclosed in place — never a silent truncation.
3. **Unavailable or stale is an explicit state.** A chunk id the index no
   longer holds renders a loud `[full source unavailable: …]` marker around
   the snippet; a stale chunk is prefixed with a `[stale: …]` warning that
   the content reflects the ingested version. A search snippet must never
   silently pose as full source.
4. **The chunk id survives** as `Provenance::fetch_id` next to the
   human-readable `path:start-end` locator, so review/diagnostic surfaces can
   re-fetch exactly what the locator points at.

## ADR-0089: Coverage Counts Source Families; The Report Accounts Retrieval Per Question

Status: accepted. Refines ADR-0078's coverage scoring and ADR-0079's loud
accounting (LocalHub#33).

Coverage used to treat every locator as an independent origin, so two chunks
from two files of one repository marked a question `covered`, ended its
follow-up rounds, and — with zero admitted web evidence — produced a fully
covered report that never said why web contributed nothing.

1. **Independence is measured in origins *and* source families.** An origin
   stays `source label + locator` (each web host one origin). A *family* is
   each web host for web evidence and the source label for everything else:
   files of one repository are one family. Full `Covered` requires ≥ 2
   floor-passing observations from ≥ 2 origins **and** ≥ 2 families; the same
   counts inside one family yield the new verdict `CoveredSingleSource`,
   rendered "covered (single source — not independently corroborated)".
   Local-only research stays legitimate — it is reported as locally
   supported, never as cross-validated.
2. **A single-family question keeps earning bounded follow-up rounds** while
   the source set could still supply a second family (another source label,
   or a web source — hosts are families); rounds stay bounded by the round
   budget and saturation. With only one family reachable, single-family
   coverage is the honest ceiling and is not re-targeted.
3. **Per-question, per-source retrieval accounting rides the report.** Every
   source call returns an account (proposed, admitted, rejected-low-relevance,
   policy-skipped, redirects, failures) that the engine merges per question,
   adds the engine-side below-floor count to, and publishes on
   `QuestionCoverage`; the Markdown report renders it with the admission
   diagnostics of ADR-0088. Counts and reasons only — no source content, no
   unredacted queries (those stay in the egress audit). With web enabled, a
   question with zero admitted web evidence carries an explicit source-gap
   line.
4. **A failing URL is a counted outcome, not a source abort.** A transport
   error or unreadable body on one web fetch is audited (`fetch-error`),
   counted as failed, and the gather continues — it no longer discards the
   evidence the same call already gathered.

## ADR-0088: Local Research Evidence Is Admitted At Question Level, Never By Within-Source Rank

Status: accepted. Amends ADR-0087 §2 (and the ADR-0086-rationale relative
normalization it referenced) for the local research source (LocalHub#32).

Relative-to-best normalization pinned every query's best non-zero local hit
to relevance `1.0`, so a corpus whose least-bad match shared only generic
terms with the question still cleared the `0.25` admission floor and could
become a supported finding. Rank answers "how does this hit compare with its
corpus siblings," not "does this answer the question" — the floor needs the
latter.

1. **Within-source rank and question-level admission are separate values.**
   The bm25-derived rank (relative to the query's best hit) is kept for
   ordering and diagnostics only. The relevance the engine's floor sees is a
   question-level admission signal.
2. **The admission judge is shared with web evidence.** When a model resolves
   (ADR-0087's reuse-only resolution), each candidate local chunk is
   classified against the sub-question with the same strict-JSON contract as
   a fetched page; a rejection is a counted outcome. Without a model, the
   deterministic fallback is `term_overlap_relevance` — significant-term
   coverage of the question against the chunk content — never the rank.
3. **Zero admitted local evidence is an honest outcome.** No hit is promoted
   merely for being its corpus's best; adding one genuinely answering chunk
   admits that chunk without lifting its corpus siblings, and unrelated
   chunks cannot change an admitted hit's relevance.
4. **The score contract is explicit.** `KnowledgeHit::relevance` (raw
   engine signal), the within-source rank, and the final admission relevance
   plus reason travel on the evidence as an `AdmissionTrail`, rendered
   content-free in the report's retrieval accounting (ADR-0089). Accepted
   memory keeps its reviewed-at-face-value relevance (ADR-0011), with its
   own trail reason.

## ADR-0087: Research Evidence Passes An Admission Gate; Promotion Writes Lessons, Never Source Dumps

Status: accepted. Amends ADR-0060 (which scoped the model to decomposition
only) and refines ADR-0067/0072/0075's evidence contract at the promotion
boundary; the review experience keeps the complete bounded evidence.

Two coupled defects (LocalHub#30, #24): every fetched page above zero term
overlap became a `Supported` finding and a low-confidence memory candidate
regardless of whether it answered anything, and the candidate fused its
statement with the full fenced source — so a later promotion wrote entire web
pages (navigation chrome included) into searchable accepted memory with the
trust bonuses of curated knowledge.

1. **Model-backed relevance admission for fetched web content**, immediately
   after reduction and bounding, before coverage, synthesis, findings, or
   candidates. Strict-JSON classification only (`relevant`, `score`,
   `reason`) against the sub-question — the model judges, it never authors
   or rewrites a finding. Model resolution is **reuse-only**: LocalMind's
   `[inference]` chat model first (when configured with its research feature
   enabled), the host's already-resolved default provider second; no third
   provider or research-specific model setting exists. A rejected page is
   recorded in the egress audit (`rejected-low-relevance`) — inspectable,
   never a silent drop — and its content never becomes a finding. An
   unavailable model or unusable reply degrades to the deterministic path.
   This deliberately amends ADR-0060: bounded fetched content may now be
   sent to the (local/consented) model for classification; the synthesis
   contract is unchanged (provenance-preserving heuristic, D010).
2. **The coverage floor is an admission floor.** Deterministically, evidence
   below the floor (`0.25`, one shared constant) neither counts toward
   coverage nor enters synthesis/findings/candidates; the count of withheld
   items is disclosed as a retrieval note. Local knowledge hits are
   normalized relative to their query's best hit before the floor (bm25
   magnitudes are corpus-dependent — same rationale as ADR-0086).
3. **Statement and evidence ride the candidate separately** (LocalMind
   D-LM-0029). The candidate's lesson text is the concise statement plus its
   source line; a distilled excerpt carries its full bounded source in the
   candidate's review-only evidence field and is marked as requiring a
   reviewer's edit before promotion — promotion of an unedited excerpt is
   refused with an actionable error. Sentence-free boilerplate statements
   (navigation chrome) get the same edit requirement, and are never
   auto-deleted. The report and review surfaces keep the full evidence
   (ADR-0067/0072/0075 hold); only the promotion projection narrowed.
4. **Legacy items are untouched**: candidates without the new fields promote
   exactly as before, and LocalMind's quality classifier retroactively flags
   existing raw-dump accepted memories for manual review (flag-only).

## ADR-0086: The Context Pack Ranks On Normalized Relevance — Reserve Floors, Task-Scored Session Facts, A Relevance-Ordered Window

Status: accepted. Amends ADR-0015: the composite rank's relevance component
is no longer the raw source score; everything else that ADR guarantees —
deterministic two-phase allocation, per-source reserves, the inspectable
signal breakdown, and manual-pin/accepted-memory protection — stands.

Three field failures shared one root (LocalHub#22, #25, #26): the allocator
summed raw scores from incompatible scales (ingest bm25 at a 10⁶ fixed
point, memory FTS at 10², fixed single digits for graph and session rows),
so the bounded bonuses were noise for one source and decisive for another;
reserves filled in source-priority order with no relevance requirement; and
the model-visible window rendered the pack in allocation order, letting one
source's reserve hide every relevant hit from the rest.

1. **One bounded relevance range.** Every candidate carries a unit relevance
   in `0.0..=1.0`, scaled to 200 rank points — the only relevance compared
   across sources. The two lexical sources (ingest, accepted memory) are
   normalized **relative to their own best hit of the query**: bm25
   magnitudes are corpus-dependent (IDF collapses toward zero on a
   degenerate corpus where every document matches — observed on the
   single-file test fixture), so no absolute bm25 curve can hold; relative-
   to-own-best is bounded, corpus-independent, and preserves each source's
   internal order, including the hybrid keyword-above-vector tiering inside
   ingest. Session facts score by lexical task overlap; graph rows carry
   fixed moderate values; a manual pin is `1.0`. Raw scores remain in the
   signal breakdown (`raw_relevance`) for diagnostics and are never compared
   across sources. On the 200-point scale the bonuses (source quality 5..40,
   recency ≤50, file match 20, confidence ≤15, proximity ≤9) close small
   relevance gaps and cannot overturn decisive ones — measurable and
   bounded, pinned by tests.
2. **Reserves demand relevance.** A candidate below its source's reserve
   floor (session facts 0.25; lexical sources 0.05; pins and graph rows
   unfloored — user-chosen and task-derived respectively) cannot consume its
   source's reserve. It still competes in the shared pool, so recall is not
   lost; the unused reserve falls to the pool. Reserve *protection* now
   means: relevant trusted content survives a flood — not: any content with
   the right source label gets a guaranteed slot.
3. **Session facts are scored against the current task** by term overlap
   before they become candidates at all; a fact sharing no significant term
   with the task contributes nothing, and one incidental term on a
   three-plus-term task reads as below-floor (mirroring the accepted-memory
   engine's coverage gate). Recency stays a boost for relevant facts, never
   a substitute for relevance.
4. **The visible window is relevance-ordered.** `knowledge_search` renders
   the selected pack sorted by final score, withholding entries below a
   small relevance floor (manual pins always render), and honestly returns
   fewer than `max_hits` — including zero — when nothing clears it. This
   filters and orders the *rendering only*; it never re-allocates or removes
   protected entries from the pack.

## ADR-0085: A Clean Stop Settles The Plan Panel — Truthful Statuses, Ended Presentation

Status: accepted.

The task checklist is a pure mirror of the model's most recent `update_plan`
call, and nothing reconciled it when a turn stopped: if the model finished
without a final all-`done` update (common — the tool relies on the model
remembering), steps lingered as `in_progress`/`pending` until the next
prompt's plan replaced them (LocalHub#20).

1. **A normal completion (`StopReason::Done`) settles the panel** via a
   dedicated `PlanSettled` UI event: the header becomes `plan (d/n) — turn
   ended` and non-`done` steps render inactive (dimmed). Statuses are never
   rewritten — a step the model left unfinished still says so; only the
   liveness presentation changes. Any later `update_plan` makes the panel
   live again.
2. **Abnormal stops (cancelled, timed out, degraded, provider error, budget,
   no-progress) never settle** — there the dangling `in_progress` view is
   the truth, and the UI must not dress an interrupted turn as an ended one.
3. The UI does not depend on the model remembering a final `update_plan`;
   no prompt-nudge change rides along.

## ADR-0084: Recalled Prompts Carry Their Pastes — Placeholders Rehydrate Instead Of Replaying Verbatim

Status: accepted. Extends ADR-0040 (durable prompt history) additively; the
history file format version stays 1.

A prompt containing a large paste is collapsed to a placeholder (`[10 pasted
rows #1]`) in the composer, and only the placeholder form was recorded into
recall — in-session (the placeholder→content map was cleared on submit) and
durable (only the visible text was persisted). Recalling such a prompt and
submitting it sent the literal placeholder string to the model; the pasted
content was unrecoverable (LocalHub#19).

1. **A recall entry is text plus its paste mappings.** The composer's three
   recall lists and the durable `HistoryEntry` each carry the
   placeholder→content pairs the prompt's visible form depends on (only the
   mappings whose placeholder actually occurs in the submitted text).
   Recalling an entry restores its mappings into the active paste set, so the
   input line stays compact and submit expands exactly as the original did —
   in-session and across restarts.
2. **Placeholder numbers are session-monotonic.** The paste counter no longer
   restarts after each submit, so two pastes in one session can never share a
   placeholder string; on the residual cross-session collision (same row
   count, same number) expansion prefers the newest mapping.
3. **Paste content in history stays raw, like the prompt text around it** —
   the module's founding rule (recall must be verbatim; redaction would
   corrupt what the user resubmits) applies unchanged, and the same privacy
   controls cover it: the `[history] persistence` opt-out (mappings ride the
   same entry and the same no-op), the 0600/per-user file, the entry cap.
4. **A per-entry budget (64 KiB of paste content) bounds the file.** The
   global history is re-read and rewritten every submit, so a monster paste
   would tax every future submission. Over budget, the entry is kept without
   its mappings — recall then replays the placeholder, exactly the old
   behaviour, for that entry only.
5. **Old history lines load unchanged** (`pastes` defaults to empty; entries
   without pastes serialize in their old shape), and old builds reading a new
   file ignore the extra field.

## ADR-0083: Session Logs Append And Recover — One Damaged Line Never Loses A Session

Status: accepted. Refines the store's write discipline (crate docs previously
promised temp-then-rename for *everything*); resume, replay, and audit stay
one mechanism.

A session's `<id>.events.jsonl` reached the field with one physically
truncated line (LocalHub#21): the record was cut mid-`[REDACTED]` marker —
after serialization and redaction, so the damage was byte-level, most likely
a torn write during a crash or power loss (`fs::write` never syncs data
blocks, so a rename can survive a crash whose data does not). The reader
(`read_events` → `collect::<Result<…>>`) was all-or-nothing, so that single
line poisoned all 1,923 intact events and resume refused the session
entirely. Worse, every append re-read and rewrote the whole file, faithfully
carrying the damaged line forward forever — O(n) per append and structurally
unable to self-limit corruption.

1. **Line-delimited session logs (events and transcripts) grow by guarded
   append (`append_line`), not read-modify-rewrite.** Appending one record
   touches only the tail: existing damage is never re-written, re-serialized,
   or propagated, and each append is O(1). If the current tail is an
   unterminated line (a torn write), the append first seals it with a newline
   so the damaged bytes stay quarantined on their own physical line and can
   never swallow the new record.
2. **Reads recover around damage instead of dying of it.**
   `read_events_recovering` skips a line that is not a parseable record and
   counts it; `read_events` and `read_transcript` share the behavior. Resume
   (`load_session`) returns the skip count as a `SessionRecovery`, and every
   resume surface (REPL notice, `print --resume`, `rpc serve`) tells the user
   when lines were skipped. The file itself is left untouched — recovery is
   read-side, and the log remains inspectable evidence.
3. **The version fence stays fatal.** A line whose `v` is newer than the
   build understands is still a typed `UnsupportedFormat` error, never a
   skip: that is a build/downgrade incompatibility, not disk damage, and
   silently dropping records a newer build wrote would be a silent misparse
   (unchanged from the events module's founding rule).
4. **Appends still do not fsync.** Durability of the final pre-crash record
   is not the goal — bounded blast radius is. With appends the worst a crash
   can leave is one torn tail line, which (1) and (2) contain and recover.

## ADR-0082: Folder Ingest Bridges Markdown Into LocalMind's Documentation Index

Status: accepted. Narrows ADR-0013 (ingestion is derived, disposable state
under `.localmind/ingest/`).

The LocalMind UI presents a Docs tab, but nothing in the product flow
populated it: folder ingest wrote only its derived chunk store, and the sole
writer of the documentation index (`doc_chunk`) was a separate CLI command
(`localmind ingest docs`) users had no reason to know about. The tab read as
broken (LocalHub#18).

1. **Each ingest run bridges candidate Markdown files into the project's
   `doc_chunk` index** through `localmind_store::ingest_doc_text`, reusing the
   walk's own candidate set (include/exclude rules honored) and redacting
   content with the same scrub every persisted chunk gets.
2. **This is a deliberate exception to ADR-0013's derived-state-only rule, and
   it is not a memory write.** `doc_chunk` is a documentation *index* of files
   already in the workspace — not accepted memory, so the review gate does not
   apply. The research-report bridge (`[research] ingest_report`) set the
   precedent; that one stays opt-in because its content is model-generated,
   while this one is on by default (`[ingest] docs_index = true`) because the
   content is the user's own files and an empty-by-default Docs tab recreates
   the bug.
3. **A hash ledger (`.localmind/ingest/docs-index.json`) keeps the bridge
   incremental and honest**: unchanged files are a no-op, vanished files are
   deleted from the index, and the ledger only ever deletes paths the bridge
   itself wrote — doc entries from other hosts (research reports) are never
   touched. Best-effort throughout: a doc-index failure never fails the run.
4. **`[ingest] embed_chunks = false` keeps its promise across the bridge.**
   Doc-chunk vector writes ride the same suppression as chunk-store
   embeddings (`ingest_doc_text`'s `embed` flag): text still lands and stays
   browsable, it just is not semantically searchable until embedded.

## ADR-0081: Intake Carries A Guidance Gate — Model-Proposed Axes, A Deterministic Score, And A Pause Instead Of A Guess

Status: accepted. Builds on ADR-0010 (the model proposes, the runtime
decides) and ADR-0035 (planning judgment expressed as contracts on the
documents the flow already produces).

Job 1 happily turned an under-specified idea into a confident `brief.md`:
nothing checked whether the idea actually contained the product decisions the
brief was about to encode, so the harness would pick an interpretation and
build the wrong thing — burning a full plan-and-execute cycle before the user
found out (LocalHub#15).

1. **The model proposes axes; the runtime scores them.** An off-by-default
   pre-brief assessment (`[harness.guidance]`, per-run `--guidance` /
   `--no-guidance`) has the model enumerate the idea's *decision axes* — the
   small set of product decisions that would change what gets built, invented
   per idea, never a fixed domain list — marking each resolved (the idea's own
   words, quoted verbatim) or not specified, with a settling question for open
   axes. The score is deterministic and runtime-computed: resolved ÷ total,
   `1.0` on zero axes. The model never reports its own score.
2. **Below the threshold, intake pauses instead of guessing.** On a terminal:
   stdin Q&A (empty answer delegates that one axis); answers fold into the
   idea as an explicit user-decisions block, so the brief still stands without
   the transcript. On a non-terminal: a structured `needs_guidance` JSON
   report, no `brief.md`, exit 0 — pausing is a deliberate outcome, not an
   error. `--assume-judgment` proceeds anyway with the delegation recorded.
3. **The score is a signal, never a safety claim.** Its recall failure mode is
   structural: an axis the model never lists cannot count against the score,
   so a confidently wrong high score is possible. Every surface that shows the
   number also records the full axis list (`.localpilot/intake.jsonl` carries
   axes, score, threshold, questions, answers, and any assumed-judgment flag),
   and an instrument suite gates trust: an authored idea corpus with
   ground-truth axes, an offline ordering self-test (every under-specified
   fixture strictly below every well-specified one, violations named —
   refuse-to-trust, not a softened threshold), and live-gated recall/precision
   drift checks where a missed ground-truth axis fails its fixture regardless
   of the score.
4. **Scope.** v1 gates ground-up `harness intake` only, asks over stdin (no
   multiple-choice UI machinery exists), scores axes equal-weight, and records
   the clarified-before-brief signal in LocalPilot's own artifacts rather than
   the shared scorecard schema.

## ADR-0080: Provider HTTP Timeouts Bound Silence, Not Duration

Status: accepted. The provider adapters applied `request_timeout_secs`
(default 600) as a whole-request `reqwest` deadline, covering the entire
streamed response. On a slow local server — CPU-fallback inference is the
common case — a healthy turn simply outlasts the deadline: the client cuts
the connection at exactly 600 s, the cut surfaces as a stream truncation,
and the turn loop retries an identical request into a server that is now
*slower* (the abort invalidated its prompt cache; SWA/hybrid models
reprocess the full prompt from zero). Each retry is guaranteed to die the
same way, and after the retry ceiling the user is told the server "may have
crashed or run out of VRAM" — blaming the server for a client-imposed
deadline (LocalHub#17).

1. **The HTTP layer bounds liveness only.** `request_timeout_secs` is now a
   **stall window**: the longest tolerated silence while a response is open —
   from sending the request to the first byte (a local server may hold
   headers back until prompt processing ends), and between stream chunks
   after that. A server that keeps streaming, however slowly, is never cut
   off. Total turn duration is the harness's concern
   (`[harness] turn_timeout_secs`), not the transport's. TCP connect gets
   its own short budget (30 s) so an unreachable server still fails fast.
2. **A stall is its own error, and it does not retry.** A tripped stall
   window surfaces as `ProviderError::StreamStalled` — distinct from
   `StreamTruncated` (the server *closed* the response early, which stays
   retryable). A stalled server is most likely still working, just slower
   than the window; re-issuing the identical request cannot finish faster
   and restarts prompt processing from zero on models without reusable
   prompt cache. The turn stops immediately with guidance naming the two
   real remedies: check GPU offload (CPU-speed inference), or raise
   `request_timeout_secs`.

Fixes the failure mode where 4 × 10-minute futile retries ended in a
message that misdirected the user at the server.

## ADR-0079: Research Evidence Is Deduplicated, Diversity-Capped, Relevance-Scored, And Every Cut Is Loud

Status: accepted. At exhaustive scale (ADR-0078 rounds × ADR-0076 open-web
reach) the evidence pool needs discipline that a single-pass loop never did:
mirrors repeat content, one domain can flood a question, and web evidence
carried a flat 0.5 relevance that said nothing about the page.

1. **Near-duplicates fold, provenance survives.** Word-shingle Jaccard
   (≥ 0.7, std-only hashing) folds a snippet that re-states kept content into
   the survivor; the duplicate's provenance rides on the surviving evidence
   (`also_from`) into the finding's `supporting` list and the coverage
   account — a mirror on a second origin counts as corroboration and as an
   independent origin, it just doesn't repeat the text.
2. **No origin saturates a question.** After each question's gather, a soft
   per-origin cap (3 snippets per question per origin, highest relevance
   kept) applies whenever more than one origin is answering — order-
   independent, and a lone origin is never capped.
3. **Web evidence is scored, not assumed.** A fetched page's relevance is its
   content-term overlap with the sub-question (the shared search path's
   ≥ 2-term coverage rule applied to pages: fewer than two matching terms
   floors at 0.1, under the coverage floor), replacing the flat constant —
   so an off-topic page can be fetched yet never counts as coverage.
4. **Every cut is loud.** Folds, diversity drops, the evidence cap, the time
   budget, and cancellation each append a `Retrieval notes` line to the
   report and the run output — silent truncation reads as "covered
   everything" when it didn't (the standing lesson from the evidence-budget
   saga, ADR-0075).
5. **Fetching is polite.** Within a run: repeat visits to a host are paced by
   the host's own last response time (clamped 250 ms–3 s), and a 429/5xx
   cools the host down for the rest of the run — a host-level signal, not a
   per-URL one — audited as `host-cooldown`. Rate-limited designated search
   tools were already classified (ADR-0077); together the run degrades
   gracefully instead of hammering.

Extends ADR-0075/0076/0077/0078.

## ADR-0078: The Research Loop Is Multi-Round And Coverage-Driven, With Deterministic Scoring And Stops

Status: accepted. The research loop was single-pass: one gather per
sub-question, and a question whose gather returned nothing was reported as an
"open question" with no further attempt — the loop stopped exactly where a
weak local model's first query landed. Field evidence (a 16-source run ending
with two open questions) showed that reads as shallow, not safe.

1. **Rounds, targeted.** Round 1 gathers for every sub-question; each later
   round re-queries only the questions not yet covered. `max_rounds = 1`
   reproduces the old single-pass behaviour exactly.
2. **Coverage is scored deterministically.** Per question: evidence at or
   above a relevance floor counts, and independence is measured in distinct
   origins (web host, or source label + locator). Covered = ≥ 2 floor-passing
   snippets from ≥ 2 origins; Weak = some evidence; Open = none. No model
   judges coverage — the scoring is unit-tested arithmetic, so a weak model
   cannot talk the loop into stopping (or spinning).
3. **Follow-up queries are retrieval-side and drift-guarded.** A targeted
   question is re-asked verbatim plus one reformulation from the
   `Synthesizer` seam. The default (and only shipped) reformulator is
   deterministic pseudo-relevance expansion: salient terms from the
   question's own evidence, each required to appear in ≥ 2 distinct origins
   (one off-topic page cannot steer the query), appended to the original
   question. The model may later assist through the same seam — retrieval
   assistance only, never synthesis (the ADR-0060 provenance contract is
   untouched).
4. **Depth escalates for stubborn questions.** Later rounds widen the
   per-source gather depth (×2, capped ×3), so a re-query can reach past the
   first page of results — still inside the caps.
5. **Every stop is explicit, all any-of.** All covered · a round with zero
   previously-unseen evidence (saturation — dedup is against everything
   *seen*, not kept, so re-finding old ground is not progress) · `max_rounds`
   · a hard total-evidence cap · an optional wall-clock budget · an external
   stop flag (`RunControl`), which yields a partial but well-formed outcome
   for cancellation.
6. **The report carries the account.** Per-question coverage (verdict,
   counts, distinct origins) and per-round summaries ship in the outcome;
   open questions are exactly the questions still `Open` after the final
   round. Both research surfaces print the round lines and a coverage
   summary.

Extends ADR-0060; the sanitize/cross-check passes and ADR-0072 candidate
provenance are unchanged.

## ADR-0077: Designated MCP Search Tools Propose Research URLs — Leads, Never Evidence

Status: accepted. Research web retrieval had no real search: the model
proposed URLs from its own parametric memory (a documented limitation of
ADR-0060's v1), which is exactly the weakness a small local model brings.
MCP search servers exist and users run them; the question was how research
uses one without breaking the egress boundary (ADR-0076) or the clean-room
rule.

1. **Explicit designation, never discovery.** `[research.mcp] tools` names
   `(server, tool)` pairs referencing `[mcp.servers]` entries; empty means no
   MCP server is consulted. Auto-detecting "search-capable" tools is both
   unreliable (no naming convention exists across search servers: `search`,
   `brave_web_search`, `tavily-search`, `web_search_exa`, …) and
   policy-hostile (consulting a server egresses the sub-question). The only
   portable input is `query`; the proposer sends only that.
2. **Proposals are leads, never evidence.** Extracted URLs join the model's
   proposals (search first, model filling the remaining budget, exact-dedup)
   and flow through the unchanged `WebAccess` gate: allowlist/disallowlist,
   no-redirect bounded fetch, per-request audit. A search result's snippet
   text never becomes evidence — only a gated fetch does. The audited
   boundary stays total.
3. **The search call is audited egress.** Each designated-tool invocation
   writes an audit line (`decision=search`, or `search-error` /
   `search-rate-limited` / `search-timeout`) carrying the redacted query —
   the same redaction contract as every other outbound research byte. The
   run disclosure names every designated tool before any call.
4. **Best-effort by construction.** Connection failures are disclosed and
   skipped; a call is bounded by a fixed timeout (the stdio transport has
   none of its own); an `isError` result is classified (rate-limit vs
   failure) and never parsed for URLs; a URL-less result is an empty round,
   not an error. Research works with search and no model, a model and no
   search, both, or neither.
5. **Parsing is spec-grounded and shape-tolerant.** The extractor
   (`localpilot-mcp`) reads `structuredContent`, JSON-in-text (the
   spec-sanctioned dominant shape), plain-text scans (`URL:` lines, markdown
   links, bare URLs), and `resource_link` items, in that order, iterating
   every content item. Research consumes the MCP **client** crate directly —
   it does not enter the agentic tool loop, and the designated call rides
   the research run's own consent + disclosure rather than a per-call
   permission ask (the run banner is the consent surface; the permission
   engine still governs MCP tools in normal sessions).
6. **Clean-room posture.** LocalPilot itself never scrapes a search engine.
   Reach comes from the user's explicitly designated server; docs disclose
   that some community servers scrape while vendor/self-hosted ones speak
   official APIs — that choice and its provenance belong to the user.

Extends ADR-0060/ADR-0076; the ADR-0031 catalog/broker and ADR-0073 server
adapter are untouched.

## ADR-0076: Research Web Egress Is On By Default — Disclosed, Audited, Disableable — On Both Surfaces

Status: accepted. Amends ADR-0060 point 5 (web egress off by default,
subcommand-only) after field evidence that default-off research is
self-defeating for the product's job: a local model's parametric memory
cannot carry a research run, and a run that silently stops at whatever the
local stores contain reads as shallow rather than safe. The product-owner
ratified flipping the default; the egress *boundary* does not move.

1. **On by default, open reach by default.** `[research.web].enabled`
   defaults to `true`, and an unset allowlist defaults to `["*"]` (open web).
   An **explicitly** empty `allowlist = []` keeps the old meaning — every
   host needs confirmation, so nothing is fetched in v1 — because unset and
   empty are different user statements: absence takes the product default,
   a written empty list is a deliberate restriction.
2. **Both surfaces, same posture.** Interactive `/research` now runs the same
   web-enabled path as the subcommand (amending the interactive-local-only
   scoping ADR-0060 inherited from its plan), printing the same disclosure
   into the transcript. One research surface, one egress posture.
3. **The kill switches are two, and one is absolute.** `--no-web` skips the
   web source for a run — no fetch, no URL-proposal model call.
   `[research.web].enabled = false` removes the outbound path entirely and
   cannot be overridden by any flag: `WebAccess::grant_session` remains a
   no-op against config-off, so the guarantee is structural, not
   conventional. `--web` is kept as a compatibility no-op.
4. **Everything the 2026-06-30 security review pinned still holds.**
   Disallowlist beats allowlist including `*` (ADR-0068); only the redacted
   sub-question ever leaves the machine; every outbound request (and every
   skipped host and unfollowed redirect) is audited one line each to the
   egress audit log, whose default path applies whenever web is active;
   redirects are never followed; fetches stay time- and size-bounded.
5. **The disclosure is the consent carrier.** Every web-active run prints,
   before any request: the default-on posture and both off-switches, what
   egresses, the effective reach ("open web" under `*`, the domain list
   otherwise, or the explicit-empty warning), disallowlist carve-outs, and
   the audit-log path. Per-session consent (`grant_session`) is recorded
   after the disclosure rather than after a `--web` flag.

This is a ratified, documented exception to the default-off rule (rule 1) of
the ecosystem remote-egress policy; the policy's other four rules —
explicit control, disclosure, auditability, kill switch — are unchanged and
load-bearing.

## ADR-0075: Web Evidence Is Reduced To Markdown And Carried In Full, With Truncation As A Loud Safety Net

Status: accepted. Refines ADR-0067/0072 after the third and fourth rounds of
the same field report (LocalHub#1): evidence still ended in a silent mid-word
"… (truncated)" — a display budget far below what a source legitimately
gathers — and the flat-text HTML reduction had discarded the structure
(headings, links, code blocks) that a reviewer and a model both read.

1. **HTML becomes Markdown at gather time, not flat text.** The
   dependency-free reducer maps structural tags onto Markdown — headings to
   `#`, list items to `- `, `<a href>` to `[text](url)`, `<pre>` to a fenced
   code block whose fence is sized past any inner backtick run, inline
   `code`/`strong`/`em` to their Markdown forms — while still dropping whole
   non-content elements (script/style/chrome) body-and-all, decoding entities,
   and staying total and panic-free on malformed input. The `Content-Type`
   gate is unchanged: declared non-HTML bodies stay verbatim.
2. **Content is bounded at gather; render is not a budget.** The only real
   content bound is the per-fetch cap on already-reduced text. The render
   ceiling (`MAX_EVIDENCE_CHARS`) sits deliberately *above* every gather
   bound, so a finding's full source rides the Markdown report and the review
   candidate intact — a reviewer judges and reuses the whole content, which
   was the point of carrying evidence at all.
3. **Any cut is loud and lands on a line boundary.** Both the fetch bound and
   the render safety net cut on a line boundary (falling back to a plain cut
   only for one enormous line) and append an explicit note saying a cut
   happened — the render note quantifies kept-of-total characters, and the
   finding's provenance URL always points at the full source. A silent
   mid-word ellipsis is never emitted.
4. **One-line excerpts flatten Markdown syntax too.** The sanitize pass
   distils excerpts from the original multi-line text (Markdown markers are
   positional), stripping fences, heading/list/quote markers, and collapsing
   `[text](url)` to its text, so a claim line never leaks syntax; an over-long
   prose statement is titled `Excerpt from <source>: …` like any other blob.
5. **Still no model rewrite.** Synthesis stays heuristic (ADR-0060): findings
   are never model-authored, so a finding can never carry an unbacked claim.
   Readability comes from faithful reduction, not generation.

## ADR-0074: Driver Corrections Are Captured With Honest Provenance And Ride The Review-Gated Queue

Status: accepted. Extends ADR-0037/0072 to sessions driven by an external
agent host over the MCP adapter (ADR-0073): the driver's corrections are
training signal, and they must reach memory the same gated way everything
else does — no new store, no auto-promotion, no masquerade.

1. **Capture at the adapter, durably.** Steers, cancellations, and permission
   replies are recorded as `driver_intervention` events in the session event
   log (format v8, additive), carrying the action, its detail (the steer text
   or the answered ask's tool+detail), the running activity when known, and
   the driving client's self-reported identity from the MCP handshake. The
   log is written whenever the runtime is free — the pinned turn future owns
   the runtime mid-turn, so mid-turn interventions persist at the next turn
   boundary.
2. **Corrections become candidates; consent does not.** On disconnect the
   session's steers, cancels, and denies are offered to the ADR-0037
   review-gated queue (capped per session); approvals stay event-log-only —
   routine consent teaches nothing and would flood review.
3. **Honest provenance, ADR-0072 pattern.** The candidates are enqueued under
   the `driver-intervention` session label with a `driver-` id prefix, a
   `driver_intervention` evidence kind, and evidence naming the driving
   client — never presented as the session's own retrospective. All existing
   gates apply: quality bar, near-duplicate folding, human promotion
   (ADR-0011).
4. **Candidate text leads with the correction.** The review queue folds
   lexical near-duplicates, so templated boilerplate would merge distinct
   corrections into one candidate; the composed text puts the distinct
   content first and keeps framing minimal. Pinned by capture, provenance,
   filter, and cap tests across the adapter, bridge, and host layers.

## ADR-0073: An MCP Server Adapter Exposes The Session Runtime To Agent Hosts

Status: accepted. Extends the headless-drive family (native RPC, ACP) with a
third stdio adapter in `localpilot-rpc`, so an MCP client — an agent host
such as Claude Code or Codex — can drive and steer a LocalPilot session
through ordinary tool calls (`localpilot mcp serve`).

1. **Same crate, same runtime, same plumbing.** The adapter lives beside the
   ACP adapter in `localpilot-rpc` (the crate that owns headless wire
   adapters over the in-process `SessionRuntime`), reuses the `RpcApprover`
   halves, the native protocol's event vocabulary, and a shared LF-framed
   JSON reader. `localpilot-mcp` remains the MCP *client* only. No HTTP
   server, no SDK dependency: JSON-RPC 2.0 (spec revision 2025-06-18) is
   hand-rolled, as the ACP adapter proved viable.
2. **Pull-based events with counted loss.** MCP is request/response, so
   session events are buffered in a bounded, monotonically numbered feed;
   the `events` tool pages after a client cursor with a server-capped
   bounded wait (well under common client tool timeouts). Overflow drops
   oldest entries and reports the count — a lagging client sees that it
   lagged, never a silent gap.
3. **The permission engine stays authoritative.** The client only answers
   asks it is shown; an unanswered ask denies, exactly as non-interactive
   mode; `--no-approvals` withholds the reply tool entirely
   (watch-and-steer: every ask denies, fail-closed). Nothing the adapter
   exposes can widen an engine verdict.
4. **A served session is a full session.** `steer`/`follow_up` dispositions,
   session resume (`--continue`/`--resume`, shared with `rpc`), and the
   closeout-on-disconnect learning path behave identically to the other
   interactive hosts. Pinned by a duplex integration suite and CLI parse
   tests.

## ADR-0072: Research Memory Candidates Carry Honest Provenance, And Excerpts Are Distilled, Not Dropped

Status: accepted; point 2 amended (see below) after the original rule proved to
zero the pipeline. Refines ADR-0060/0067/0037 after a review-queue item proved
unreadable: a `/research` candidate whose body was a truncated console-log
excerpt, enqueued under the `completion-retrospective` session label — the
reviewer could tell neither where it came from nor what it was supposed to
teach.

1. **Honest provenance on the shared queue.** Research findings keep riding
   the `write_retrospective_lesson` review-gated path (ADR-0060), but the
   lesson now carries its origin: a research candidate is enqueued under the
   `research` session label with a `research-` id prefix and
   `research_finding` evidence kind; only genuine completion retrospectives
   are labelled `completion-retrospective`/`retro-`.
2. **A candidate body is the distilled statement, not the raw blob.** The
   ADR-0067 sanitize pass reduces a finding whose statement was a raw source
   blob to a readable, single-line, length-capped excerpt titled with its
   source (`Finding.statement`), and moves the full text to `Finding.evidence`.
   `candidates_from` uses that already-distilled `statement` as the candidate
   body; the raw blob in `evidence` stays in the rendered report only. So the
   reviewer sees a clean claim with provenance, never a pasted log or code
   chunk — the original readability goal — without discarding the finding.
   *(Superseded in part: the "report only" clause proved to leave the reviewer
   a one-line excerpt they could neither judge nor reuse; the full source now
   rides the review candidate as a fenced evidence block under the distilled
   claim, and is carried in full per ADR-0075.)*
3. Pinned by a candidates-filter test and a queue-label test.

**Amendment (regression fix).** Point 2 originally *excluded* every finding with
`evidence.is_some()` from the review queue (report-only). Because synthesis is
provenance-preserving and heuristic (the model only decomposes; every finding is
a gathered source excerpt), the sanitize pass sets `evidence` on essentially all
findings over code/HTML/long-doc sources — so the exclusion silently enqueued
*zero* candidates for the common research run, and nothing reached the LocalMind
review UI. The rule is corrected to distil-not-drop: an excerpt finding stays a
candidate, carrying its sanitized one-line `statement` (already readable and
length-capped) as the body, while the raw blob remains report-only. Lesson: a
candidate filter must be validated against the real producer's output (heuristic
excerpts), not an idealized clean-claim fixture.

## ADR-0071: Permission Profile Slash Commands Apply Mid-Turn Through A Shared Engine Handle

Status: accepted. Extends ADR-0070 after `/unrestricted` proved to be
idle-only: a profile switch only reconfigures LocalPilot's own permission
engine, so making the user wait out a generating model was gratuitous.

1. **The engine is shared and swappable.** `SessionRuntime` holds the
   permission engine behind a `PermissionEngineHandle`
   (`Arc<RwLock<PermissionEngine>>` in `localpilot-sandbox`); the runtime
   snapshots it fresh per tool call (it always re-decided per call — the swap
   just needed a path around the turn's mutable runtime borrow). A poisoned
   lock recovers the inner value: a half-finished swap is impossible, both
   pre- and post-swap engines are valid.
2. **Profile slash commands join the mid-turn allowlist.** `SetProfile` is
   accepted by the live-slash gate — the same pattern `/bg` uses (a cloned
   handle, never the borrowed runtime) — swaps the shared engine, updates the
   footer, and notices that the profile is in force from the next tool call.
   `set_permission_profile` (the idle path) writes through the same handle, so
   the two paths cannot diverge.
3. **Scope: the agent turn only.** Drives without a live session runtime
   (compaction, research, the harness resume — which constructs its own inner
   runtime from the profile captured at start) degrade to an explanatory
   notice. A pending approval prompt still captures its y/n keys first; the
   switch applies once answered.
4. Pinned by an engine-handle swap test, a runtime dispatch test (deny → swap
   via handle → next dispatch allowed), and a live-slash wiring test.

## ADR-0070: Out-Of-Workspace Access Is Grantable — Prompt, Standing Read Grants, And An Unrestricted Profile

Status: accepted. Amends the permission-profile rules in docs/07 after an
out-of-workspace `list_files` denial proved to be a dead end: under `bypass`
the path effect was hard-denied with no prompt (strictly *weaker* than
`default`, which asks), and no profile offered a standing or non-interactive
grant.

1. **Bypass asks instead of hard-denying the workspace boundary.** An
   out-of-workspace path effect under `bypass` now takes the same gate as
   `default`: prompt when interactive, deny when non-interactive. Bypass's
   "no prompts" property is narrowed to everything *inside* the boundary; the
   one prompt it keeps is the boundary itself. This is a permission-contract
   change; the pinned test is
   `bypass_asks_for_out_of_workspace_paths_and_denies_them_headless`.
2. **`[permissions] extra_read_roots` — standing read grants.** Configured
   directories are canonicalized at startup and widen only the *read* scope
   (`Workspace::read_scoped`), in every profile and non-interactively. Writes
   and `Workspace::resolve` keep the hard boundary; secret-like reads keep
   their gate; a root that fails to canonicalize is reported and skipped —
   never silently widened or ignored.
3. **A fourth profile, `unrestricted`.** Approves every effect — out-of-
   workspace paths included — with no prompts; the user explicitly accepts
   full responsibility. Never the default; set per launch (`--permission
   unrestricted`), per session (`/unrestricted`), or per project config. It
   is rendered in the footer in the strongest warning style, implies
   workspace trust like `bypass`, and does not disable redaction, logging,
   or harness rule verdicts.
4. **Denials are actionable.** An out-of-workspace permission denial names
   the target and all three remedies (interactive approval,
   `extra_read_roots`, `--permission unrestricted`) in the model-visible
   error, so the model can relay how the user lifts the restriction instead
   of dead-ending.

## ADR-0069: Oversized Single-File Writes Are Prevented At The Prompt And The Write Tool, Not Only Recovered

Status: accepted. Complements ADR-0038 (an oversized malformed write is
recovered by chunking): that rung is the *cure*; this ADR adds *prevention* so
the pathological single-huge-file generation is usually avoided before the call
is ever sent.

1. **Always-on prompt guidance.** The agent system prompt now instructs the
   model to split a large implementation across several small, modular files
   rather than emitting one enormous file, and to treat a "keep it in one file"
   request as a preference that yields once a file would become very large. This
   is the only surface that reaches every run — the curated seed lessons are
   ingest-gated and do not.
2. **Soft write-size guard in the tool path.** `write_file` refuses a single
   payload larger than a soft limit (64 KiB) with a message steering the model
   to split into modular files or build the file up with `append_file`. The
   limit sits above any normal source file, so it only trips on the pathological
   path; `append_file` is deliberately unguarded because it is the piece-wise
   escape hatch.
3. **Recovery is unchanged.** The ADR-0038 chunked-write recovery remains as the
   backstop for an oversized call that still slips through. Prevention plus cure,
   not prevention instead of cure.
4. **Seed reinforcement.** The opt-in curated coding lessons gain the same
   "decompose a large implementation into modular files" guidance; this only
   reaches a project after `ingest run`, so it reinforces (1), it does not
   replace it.

## ADR-0068: Research Web Egress Supports Wildcard Allow And A Deny List, With Deny Winning

Status: accepted. Extends ADR-0060 (research web egress off by default). The
`[research.web]` gate gains broader-but-carve-out expressiveness without
weakening the fail-closed default.

1. **Wildcard patterns in both lists.** A list entry of `*` matches every host;
   `*.example.com` matches `example.com` and any subdomain; a bare domain keeps
   the prior exact-or-subdomain rule. One `host_matches` helper serves the
   allowlist and the new disallowlist (no duplicate matcher).
2. **A `disallowlist`, checked first.** `[research.web].disallowlist` blocks
   hosts even when the allowlist — including `*` — would permit them. Deny is
   evaluated before allow and wins ties, so `allowlist = ["*"]` with a
   `disallowlist` allows broad access while carving out specific domains.
3. **Fail-closed default is unchanged.** `enabled = false` with empty lists still
   fetches nothing, and per-session operator consent is still required at
   runtime; the wildcard is only ever an entry the user typed, never a default.
4. **Egress-safety is pinned by tests**: deny-beats-wildcard-allow,
   deny-beats-exact-allow, `*.domain` matches apex and subdomain, and the empty
   fail-closed default.

## ADR-0067: Research Findings Are Concise Claims With Raw Source Text Carried As Separate Evidence

Status: accepted. Refines ADR-0060 (the research loop) after raw retrieved
chunks (JavaScript, HTML boilerplate, analytics) were surfacing verbatim as
"findings" and breaking the report layout.

1. **A finding is a claim, not a snippet.** `Finding` gains an `evidence` field.
   A deterministic sanitize pass in the research loop (one choke point, after
   synthesis and before cross-check) moves any statement that is a code/HTML blob
   or is over-long into `evidence` and replaces the statement with a concise,
   single-line excerpt; a clean statement is only whitespace-flattened. No
   finding is ever dropped.
2. **The model-free path stays model-free.** This is a deterministic sanitize,
   not a model-backed synthesizer (explicitly not built): the degrade path
   labels an excerpt honestly ("Excerpt from {source}: …") rather than
   fabricating a summary. A future model-backed synthesizer could improve wording
   without changing this contract.
3. **One pass fixes both consumers.** Because sanitize runs in the loop, both the
   rendered Markdown report and the enqueued review-queue memory candidates get
   the clean statement — a raw blob can no longer leak into memory.
4. **Rendering is defensive.** The Markdown heading flattens the statement so it
   can never break the `_(supported)_`/`Sources:` layout, and evidence renders in
   a fenced block whose fence is chosen longer than any backtick run inside it,
   truncated at a display cap.

## ADR-0066: The Discard/Reset Rung Is Real, Fed By Rule Severity, With A True Working-Tree Restore

Status: accepted. Implements the harness-spec anti-sunk-cost `discard`
verdict that had been documented but structurally unreachable
(`RuleVerdict::Discard` was never constructed; the worker folded `Discard`
into `Retry`; the resume arm was dead and restored nothing).

1. **Construction path is config-driven rule severity.** `RuleSeverity` gains
   `discard`: a rule configured `[harness.rules] <name> = "discard"` (e.g.
   `quality_gate = "discard"`) escalates its actionable `retry` failures to
   `RuleVerdict::Discard`. It is an *escalation*, not a ceiling — `block`
   still wins, `allow`/`warn` pass through. Rule-level only: per-check
   `severity = "discard"` is rejected at config load, because the per-check
   severity rides the shared check-runner contract, which has no discard
   notion.
2. **The worker preserves discard-ness.** `StepAction::Discard` ranks between
   `Retry` and `Block`; the resume loop feeds it to the anti-sunk-cost
   `StepLoop` as `AttemptResult::Discard`, reaching the previously-dead
   `DiscardAndReset` arm.
3. **DiscardAndReset now restores committed state** before the fresh attempt:
   `git reset --hard HEAD` + `git clean -fd` (ignored files — the
   `.localpilot/` execution record — untouched; the resume flow refuses to
   start over unrelated uncommitted changes, so everything removed belongs to
   the discarded attempt). The abandoned attempt still closes its transcript
   branch with a failure digest and the fresh attempt forks from the step
   anchor, so the discarded line stays auditable.
4. **Default behaviour is unchanged**: no baseline rule defaults to
   `discard`; without the config the ladder is Retry-only exactly as before.
   An end-to-end harness test on a scratch repository pins the contract: a
   discarded attempt's stray file never leaks into the next attempt or the
   step commit.

## ADR-0065: Memory-Injection Retrieval Consumes The Engine's Rerank Stage, Opt-In Via The Engine's Own Config Keys

Status: accepted. Refines ADR-0059 (semantic relevance gate); the engine-side
contract is LocalMind D-LM-0026.

The always-on context-injection retrieval (`context_hits`) gains an **opt-in
rerank stage** consumed from the engine's `localmind-search` crate instead of
a host-grown reordering: when the project's `.localmind.toml` sets
`[retrieval] rerank = true` *and* an embedding endpoint is configured
(`ProjectConfig::rerank_active`, the engine's single gate), the top
`rerank_window` keyword candidates are reordered by the **same stored-vector
cosines the ADR-0059 relevance gate already computes** (`rerank_scored`, the
engine's stored-vector entry point) — no second embedding pass on the
injection path.

1. **Keyword stays the candidate floor** (mirrors D-LM-0022): rerank reorders
   keyword candidates, never introduces hits.
2. **Default-off and byte-identical when off**: without the flag, without an
   endpoint, or without stored vectors the injected order is unchanged — the
   ADR-0045/0059 posture (additive, best-effort, offline-identical) holds.
3. **Gate and rerank share one score source** (the `vector_index` cosines), so
   filtering and ordering can never disagree about a memory's relevance.
4. This closes the ecosystem's one live parallel-retrieval risk by consuming
   the engine capability rather than duplicating it (reuse-before-add).

## ADR-0064: The Non-Developer TUI Is Native ratatui, With The Guided-Launcher Doctrine Enforced By Tests

Status: accepted. Applies to LocalBox (which ships the TUI) and records the
ecosystem doctrine; LocalPilot's own inline-TUI rules (ADR-0021, ADR-0039) are
unchanged and this decision follows their scrollback-safety lineage.

The launcher TUI for non-developers is a native **ratatui** flow inside the
product binary. The previous stack — a .NET/Terminal.Gui TUI talking to an
out-of-process PowerShell backend over a JSON seam — is retired with the
PowerShell sources.

1. **Flow logic is pure; frontends only pick indexes.** Every guided-flow
   decision (vocabulary, plan summary, customize transitions, save gates) is a
   pure function layer with unit tests. The interactive frontends — a ratatui
   inline-viewport list on a TTY, numbered plain-text menus otherwise — do
   nothing but select indexes, so behaviour cannot fork between them and a
   non-TTY session degrades gracefully by construction.

2. **The non-dev doctrine is enforced by test, not convention.** Plain language
   over jargon (a snapshot fails if implementation vocabulary leaks into the
   plan summary); progressive disclosure (the fast path is model → short
   confirm → launch, with power knobs one level down in Customize); safe
   recommended defaults with markers; a recommended-tier model filter with an
   explicit reveal for the rest (and a no-dead-end fallback when nothing is
   marked recommended); fit-aware colouring; one-keystroke replay of a saved
   default; scrollback-safe inline rendering (no alternate screen, no
   whole-screen clear).

3. **The TUI talks typed in-process calls.** There is no JSON back-channel
   between the interface and the launcher: the ratatui flow calls the launcher
   library directly, so the old TUI-API seam (and its smoke guard) retired with
   the .NET TUI instead of being rebuilt in Rust.

Boundary: machine consumers are not the TUI's job — scripted use goes through
the CLI commands and their machine-output discipline (JSON to a clean stdout).

## ADR-0063: The No-Think Filter Runs In-Process, Hosted By The Product Binary

Status: accepted. Removes the last Python runtime dependency from the local
stack; honours the provider contract (ADR-0001) — the filter presents the same
documented Anthropic-compatible surface agents already speak.

The think-stripping proxy that sits between a coding agent and a local
llama-server is a Rust **in-process** HTTP filter (axum on the shared runtime
tier), not a Python sidecar script.

1. **The filter is a library with a bind-and-serve entry point.** The shared
   runtime crate owns the streaming think-block stripper, the `/health`
   endpoint reporting the proxied target (`{target_host, target_port}`), and
   the forward path — unit-testable against a mock upstream with no socket.

2. **The product binary hosts it by re-invoking itself.** LocalBox spawns the
   proxy as its own executable in a plumbing mode, so there is no second
   binary to distribute or version, and proxy lifecycle is ordinary process
   management: the tri-state target check (right target / wrong target /
   down), repoint-not-restart, and reap-before-probe semantics are preserved.

3. **A gateway posture is explicit and guarded.** The proxy binds loopback by
   default. A LAN exposure carries an optional API key that gates every
   forwarding request (both common header spellings; `/health` stays open for
   target checks), and a public-looking bind with no key is refused with a
   remedy unless explicitly opted into.

Boundary: the filter never rewrites model output beyond reasoning-block
routing; sampler and template policy stay on the server side.

## ADR-0062: The Shared llama Tier Is A Public Repo Of Narrow Crates, Consumed By Rev-Pinned Git Dependency; The Launcher Contract Is A Trait

Status: accepted. Extends the narrow-crate rule (ADR-0001) across repository
boundaries; clean-room provenance (ADR-0005) applies to the shared tier in
full.

The domain logic that LocalBox, LocalBench, and LocalPilot all need lives in a
separate **public** repository of narrow crates (`localx-llama`), consumed by
each product as a **git dependency pinned by revision** (recorded in each
consumer's committed `Cargo.lock`; a local path override is used during active
development). It is not a git submodule, and it is not vendored.

1. **Three crates, one responsibility each.** A pure domain crate (model
   catalog types, server-argument construction, VRAM/fit, config-layer
   precedence, the tuner store schema, the launcher contract); a runtime crate
   (process lifecycle, pin-verified downloads, health classification, the
   in-process no-think filter, port and spawn utilities); and an eval crate
   (the capability-scorecard wire contract, discipline metrics, the blinded
   judge core, ablation, the gate-mediated check runner, verify-command
   detection, the test-count grade table).

2. **The launcher contract is a Rust trait with a versioned envelope.** What
   was a documented list of PowerShell functions is now a trait the launcher
   implements and consumers depend on: parameter obligations are the type
   system's job, and compatibility is a small versioned envelope
   (`api_version` / `launcher_export_version` / supported targets and
   runtimes) gated by the consumer with a suffix-free product-version floor.
   Conformance runs cross-repo in CI in both directions, so a breaking change
   fails at its source.

3. **LocalPilot's eval primitives moved down; hosts keep adapters.** The
   shared eval crate owns contract types and pure math; anything bound to a
   host (session-trace projection, the permission engine, live judge calls)
   stays in the host as a thin adapter re-exporting the shared names, so the
   host's public API survives the extraction.

4. **Public visibility is load-bearing.** Consumers are public repositories:
   a private shared dependency would break `cargo build` from source and
   public CI. The tier holds nothing secret, so it is public, and no CI needs
   a fetch token.

Boundary: consumers must track a single revision of the shared tier per
lockstep (two revisions of the crate in one dependency graph split the trait
into incompatible types); pins are advanced deliberately, never floated.

## ADR-0061: Vision (Image-Input) Capability Resolves Config > Probe > False, And The Image Gate Is Lifted To Match

Status: accepted. Keeps the change additive and default-off (cross-cutting
KISS/default-off): an undeclared, unprobed provider is byte-identical to before.
Honours the provider-contract capability model (ADR-0001, no vendor coupling) and
clean-room provenance (ADR-0005) — the probe reads a documented public llama.cpp
endpoint, never a private one.

LocalPilot assumed every local OpenAI-compatible server was text-only: the OpenAI
adapter advertised image input **only** for the official hosted API, so an
image-capable local model (e.g. a llama.cpp server with a multimodal projector
loaded) was refused images, and an image attached to any other model was sent
blind. A model's vision support is a *capability* that should be resolved and
acted on, not assumed from the source.

1. **Capability resolves from two signals, precedence config > probe > false.**
   The resolved vision support is: (1) an explicit per-provider `supports_vision`
   config flag (authoritative — set by the user, or auto-written by LocalBox on a
   vision launch); else (2) a best-effort, read-only discovery-time **probe**; else
   (3) `false`. Config always wins so a user can override a wrong probe, and an
   unknown probe never asserts a capability. The pure resolver
   (`resolve_vision`/`VisionSource`) is shared by every surface so they cannot
   diverge.

2. **The OpenAI `Image` input gate becomes `OfficialApi OR resolved-true`.** The
   adapter still advertises image input for the official API; a local/custom server
   advertises it when vision resolves true (config-declared at provider-build time;
   a probe lift is applied at the interactive image-attach seam). With the flag
   defaulting unset, **no existing local provider changes behaviour**.

3. **The probe is read-only `/props`, best-effort, default-on; no trial-image
   probe.** llama.cpp's `llama-server` exposes a documented `GET /props` reporting
   `modalities.vision` (set when an `--mmproj` projector is loaded); the probe reads
   it and runs **no model inference** (`/props` is at the server root, so a trailing
   `/v1` is stripped first). It is toggleable via `[discovery] vision_probe` (default
   `true`) and is skipped when config already declared. An unreachable, non-200, or
   signal-less server resolves to `None` (unknown ⇒ no vision), never a false claim.
   An **active** trial-image probe (sending a 1×1 image through inference) is *not*
   in v1.

4. **LocalBox is the authoritative declarer for its launch path.** When LocalBox
   loads the projector for a `-UseVision` agent launch it writes `supports_vision =
   true` into the generated `.localpilot.toml`, so the primary local-vision path is
   zero-config and never depends on the probe. (LocalBox-side; recorded in its
   CHANGELOG.)

5. **The preflight refuses-with-guidance, never sends blind.** An image attached to
   a model not known to accept one is refused with an actionable message naming how
   to declare `supports_vision`; a declared/probed-vision model passes.

Boundary: LocalPilot does **not** augment the upstream `GET /v1/models` response
with a non-standard vision field — llama-server owns that response, so augmenting
it would be stateful, fragile, and non-standard; that is explicitly deferred. The
`supports_vision` flag is a user/launcher **assertion**: a wrong declaration (a
text-only model marked vision) can still send images that the server rejects, which
`doctor`/`models` surface so the assertion is visible. `doctor` stays offline
(deterministic, no egress), so it surfaces the config-declared half; the full
config-or-probe resolution and its source surface in `localpilot models`, which
already performs gated network discovery.

Addendum (image paste in interactive chat): an explicit image paste (Ctrl+V, or a
terminal that routes it as a bracketed paste) re-runs this same config > probe
resolution once before the preflight decides, so a vision server that became
reachable after session start is picked up; on refusal the notice now names both
levers (`supports_vision` and `[discovery] vision_probe`). Separately, a clipboard
read that fails for any reason other than "no image present" always surfaces a
notice, so an image paste never fails silently — closing the "nothing happened, no
message" gap without changing the default-false capability posture.

## ADR-0060: Research Is A Two-Surface, Provenance-Preserving Loop In Its Own Crate, With Web Egress Off By Default

Status: accepted. Honours the ecosystem remote-egress policy (the five rules:
default-off, explicit opt-in, disclosed, auditable, disableable) and reuses the
loop-safety rails (ADR-0055), the review-gated candidate path (ADR-0037), and the
single workspace redactor (ADR-0011). Keeps the LocalMind adapter thin (ADR-0036)
by landing the loop in a new crate, not the adapter. The bundled `deep-research`
harness concept is a behavioural reference only — code, prompts, identifiers, and
UI copy are original (ADR-0005, clean-room).

LocalPilot can already retrieve across local code, accepted memory, and ingested
knowledge, but only one source at a time and without a loop that decomposes a
question, cross-checks support, or produces a reviewable artefact. A *research*
capability wants to fan out across those sources — and, for some questions, the
web — then synthesise with provenance. The web reach is the hazard: LocalPilot's
posture is local-first with no telemetry, so any outbound path must be inert
unless the operator explicitly turns it on for that run.

1. **One engine, two surfaces.** A single bounded loop is exposed as both an
   interactive `/research` (one-shot `/research <topic>` runs and returns to the
   prior mode; a bare `/research` enters a persistent research mode where a typed
   prompt is researched) and a headless `localpilot research <topic>` subcommand
   (`--no-report`, `--no-memory`). The interactive surface is a thin shell over
   the loop; no research logic lives in the TUI/CLI layer.

2. **Host-neutral loop in a new crate (`localpilot-research`).** The crate defines
   the `Source`/`Synthesizer` traits, the bounded `run_research` loop, the value
   types (`Provenance`/`Evidence`/`Finding`/`ResearchReport`), the Markdown
   renderer, the review-candidate spec, and the pure web-egress policy gate. It
   carries no filesystem, network, or model dependency. The concrete sources
   (knowledge/memory/web), the model-backed synthesizer, the report writer, and
   the candidate enqueue live in the binding layer (`localpilot-cli`), where those
   capabilities already exist — so the loop stays dependency-light and
   unit-testable with fakes, and the LocalMind adapter gains no research logic
   (ADR-0036).

3. **Dual output, always review-gated.** A run emits both a redacted, human-
   readable Markdown report and review-gated memory candidates. Candidates derive
   only from supported, provenance-backed findings, are redacted, and are enqueued
   through the existing review path (the `write_retrospective_lesson` queue,
   ADR-0037) — never written to accepted memory. Knowledge production stays
   human-gated.

4. **The model assists decomposition only; synthesis stays provenance-preserving.**
   When a provider and model are configured, the model decomposes the topic into
   sub-questions (one per line, bounded by `[research].max_questions`). Synthesis
   is **not** delegated: each finding is a gathered evidence snippet carrying that
   snippet's provenance, and the loop's adversarial cross-check downgrades any
   finding with no supporting evidence to `unsupported`. So the model can never
   inject an unbacked or hallucinated finding into a report or a memory candidate.
   With no model, decomposition degrades to the single-topic heuristic and a run
   still completes.

5. **Web egress is off by default and opt-in by construction.** The whole web path
   is governed by `[research.web]` (`enabled` default `false`, an `allowlist`, and
   an `audit_log` path) layered with per-session, runtime consent:
   - **Default-off / disableable.** `enabled = false` (the unset default) removes
     the entire outbound path; the loop runs local-only. The switch is the kill
     switch — runtime consent can never override it.
   - **Explicit, loud opt-in.** Web research is reachable only via the headless
     `research --web` flag, which prints an egress disclosure (what is sent — only
     the redacted sub-question — which allowlisted domains, and the audit-log path)
     and then records a per-session opt-in that is never persisted. The interactive
     surface stays local-only. A freshly built run is inactive until both config
     and the per-session opt-in agree (fail-closed).
   - **Allowlist-only per host.** Each candidate URL's host is parsed with a real
     URL parser; an allowlisted host is fetched (bounded bytes + timeouts), every
     other host is **skipped and logged** — there is no interactive per-fetch
     prompt in v1. (The policy gate already models a `NeedsConfirmation` decision
     for a future UI; v1 treats it as skip.)
   - **Auditable.** Every outbound request and every skip appends one line to the
     audit log (decision, host, URL, redacted sub-question) — metadata and the
     redacted question only, never gathered evidence or file contents.

Reason:

- a research loop that fans out across sources is a genuinely new operating mode,
  but its *logic* is host-neutral; isolating it in its own crate keeps the LocalMind
  adapter thin (ADR-0036) and lets the security-sensitive gate be tested in
  isolation with fakes
- the trust hazard is egress, so the egress path is made inert by construction:
  default-off, a loud per-run opt-in with disclosure, an allowlist with skip-on-
  miss, and an audit trail — the five remote-egress rules, satisfied structurally
  rather than by convention
- delegating *decomposition* to the model is low-risk (it only shapes which
  questions are asked); delegating *synthesis* is not, because an ungrounded
  finding could become a report claim or a memory candidate — so synthesis keeps
  real provenance and the cross-check stays authoritative
- candidates route to the existing review queue, so research can never silently
  enlarge durable memory

Consequence: the default build makes no network request and is byte-identical to
the no-web path; `research --web` against an empty allowlist fetches nothing (every
host confirms→skips) and says so up front. Live evaluation of model decomposition
and URL-proposal quality on a local model is opportunistic, not blocking (the
offline `wiremock` egress tests are the accepted bar). A future interactive
per-fetch confirm flow can light up the gate's existing `NeedsConfirmation`
decision without changing the crate boundary.

## ADR-0059: Accepted-Memory Injection Is Gated By Semantic Relevance, And A Lesson That Hurt The Uplift Eval Is Routed To Review

Status: accepted. Implements the two halves ADR-0045/ADR-0046 left as mechanism:
the relevance gate now has a real (semantic) signal, and the outcome-aware
down-weight is wired to an outcome. Reuses LocalMind's `embed_query` /
global-aware `vector_search` (D-LM-0023) and the route-to-review flag
(D-LM-0016).

A model-pinned benchmark showed learning is net-positive but noisy: a
same-language but off-topic lesson injected into an unrelated task and misled the
model (negative transfer). ADR-0045's relevance gate keyed only on the keyword
bm25 score, which is **unnormalized** — there is no portable threshold to tighten
— and ADR-0046's outcome-aware down-weight was never wired to an outcome.

1. **Semantic relevance gate (default-on, best-effort).** The injection layer
   embeds the prompt once per turn and scores each keyword candidate by
   **normalized cosine** over the stored vectors, gating any hit below
   `[memory] injection_min_cosine` (default `0.6`; `0.0` disables). Because cosine
   is normalized and portable it ships **default-on** — unlike the
   default-preserving levers of ADR-0045. It is **best-effort**: with no embedding
   endpoint, an unreachable one, or an unembedded lesson, the hit carries no
   cosine and is injected exactly as on the keyword path, so a no-embed run is
   byte-identical. The keyword bm25 search stays the candidate floor; cosine only
   re-filters it, never selects.

2. **Outcome-aware down-weight (default-off).** When the uplift A/B eval shows an
   arm that injected a set of lessons under-performed its control, those lessons
   are routed to review (never deleted) for a human to re-judge, joined by the
   per-turn `memories_used` audit. The join is the **A/B verdict, not a live
   turn** — a single turn is too weak a signal (ADR-0046's intent). Gated by
   `[memory] outcome_downweight` (default off); only `memory`-layer ids are
   eligible; reversible.

Both reuse existing primitives — no new retrieval engine, no second flag path —
and never delete. Rollback is `injection_min_cosine = 0.0` and leaving
`outcome_downweight` off.

## ADR-0058: The Solve Loop Can Build, Test, And Edit In The Workspace — De-Verbatim cwd, `&&` Shell, Anchored Edits, Verify-On-Eval

Status: accepted. Refines ADR-0054 (the verify-before-done gate — flips its
deferred default on *for eval*) and ADR-0007 (tier-1 parity — this is largely a
Windows-parity fix). Builds on the read-only behaviour reference under
clean-room rules (ADR-0005): the behaviours below were replicated originally, no
code/prompt/identifier copied.

A model-pinned benchmark exposed that the solve loop was **blind on Windows**.
The sandbox canonicalizes the workspace root to a verbatim extended-length path
(`\\?\C:\…`) for path containment; handed verbatim to a child process as its
working directory, a launched shell cannot use it (cmd falls back to
`C:\Windows`, PowerShell resolves relative paths against a broken `$PWD`), so
every model-issued build/test command ran **outside** the workspace and failed.
The grader ran in a separate raw cwd and still scored the final file, which
**masked** the bug and depressed solve quality — and poisoned the learning
corpus (≈25% of warm-store lessons were artifacts of the broken tooling, e.g.
"PowerShell doesn't support `&&`"). Four changes make the loop robust, each
guarded so containment and no-regression hold:

1. **De-verbatim child-process cwd.** `Workspace::process_dir()` returns the root
   in a form a child can use (`dunce::simplified`, which leaves a path verbatim
   when it cannot be safely shortened). Every spawn — the shell and git tools,
   background processes, and the verify gate's runner — uses it. The verbatim
   `Workspace::root()` and its `starts_with` containment boundary are **never**
   touched; `process_dir()` is a spawn-only accessor, never used for a contained
   path. The sandbox boundary tests (verbatim/case/UNC/ADS) stay green.

2. **`&&`-capable shell.** The Windows shell wrapper prefers PowerShell 7
   (`pwsh`) when it is on PATH (cached detection), falling back to
   `powershell.exe`. `pwsh` supports the `&&`/`||` chain operators that Windows
   PowerShell 5.1 lacks, so a chained command runs as written instead of teaching
   the corpus junk shell lessons. *Prefer*, not *require*. A timed-out command's
   whole process tree is killed (`taskkill /T /F`; a process-group `kill` on
   Unix) so a hung build's grandchildren never orphan.

3. **Anchored, indentation-tolerant edits with guiding errors.** `edit_file`,
   `multi_edit`, and `apply_patch` share one matcher: an exact unique match
   first, then a single leading-indentation-tolerant rung that applies only on a
   *unique* block whose indentation differs by one consistent whitespace prefix
   (re-indenting the replacement to the file), else a guiding error (the match
   count, or the nearest line + a re-read hint). Matching stays **anchored, never
   fuzzy** — no Levenshtein/best-guess location, because a wrong-location edit is
   worse than a failed one. CRLF handling and `multi_edit`/`apply_patch`
   atomicity are unchanged.

4. **Verify-before-done default-on for `eval`.** ADR-0054 left the gate off
   "pending a fair arm"; `eval` is that arm. `localpilot eval` defaults the gate
   on (opt out `--no-verify`, byte-identical to the prior behaviour), so the
   benchmark measures compiled+tested solves. Interactive and `print` are
   unchanged (the `[harness] verify_before_done` config default stays `false`).
   Stack detection gains a C++ branch: C++ sources at the root are compile-checked
   with an artifact-free `g++ -std=c++17 -I. -fsyntax-only <sources>` — a single
   `CheckRunner` program+args that writes no build artifacts (so it never pollutes
   the captured diff) and catches the dominant "never compiled" failure, rather
   than the three-command CMake configure/build/`ctest` pipeline that does not fit
   one call (a full `ctest` stays available via `verify_command`).

Reason:

- a verbatim `\\?\` path is a *containment* form, not a *spawn* form; the fix is
  to distinguish the two, not to weaken containment — so the security boundary is
  unchanged and the launched tools finally run where the work is
- the harness defect did not just lower scores, it **poisoned the learning
  corpus**; fixing the build/test/edit surface is a prerequisite to any learning
  re-run, and removes the source of the `&&` bug-lessons at the root
- anchored-and-unique edit matching turns the common "model rewrites the whole
  file because its `old_text` indentation was slightly off" failure into a
  landed edit, without ever risking a best-guess wrong-location write
- a benchmark that scores code the model never built measures the grader's cwd,
  not the solver's; defaulting verification on *for eval* closes that gap while
  leaving interactive behaviour untouched

Consequence: the eval baseline now reflects verified solves (the in-flight
comparison ledger is reset before the next run, dropping the blind-cwd failures
and keeping the valid results). The warm-store cleanup — pruning the
bug-artifact lessons and investigating the semantic-dedup miss — is a separate
operational/LocalMind follow-up before any learning re-run. The C++-as-`g++`
choice is recorded as a deliberate, single-command refinement of "CMake → ctest".

## ADR-0057: Ingest Keyword Retrieval Ranks By FTS bm25, And Short Query Terms Match Whole Tokens

Status: accepted. Refines ADR-0025 (the indexed chunk store) — specifically its
"ranking is unchanged" clause, which this record supersedes.

ADR-0025 narrowed candidates through the FTS5 index but then **re-derived** a
relevance score in Rust: a flat term-count (`text.matches(term)`) plus a `+3`
substring path bonus, re-sorting by that. Two flaws followed. First, the scorer
matched **substrings mid-word**, and the candidate expression wildcarded **every**
term as a prefix (`"term"*`) — so a 2-character query term like `an` matched the
token `and` (and `do` matched `docker`/`documentation`), floating irrelevant
chunks into the keyword tier. Second, the flat term-count **discarded bm25's
IDF weighting**, so a common token counted the same as a rare one.

Ingest keyword retrieval now ranks by the FTS index's **bm25** score directly
(IDF-weighted; a common token contributes far less than a rare one), with the
`path` column weighted above the body (`bm25(fts, 0.0, 5.0, 1.0)`) so a term that
names a file boosts that chunk — the principled replacement for the substring
path bonus. The Rust term-count rescore is gone; `ChunkStore::search` returns each
row paired with its (negated, higher-is-better) bm25 relevance, and the keyword
order is the index's order. Query terms of three or more characters still match as
FTS **prefixes** (so `pars` matches `parser`); **shorter terms match a whole token
exactly** (so `an` matches only the token `an`, never `and`).

`KnowledgeHit::score` is therefore a bm25-derived relevance (fixed-point scaled),
not a term count. The hybrid keyword+vector blend (the chunk-vector layer) is
unchanged in shape: keyword hits keep an absolute floor above every vector-only
hit, with cosine sub-ordering; only the keyword tier's internal ordering moved
from term-count to bm25.

Reason:

- bm25 is the FTS index's own IDF-weighted ranking; re-deriving a cruder flat
  count on top threw that away and reintroduced the very "common words rank high"
  problem bm25 exists to solve
- substring + unconditional-prefix matching is not token-aware; gating the prefix
  by term length and matching short terms as whole tokens removes a class of false
  hits (`an`⊂`and`, `do`⊂`docker`), not just one symptom
- column-weighted bm25 expresses the path-name boost inside the ranker instead of
  a parallel substring pass, so there is one matching definition, not two

Consequence: the prior `linear_scan` no-regression oracle (which pinned the
term-count ranking) is retired; new tests pin the bm25 behaviour, the
whole-token short-term match, and the path-weight boost. This changes observed
ordering for some queries (by design — it is the fix), so it is a deliberate
ranking-contract change, not a silent one.

## ADR-0056: Project Instruction Files Are Injected Directly Into Context, Ungated, Every Turn

Status: accepted.

Context: a project's instruction files (`CLAUDE.md`, `AGENTS.md`) are the user's
authoritative orientation for the agent. LocalPilot discovered and merged them
(`ContextDiscovery`, with precedence + `@`-imports), but the **only** consumer was
the LocalMind ingest path: the merged document became a derived chunk in the
review-gated learning store, surfaced via `knowledge_search` only after a human
accepted it (and only when learning was enabled). So a fresh checkout's
`CLAUDE.md` could *never* reach the model — the opposite of how a mature harness
treats a repo's own instructions.

Decision: inject the merged instruction document **directly into the turn
context every turn**, ungated and independent of learning, behind
`[context] inject_instructions` (default **on**). The injection reuses
`ContextDiscovery::discover().render()` (no second discovery walker) through a
`ProjectInstructionsContext` hook — the same `ContextHook` fabric
`project_analysis` uses, so the block folds into the leading system message, is
never persisted, and does not accumulate. It is **bounded**
(`instruction_char_budget`, truncate-with-marker over budget) and **redacted**
through the canonical host redactor before it is sent. Discovery is also widened:
a first-class **`Navigator.md`** convention (LocalPilot's own, highest precedence)
and **`.github/copilot-instructions.md`** (GitHub Copilot's, lowest), ranked by
kind within a tier (`Navigator` > `CLAUDE` > `AGENTS` > Copilot).

Consequences: a repo's instructions are respected immediately on a fresh
checkout, with learning off and an empty store — the parity gap a model-pinned
sweep exposed. The ingest→retrieval path is unchanged and still review-gated, so
`knowledge_search` keeps working; the two paths are complementary (direct
injection for always-on orientation, retrieval for on-demand recall). The
injection is **default-on** because reading a repo's own committed instructions
is the expected, safe behaviour — not a speculative feature — but it is bounded,
redacted, and opt-out (`inject_instructions = false`) to cap context cost and
contain a secret-bearing file. The `.cursorrules`/`GEMINI.md` conventions are a
documented future option behind config, deferred (YAGNI) until a project needs
them. Clean-room: the injection wording is original to this repository.

## ADR-0055: A Fresh Project Self-Bounds — Built-In Loop Safety Rails When The Config Is Silent

Status: accepted.

Context: the per-turn tool-call budget (ADR-0029) and `turn_timeout_secs`
(ADR-0049) both ship **off by default**. With neither set — an empty or minimal
`.localpilot.toml`, the out-of-the-box state — a turn ran with no cost ceiling and
no wall-clock bound. The always-on degenerate-loop guard (ADR-0052) catches a
*spinning* turn (repeated/cyclic or failing calls), but not a long
varied-but-non-converging trajectory or a single hung operation: a weak local
model could run a turn to an external SIGKILL that printed no scorecard. An
unbounded out-of-the-box loop is a defect, not a feature default.

Decision: when `[harness]` leaves a rail unset, apply a **conservative built-in
bound** rather than running unbounded. An explicit `[harness]` value always wins;
the default only fills an unset rail. The bound is profile-aware, because the
safety need is strongest where no human is watching:

- **Headless** (`eval` / `print` / a `harness` step): a tool-call ceiling (200)
  **and** a wall-clock bound (600 s).
- **Interactive** (REPL / `serve`): a higher tool-call ceiling (500) and **no**
  default wall-clock — a long interactive turn is legitimate and the user can
  cancel it; the ceiling still stops an unattended runaway.

Resolution lives in one place — `HarnessConfig::resolved_rails(interactive)` in
`localpilot-config` — which every runtime-construction site (the headless
`build_runtime`, the harness step runner, the REPL, and `serve`) calls, so the
default is consistent and explicit-wins is enforced once. The verify gate
(ADR-0054) is also tied to the no-progress signal: when its build never goes
green within the re-entry cap the turn stops with `StopReason::NoProgress`, not a
clean `Done`.

Consequences: this is a **safety default, not a feature lever** — unlike the
verify gate, broker, and global memory (which ship off pending corpus evidence),
an unbounded loop is a bug, so the rails ship **on** by default. The values are
conservative enough not to cut a legitimate run (an ordinary task stays well
under 200/500 calls and a step finishes well under 600 s); the soft/hard budget
split (ADR-0029) and the clean `TimedOut`/`BudgetExceeded` finalize + handoff
(ADR-0049/0052) are unchanged — this only fills the *default*. Rollback/tuning is
config: set explicit `tool_call_budget`/`turn_timeout_secs`. Refines
ADR-0029 (progress-aware budget) and ADR-0052 (degenerate-loop guard); a
library consumer building `SessionConfig` directly is unaffected (the default
fill is in the config-resolution layer, not the harness).

## ADR-0054: A Turn Verifies The Workspace Builds Before It Is Allowed To Finalize (Opt-In)

Status: accepted.

Context: a solve loop finalizes when the model stops calling tools — it "submits"
by replying with no tool call. That accepts a turn's work without ever confirming
it builds. A model-pinned corpus sweep showed this is the single largest avoidable
cause of compiled-language losses: every C++ loss was a workspace that did not
compile, yet the turn declared success. The harness already has a command runner
(the quality-gate [`CheckRunner`], ADR-0009) and a feedback-and-retry shape
(`harness resume`); what was missing was running a build/test signal on the
*finalize* transition and feeding a failure back instead of stopping.

Decision: add an opt-in **verify-before-done** gate. With
`[harness] verify_before_done` on, a turn that would finalize with no tool call
first runs a verification command; on failure the captured diagnostics are fed
back as the next turn's input and the loop continues; on a pass, no detectable
target, or the gate off, the turn finalizes as before. The command is resolved
from `[harness] verify_command` (a whitespace-split command line, no shell) or
detected from the workspace's marker files; a workspace with neither is a clean
no-op. It **reuses** `CheckRunner` (one permission-gated command path, no second
engine) and does **not** add a second retry loop. It is bounded by the per-turn
tool-call budget and `turn_timeout` rails *and* a fixed re-entry cap, so it can
never loop forever; a denied or unstartable command finalizes the turn without a
signal rather than wedging it. `localpilot eval --verify`/`--verify-command`
enables it for one run so a benchmark arm can quantify its lift.

Consequences: the definition of "done" gains an optional, observable build/test
postcondition at the finalize seam — distinct from the per-call contract
verifier (`localpilot-verify`, which checks a single tool call's postconditions,
not the whole workspace). It ships **default-off** (a feature lever; features ship
off): defaulting it on is gated on corpus evidence (a fair arm) per the
validation-evidence policy (D008). Rollback is config — unset the flag. The
detector covers the common single-command stacks; a stack it does not cover
(e.g. a CMake-only C++ project) is served by an explicit `verify_command`, which
the benchmark sets per language.

## ADR-0053: The Outward Self-Improvement Surface Is Human-Gated By Construction, Draft-Only, And Default-Off

Status: accepted.

ADR-0034 built the self-improvement loop's write half so the human gate is
structural: the agent may **propose** a code patch in an isolated worktree, but
promoting it onto the branch requires a value-typed `ApprovalToken` the autonomous
loop cannot mint. That ADR deferred the *outward* analogue — emitting an issue/PR
to an external repo — as a safety call, not a scope cut. This record lifts that
deferral and ships the narrowest publishable outward surface under the same gate.

Decision: add an `OutwardDraft` to `localpilot-patchgen` as a sibling of
`ProposedPatch`, reusing the **exact same** `ApprovalToken` — one token type, one
mint path, no second gate.

- **Human-gated by construction.** Authoring or persisting a draft mints no token
  and touches no network. The only producer of a runnable `PublishPlan` is
  `OutwardDraft::publish_plan(&self, &ApprovalToken)`; the token's sole constructor
  (`ApprovalToken::approve`) is called only on the explicit `emit-draft --approve`
  CLI path, mirroring `promote`. So the loop can author a draft but can never
  publish one — a standing API-shape test pins that every loop-reachable operation
  (build/persist/load/list/preview) completes token-free.
- **Default-off, fail-closed allowlist.** New `[self_improvement]` config:
  `enabled` (bool) + `outward_targets` (`owner/repo` allowlist), both off. A draft
  is refused at propose time unless the feature is enabled **and** the target is
  allowlisted; the allowlist is re-checked at emit time.
- **Draft-only, never promote.** Publication runs `gh issue create` /
  `gh pr create --draft` only, via the official `gh` CLI (clean-room — no private
  endpoint). The argv is built so it can never carry `ready`/`merge`/`--web`/an
  edit/comment/close, is passed as an array (no shell), and `emit-draft` is
  **dry-run by default** (no `--approve` ⇒ print the plan, publish nothing). A
  `gh auth status` preflight surfaces the resolved account before approval.
- **Redacted and traceable.** The draft title/body are redacted with the shared
  workspace redactor at construction (so a secret never reaches the local
  `.localpilot/outward/` store), and the body carries the change provenance
  (finding + source + rationale). Every emit appends a redacted, token-free
  lifecycle event.

The propose surface's pure finding→draft-spec mapping lives in the read-only
`localpilot-selfreview`; the gated artefact, store, and `gh`-running command live
in `localpilot-patchgen`/`localpilot-cli`, keeping the read-only scanner free of
provider/network deps. Extends ADR-0034.

## ADR-0052: An Always-On Degenerate-Loop Guard Bounds A Spinning Turn Even When The Cost Budget Is Off

Status: accepted.

The per-turn tool-call budget (ADR-0029) is opt-in: with neither `tool_call_budget`
key set it is `Unlimited`, and a turn runs with no cost ceiling and no no-progress
stop. That left one unbounded path — a turn that keeps calling tools without making
progress, most sharply a weak local model whose tool calls are all denied or all
fail, loops until the model happens to stop. Observed live: under a permission
profile that denied the probe tools, a model produced ~1240 messages over ~17
minutes. The per-turn timeout (ADR-0049) bounds one turn, not the loop, and the
per-tool failure threshold only nudges (a model evades it by cycling tools).

Decision: add an always-on degenerate-loop guard in the session loop, independent
of the opt-in budget. When the budget is `Unlimited`, the turn still stops with
`StopReason::NoProgress` if either signal fires:

- the progress-aware no-progress detector (ADR-0029) trips — a repeated or cyclic
  *successful* call set, which previously only stopped a turn when the budget was
  configured; or
- a run of `UNPRODUCTIVE_CALL_LIMIT` consecutive *failing* calls with no successful
  call in between — the denied/failing spin the no-progress detector never sees (it
  is fed only by successful calls). The streak resets on any successful call.

This narrows, it does not widen, the budget contract: "budget off" still means no
*cost* ceiling, and the bound never fires on a productive turn (distinct,
progressing calls never trip the detector and never accumulate a failure streak).
When the budget is configured, the controller (ADR-0029) still owns the no-progress
stop and this guard is inert. The limit is a fixed conservative constant, not a
config knob — it is a safety backstop, not a cost control, and the opt-in budget
remains the tunable lever. Pinned by the `localpilot-harness` budget tests: a
spinning loop and a run of failing calls both halt with the budget off, and a long
productive turn is not cut. Refines ADR-0029.

## ADR-0051: Tool Arguments Get A Schema-Aware Error And An Opt-In, Contract-Gated Repair

Status: accepted.

A local/open model often emits a tool call whose arguments are well-formed JSON
but violate the tool's contract — a bare string where an array is expected, a
stringified array, a markdown autolink in a path. Today the model was handed the
raw serde error string and wasted a turn (or degraded the session). The validity
metric scaffold (`CallRecord.schema_valid`, `DisciplineMetrics.schema_valid_rate`)
shipped but was never lit up.

Decision: add three coordinated, measurement-gated stages on the existing
deserialize chokepoint — no new crate, no global preprocess — built as additions
to `localpilot-tools` and wired once at the pre-dispatch seam.

- **A shared schema validator** (`localpilot-tools::validate`) classifies a call's
  structural issues by failure mode. It lights up `schema_valid` in the evidence
  projection and the `eval` scorecard (`schema_valid_rate`), and emits redacted
  `tool_input_valid` / `tool_input_invalid` session events. The raw `schema_valid`
  signal stays honest — a repaired call is recorded as raw-invalid plus repaired,
  never flipped, so the anti-gaming paired metric can see the model's real rate.
- **A model-readable error** replaces the raw serde blob with a concise,
  schema-aware message (offending field, expected shape, a valid example from the
  tool's `ToolContract.examples`), built value-free so it cannot leak a secret.
  It is delivered as the tool result at the pre-dispatch seam (the validator-first
  / retry-with-error pattern). `[tools] readable_errors` defaults **on**; off
  restores the raw message exactly. The arg-shape retry loop is bounded by the
  per-turn tool-call budget (ADR-0029), not the bad-output degrade counter — a
  recoverable argument mistake is not degenerate output, so the new
  `RecoveryAction::RepairToolArguments` rung is non-degrading.
- **A validator-first repair stage** repairs **only** the validator-reported
  fields with three schema-typed rules (`wrap_bare_string_as_array`,
  `parse_stringified_json`, `unwrap_markdown_autolink`), re-validates, and either
  runs the repaired call (with a model-visible note) or falls back to the readable
  error. `[tools] repair` is `off|warn|on`, default **off**, warn-before-on.

Safety is provable from the contract: repair runs only on a `ReadOnly`/`ProjectWrite`,
non-`Irreversible`, non-MCP tool, and never on a content/command field. To make
that gate provable, three under-classified git contracts are corrected to their
honest side-effect class (`git_restore` → `Destructive`, `git_commit` →
`ExternalWrite`, `apply_patch` → `Destructive`); this is advisory metadata only —
the permission path and the prompts are unchanged, and the verifier does not read
`side_effect`. A schema-intent marker (`#[schemars(schema_with = "...")]` helpers
emitting `x-localpilot-intent`) declares each field's intent so a rule keys off the
declared meaning, not a field-name guess, and a content/command field is provably
repair-exempt. Repair changes arguments, never authority: the permission engine
runs on the repaired input (reveal-never-grant). Every repair and every high-risk
refusal is a redacted session event.

LocalMind participation is reuse-only and opt-in (`[tools] repair_learning`,
default off): the session's repair patterns are offered to the existing
review-gated queue as aggregate, redacted candidates — no raw inputs, no accepted
memory, no new store. Reject the marketing (repair does not make a weak model
out-reason a strong one); the offline `localbench-uplift-v1` A/B closes the work
with a `task_completeness`-paired-with-`tool_call_validity` headline (a validity-only
lift is a no-win) and a cheap-prompt control; the live local-model arm is
opportunistic (D008). Supersedes nothing; extends the Tool Discipline track (the
cure sibling of grammar's prevent, ADR-0044, and the model re-prompt, ADR-0038).

## ADR-0050: `doctor` And `models` Are Agent-Consumable Under The `--format` Contract

Status: accepted.

A dogfood run drove a coding-agent wrapper against the CLI and found two
agent-hostile surfaces. `doctor` had only human text, so the wrapper string-matched
`providers:`. `models` prompted for network approval and, run non-interactively,
printed `skipped (network request not approved)` and exited **0** — a silent
no-op a wrapper reads as success. The same run hit a stale PATH binary that lacked
`--workspace`, with no way to detect the drift programmatically.

Decision: extend the ADR-0048 `--format human|json` plumbing (the same `output`
module, `--json` alias, non-terminal-defaults-to-JSON resolver) to both commands
rather than invent a second JSON convention.

- **`doctor --format json`** serializes the existing `DoctorReport`, enriched with
  the signals a wrapper needs: the resolved executable path (`current_exe`), the
  build's `git describe` version, provider kind + base URL + model + context
  window, the resolved LocalMind store root, and an append-only list of **capability
  tokens**. Drift *detection* stays the caller's job (compare exe path + version);
  doctor only reports the facts. Capability tokens let a wrapper feature-detect a
  surface (e.g. `learning-workspace-flag`) instead of inferring it from a version
  number. The credential is reported as a source label (`keychain`/`file`/`env`/
  `none`) only — never the value, the ADR-0048/secrets invariant.
- **`models` is non-interactive-safe.** It gains `--format human|json` and a `--yes`
  flag. Under a non-interactive run (no TTY, or `--yes`) it never blocks on a stdin
  prompt: an `Ask` decision without `--yes` is reported as `approval_required`, a
  policy `Deny` as `denied`, and an unreachable endpoint as `unreachable` with the
  error — and the command **exits non-zero** when a listing was incomplete
  (unreachable or approval-required). It no longer silently skips. JSON is a
  script-stable array (an empty result is a valid `[]`).

Reuse, not fork: one `output` resolver, one `--format` vocabulary across `learning
search` / `memory search` / `doctor` / `models`. Additive and reversible — the human
forms are unchanged for an interactive caller, and the JSON surfaces are new. Tier-1
parity holds (`IsTerminal` is the std cross-platform gate). Extends ADR-0048; the
drift signal is *detection only* — no auto-update and no PATH scan.

## ADR-0049: `print` Is Closed-Pipe-Safe And Bounds A Turn With A Parseable Handoff

Status: accepted.

A dogfood `print --allow-writes` run hung for minutes, then aborted with `failed
printing to stdout: The pipe is being closed` when its reader closed stdout. Two
distinct defects: the streamed-answer write used the bare `print!`/`println!`
macros, which **panic the process** on a write error (the forbidden runtime-path
panic — `docs/13-rust-best-practices.md`); and the turn had no bound, so a long or
stuck turn hangs indefinitely with no terminal state a caller can read.

Decision: make the non-interactive `print` path always reach a clean, readable
terminal state.

- **A closed reader is a clean stop, not a panic.** The streamed write is checked;
  an error classified as the consumer going away — `ErrorKind::BrokenPipe`, or the
  Windows `ERROR_BROKEN_PIPE` (109) / `ERROR_NO_DATA` (232, the observed "The pipe
  is being closed") raw codes — cancels the turn and exits **141** (the POSIX
  SIGPIPE convention, `128 + 13`, that broken-pipe-aware tooling already expects),
  so a wrapper can tell "the reader left" from a real failure. Any other IO error
  is still surfaced (to stderr), never as a panic. The raw-code check is explicit
  so the classification holds on every tier-1 platform, not only where std maps the
  codes to `BrokenPipe`.
- **A turn is bounded by an optional wall-clock timeout.** `[harness]
  turn_timeout_secs` (unset by default — no behaviour change) stops a turn that
  runs past it with a new `StopReason::TimedOut` instead of hanging. The deadline
  is an absolute `tokio` instant checked at the loop boundary and armed in the
  stream `select!`, so it cannot drift across iterations.
- **Every turn leaves a bounded, parseable handoff.** At the turn's single exit the
  runtime records a `TurnHandoff` (stop reason, tool-call count, files changed,
  whether memory was written) and `print` renders it as one machine-readable
  `handoff:` JSON line on **stderr** — never stdout, so it can't pollute the
  answer. The granular durable record stays the session event log
  (`ToolFinished`/`MemoriesUsed`/`TurnEnded`); the handoff is a derived summary, so
  this adds no second reporting channel. `memory_written` is always `false` on the
  `print` one-shot path (it reads accepted memory but never closes out, per
  ADR-0018), which is exactly the signal a caller needs to decide to run a close-out.

Additive and reversible: the timeout is opt-in and off by default, the handoff is a
new stderr line, and the exit code is raised only when the consumer actually went
away. Rollback is reverting the checked-write wire and the deadline arm. Tier-1
parity holds — the broken-pipe code differs per OS and all variants are matched.

## ADR-0048: Non-Terminal Callers Get Structured Output By Default

Status: accepted.

`learning search` already grew a `--json` flag, yet a dogfood run proved that
*adding* a flag is not enough: both the human operator and the local model missed
it and tab-parsed the human table, because the parent `--help` hides leaf flags
and the human output advertised no structured alternative. The failure is a
*discoverability* class, not a missing capability.

Decision: the read commands resolve their output format from context rather than
forcing the caller to know a flag.

- **Non-terminal stdout defaults to structured.** When stdout is not a terminal
  (`std::io::IsTerminal` — a pipe or a file, i.e. a program is reading),
  `learning search` and `memory search` emit a JSON array by default; a real
  terminal still gets the human table. The gate is strictly `!stdout.is_terminal()`
  so an interactive session is never changed.
- **A uniform `--format human|json` overrides either way**, with `--json` kept as
  an alias for `--format json`. `--format human` forces the table even when piped
  (the escape hatch for a consumer that parses the text); `--format json` forces
  JSON on a terminal.
- **An affordance hint points at the structured form.** When the human table is
  shown interactively, a single stderr line names `--format json` / `--json`. It is
  suppressed when output is already structured or non-interactive, so it can never
  pollute a pipe.

Reuse, not a second serializer: both commands emit through the existing `--json`
writer (`SearchHit` serialization), and the format/hint logic lives in one small
`output` module the two commands share. Stdout stays script-stable in every case —
an empty result is a valid empty JSON array, and the diagnostics ride on stderr.

Tier-1 parity: `IsTerminal` is the std cross-platform check (Windows/Linux/macOS),
and the resolver is unit-tested independent of a real terminal. Rollback is the
flag-gated behaviour: revert the resolver wire (no state change). Scope is the two
search commands — the surfaces the run tab-parsed; other read commands keep their
current output.

## ADR-0047: An Advisory Whole-Repo Teardown Sweep At The Completion Seam

Status: accepted.

The harness mirrors the developer-process ceremonies it asks of its own builders:
the completion **retrospective** (ADR-0035) is the backward look at the brief, and
the quality gate (ADR-0009) blocks a step on failing checks. What it lacked is the
whole-repo **cruft sweep** the plan template now requires at a plan's §7 gate (the
c0degeek `cleanup-audit`): dead/abandoned code, duplicate/parallel logic,
over-engineering, redundant data access, and doc/test drift surfaced as triaged,
advisory findings before work is called done.

Decision: extend the existing read-only scanner `localpilot-selfreview` — already
the whole-repo analog of `cleanup-audit` — with detectors for those categories, and
run it at the completion seam alongside the retrospective behind a default-off
`[harness] teardown_sweep` flag. It is **not** a second scanner and **not** a new
gate:

- **Extend, don't fork.** New detectors join the one bounded walk and the one
  ranked `Report`; findings carry the existing severity/confidence plus a new
  `risk`, a recommended action, and the hidden-usage channels the detector ruled
  out (the cleanup-audit safety invariant). No finding reaches high confidence from
  the absence of local references alone.
- **Lean on tools, don't re-derive them.** Categories tooling owns — unused deps
  (`cargo machete`), unused imports/vars and dead-code warnings (`clippy`),
  advisories (`cargo deny`) — are surfaced as pointer findings that name the
  command to run, never reimplemented.
- **Advisory by construction (ADR-0034/0035 lineage).** The sweep is deterministic
  and offline (no provider call), read-only, and human-gated: it never blocks
  completion, edits code, or commits, and nothing is auto-enqueued as accepted
  memory (review-gated only, ADR-0037). A finding opens a *new* plan; it is never
  folded back into the run that surfaced it.

Opt-in and default-off: the completion sweep runs only under `[harness]
teardown_sweep = true`; the same pass is available on demand as `self-review
--cleanup`. Rollback is the flag (off) or reverting the one-line seam wire. The
`localpilot-selfreview` report schema stays `localpilot-selfreview-v1` — the new
fields are additive and serde-defaulted, so an existing consumer is unaffected.
Refines ADR-0034 (self-improvement loop) and ADR-0035 (advisory completion
retrospective).

## ADR-0046: Promote A Curated Lesson To A Rule Cue; Down-Weight By Routing To Review

Status: accepted.

Two ways to make memory help a weak local model better:

1. **Rule cue.** A curated lesson can be promoted to an **always-on rule cue** —
   terse guidance injected every turn independent of prompt relevance. A weak
   model acts on a short, always-present rule better than on a paragraph it must
   retrieve. Per the rules-vs-skills model (ADR-0027) a rule cue is **advisory**
   (content the agent reads), not an **enforced** harness rule (the rule engine's
   `Block`/`Warn` gates are untouched): promoting a lesson never gates execution.
   Opt-in: a seed lesson carries the `rule-cue` tag; at seed time its memory id is
   recorded in a host-side cue registry (`.localmind/rule-cues.json`), and the
   context hook injects those memories always-on under a `rule-cue` audit layer.
   A cue is excluded from the relevance-retrieval block so it is never injected
   twice. Injection assembly is the host adapter's job (ADR-0036), so the
   promotion list is host state keyed to engine memory ids, not engine state.

2. **Outcome-aware down-weight.** When the uplift eval shows a lesson did not
   improve (or hurt) outcomes, the host's learning loop **routes it to review** —
   it never auto-deletes. This reuses the engine's reasoned route-to-review flag
   (LocalMind D-LM-0016): the memory stays active and a human re-judges it, the
   same human-gated discipline as change-aware invalidation and the
   self-improvement loop (ADR-0034).

Both are additive and opt-in; the default path (no promoted cues, no flagged
lessons) is unchanged, and each ships default-off until the uplift eval clears
it. Rollback is removing the `rule-cue` tag / clearing the cue registry, and
clearing a review flag.

## ADR-0045: Accepted-Memory Injection Earns Its Context Cost

Status: accepted.

Always-on accepted-memory injection took every top-k match up to a fixed
1200-char cap, regardless of match strength, regardless of the model's context
window, and even when a memory restated guidance a harness rule already enforces.
On a weak/small model that is wasted context the model spends effort on without a
behaviour gain.

Decision: the host injection layer (`localpilot-localmind`) gains a `[memory]`
policy, every field of which **defaults to the prior behaviour** (additive,
opt-in):

- **Relevance gate** (`injection_min_score`, default `0`): a retrieved memory
  whose score is below the threshold is not injected, so a weak match cannot fill
  the per-turn budget.
- **Context-window-aware budget** (`injection_context_aware`, default `false`):
  when on, the injected char budget is scaled toward the default provider's
  declared context window (a small model gets less), never above
  `injection_char_budget` and never below a one-line floor.
- **Dedup-vs-enforced** (`injection_skip_categories`, default empty): a memory
  whose lesson category is listed is skipped, because a rule already enforces
  equivalent guidance — injection should add signal, not restate a rule.

The category needed for the dedup is exposed by the engine on the search result
(LocalMind D-LM-0015), so the host does not do a second lookup. The policy is
host-side because injection assembly is the adapter's job (ADR-0036), not the
engine's. Every lever ships **default-off** until the uplift eval clears it; this
ADR records the mechanism and the default-preserving contract, not a default
change. Rollback is leaving (or resetting) the `[memory]` defaults.

## ADR-0044: Selectable Constraint Encoding To Reach A Local Server's Grammar

Status: accepted.

The constrained-decoding capability targets a local OpenAI-compatible server
whose grammar engine makes tool-call arguments schema-valid by construction. We
sent the constraint only as the OpenAI structured-output `response_format`
wrapper (`{ type: "json_schema", json_schema: { schema } }`). Some local
`llama-server` builds — including a turboquant build — reject that wrapper with a
client error, so the F2 fallback drops the constraint and the server runs with
native tool-calling, never engaging its grammar.

Decision: the wire encoding of a tool-call constraint is **selectable** per
provider via a `constraint_mode` option:

- `response_format` (**default, the floor**) — the OpenAI structured-output
  wrapper. Unchanged for every existing provider, hosted or local.
- `json_schema` — a documented llama.cpp server extension: the JSON schema is
  sent as a **top-level `json_schema` field**, which the server compiles to a
  GBNF grammar internally. This reaches the same grammar engine without us
  authoring or shipping a GBNF converter, and without the wrapper the server
  rejects.

The selector is a local concern — it never leaks into the request body — and an
unknown value falls back to the default, so a typo cannot break a turn. The F2
reject→native fallback (a client error on a constrained request caches the
rejection and drops the constraint for the session) is unchanged and remains the
floor for any server that accepts neither encoding.

Provenance (clean-room): the top-level `json_schema` field and the GBNF
`grammar` field are part of the **documented public llama.cpp HTTP server API**;
no private or undocumented endpoint behaviour is used. A raw-GBNF `grammar` mode
is intentionally **not** implemented — the documented `json_schema` field reaches
the same grammar engine server-side, so a hand-written schema→GBNF converter
would add surface for no capability gain.

Rationale: this is the smallest additive change that lets a constraint engage a
grammar on a server that rejects the OpenAI wrapper, it is opt-in and
default-off, and it preserves the native-tool-calling floor. The mode ships
default-off until an uplift eval clears it; this ADR records only the mechanism,
not a default change.

Boundary: this does not change which providers *declare* constrained decoding
(still gated to local servers), nor the fallback semantics; it only changes how a
declared, non-rejected constraint is encoded when `constraint_mode = "json_schema"`.

Live finding (2026-06-22): against a turboquant `q3635ba3bapex` server, the
top-level `json_schema` field is **not** sufficient — it returns the same
`400 "empty grammar stack after <think>"` as the `response_format` wrapper,
because the json-schema→grammar conversion forbids the model's `<think>` opening.
Only a raw **GBNF `grammar` field** engages (turboquant's lazy-grammar tolerates
the `<think>` prefix, returns `200`).

So a third encoding was added: `constraint_mode = "grammar"` emits a top-level
GBNF `grammar` built from the tool names — a *valid tool call* grammar
(`{ "name": <one of the tools>, "arguments": <any JSON object> }`) with a JSON
sub-grammar authored from the JSON spec (original, not copied). Live-verified:
the grammar engages on the turboquant server (`200`, the model emits a valid
constrained tool call after its `<think>` prefix). Argument payloads are
constrained to *valid JSON*, not to each tool's own argument schema — that finer
per-schema constraint (a generic json-schema→GBNF converter) remains a follow-up.

All three encodings ship **opt-in**, default `response_format`. The `grammar`
encoding now *engages* the grammar (subject-02's goal), but a default-on change
still needs a tool-discipline uplift eval to clear it (D002); on `q3635ba3bapex`
discipline is near-ceiling (native already `first_call_arg=100%`), so it stays
default-off pending that measurement.

## ADR-0042: BYOK Credential Storage; No Subscription-OAuth Login

Status: accepted.

LocalPilot's credential helper (`login` / `logout`) is **bring-your-own-key
only**: the user creates a standard API key in the provider's own dashboard
(`login` can deep-link there), pastes it, we validate it with one minimal
request, and store it in the **OS keychain** (Windows Credential Manager / macOS
Keychain), or a restrictive-mode fallback file when no keychain backend is
present. Stored credentials enter resolution at a new top tier, ahead of the
existing environment variable: **keychain → fallback file → `api_key_env` →
config**, so a logged-in user needs no environment variable. `doctor` reports the
resolved credential *source* (keychain / file / env / none), never the secret.

No subscription-OAuth login is added, and **no code path obtains, stores, or
routes Claude Free/Pro/Max or ChatGPT Plus/Pro subscription credentials**, nor
adds a "sign in with Claude/ChatGPT" flow. Primary-source research (2026-06-21)
confirmed neither provider offers a sanctioned OAuth flow that mints a standard
pay-per-token API key for a third-party client: Anthropic's terms forbid routing
third-party requests through subscription credentials (server-enforced since
early 2026), and OpenAI's "Sign in with ChatGPT" is subscription-billed and
locked to OpenAI's own first-party tools. BYOK is therefore the only sanctioned
path. (Full findings: LocalHub research note; the early-2026 enforcement is
flagged "re-verify before release".)

Decisions folded in:

- **Keychain crate selection.** The `keyring` crate (v3, exact-pinned) backs the
  Windows Credential Manager store behind an opt-in `keychain` Cargo feature, so
  the default build links no native credential deps and stays green headless. The
  macOS (`apple-native` → `security-framework 3.7`) and Linux
  (`sync-secret-service` → `zeroize_derive 1.5`) native backends are *not* built:
  both pull a transitive requiring Rust edition 2024, above this workspace's MSRV
  (1.82), and they break `cargo deny --all-features` cross-target metadata. macOS
  and Linux therefore use the `0600` fallback file. Behaviour parity (ADR-0007)
  holds across all three platforms — `login` / `logout` / resolution / `doctor`
  work everywhere; only the secret backend differs (Windows keychain vs file), and
  that difference plus the keychain-absent fallback is documented. (Revisit the
  native macOS/Linux backends when the keyring dependency tree builds on the MSRV.)
- **Best-effort keychain, never blocking.** A keychain that is absent or locked
  is a miss, never an error: the store falls back to the file and resolution
  falls through to the environment, so startup and a live session never depend on
  keychain availability.
- **Secret discipline.** The pasted key is wrapped in `Secret` immediately, never
  logged or echoed in full (only a masked prefix/suffix), and the fallback file is
  owner-only (`0600` on unix) under the user-profile directory. The `Secret` type
  still refuses `Serialize`, so the only places a key leaves the wrapper are the
  audited keychain/file writes.

Reason:

- the only sanctioned credential path is BYOK, so the value we can add is a better
  BYOK *experience* (deep-link + paste + validate + keychain) — never a prohibited
  subscription login. The legality basis, the keychain choice, the resolution
  precedence, and the redaction guarantees are durable and security-sensitive, so
  they live here. Implementation, config keys, URLs, and tests are original to this
  repository; only public key-creation URLs and published policy are referenced
  (clean-room, ADR-0005).

## ADR-0041: Mid-Session Provider/Model Switch Selects An Already-Built Provider

Status: accepted.

A live session can switch its active provider and/or model mid-conversation via
the `/model` command without losing the transcript. The provider registry already
builds **every** configured provider up front into a `HashMap<id, Arc<dyn
ModelProvider>>`; the runtime is handed a shared `Arc<ProviderRegistry>` and the
switch re-points its `provider` + `config.model` by looking the target up — it
does **not** rebuild or re-authenticate a provider on switch. The transcript is
provider-neutral (`Vec<Message>`) and is left untouched, so the conversation
continues against the new provider on the next turn.

Decisions folded in:

- **Turn-boundary only.** A switch is refused while a turn is in flight (a typed
  error) and applied cleanly between turns, so the transcript is never re-pointed
  mid-turn (which could strand a partial tool-call turn the new provider cannot
  continue). The interactive host already defers slash commands until the turn is
  idle; the runtime guard is the belt-and-suspenders contract.
- **Model follows the provider.** A provider-only switch adopts the new provider's
  configured default model; when it has none, the current model name is kept and a
  non-fatal warning is surfaced. `/model <provider> <model>` sets both.
- **Listing reuses discovery.** The `/model` picker lists providers from config and
  their models through the existing `discover_models()` path (`GET /models`) that
  `localpilot models` already uses — no new `ModelProvider` trait method. Discovery
  failure is non-fatal: the configured model is shown with a note.

Reason:

- reusing the build-all-up-front registry makes the switch the cheapest correct
  operation (a lookup, no new auth, no lost state), and pinning it to a turn
  boundary keeps the provider-neutral transcript continuable. A single attached
  registry handle, mirroring the existing `set_*` runtime mutators, avoids a second
  provider-construction path.

## ADR-0040: Prompt History Is A Global Per-User JSONL Store With Project-Scoped Recall

Status: accepted. Adds a per-user persistence surface beside the project-local
session store (`localpilot-store`); the committed storage conventions (atomic
temp-then-rename writes, line-delimited JSON) are unchanged.

The interactive composer's Up/Down recall was in-memory only: every launch
started empty and a restart lost everything. Recall is now seeded from a durable
store so it survives a restart.

The store is a single global append-only JSONL file under the per-user directory
(`%APPDATA%/localpilot` on Windows, `$XDG_CONFIG_HOME`/`~/.config/localpilot`
elsewhere — the same base as `config.toml`), **not** the project-local
`.localpilot/`. Each record carries the visible prompt text, the directory it was
submitted in, and a timestamp. At session start the store is loaded and recall is
seeded with **only the current directory's** entries; a key (Ctrl-T) toggles a
view of every project's entries. Each submitted prompt is appended.

Decisions folded in:

- **One global file, per-project recall filter (not a file per project).** A
  single store is simpler to manage and crash-safe; the directory tag keeps recall
  relevant to the repo, while the toggle preserves cross-project reach. A per-repo
  file would fragment the store and break "view all".
- **Opt-out, default-on (`[history] persistence = "save-all" | "none"`).** The
  best default is recall that survives a restart, but prompts can hold secrets, so
  a full off-switch is mandatory. `none` reads nothing and writes nothing.
- **Stored raw, not redacted (the deliberate divergence from transcripts).**
  Transcripts redact before write; a history entry exists only to be recalled
  verbatim into the composer, so redacting it would recall `[REDACTED]` and defeat
  the feature. The privacy controls are instead the opt-out, mode `0600` on unix
  (the per-user directory's ACL on Windows — tier-1 parity is behaviour parity,
  ADR-0007), the per-user location, and a bounded on-disk cap.

Consequences:

- Recall survives restarts and stays scoped to the project by default; the full
  store is one keystroke away.
- The store is line-crash-safe: a partial final line from an interrupted append is
  skipped on load, and the file is trimmed to a maximum entry count on write so it
  cannot grow unbounded.
- A prompt that carried a collapsed large paste recalls the placeholder, not the
  expanded content — identical to the pre-existing in-session recall behaviour.
- Disk I/O is off the hot path: one bounded, tolerant read at session open and a
  single appended line per submit, never blocking the turn loop; a write failure is
  surfaced as a notice and never breaks the session.

Reason:

- a durable, project-aware recall is a real ergonomic win for a terminal REPL, and
  the secret-leak risk it introduces is fully controlled by the opt-out plus the
  restrictive mode and location. The shape (global JSONL + directory tag +
  per-project filter + opt-out + `0600`) was cross-checked against read-only
  behaviour references; the implementation, config keys, paths, and tests are
  original to this repository (clean-room, ADR-0005).

## ADR-0039: The Inline Live Region Is A Fixed-Height Band, Re-initialised Only On Terminal Resize

Status: accepted. Refines ADR-0021 (inline rendering); the committed ratatui +
crossterm stack is unchanged.

The interactive REPL's inline live region reserves a constant height and is held
there for the life of the session. It is re-initialised only when the terminal's
own dimensions change — a window resize, or a clamp on a short window — never when
its own content changes height.

ADR-0021 originally sized the region to its content and re-initialised the terminal
on every height change. That dropped scroll-up history: ratatui's `insert_before`
only moves a committed block into the terminal's native scrollback once the inline
viewport has reached the bottom of the screen; until then, committed blocks sit
on-screen *above* the viewport. The per-content re-initialisation (`clear` + a fresh
`Terminal`) ran on every composer/activity/picker height change and clobbered those
not-yet-scrolled-back rows, leaving a hole in the middle of scroll-up history — early
conversation gone — while pre-launch shell output, already in real scrollback,
survived.

Consequences:

- Committed transcript blocks are never dropped by a live-region height change;
  history fidelity no longer depends on terminal or session timing.
- The region reserves a small constant band, so a modest blank gap sits above the
  composer when idle. The band height (`LIVE_REGION_HEIGHT` in the terminal driver)
  is the single tuning knob, trading the idle gap against how much in-progress output
  is visible at once.
- The activity tail, composer, and autocomplete pickers render *within* the band
  (each already caps and scrolls internally) instead of growing it.
- A terminal window resize still re-initialises the viewport. This is rare and a
  repaint is expected then; the per-frame churn that caused the loss is gone.

Reason:

- holding the band fixed removes the teardown that caused the loss outright, which is
  simpler and stronger than reducing its frequency. The fixed-height commit path is
  pinned by offline tests over a `TestBackend` (which records a scrollback buffer);
  the user-visible loss is confirmed by manual terminal testing. Behaviour was
  cross-checked against a local read-only behaviour reference, while the
  implementation, prompts, and tests are original to this repository (clean-room,
  ADR-0005).

## ADR-0038: An Oversized Malformed Write Is Recovered By Chunking The Write

Status: accepted. Extends the bad-output recovery ladder (`localpilot-recovery`)
and the tool surface (`localpilot-tools`).

A local model that cannot emit a large file-write tool call as one well-formed
payload makes the provider reject the streamed arguments. The session then treated
this as a generic bad turn: it re-prompted blindly, and because the recovery ladder
only ever shrank *input* context (which does nothing for an oversized *output*), the
model replayed the same too-large call until the repair budget was spent and the turn
degraded — the file never written. This was the root cause of a real session that ran
48 clean tool calls and then failed only on the final large document write.

The malformed payload is invisible to the harness turn loop — the provider adapter
rejects unparseable tool arguments before they become a parsed `ToolCall` event, so
the harness cannot measure the attempted output post-hoc. The failed tool *name* is,
however, in scope where the adapter fails to parse. The fix routes that name out and
acts on it:

- **A typed provider signal.** `ProviderError::MalformedToolArguments { tool, bytes,
  reason }` carries the failed tool name and argument size out of the
  OpenAI-compatible adapter, replacing a flat `StreamDecode` string. Like a stream
  decode, it does not stop the turn — it is a recoverable bad turn.
- **An output-side recovery rung.** `RecoveryAction::RequestChunkedWrite` is the
  counterpart to the input-shrink actions, emitted from the first repair attempt for
  the malformed-output kinds.
- **A targeted repair prompt.** When the failed call was a file-write tool, the
  generic repair prompt is replaced by one that steers the model to write the file in
  pieces — the first section with `write_file`, each remaining section with the new
  `append_file` builtin — rather than replaying the oversized call. The gate is the
  failed tool *name*, not a tunable byte threshold (the name is exact; a threshold is a
  magic number).
- **A first-class append.** `append_file` makes "write in pieces" a native primitive
  instead of tail-anchor `edit_file` gymnastics: atomic, newline-preserving,
  binary-refusing, non-idempotent.

Rejected: transparent harness auto-split of an oversized `write_file` — it pushes
atomicity and partial-failure handling into the harness for no gain over teaching the
model the chunked pattern. Also out of scope: a model-serving fix; LocalBox already
pins the `tqp-v0.2.0` lazy-grammar build, and this is a distinct failure class.

Reason: the recovery acts on the *content* of the next attempt (write smaller), not
just the fact of a retry, so an oversized write completes within the existing repair
budget instead of degrading. The same change also wires up the ladder's previously
inert input-shrink actions (`ReduceContext` / `SummarizeOversizedToolResults`), which
were computed but never consumed, so a repeated bad turn now compacts history before
re-prompting.

## ADR-0037: Completion-Retrospective Lessons Are Offered To Review-Gated Memory

Status: accepted. Builds on ADR-0035 (the advisory completion retrospective that
records lessons to `LESSONS.md`), ADR-0011 (store split / review-gated accepted
memory), and ADR-0034 (the review-gated memory path). Closes the deferral recorded
in the ADR-0035 "review-gated bridge" note (and DriftRemediation D007).

The completion retrospective writes advisory lessons to the root `LESSONS.md`, a
human-editable mirror. An un-gated file next to a review-gated memory engine is a
half-measure: a lesson worth keeping should be promotable to accepted memory through
the same human review every other memory passes, not stranded in a file. So each
retrospective lesson is **also offered** to LocalMind's review-gated candidate queue.

**The bridge.** `localpilot_localmind::write_retrospective_lesson` enqueues a lesson
as a `CandidateLesson`; the cli calls it for each lesson after the run prints the
retrospective summary.

- **Advisory and non-blocking.** A failed enqueue never breaks a finished run; the
  host swallows the result (`if let Ok(Some(_))`), which is safe because the bridge
  returns `Err`, never a panic. `LESSONS.md` is written by the retrospective before
  the offer runs and is left untouched — it stays the human-editable mirror.
- **A different shape from a loop-outcome lesson.** A retrospective lesson is a
  free-text advisory note: it sets **no** accepted/rejected `outcome` and **no**
  change-provenance ref (reusing `LoopLesson` would fabricate both — the exact reason
  the bridge was deferred). It is a `Process` candidate, `completion_retrospective`
  evidence kind, with a deliberately lower prior confidence (`0.4`, below the
  loop-outcome `0.75`) — an unverified self-observation, not a confirmed patch outcome.
- **Queue-noise policy.** Too-short/sentinel lessons are skipped; duplicates are
  deduped by the review queue's own canonical-hash (a repeat bumps a seen-count). No
  custom dedup — the store already provides it.
- **Review-gated by construction.** The candidate is `PromoteToMemory`; promotion to
  accepted memory stays a human step (ADR-0011); a rejected candidate never reaches
  memory. The bridge writes **no** accepted memory and adds **no** second redaction
  authority.

Reason: routing advisory lessons through the same human-gated queue makes
`LESSONS.md` a mirror rather than a competing sink, without loosening any safety floor
— promotion is still human, and the lower prior confidence plus the store dedup keep
the queue from being flooded. The mapping is a small adapter function plus a few
advisory host lines; the harness gains no LocalMind dependency (the edge stays
host→adapter).

Supersedes nothing.

## ADR-0036: The `localpilot-localmind` Adapter Boundary And Its Extraction Trigger

Status: accepted. Builds on ADR-0011 (store split: `.localpilot/` execution record
vs `.localmind/` memory) and ADR-0012 (`.localpilot/` derived state is disposable).
Host-side only: the engine's matching invariant (host-neutral `localmind-core`) is
already LocalMind `D-LM-0002`, so this record adds no new cross-engine decision.

`localpilot-localmind` has grown from a thin adapter into a sizable subsystem:
the ingest engine alone is ~3000 lines, alongside the chunk store, layered pack,
cold-start primer, the derived search index, and the model-callable tools. This is
defensible today — it is one cohesive host-side concern (turn a workspace into
retrievable, redacted, host-owned derived context for the bundled engine) — but it
will not stay defensible if the next knowledge feature simply lands here too.

**The boundary, as it stands.** The adapter owns the *host* role and nothing the
engine owns:

- **The host owns filesystem walking and redaction.** Discovery, ignore rules,
  and the canonical redaction pass run host-side before anything is persisted —
  one redaction authority (ADR-0011); LocalMind's import redaction is defense in
  depth, never a second authority. The engine never walks the filesystem itself.
- **`.localmind/ingest/` derived state is disposable and rebuildable** (ADR-0012):
  the index/chunk/pack artifacts can be deleted and regenerated from the workspace
  plus the engine; nothing durable lives only there.
- **Accepted-memory writes stay LocalMind review-gated.** The adapter enqueues
  candidates; promotion to accepted memory is a human, review-gated step in the
  engine. The adapter never writes accepted memory directly.
- **The dependency edge is one-way:** LocalPilot depends on LocalMind, never the
  reverse; `localmind-core` stays host-neutral.

**The extraction trigger (no move now).** Before the **next** major
ingestion/knowledge capability lands in `localpilot-localmind`, pick one of two
splits rather than growing the adapter again:

1. split the derived **index / search / pack** primitives into a narrower
   LocalPilot crate (e.g. `localpilot-localmind-index`), leaving the adapter as
   the contract/redaction/permission seam; **or**
2. move the host-neutral derived-context primitives **behind a LocalMind API**, so
   the engine owns them and the adapter shrinks to capture + redaction + wiring.

Either split must preserve the four invariants above. This ADR is the recorded
trigger; it deliberately does **not** move code today (the current size is
cohesive and tested), so a future contributor extends against a fixed boundary
instead of an ad-hoc one.

Reason: recording the boundary + trigger now is cheap and keeps the "adapter, not a
second engine" intent legible; discovering the boundary only when the subsystem is
already too large is the expensive path. Deferring the move avoids a churny
refactor with no present payoff (§ KISS/YAGNI) while still preventing silent
unbounded growth.

Supersedes nothing.

## ADR-0035: Plan Mode Carries Planning Judgment — Reuse-Before-Add, Acceptance-Criteria Coverage, And An Advisory Completion Retrospective

Status: accepted. Builds on ADR-0010 (the runtime validates and controls — the
model proposes, the runtime decides) and ADR-0003 (`brief.md` / `PROGRESS.md` /
`DECISIONS.md` are authoritative, user-editable runtime documents).

The harness plan-mode flow (idea → `brief.md` → `PROGRESS.md` → per-step resume)
was strong on machinery — per-step commits, the anti-sunk-cost replan loop, the
quality gate, the evidence-grounded decision log — but thin on the planning
*judgment* a good plan needs. This record fixes three additions, all expressed as
properties of the documents the model already produces, with the runtime
plan-document shape unchanged (a flat numbered step list, no sub-plan ceremony).

**Reuse before add, and cover every criterion (planning time).** The planner is
instructed to study the repository summary it already receives and prefer a step
that extends or reuses an existing module/type/function — naming it — over adding
parallel code, and to produce a step list that collectively satisfies every
acceptance criterion in the brief. These are contracts on the generated document,
not a `PROGRESS.md` schema change.

**Risks and doc-currency (structural cues).** `brief.md` gains an optional
`## Risks & Rollback` section (absent in an older brief, rendered only when
present, round-trips losslessly); the per-step worker prompt tells the model to
update the matching documentation in the same step when a change alters observable
behaviour, configuration, or interfaces.

**An advisory completion retrospective (completion time).** When no incomplete
step remains, the harness runs one bounded review over the brief and the completed
plan — acceptance criteria still unmet, scope drift, tests that pin implementation
detail — and appends durable lessons to a root `LESSONS.md` (a new authoritative,
user-editable runtime document alongside `DECISIONS.md`). The retrospective is
**advisory by construction**: it reports findings and records lessons; it never
blocks completion, never edits shipped code, and never commits. It runs once,
after the final step is already committed, and a provider/quota error there is
swallowed so it can never break a finished run.

Reason:

- planning judgment belongs in the instruction that generates the document, not
  in a new analysis engine bolted onto the loop — reuse-before-add and
  criteria-coverage are properties of the plan the model writes, so they live in
  the planner prompt and are pinned by a stable prompt snapshot plus a behaviour
  test that the contract and its inputs reach the model;
- the retrospective stays on the propose side of ADR-0010's boundary: like the
  read-only stages of the self-improvement loop (ADR-0034) it only *reports*, so a
  confused or wrong review costs an advisory line, never an unintended write or a
  blocked completion;
- a flat `PROGRESS.md` is correct for plan mode's scale — importing sub-plan
  structure (subjects, owners, gates) would add ceremony a single autonomous build
  does not need;
- `LESSONS.md` mirrors `DECISIONS.md` (root-sited, append-only, round-tripping,
  user-editable) so the lessons are reviewable and survive a context reset, rather
  than hiding in a disposable cache.

Supersedes nothing. A config switch to disable the retrospective, structured
per-step → criterion reference tags, and writing lessons back into LocalMind
memory are explicit non-goals here; each is a separable later decision behind the
same seam.

**Review-gated bridge — shipped (ADR-0037).** `LESSONS.md` remains the
human-editable *mirror*; each retrospective lesson is now **also** offered to
LocalMind's review-gated queue as a `Process` candidate with its own lower prior
confidence and the store's canonical-hash dedup — never accepted memory without a
human promotion. The bridge deliberately does **not** reuse
`write_loop_lesson`/`LoopLesson` (a patch-outcome shape that would fabricate an
`outcome` + a change-provenance ref a completion retrospective lacks); it is a
separate `write_retrospective_lesson` mapping, advisory and non-blocking, with the
harness still free of a LocalMind dependency (the edge stays host→adapter). See
ADR-0037 for the full decision.

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

**As shipped (2026-06).** The read-only half (`localpilot-selfreview`: observe →
detect → propose, advisory findings report) **and the write half** are now wired.
The write half (`localpilot-patchgen`: worktree proposal + `ApprovalToken`-gated
promotion) is reached only through `localpilot self-review propose-patch` /
`promote` / `discard`: `propose-patch` has a model author a minimal, scope-confined
edit for a ranked finding into an isolated worktree and **stops**; `promote` applies
it onto `main` only when an explicit human `--approve` mints the token (fast-forward
only, never pushes); `discard` drops the worktree/branch. A proposal **persists
across invocations** via the on-disk worktree plus its provenance record
(`ProposedPatch::persist`/`reopen`), so a human reviews the diff between proposing
and promoting; reattaching mints no token and writes no `main`. The by-construction
invariant is unchanged: the sole `ApprovalToken` constructor is the explicit-human
`--approve` path, and `promote`'s signature requires the token, so no autonomous or
reattach path reaches a `main` write without a human act. Behaviour is proven
against `FakeProvider` offline (D008); a live local-model run is opportunistic.
Edit generation, model-judged critique quality, and any move toward reducing the
gate remain separate later decisions.

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
whole index is never loaded. *(The term-count rescore was later replaced by the
index's own bm25 ranking — see ADR-0057, which supersedes this "ranking is
unchanged" clause.)* Refresh updates only the paths that changed:
unchanged files are reused by `path:content_hash`, a changed file's prior rows
are kept as stale tombstones pointing at the new hash, and a vanished file's rows
are tombstoned with no successor. An existing `chunks.json` migrates into the
database on first open and is then removed; `ingest rebuild` recreates the store
from source. Only the large chunk index moved — the small manifest/job/review/
last-pack files stay JSON.

The stepper has since taken two additive steps, each nullable/defaulted so a
pre-existing store upgrades clean: **v2** adds a `context_prefix` column
(contextual chunk prefixing); **v3** adds a nullable `language` column
(language-tagged chunks + workspace-language-filtered search, reusing
`localmind_store::language_for_extension`); **v4** adds a rebuildable
`ingest_chunk_vectors` table (chunk id → LE-f32 BLOB + content fingerprint +
model + dimensions), mirroring the accepted-memory `vector_index` shape, for
best-effort chunk embeddings. Embeddings are gated on a configured embedding model
(the same `InferenceCapability` gate accepted memory uses) and never fail ingest;
chunk vectors are dropped with their chunks, so the keyword path and the
disposable/rebuildable contract are unchanged.

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

## ADR-0043: Curated Lesson Seeding And A Re-Enable Toggle For The Memory A/B

Status: accepted.

LocalPilot can seed a curated, author-reviewed set of best-practice lessons
directly into LocalMind accepted memory, and can re-enable context injection it
previously disabled. Two host-side additions, no engine change:

- **`localpilot learning seed --file <pack.json>`** reads a seed pack
  (`{ "lessons": [ { "body", "category"?, "confidence"?, "related_files"?,
  "related_entities"?, "evidence"?, "tags"? } ] }`) and writes each lesson as
  active accepted memory through `MemoryPersistence::persist_memory_entry` — the
  write path `localmind-store` already sanctions for "hosts accepting memory
  through their own review surface". It is **idempotent**: a lesson whose
  whitespace-normalised body already exists is skipped, and the memory id is a
  stable FNV-1a hash of that body. `--dry-run` validates and counts without
  writing.
- **`localpilot memory enable`** clears the `.localmind/context-injection-disabled`
  flag that `memory disable` writes (idempotent), giving the previously one-way
  toggle a counterpart.

Rationale: the in-session candidate→review→promote queue is the right path for
lessons *discovered* during work, but a curated pack of durable best-practice
lessons is reviewed *at authoring time* — routing dozens of hand-written lessons
through the per-session queue adds no safety and much friction. The human gate
moves to authoring, not the queue; nothing is auto-extracted. The enable toggle
exists so a lesson-on vs lesson-off measurement can be scripted (disable → run →
enable → run) rather than hand-deleting a flag file, and the
memories-used audit (`localpilot memory used`) proves an arm actually injected.

Boundary: seeding writes accepted memory directly and therefore skips the
contradiction-detection and code-graph anchoring that promotion-through-review
adds; that is acceptable for curated prose lessons, and `related_entities` can
still be supplied for retrieval. The seed path is additive — projects that never
run `learning seed` are unchanged, and seeded memory is ordinary accepted memory
(searchable, deletable, injection-gated like any other).
