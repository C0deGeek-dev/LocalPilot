# Configuration

LocalPilot reads `.localpilot.toml` from the user config directory and the
project root (project overrides user), with environment variables and CLI flags
layered on top. `localpilot init` writes a starter file; `localpilot doctor`
shows the resolved search paths.

## Stability

The configuration schema is **stable under semantic versioning** from v1.0:

- Within a major version, the documented tables and keys below keep their
  meaning. New optional keys may be added (a minor change); existing keys are
  not renamed, removed, or retyped without a major-version bump and a documented
  migration.
- **Unknown keys are ignored**, so a config written for a newer minor version
  still loads on an older binary, and vice versa. Per-provider keys the core
  does not model are preserved (see `[providers.*]` options).
- Defaults are stable: an omitted key behaves as documented here.

The configuration schema is SemVer-stable (LocalPilot has been ≥ 1.0.0 since
2026-06-24); any change is noted in `CHANGELOG.md`.

## Interactive chat host

Interactive `localpilot chat` runs the full-screen terminal application:

```powershell
localpilot chat
```

The full-screen application is the only interactive host; there is no host
selector. Its layout does not affect non-interactive/plain output.

For the full-screen host, `LOCALPILOT_CHAT_THEME` accepts
`default`, `dim`, `high-contrast`, or `colorblind`. An invalid value is shown as
a sanitized timeline warning and falls back to `default`. The default uses a
stable true-color palette so large frames and active chrome do not inherit a
terminal profile's potentially saturated ANSI colors. `NO_COLOR` disables the
palette while retaining non-color focus, selection, success, and error cues.
The human-facing `skills available` and `skills research` catalog reports reuse
the same semantic palette when stdout is a terminal. They emit no ANSI escapes
when piped, captured by `/skills`, or when `NO_COLOR` is present; their labels,
two-line entries, and blank-line boundaries remain the primary cues.
Mouse selection remains highlighted after release so Ctrl+C or a right-click in
the timeline can copy it explicitly. A right-click inside the composer pastes
clipboard text through the same atomic path as other paste input, so multiline
content never submits line by line. Set `LOCALPILOT_CHAT_COPY_ON_SELECT=true`
(or `1`) to copy immediately on release; `false` and `0` retain the default
behavior. These environment-only controls do not extend the stable TOML schema.
Mouse reporting is enabled by default; set `LOCALPILOT_CHAT_MOUSE=false` (or
`0`) before launch for a keyboard-only fallback that leaves pointer handling to
the terminal.

Input submitted while the model is working appears immediately as a pending
timeline row. Escape turns the leading run of plain-text pending prompts into
ordered steering and restarts the current provider turn with that direction;
Ctrl+C first stashes and clears a typed composer, then cancels on the next
empty-composer press, then exits on a following consecutive press. With no typed
draft, cancel remains the first rung; a selection still copies before composer
handling. Restore a stashed draft with Ctrl+S. A pending shell command or image
prompt remains an ordering barrier, so Escape cancels the current work and then
the queued operations continue in their original order rather than being
reordered.

Set `LOCALPILOT_CHAT_SCREEN_READER=true` (or `1`) for the target-shaped
full-screen accessible projection: tabs become a wrapped current-tab sentence,
conversation rows use explicit textual roles and states, decorative banner and
prompt chrome is removed, dialogs expose their current selection in text, and
in-app scrollbar glyphs are hidden. The composer and alternate-screen lifecycle
remain unchanged, while non-TTY output continues to use LocalPilot's separate
plain path. Terminal chrome is static; there is no animated spinner or motion
effect to disable.

Tool calls appear as one compact running/completed/failed/cancelled row. An
expandable row shows a chevron and its current hidden-row count; clicking the
status prefix focuses it and expands the complete retained target detail and
captured output. F7/F8 move focus through tool rows, Enter toggles the focused
row, and Escape returns to the composer; ordinary typing also releases tool
focus before editing. Expansion preserves the headline's screen row except for
a deterministic clamp at the bottom of the timeline.
Finished rows include elapsed time and align their metadata at wide widths when
that cannot force a wrap. Terminal-only output is bounded to 256 KiB with both
an explicit middle-omission marker and a `terminal view truncated` headline
label, while collapsed rows use the separate temporary hidden-row cue. The
ordinary running, successful, and cancelled preview is at most four wrapped
rows. A failed tool uses a fixed eight-row preview—two leading rows and the six
newest retained rows—so the diagnostic tail is visible without unbounded
expansion. Its ellipsis gutter and screen-reader label identify earlier hidden
rows; copying across the jump inserts an omission marker. The
provider transcript and retained tool-output path keep their existing independent
limits. A tool row
stays interleaved with the assistant text and reasoning that surround it: text
streamed after a tool call opens its own row below that tool, in the order the
model produced it, rather than merging into the assistant paragraph that came
before the call. A new assistant/reasoning row ignores leading CR/LF provider
framing (and an all-whitespace opening chunk); later chunks preserve their
whitespace exactly.

Interactive chat also advertises the builtin `ask_user` tool through one shared
host-capability contract. In the full-screen host, up to four questions appear
in order as asking timeline rows and centered numbered-choice dialogs; each can
be single- or multi-select, carries optional choice descriptions, and always
offers a free-text Other row. The same state is projected as borderless
role-labeled text in screen-reader mode. Arrows, Space, Enter, Escape and mouse
focus stay inside the dialog as applicable, while wheel/Page navigation can
continue to move the conversation behind it. Answered or dismissed questions
resolve their existing timeline row in place. Headless hosts wire no prompter,
so they report the capability unavailable instead of waiting forever.

When workspace trust is required, the full-screen host presents it as a
full-width timeline dialog. The first choice trusts the workspace for the
current process only, the second also remembers it for later sessions, and the
third exits without trusting. Arrow keys or the mouse move the focused choice;
Enter confirms it and Escape exits. Screen-reader mode exposes the same three
choices and current selection as text. Session-only trust starts the same
trust-gated workspace services without writing the trusted-folder list.

Trust can also be managed outside chat: `localpilot trust add [PATH]` /
`remove [PATH]` / `list` / `status [PATH]` (paths default to the current
directory), and `localpilot doctor` reports the current folder's trust state and
store path. Trust is exact-folder — not inherited by subdirectories — and stored
one canonical path per line in `trusted-folders.txt` next to the user config
(`%APPDATA%\localpilot\` on Windows, `$XDG_CONFIG_HOME/localpilot/` elsewhere).
The `bypass` and `unrestricted` profiles skip the prompt, so the CLI is the
persistence route for a folder always worked in those profiles. See
[07-security-and-privacy.md](07-security-and-privacy.md).

The full-screen host resolves the provider's vision capability from config, else
a best-effort probe. On a vision-capable model an image can be attached three ways, each
as an atomic placeholder: Ctrl+V a copied bitmap; Ctrl+V a copied image file
(read from the clipboard file list when no bitmap is present); or paste/drop an
image-file path into the composer. Supported formats are PNG, JPEG, WebP, and
GIF, identified from magic bytes. The encoded image is sent only with that
submitted turn and is never written to prompt history. Provider declarations
may set `supports_vision = true`, while `[discovery] vision_probe = true` enables the
existing best-effort local-server capability probe. Every path surfaces a notice
— on success, on a capability refusal (naming both levers), on an absent,
unreadable, unsupported, multiple, or oversize input, and when an open overlay or
dialog owns the composer — so a paste never silently does nothing. Quick help
(`?`) and `/help` document the three forms.

During a task, the agent can use `read_image` to inspect a local image by path
without another user attachment. This call uses the normal file-read permission
rules and the same formats and 5 MiB encoded-size limit.

Ctrl+G edits the idle composer in a foreground external editor. The host checks
`LOCALPILOT_EDITOR`, then `VISUAL`, then `EDITOR`; the value may contain a quoted
executable path and arguments. If none is set, the fallback is Notepad on
Windows and `vi` elsewhere. Editors that normally detach should be configured
with their wait flag (for example, `code --wait`) so LocalPilot can read the
temporary draft before it is removed. The temporary file contains visible
placeholder text only—not compact-paste or image payload bytes—and edited input
is capped at 8 MiB.

The full-screen slash picker lists the commands that have a real replacement-host
path: the `/agent` and `/harness` mode switches, session and model management
(`/model`, `/new`, `/fork`, `/clone`, `/clear`, `/quit`), the
`/help`/`/theme`/`/settings`/`/diff`/`/search` takeovers, the permission-profile /
`/effort` / `/think` toggles, the synchronous report commands (`/tree`, `/knowledge`,
`/context`, `/agents`, `/skills`, `/bg`), `/compact` and the long-running `/ingest`
runs on the operation pump, `/research` (one-shot `/research <topic>` and the
persistent bare-`/research` mode), and the `/harness-resume` / `/wait-resume` resume
commands on the pump — every full-screen command reaches a real route (nothing is
"not available in full-screen chat yet"). `/model` opens
a second-level picker built from configured providers; an exact
`/model <provider>` also executes directly.
`/help` opens a scrollable full-screen keyboard and command reference; Escape
returns to the untouched conversation, including while work continues behind
the help view. Press `?` on an empty idle composer for transient two-column quick
help. `/theme` opens a centered semantic-color preview: arrows or mouse selection
preview the entire UI, Escape restores the prior theme, and Enter accepts the
choice for the current process. Set `LOCALPILOT_CHAT_THEME` to make the launch
choice explicit. `/settings` opens a contained read-only view of the effective
terminal, appearance, provider, and session values; launch-time controls remain
environment settings. Slash invocations never enter prompt history or the
provider prompt queue. A manually typed unsupported command produces an in-app
notice instead of being sent to a model. During an active full-screen
turn, profile commands, `/bg`, `/effort`, and `/think` execute live; profile
changes apply from the next tool call, effort from the next provider request,
and background commands use the session registry. Full-screen `/help`, `/theme`,
`/search`, and exit also stay available. Enter and Ctrl+Q use the same slash
dispatcher. Other commands, including `/clear`, are refused with a notice naming
the live choices. Runtime operations without live session handles refuse
profile/background/effort changes explicitly.

The timeline's session density can also be changed from `/settings`. The
persisted launch default is configured under `[terminal]`; `compact` preserves
the current tight Tool-to-Tool layout, while `comfortable` inserts an optional
blank row only between adjacent tool rows. The separator between a tool and
following assistant or reasoning text is always present in either mode.

`/diff` opens a contained two-pane review of tracked Git changes against `HEAD`
(or the current index/worktree when no `HEAD` exists). Arrow keys navigate the
active file or content pane, Left/Right switches panes, `t` hides or restores
the file tree, and Escape returns to the untouched conversation. Diff capture
does not run external diff drivers and is bounded to 8 MiB.

## Pair collaboration

`localpilot pair` is an opt-in command that runs two independent Agent-mode
sessions on one task and workspace:

```powershell
localpilot pair "review and implement the cache fix"
```

The pair-specific CLI flags are:

| Flag | Meaning and default |
|---|---|
| `--provider-a <PROVIDER>` | Provider for peer A; the configured default provider when omitted. |
| `--model-a <MODEL>` | Model for peer A; that provider's configured model when omitted. |
| `--provider-b <PROVIDER>` | Provider for peer B; the configured default provider when omitted. |
| `--model-b <MODEL>` | Model for peer B; that provider's configured model when omitted. |
| `--permission <PROFILE>` | One profile shared by both sessions: `default`, `relaxed`, `readonly`, `bypass`, or `unrestricted`; default `default`. Any other value is a usage error. |
| `--bypass` | Explicit shorthand for `--permission bypass`. |
| `--max-rounds <N>` | Positive collaboration-round cap; default `3`. |
| `--slot-timeout <SECONDS>` | Positive wall-clock limit for one peer slot; default `600`. |

These flags are resolved at launch. There is no persistent pair-specific TOML
table: normal provider/model configuration supplies the omitted provider and
model selections, while an omitted `--permission`/`--bypass` selects the
`default` profile.

Before pruning, setup, session construction, or model work, the command prints
a cost/quota note to stderr. Two resident histories may use more tokens and
provider quota than one session, although only one model turn runs at a time.
The displayed round and per-slot time bounds, `/abort`, and Ctrl+C bound or stop
the run; they do not promise a fixed token or price multiplier.

Exit status `0` means both peers agreed on the current typed candidate. A round
cap, timeout, abort, peer/provider failure, protocol error, budget limit, no
progress, or driver failure returns a nonzero status. The command does not apply
or commit the agreed candidate automatically.

### Pairing with Claude Code and Codex (`localpilot mesh`)

`localpilot mesh` lets LocalPilot take part in a pair-programming session that
Claude Code or Codex started with the c0degeek `pair-programming` skill. That
skill keeps a file mailbox (`.pair-programming/`) in the shared working tree,
and `localpilot mesh` reads and writes the same files under the same locks as
the skill's own `pair.py`. There is no second mailbox and no server.

LocalPilot takes part as a participant: it joins a session, reads its mail,
posts, acknowledges, reports its health, hands a unit of work over or accepts
one, closes a unit it owns on its reviewers' agreement, and registers a
delivery endpoint. Starting, parking, resuming and retiring sessions stay with
`pair.py`; those operations exit `2` with a pointer to it.

```powershell
localpilot mesh --repo . join --role localpilot --timeout 60
localpilot mesh --repo . watch --role localpilot --timeout 600
localpilot mesh --repo . post --role localpilot --kind ANSWER --reply-to codex:4 --body-file answer.md
localpilot mesh --repo . status
```

The arguments follow `pair.py`, so the same conformance suite checks both
implementations; the suite LocalPilot runs is vendored in
`crates/localpilot-mesh/conformance`. Its mixed-writer soak also runs
`pair.py` and `localpilot mesh` against one live mailbox at the same time and
checks that nothing is lost, duplicated or torn: a short run in every test
run, and a long one nightly on Linux and Windows. The anchor tree is `--repo`, then
`PAIR_REPO`, then the current directory.

| Exit status | Meaning |
|---|---|
| `0` | Success. |
| `1` | A refusal (the reason is on stderr), or a `watch`/`join` that timed out. |
| `2` | A usage error, or an operation only a full implementation provides. |
| `4` | `guard-write` denied the write (`WRITE_DENIED` on stderr). |
| `5` | A delivery request was refused (`REFUSED <code>` on stderr). |
| `6` | The message is posted, but its bookkeeping did not complete (`POSTED_INCOMPLETE msg_id=<id> cause=<cause>` on stderr). Do not post it again. |

A post is durable once its message is appended to the journal. After that
point the post waits up to 65 seconds for the session's state lock, and any
later failure exits `6`, never `1` with "mailbox lock busy": a retry would
post the message twice.

Posting wakes the recipients that are listening. After any operation that
appended a message, and once it holds no lock, `localpilot mesh` sends one
wake-up to each addressed recipient that has a live delivery endpoint: a
Windows named pipe or a Unix socket, recorded in the session by the
recipient's endpoint host. A wake-up carries no message, only "you may have
mail"; the mailbox stays the record. Each outcome (`sent`, `refused`,
`failed` or `timeout`) is written to the sender's push log in the session.
All the wake-ups of one post share a 2-second limit, and they never change the
operation's exit status or output. `mesh run` does the same after every
message it posts, keeps at most one wake-up per recipient in flight, and
cancels any still pending at the limit. An endpoint address that is not a
well-formed pipe name or absolute socket path for this platform is never
dialled. Set `PAIR_NO_PUSH=1` to turn wake-ups off.

#### The participant engine (`localpilot mesh run`)

`localpilot mesh run` takes part on its own: the protocol runs in code, and a
model is asked only for judgement.

```powershell
localpilot mesh --repo . run --role localpilot --model qwen3-coder --timeout 300
```

It joins as soon as a session names the role, then handles each message as
it is delivered:

- It checks a review request's `Fingerprints` manifest itself before any
  model is asked. The manifest has one `path=<12 hex>` or `path=deleted` line
  per changed file, and the fingerprint is SHA-256 with CRLF read as LF. A
  manifest that does not match the tree gets a `REVISE` from the engine.
- It asks the model for a verdict when it is a required reviewer, or for a
  reply to a message that expects one. The model answers with one JSON
  object. The engine writes the verdict header from the findings. An answer
  the protocol does not allow is refused and asked for once more. After a
  second failure the engine posts an `ESCALATE` instead of the model's text.
- It acknowledges a message only after answering it. A message already
  answered is acknowledged without a second answer. An answer for a unit
  that has since moved on is not posted.

The model's turn runs `readonly` under the session's write lease, with its
transcript kept in memory, no configured MCP server started, and none of
the user's `[[permissions.allow_commands]]` grants in force, so it can
read the tree but never change it. The manifest is checked again after the
model answers; if the tree moved during the review, the engine posts a
`REVISE` in place of the model's verdict.
`run` needs a schema-2 session (started with `--with`) under acknowledged
delivery, and the native writer.

With `--own`, LocalPilot also does the owner's work when a unit is handed to
it:

- It accepts the handoff through the protocol operation, which refuses if
  the tree moved since the offer.
- It has the model implement the task in a turn that may write only while
  the session says LocalPilot owns the tree. That turn runs with `bypass`
  so the model can run tests headless. File tools keep the workspace
  boundary; shell commands are not contained.
- It builds the review request itself. The manifest covers everything
  committed since the unit's base plus everything uncommitted, and the
  engine fingerprints each path.
- It answers `REVISE` findings in further rounds, and escalates after three
  rounds or after a round that changes nothing.
- It closes the unit only when every required reviewer has agreed.

Without `--own`, a handoff offered to LocalPilot is declined with a note.
`--own` needs a Git anchor; a session without version control is refused
before any model work. A required reviewer's `ESCALATE` on the open request,
or its `STOP`, ends the owner's run. After a hand-away and back, only
requests and verdicts from the current ownership count.

With `--listen`, the engine is woken the moment a peer posts to it, instead
of finding the mail on its next look:

- It registers a delivery endpoint for its role from its own process and
  keeps the endpoint's token in memory only: it is never printed, put in the
  environment or passed on a command line.
- On Windows the endpoint is a named pipe that only your own user can open.
  On Unix it is a socket in a private (`0700`) directory, and a connection
  from another user is closed.
- Each wake is checked: a malformed request, another role, another session
  or an old generation is refused with its reason. A valid wake records a
  receipt and runs the engine's loop at once.
- The registration is a 60-second lease renewed every 20 seconds on its own
  schedule, so a long model turn cannot let it lapse. If another process
  takes the endpoint over, the engine stops listening, says so
  (`LISTEN_STOPPED`), and never takes it back.
- The mailbox is still read every `--poll` seconds, 30 by default when
  listening, so a missed wake loses nothing.
- On a clean exit (`STOP`, `--once`, Ctrl-C) the endpoint is retired. If the
  process dies instead, the lease expires within a minute and a new run can
  register again.

`localpilot doctor` shows, in its `mesh` section, the writer, the anchor tree
`mesh` would use, its active session, and whether the engine can join it as
`localpilot` (and if not, why). It reads the mailbox and writes nothing.

| Exit status | Meaning |
|---|---|
| `0` | Stopped cleanly: a `STOP`, or `--once` after one delivery. |
| `1` | An error, or no session named the role within `--timeout`. |
| `2` | A usage error, or the delegate writer is selected. |
| `4` | The session cannot host the engine (the reason is on stderr). |

#### Artifacts, usage, quota takeover, and advisory context

The native participant implements mailbox protocol 1.2. Mail can carry
`--artifact <sha256-id>` references; every attached bundle is verified before
posting and shown when mail is read. Immutable artifact creation and extraction
remain reference operations: use `pair.py artifact put|list|show|get` or
`pair.py verify-request --pack`. `mesh artifact` and `mesh summary` refuse with
exit 2, including under the delegate writer. Hash verification proves stored
bytes, not the truth of a test claim or the quality of a document.

```powershell
localpilot mesh usage
localpilot mesh usage --role codex --report-id turn-1 --input-tokens 100 --output-tokens 20
localpilot mesh usage --role codex --report-id turn-2 --cost-microusd 200 --limit-percent 80
```

Reports are incremental and idempotent by role and report id. A conflicting
retry refuses. Totals stay separate per session, participant, unit and source;
the native engine records provider token counts after each model turn as
`engine`, while CLI reports are `reported`. Sources are audit claims on a shared
trusted filesystem. Missing price or quota stays unknown. Totals always disclose
partial coverage and the fraction of reports with known costs; no price is
guessed. `status`, cockpit JSON, and the terminal cockpit show the ledger. The
terminal shows three groups and discloses any additional groups; `mesh usage`
and cockpit JSON show all groups.

Before a duty holder is rate-limited, it can authorize a named successor:

```powershell
localpilot mesh takeover-authorize --role codex --to localpilot --duty reviewer
# Only after codex has a recorded pause:
localpilot mesh takeover --role localpilot --from codex --duty reviewer
# After codex's terminal has joined again, return the seat explicitly:
localpilot mesh reviewer-transfer --role localpilot --to codex
```

`--duty owner` authorizes ownership instead. `takeover-authorize --revoke`
cancels an authorization. Only three-party sessions with a healthy independent
required reviewer can transfer a duty. Authorization is scoped to session, unit
and epoch; the transition consumes it, invalidates old reviews, clears waits,
records its event, and preserves the original holder's pause. Recovery never
returns a duty automatically. Ownership returns by ordinary handoff. Sessions
using quota transitions require `quota-takeover`; older writers refuse them.
The engine never initiates a takeover itself.

Start through the reference with `--context-summaries` to opt in once. Then
`pair.py summary --role <r>` creates an advisory artifact, or native
`localpilot mesh memory --role <r>` captures visible recent mail into LocalMind.
Capture reads at most 256 KiB per journal and uses its last 50 records, with
bodies capped at 1,000 characters after known-secret filtering. Source message
ids are preserved. LocalMind's own learning opt-out is honored. Extraction is
deterministic and local; candidates remain pending even under automatic review
mode. No model is called and no accepted lesson is created automatically.
Filtering is best-effort: inspect material before sharing it. Summaries never
enter agreement, ownership or acknowledgement rules. `mesh memory` requires
the native writer; the delegate has no LocalMind capture operation.

#### Waiting for mail (`localpilot mesh wait`)

`localpilot mesh wait` is a one-shot waiter that a host (Claude Code or
Codex) runs in the background in place of `pair.py watch`:

```powershell
localpilot mesh --repo . wait --role claude --timeout 3600
```

It registers the role's delivery endpoint from its own process, looks at the
mailbox once, and then sleeps until a peer's post wakes it, or until the next
look every `--poll` seconds (30 by default). When mail arrives it prints
exactly what `watch` prints and exits `0`; at `--timeout` it exits `1`,
silent. It retires the endpoint on the way out. If it cannot hold the
endpoint, because another live process holds it or a crashed waiter's lease
has not yet run out, it says so on stderr and polls like `watch`.

- `--ack-through` acknowledges first, as `watch --ack-through` does.
- `--nudge-only` reads nothing: it moves no cursor, writes no receipt, and
  exits `0` with one line, `NUDGE sender:N[,sender:N]`, once there is
  unacknowledged mail for the role. With `--after sender:N[,sender:N]`, only
  mail newer than those points ends the wait. This is for a notifier that
  must not read the mail itself.
- `--exit-with-parent` stops the waiter, retiring the endpoint, when its
  stdin reaches end of file. A parent that runs it with a pipe it never
  writes gets it stopped whenever the parent exits, even when it is killed.

Every look is bound to the session the waiter started in, so a session
switched meanwhile never hands it another session's mail.

#### The mailbox as MCP tools (`localpilot mesh mcp`)

`localpilot mesh mcp --role <r>` serves the mailbox over stdio MCP for a host
that would rather call tools than run commands. Register it in your user
configuration, never in a project file a clone could change:

```powershell
claude mcp add --scope user pair-mesh -- localpilot mesh mcp --role claude
codex mcp add pair-mesh -- localpilot mesh mcp --role codex
```

The server's tools are `status`, `peek`, `ack`, `post`, `handoff`, `verdict`
and `evidence`. Each is one call to the same operation `localpilot mesh <op>`
runs, and returns what that command would print (`evidence` returns its JSON
packet, also as structured content). The role is fixed when the server
starts and is never a tool argument, so a client can only act as that
participant.

- `peek` presents mail under acknowledged delivery but does not acknowledge
  it; it is shown again, marked `REDELIVERED`, until `ack` or a post's
  `ack_through` acknowledges it.
- `verdict` answers the open `REVIEW_REQUEST`. The header (decision, round,
  blocking and important counts) is written from the findings. It is refused
  unless `reply_to` is the owner's latest request in the unit, the caller is
  a required reviewer that has not answered it yet, and its fingerprint
  manifest still holds. A finding may carry an `anchor` from `evidence`; see
  below.

No tool waits for mail, so a tool call never holds a host's turn open. Wait
with `localpilot mesh wait` in a background task.

#### Watching a session (`localpilot mesh cockpit`)

`localpilot mesh cockpit` is for the human watching a pair session. It is an
observer: it has no role and registers no delivery endpoint, and it never
writes the mailbox itself (its actions, below, each run a protocol command).
In a terminal it shows a full-screen view of the session,
re-read every half second: the session, its participants, the open review,
open waits (a message still owed replies, and by whom) and pauses, and an
activity list of everyone's recent records, each with its recipients
(`-> codex`) and the message it answers (`re claude:3`) so a thread can be
followed, one row per record, clipped at the pane's edge (Up, Down, Page Up, Page Down and Home scroll it; `q`, `Esc` or
Ctrl-C quits). If the session cannot be read, it says so rather than showing
"no session". It needs a build with
the terminal UI. `--json` prints one snapshot instead and exits. Either way
the snapshot shows:

- the owner, required reviewers and advisers, unit, phase and status;
- any pending handoff, waits and pauses;
- each participant's health, its unacknowledged mail and its latest records;
- the open review request with the verdicts that answer it. Only a verdict
  that names the request (`reply_to`) is shown as its answer; a verdict that
  names none, as a two-party session posts them, is listed as `unlinked`.

A snapshot is read from several files, so after reading it the whole session
record and every participant's health are read again. If any of it changed
in between, even within the same second, the snapshot is taken again, at
most three times; after that it says
`"consistent": false` and names what moved, rather than showing a mix of two
states. A part that cannot be read is listed in `read_errors`.

Each journal is read only in its last 256 KiB, so a snapshot costs the same
however long the session has run. A participant whose journal is longer says
`recent_truncated`, and if the open review request lies before that window,
`review_state` is `beyond_window` rather than a guess.

In the full-screen view, a key starts a human action:

| Key | Action | Command it runs |
|---|---|---|
| `s` | STOP, with a reason | `post --kind STOP` (with `--to` every other participant in a session started with `--with`, since only an ESCALATE may be broadcast) |
| `a` | answer an open QUESTION or ESCALATE (when several are open you pick one; there is no default), as a role it still waits on | `post --kind ANSWER` (with `--reply-to` in a session started with `--with`) |
| `h` / `d` / `w` | accept, decline or withdraw the pending handoff | `handoff-accept`, `handoff-decline`, `handoff-withdraw` |
| `p` / `r` | park the session, or resume a parked one | pair.py's `park` and `resume` |

Each action asks which participant to act as, every time and with no
default, then shows the exact command line it will run (program, arguments
and `--repo`) and runs it only after `y` or Enter (`Esc` cancels at any
step). Choices are numbered; a number is taken as soon as no longer one could
follow it, otherwise on Enter. The action line may take up to half the
screen; a command too long to show there in full cannot be confirmed until
the text is shortened or the terminal enlarged. The role's own rules apply: the cockpit adds no
authority, and a refused command is shown with its exit code and message.
Every command carries `--actor human`, recorded on the message or on the
session event. That is a claim made by whoever pressed the key, not an
identity. Participant operations run through `localpilot mesh` itself, so the
configured writer applies. Park and resume exist only in pair.py, so they
run the configured `delegate_command`; without one, the cockpit says so and
runs nothing. An action only offered for what the view shows (a pending
handoff, an open question) can lag the session by up to half a second.

#### Evidence for the other participants (`localpilot mesh evidence`)

`localpilot mesh evidence --role <r>` answers a participant of the active
session with read-only evidence about the shared tree, as one JSON packet on
stdout. Every packet names the session, the unit and the Git `HEAD` it was
taken at. Without version control, or when Git cannot resolve `HEAD`, `head`
is null and `head_unavailable` says why: the service never rescans the whole
tree for a digest.

```powershell
localpilot mesh --repo . evidence --role claude locate --query "fn parse" --glob "src/**"
localpilot mesh --repo . evidence --role claude anchor --path src/lib.rs --start 10 --end 14
localpilot mesh --repo . evidence --role claude verify --anchors '[{"path":"src/lib.rs","start":10,"end":14,"sha":"<64 hex>"}]'
localpilot mesh --repo . evidence --role claude diagnostics --path src/lib.rs
```

- `locate` finds a literal, or a regular expression with `--regex`, and
  returns each hit as an anchor with its line. It honours the tree's
  `.gitignore` files and `.git/info/exclude` (not your global excludes) and
  `.pairignore`, in the same grammar the no-VCS scan uses. It stops at
  2,000 files, 1 MiB per file, 64 MiB in total, 10 seconds or 200 hits, and
  says which limit it reached (`truncated`). Every file it did not search is
  named: `too_large`, `changed_during_read`, `unreadable`, and
  `unaddressable` for a name that is not valid Unicode (no anchor could
  address it). `complete` is true only when nothing was left out.
- `anchor` pins lines `start..end` of one file: SHA-256 of those lines as the
  file is now, with CRLF read as LF.
- `verify` checks anchors against the files as they are now, uncommitted
  changes included: `ok`, `moved` (the lines are exactly once elsewhere in
  the same file), `ambiguous` (in more than one place; none is chosen),
  `stale` (the lines are gone, or the file is), or `unknown` when the file
  could not be checked (a link, too large, changed while read, unreadable) or
  looking for moved lines would hash more than 64 MiB across the request. A
  hash anyone computed verifies the same way: a match proves the file now
  holds those lines, not who took the anchor or when. One request checks at
  most 100 anchors.
- `diagnostics` runs a fixed set of Git reads (`status`, and `diff --stat`
  against the unit's base) with no optional locks, no filesystem monitor and
  no external diff, within 10 seconds and 256 KiB each, and reports whether
  each `--path` exists. Without version control the Git reads are
  `unavailable`. No build or test command is run: that needs an OS sandbox.

A review finding can cite lines by an anchor. In a verdict (from `mesh run`
or the MCP `verdict` tool), a finding's optional `anchor` must name the
finding's own file; each is checked against the tree when the verdict is
written, and the finding shows the result, for example
`- src/lib.rs:10-14 [blocking] (anchor=moved:12-16) ...`. A finding with only
a file and line is posted as it always was, with no `anchor=` tag, so it is
never shown as checked. `mesh run` lists an anchor for each changed range in
the review brief it gives the model (at most 40), taken from the files as it
reads them, and asks the model to copy one rather than make one up.

The service never writes, and never reads a file through a link or
junction: a link found before opening is refused, one swapped in before the
open is caught on the file handle, and a directory on the way that became a
link while the file was read is reported as changed. Each file is read only
up to its limit, from the handle itself. It refuses the mailbox, `.git` and
secret-looking names such as `.env` or `id_rsa`, in any case and under any
other name the file system gives them (such as a Windows short name). That
name check is a filter, not a guarantee that no secret is ever read. A
refused request exits `1` with the reason on stderr.

#### Switching the writer

If the native writer misbehaves, hand every operation to the skill's own
`pair.py` instead, with no change to how the command is called:

```toml
[mesh]
writer = "delegate"
delegate_command = ["python", "C:/Users/me/.claude/skills/pair-programming/scripts/pair.py"]
```

Or for one terminal only, with the same array in the environment:
`LOCALPILOT_MESH__WRITER=delegate` and
`LOCALPILOT_MESH__DELEGATE_COMMAND='["python", "C:/…/pair.py"]'`.

`[mesh]` is read from your user config and the environment only. A
project's `.localpilot.toml` cannot set it, so a cloned repository cannot make
`localpilot mesh` run a program of its choosing.

`delegate_command` is the program and its arguments as a list. Each entry is
passed exactly, spaces included, with no shell. `localpilot mesh` adds
`--repo <anchor>` and then the operation's own arguments, and exits with the
delegate's exit code. Operations only a full implementation provides still
exit `2`. An empty `delegate_command`, or one that cannot start, exits `2`,
and a configuration that fails to load exits `1`: neither falls back to the
native writer.

Every session kind the skill starts is supported. A session started with
`--no-vcs` is owned through a content digest of the tree: `.pairignore`
excludes paths, links are recorded and never followed, and a file that
cannot be read fails the operation instead of being skipped. A session with
companion repositories is held to their declared write scopes
(`guard-write --path`), and a handoff pins every companion's state as well as
the anchor's.

## Project context files

Beyond `.localpilot.toml`, a project may carry free-text **instruction files**
that orient the agent with project conventions and constraints:

- **`Navigator.md`** — LocalPilot's own first-class instruction convention, and
  the **highest-precedence** instruction file when present;
- **`CLAUDE.md`** and **`AGENTS.md`** — the widely-shared agent-instruction
  conventions;
- **`.github/copilot-instructions.md`** — GitHub Copilot's convention, treated as
  a repo-root instruction (the lowest-precedence of the four);
- **`.github/instructions/*.instructions.md`** — GitHub Copilot's **path-scoped**
  convention: each file carries an `applyTo` glob in YAML frontmatter and reaches
  the model only when a file it is about is in play.

LocalPilot discovers them, resolves their `@`-imports, and merges them into one
ordered context document. That document is used **two** ways: it is **injected
directly into the turn context every turn** (bounded and redacted — see
`[context] inject_instructions` below), so a fresh checkout's instructions reach
the model immediately, *and* the learning engine ingests it as first-class
project knowledge (so retrieval can surface a convention on demand). The direct
injection is ungated and independent of learning; the ingest path stays
review-gated (ADR-0056).

**Discovery.** Three layers are collected:

- **repo-root** — `Navigator.md` / `CLAUDE.md` / `AGENTS.md`,
  `.github/copilot-instructions.md`, and every
  `.github/instructions/*.instructions.md` at the workspace root;
- **nested** — `Navigator.md` / `CLAUDE.md` / `AGENTS.md` in subdirectories of
  the workspace (the walk honours ignore files and is depth-bounded);
- **global** — `Navigator.md` / `CLAUDE.md` / `AGENTS.md` under the per-user
  `~/.localpilot/` directory (resolved cross-platform from the home directory).

**Precedence** (most → least specific): **repo-root > nested directory >
global**, and within one tier by **instruction kind** (`Navigator.md` >
`CLAUDE.md` > `AGENTS.md` > `copilot-instructions.md` > path-scoped
`*.instructions.md`). The workspace-root files
are the authoritative project instructions and lead the merge; nested-directory
files refine within their subtree and follow (ordered by ascending directory
depth, then kind, then path, for determinism); the per-user global files are the
baseline and come last.

**Path scoping (`applyTo`).** A `.github/instructions/*.instructions.md` file may
narrow itself to the files it is about:

```markdown
---
applyTo: "**/*.ts"
---
Prefer named exports.
```

`applyTo` takes one glob, a comma-separated list, or a YAML list. The file is
injected only on turns where a file **in play** matches — the workspace files the
session has touched through tool calls, plus any workspace file the prompt names
outright, so a rule can apply on the very first turn. A scoped file whose
frontmatter omits `applyTo` (or leaves it empty) applies project-wide, like the
unscoped kinds. Unscoped instruction files are never filtered.

Only the **repo-root** `.github/` directory is read for these. `.github` is a
repo-level directory — GitHub reads only the one at the root — and per-directory
instructions already work through the nested `Navigator.md` / `CLAUDE.md` /
`AGENTS.md` walk. See ADR-0119.

**`@`-imports.** A line whose trimmed text is exactly `@<path>` imports that
file's body inline at that point (relative paths resolve against the importing
file's directory; an absolute path is used as-is). Imports may nest; resolution
is bounded by a maximum depth and guarded against cycles, so the merged output is
always finite and deterministic. A missing or unreadable import, or one past the
depth bound or in a cycle, is replaced by a short marker comment rather than
failing discovery. A prose `@mention` (with surrounding text on the line) is not
an import directive.

**Ingestion.** Folder ingestion (`localpilot ingest run` / `refresh`, and the
session-open background build) captures the merged document as a derived chunk
under a synthetic `<project-context>` path, distinct from the raw files, so
`knowledge_search` surfaces project conventions even when the source files are
large or scattered. The merged context is derived, disposable state under
`.localmind/ingest/` like any other ingested knowledge (ADR-0013); it is never
written to accepted memory without review.

**Semantic ingest.** When an embedding model is configured for the project (the
`[inference]` embedding endpoint accepted memory uses — i.e. `embedding_base_url`
+ `embedding_model` in `.localmind.toml`, the local CPU embed server), ingested
chunks are also embedded into a chunk vector index and `knowledge_search` becomes
hybrid keyword+vector retrieval: a semantically-relevant chunk the keyword query
missed is recalled, while keyword hits stay the floor (a strong keyword hit always
surfaces). Embedding is best-effort — a down or unconfigured endpoint writes no
vectors, never fails ingest, and leaves retrieval byte-identical to the
keyword-only path. With no embedding model configured this is entirely inert.
`ingest run`/`refresh` report `embedded: N of M chunks` when embeddings are
active. To keep accepted-memory embeddings but skip the per-chunk ingest embedding
cost, set `[ingest] embed_chunks = false` (default `true`); retrieval then stays
keyword-only. `ingest preview` reports a fresh-run estimate of the primary chunk
embedding calls (`estimates.model_calls` in the manifest). Set `[ingest]
max_model_calls` to a nonzero hard per-run cap; the run stops before the next
call and reports how many were used. The default `0` explicitly means unlimited.
During a capped run the secondary documentation-index bridge still stores
Markdown text but skips its own embeddings, so it cannot escape the cap. See
[10-decisions.md](10-decisions.md) (ADR-0025) and LocalMind D-LM-0022.

**Documentation index.** Each ingest run also bridges the workspace's Markdown
files into LocalMind's documentation index (`doc_chunk`), so `localmind ui`'s
Docs tab can browse and semantically search them — without this bridge, only
`localmind ingest docs` populates that tab. Content is redacted before storage
(the same scrub every persisted chunk gets), unchanged files are a no-op on
later runs, and files that vanish are removed from the index. Best-effort
throughout: a doc-index failure never fails the ingest run. Doc-chunk
embeddings follow `[ingest] embed_chunks` — with it off, docs stay browsable
but not semantically searchable. Set `[ingest] docs_index = false` (default
`true`) to keep folder ingest purely derived-state.

**Store location.** `localpilot learning` and `localpilot memory` find the
LocalMind store by walking up from the current directory to the nearest ancestor
holding `.localmind` (git-style), so a subdirectory answers from the project's
store rather than a second empty one. `--workspace <path>` pins the root
explicitly. The full contract — including the read-only, never-create search
behaviour and the three distinguished empty states — is in
[`localmind-integration.md`](localmind-integration.md#store-resolution).

**Search output format.** `learning search` and `memory search` choose their
output format from context (ADR-0048), so a program reading the output gets a
machine-readable form without having to know a flag exists:

- **stdout is not a terminal** (piped or redirected) → a **JSON array** by
  default (`memory_id`, `score`, `path`, `snippet`, `category`);
- **stdout is a terminal** → the **human table**, plus a one-line stderr hint
  pointing at the structured form;
- **`--format human|json`** overrides either way (with `--json` as an alias for
  `--format json`): `--format human` forces the table even when piped, `--format
  json` forces JSON on a terminal.

Stdout stays script-stable in every case — an empty result is a valid empty JSON
array, and all diagnostics (the format hint, the store-resolution and empty-state
lines) go to stderr.

## Reference

### `[mesh]`

Which implementation `localpilot mesh` uses to write the pair-programming
mailbox. User config and environment only: this table is ignored in a
project's `.localpilot.toml`. See [Switching the writer](#switching-the-writer).

| Key | Type | Default | Description |
|---|---|---:|---|
| `writer` | `native` \| `delegate` | `native` | `native` is LocalPilot's own implementation. `delegate` hands every participant operation to `delegate_command`. |
| `delegate_command` | array of strings | `[]` | The delegate's argv: program, then arguments, each passed exactly (no shell). Required when `writer = "delegate"`. |

### `[lab]`

The lesson lab's opt-in tiers. Logic validation needs no setting: it replays
what a run recorded and starts nothing.

| Key | Type | Default | Description |
|---|---|---:|---|
| `replay` | bool | `false` | Allow `localpilot lab replay` to run a lesson's ratified check on the commits it came from, in temporary worktrees. Honoured only from the project's **committed** `.localpilot.toml` — the same boundary that ratifies a check — and ignored when the working copy differs. Every run is previewed and needs its own confirmation (`--yes` runs headless under the permission engine's headless rules). |

```toml
[lab]
replay = true
```

### `[terminal]`

Full-screen timeline presentation preferences. An absent block preserves the
current compact layout.

| Key | Type | Default | Description |
|---|---|---:|---|
| `density` | `compact` \| `comfortable` | `compact` | `compact` keeps adjacent tool activity tight; `comfortable` inserts one optional spacer between adjacent tool rows. Both retain the required separator before assistant or reasoning text. `/settings` can change the value for the current session without rewriting the config file. |
| `group_successful_tools` | bool | `false` | When true, three or more consecutive successful tools collapse into one expandable presentation row. Any other timeline item or tool outcome ends the run. The original tools remain searchable, exportable, and retained; `/settings` can toggle grouping for the current session. |

```toml
[terminal]
density = "comfortable"
group_successful_tools = true
```

### `[provider]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `default` | string | `"local"` | Id of the provider used when `--provider` is omitted |

### `[providers.<id>]`

One table per provider. `<id>` is the name referenced by `[provider].default`
and `--provider`.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `kind` | string | — | `openai`, `openai-compatible` (alias `local`), `anthropic`, `custom`, or `google-vertex-openai` |
| `auth` | string | `api_key` | Authentication mode: `api_key` or `google_adc` |
| `base_url` | string | per kind | API base URL (required for local/custom) |
| `api_key_env` | string | none | Name of the env var holding the credential (never the value) |
| `google_project` | string | none | Google Cloud project id for `google-vertex-openai` when `base_url` is omitted |
| `google_location` | string | none | Vertex AI location for `google-vertex-openai` when `base_url` is omitted |
| `google_adc_path` | string | ADC default | Optional path to a gcloud ADC `authorized_user` file |
| `model` | string | none | Default model when a command does not pass `--model` |
| `request_timeout_secs` | int | 600 | Stall window: longest tolerated silence on an open response (to first byte, then between stream chunks) — not a total deadline; a server that keeps streaming is never cut off (ADR-0080) |
| `context_window` | int | none | Optional context-window cap in tokens. Every session host resolves the served per-slot window from model-routed `GET /props` (`default_generation_settings.n_ctx`), then the matching `/v1/models` entry (`meta.n_ctx`, then legacy context fields). A positive configured cap selects the smaller window. Training context (`n_ctx_train`) is ignored. If discovery fails, use this configured window; without either signal, retain `[harness] context_token_limit` as the existing prompt budget. Known windows reserve the provider output cap before calculating the prompt budget (ADR-0194). |
| `supports_vision` | bool | none | Whether this provider's model accepts image (vision) input. A user assertion that resolves the model's vision capability (config > probe > false, ADR-0061): `true` lifts the image-input gate even for a local server; unset/`false` keeps the text-only default. LocalBox sets this automatically when it loads a multimodal projector. See [04-provider-contract.md](04-provider-contract.md) §Vision. |
| `prompt_caching` | bool | `false` | Enable prompt caching for an Anthropic provider: place an ephemeral `cache_control` breakpoint on the stable prefix (tools + the stable system prompt) so it is cached across turns, cutting the input cost/latency of a multi-turn session. Off by default; only pays off against a backend that implements prompt caching (hosted Claude or a proxy that does) and is harmless otherwise. The per-turn volatile context (memory, project instructions) stays after the breakpoint and is re-sent each turn. Cached tokens show as `cached:N` in the footer. |

Any other keys under a provider table are preserved and passed through as
provider options (for example `max_tokens` for `anthropic`, or the
LocalPilot-owned switches `suppress_thinking` and `reasoning_round_trip`). See
[providers.md](providers.md).

**Credentials are never config keys.** For `auth = "api_key"`, a provider's API
key is never written to config. It is resolved at use with precedence: a stored
credential (OS keychain → `0600` fallback file, written by `localpilot login`) →
the `api_key_env` environment variable → none. So `login` makes `api_key_env`
optional. For `auth = "google_adc"`, LocalPilot reads the ADC file path (not the
token value) and mints a short-lived Google OAuth access token in-process. The OS
keychain backend is an opt-in build feature (`keychain`, Windows only at present;
macOS/Linux use the fallback file for API-key credentials — ADR-0042). See
[providers.md](providers.md) §Storing credentials and
[07-security-and-privacy.md](07-security-and-privacy.md) §Stored API Credentials.

### Swarm model selection

`localpilot swarm run <plan>` runs a task plan as a swarm of agents, and each
worker can run on its own model. A plan file is JSON:

```json
{
  "objective": "make the suite green",
  "mode": "light",
  "nodes": [
    { "title": "survey", "prompt": "Find every failing test." },
    { "title": "fix", "prompt": "Fix them.", "model": "careful-model", "depends_on_batch": [0] }
  ]
}
```

- `objective` (required) travels into every worker's assignment; `mode` is
  `light` (default) or `deep`; each `nodes` entry is a task with a `title`,
  `prompt`, optional `kind` (`task` or `gate`), optional `depends_on_batch`
  (indices of earlier nodes it waits on), and an optional **`model`**.
- A node's `model` selects the model that task's worker runs on. Omit it to use
  the run's default (from `--model`, or the default provider's configured
  `model`).
- **A node's model must be advertised by a configured provider** — i.e. some
  `[providers.<id>]` sets `model = "<that model>"`. Configure one provider per
  model you intend to pin. Different nodes may then target models served by
  different providers, and each worker is built on the provider that serves its
  model.
- **A model no configured provider advertises is refused before the worker is
  built** — the run fails that node loudly rather than silently falling back to
  the default. This is deliberate: a pinned model that quietly ran on something
  else would produce work that reads normally and never says so.

**Bounding the fan-out.** `--concurrency N` (default 4) caps how many workers —
and so how many models — run at once; that is the RAM/VRAM bound, since each
running worker holds one model. `--max-agents M` (default 64) is a lifetime cap
on how many workers one run may start (a runaway guard that counts finished
workers too). These bound resources, not tokens or spend.

### `[discovery]`

Best-effort, read-only metadata LocalPilot reads from a configured server at
discovery time (it never runs model inference).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `context_probe` | bool | `true` | Read served context metadata from configured endpoints at session startup and in doctor/models diagnostics. Probes are cached per process/provider/endpoint/model, including failures, with a two-second deadline for the entire sequence. Local/OpenAI-compatible endpoints receive model-routed `/props?model=…&autoload=false`, then `/v1/models` if necessary. No inference or automatic model loading is requested. Disable to use configured windows/default budgets without context-probe requests; independent of `vision_probe` (ADR-0194). |
| `vision_probe` | bool | `true` | Probe a local server's read-only llama.cpp `GET /props` endpoint for vision (multimodal projector) support, so an undeclared but vision-capable server resolves its capability without a hand edit (ADR-0061). Best-effort: an unreachable or signal-less server is treated as unknown (no vision), never a false claim, and an explicit `supports_vision` always wins. Set `false` to never probe. |

### `[harness]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `agent` \| `harness` | `agent` | Default operating-mode label. Reserved: the active mode is selected by the subcommand (`localpilot harness …` runs harness mode; `chat`/`print`/`eval` run agent mode); this key does not gate behaviour on its own. |
| `attempts_per_step` | int | `3` | Max attempts per plan step |
| `auto_commit` | bool | `true` | Commit each completed step |
| `test_command` | string | none | Command run to gate step completion |
| `tool_call_budget` | int | off | Soft start for the per-turn tool-call ceiling. A turn making forward progress runs past this up to `tool_call_budget_max`; a turn detected as making no progress stops here. Unset by default; set it to enable a progress-aware budget |
| `tool_call_budget_max` | int | built-in | Hard cost ceiling: the per-turn tool-call count that always stops the loop, regardless of progress. Setting either budget field enables an explicit budget (and an explicit value always wins). **When both are unset a conservative built-in ceiling applies** so a fresh project never runs an unbounded, externally-killed loop (a safety default; ADR-0055): **200** tool calls for a headless run (`eval`/`print`/`harness` step) and **500** for an interactive session. Raise it above `tool_call_budget` to let a productive turn extend |
| `claim_gate` | `off` \| `warn` | `off` | The no-unsupported-claim gate over the final reply. `warn` appends a visible, non-destructive note to a completed-action claim no verified tool call this turn supports (matched per claim); `off` skips it. Default `off` while its false-positive rate is measured (ADR-0023) |
| `turn_timeout_secs` | int | built-in (headless) | Bounded per-turn wall-clock timeout in seconds. When set, a turn that runs longer stops cleanly with a parseable `handoff:` summary instead of hanging — the bound a non-interactive caller (`print`) relies on. An explicit value always wins. **When unset, a headless run (`eval`/`print`/`harness` step) applies a conservative built-in bound of 600 s** so it self-bounds without a human watching (ADR-0055); an interactive session sets no default wall-clock — a long interactive turn is legitimate and the user can cancel it. To run a headless turn without a wall-clock bound, set an explicit large value |
| `verify_before_done` | bool | `false` | Verify the workspace builds/tests before a turn finalizes. When on, a turn that would end with no tool call first runs a verification command; on failure the diagnostics are fed back and the loop continues (bounded by the budget/timeout rails plus a fixed re-entry cap) instead of "finishing" code that never compiled. The config default is `false`, so interactive and `print` turns are unchanged; **`localpilot eval` defaults it on** (opt out with `localpilot eval --no-verify`, which reproduces the pre-default behaviour) so a benchmark measures compiled+tested solves. A workspace with no detectable target finalizes unchanged (with a warning). See [06-harness-spec.md](06-harness-spec.md) §Verify-Before-Done Gate (ADR-0054/0058) |
| `verify_command` | string | none | Override the verify-before-done command (a single command line, split on whitespace — no shell, like `test_command`). Unset resolves a command from the workspace stack (`cargo test`, `go test ./...`, `npm test`, `python -m pytest`, `mvn test`, `gradle test`, `make`, or — for C++ sources at the root — `g++ -std=c++17 -I. -fsyntax-only <sources>`); set this for a non-standard build/test invocation, or for a stack the detector does not cover (e.g. a CMake project whose sources are not at the root, where a full `ctest` run is wanted) |
| `rules.<name>` | `off` \| `warn` \| `block` \| `discard` | — | Per-rule severity overrides. `discard` escalates a rule's actionable (`retry`) failures to the anti-sunk-cost discard rung: the attempt is abandoned and the working tree restored to committed state before a fresh attempt (e.g. `quality_gate = "discard"`). Rule-level only — a per-check `severity = "discard"` is rejected at load |
| `guidance.enabled` | bool | `false` | Pre-brief guidance gate for `harness intake`: assess the idea's decision axes and pause below the threshold instead of writing a brief that encodes guesses. `--guidance` / `--no-guidance` override per run. The score is an inspectable signal (the full axis list is recorded in `.localpilot/intake.jsonl`), not proof the idea is fully specified — an axis the model never lists cannot count against it. See [06-harness-spec.md](06-harness-spec.md) §`localpilot harness intake` |
| `guidance.threshold` | float | `0.7` | Minimum guidance score (resolved axes ÷ total; `1.0` when none are found) that proceeds straight to a brief. Clamped to `0..=1` at use |
| `guidance.max_questions` | int | `5` | Cap on clarification questions per intake run; the most consequential open axes ask first. Floored at 1 |

Notable rule key:

| Rule | Default | Meaning |
| --- | --- | --- |
| `check_before_launch` | `warn` | When the task prompt named a local serveable target (a loopback host, or any `host:port` with an explicit port) that has not been probed this session, an attempt to launch a local server or scaffold a competing `index.html` is nudged (`warn`, the call still runs), refused (`block`), or ignored (`off`). Auto-extracted from the prompt — an external reference URL without a port is not a target. Advisory, tighten-only, best-effort. See [06-harness-spec.md](06-harness-spec.md). |

### `[context]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `project_analysis` | bool | `true` | Inject a compact, read-only project-facts block before each turn. LocalPilot derives it from manifests, lockfiles, package/dependency names, scripts, and common entrypoint markers so the model reuses existing project structure before inventing alternatives. |
| `inject_instructions` | bool | `true` | Inject the project's instruction files (`Navigator.md`/`CLAUDE.md`/`AGENTS.md`/`.github/copilot-instructions.md`, merged in precedence order) directly into the turn context every turn — ungated and independent of the review-gated learning store, so a fresh project's instructions reach the model even with learning off. Redacted before injection and bounded by `instruction_char_budget`. Set `false` to opt out (the ingest path still surfaces them via retrieval). See [06-harness-spec.md](06-harness-spec.md) and ADR-0056. |
| `instruction_char_budget` | int | `8000` | Maximum characters of merged instruction text injected per turn. Over the budget the text is truncated with a visible marker rather than dropped silently, so a large instruction set cannot crowd out the per-turn token budget. |

### `[ingest]`

Folder ingestion (`localpilot ingest run` / `refresh`, and the session-open
background build). The retrieval-facing controls are below. Endpoint failures
remain best-effort and never fail an ingest run; reaching an explicit nonzero
model-call cap stops the run with a typed, actionable error. The full semantics
are under §Project context files above.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `embed_chunks` | bool | `true` | Embed each ingested chunk into the chunk vector index when an embedding model is configured (the `[inference]` embedding endpoint accepted memory uses, in `.localmind.toml`), making `knowledge_search` hybrid keyword+vector. Only ever active with an embedding model configured — otherwise a no-op and ingest stays keyword-only. Set `false` to keep accepted-memory embeddings while skipping the per-chunk ingest embedding cost; retrieval then stays keyword-only. Doc-chunk embeddings follow this flag too. |
| `max_model_calls` | int | `0` | Hard per-run cap on primary project-knowledge chunk embedding calls, including the merged project context. `0` means unlimited. A capped run stops before call `limit + 1` with an actionable error and leaves the incomplete path retryable. The secondary documentation-index bridge still stores Markdown text but skips doc embeddings during capped runs so the cap remains hard. |
| `docs_index` | bool | `true` | Bridge the workspace's Markdown files into LocalMind's documentation index (`doc_chunk`) on each ingest run, so `localmind ui`'s Docs tab can browse and semantically search them. Content is redacted before storage (the same scrub every persisted chunk gets); unchanged files are a no-op on later runs and files that vanish are removed from the index. Set `false` to keep folder ingest purely derived-state. |

### `[memory]`

Tunes always-on accepted-memory injection. Every keyword-path default preserves
the prior fixed behaviour; the one default-on lever is the semantic relevance
gate (`injection_min_cosine`), which is best-effort — inert unless an embedding
endpoint is configured.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `injection_min_score` | int | `0` | Minimum retrieval score a memory must clear to be injected. `0` injects every match (prior behaviour); raise it so weak matches do not fill the per-turn budget. |
| `injection_min_cosine` | float | `0.36` | **Semantic relevance gate.** Minimum normalized cosine (prompt ↔ lesson, over the stored embedding vectors) a memory must clear to be injected, so a same-language but off-topic lesson cannot inject into an unrelated task. Unlike the unnormalized `injection_min_score`, cosine is normalized and portable, so this ships **default-on**. **Best-effort:** with no embedding endpoint (or an unembedded lesson) the memory carries no cosine and is injected exactly as on the keyword path — a no-embed run is byte-identical. `0.0` disables. The keyword bm25 search stays the candidate floor; cosine only re-filters it. **A hit containing the query's subject — its rarest term — is exempt:** it already earned its place lexically, and a floor high enough to drop off-topic noise is also high enough to drop a real answer phrased at a distance. The default is calibrated against a real store, where prompts whose subject appears in no memory retrieve an irrelevant one at cosines of 0.27–0.35. |
| `injection_char_budget` | int | `1200` | Char budget for the injected accepted-memory block, and the ceiling when `injection_context_aware` scales it down. |
| `injection_context_aware` | bool | `false` | Scale the injected budget toward the default provider's declared `context_window` (a small model gets less), never above `injection_char_budget`. |
| `injection_skip_categories` | list | `[]` | Lesson categories to skip injecting because a rule already enforces equivalent guidance (e.g. `["SecurityWarning"]`). Values match `LessonCategory` names. |
| `injection_language_filter` | bool | `true` | Skip an accepted memory clearly about a different programming language than the workspace's; a language-agnostic lesson stays eligible. |
| `injection_dedup_ttl_turns` | int | `0` | Suppress re-injecting an accepted memory that was already injected within the last N turns, so a persistently-relevant lesson does not spend the per-turn budget every turn and crowd out other memory. `0` disables (the default) — a memory may be injected on consecutive turns. Dropping a suppressed memory keeps it out of both the injected block and the memories-used audit. |
| `outcome_downweight` | bool | `false` | **Outcome-aware down-weight (reserved; engine built, not yet wired to a live eval).** The intent: when an uplift A/B eval shows a lesson coincided with an arm under-performing its control, route that lesson to review (never delete it). The down-weight engine and this default-off lever exist, but no in-repo uplift-eval runner reads the key yet, so it is currently inert. Off by default: a single eval is a weak signal, and the action must stay reversible. |

### `[docs]`

Controls when the agent should expand beyond local project facts into available
knowledge, docs, MCP, or tool-discovery surfaces. This does not grant any new
permission: network and MCP/tool calls still pass through the normal permission
engine.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `lookup_policy` | `local_only` \| `evidence` \| `proactive` | `evidence` | `local_only` keeps the model within repo/context unless the user asks for external information. `evidence` starts local and looks up docs/tools when local facts are insufficient, ambiguous, or a local attempt fails. `proactive` nudges the model to use available docs/MCP/tool discovery early for package or framework work. |

### `[compaction]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `deterministic` \| `smart_with_fallback` | `deterministic` | Runtime context compaction mode. Either way, the digest injected into the conversation is the finalized semantic one (goal, decisions, per-file operations, command outcomes) — budgeted to fit and ordered by recency — not a bullet placeholder, and an automatic compaction posts a host-visible notice (ADR-0161). `smart_with_fallback` keeps deterministic compaction as the completed-only fallback when no validated summarizer backend is available |
| `summary_token_limit` | int | `1024` | Target maximum size for rendered compact summaries |
| `summarizer_input_tokens` | int | `8192` | Reserved input budget for model-backed summarization when enabled |
| `summarizer_timeout_secs` | int | `20` | Timeout budget for a future model-backed summarizer call |

### `[permissions]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `profile` | `default` \| `relaxed` \| `readonly` \| `bypass` \| `unrestricted` | `default` | Permission profile. `bypass` and `unrestricted` are never the default and are always surfaced. `unrestricted` approves everything — out-of-workspace paths included — with no prompts; the user accepts full responsibility. `readonly` denies every write and every command above read-only, with no prompt |
| `extra_read_roots` | array of string | `[]` | Absolute directories granted standing **read** scope in addition to the workspace, in every profile including non-interactive runs. Writes keep the workspace boundary; secret-like reads keep their gate. A missing directory is reported and skipped at startup |
| `scratch_root` | bool or string | `true` | **User config and environment only.** `true` creates a unique per-session child under OS temp; `false` disables it; an absolute existing directory selects a custom parent. Only the owned child gets scratch scope and cleanup; the parent is never granted or removed. Failed creation warns and grants nothing. Ordinary scratch file reads/writes pass in trusted default/bypass headless sessions; secret-like names and session restrictions keep their gates |
| `allow_commands` | array of `{ program, args_prefix }` | `[]` | **User config and environment only**; a project file that sets it is refused. A structured `run_shell` call (`program` + `args`, never a free-text `command`) whose program equals `program` and whose arguments start with `args_prefix` passes the command gate in `default`, `relaxed` and `readonly`, and runs headless. Exact string match. An entry admits every call that starts with it, so name a script's absolute path in `args_prefix`. The untrusted-workspace and incognito floors still apply. `localpilot doctor` lists the entries |

```toml
# User config only: lets a readonly pair navigator run the mailbox script.
[permissions]
profile = "readonly"

[[permissions.allow_commands]]
program = "python"
args_prefix = ["D:/repos/c0degeek-ai/skills/pair-programming/scripts/pair.py"]
```

Scratch configuration belongs in the user file, for example:

```toml
[permissions]
scratch_root = true # OS temp; false disables it
# scratch_root = 'D:/private-temp' # existing user-private parent
```

Every interactive, print, harness, server and delegated session creates its own
child. The system prompt reports the actual path, and shell children receive
`LOCALPILOT_SCRATCH_DIR` plus `TEMP`/`TMP`/`TMPDIR`. File tools accept that concrete
path. Literal shell targets use the same boundary; opaque scripts require an
interactive approval or an exact structured command grant, including under
bypass. Structured `run_background` starts also use `allow_commands`.
Inspectable external paths remain gated even with a command grant. The root is
released at session teardown; crashes or filesystem cleanup failures can leave
orphan directories. See [security and privacy](07-security-and-privacy.md).

### `[quota]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `auto_resume` | `off` \| `ask` \| `run` \| `global` | `off` | When to resume a quota-paused run |
| `max_wait_minutes` | int | `360` | Cap on how long to wait before resuming |
| `resume_requires_clean_workspace` | bool | `true` | Refuse to resume with a dirty tree |
| `resume_requires_no_pending_approval` | bool | `true` | Refuse to resume through a pending approval |
| `resume_only_at_step_boundary` | bool | `true` | Resume only between steps |

### `[mcp.servers.<name>]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `command` | string | — | Command that launches the MCP server |
| `args` | array of string | `[]` | Arguments to the command |
| `env` | table | `{}` | Environment entries overlaid on the inherited environment when the server is spawned. See below. |

See [mcp.md](mcp.md).

### `[mcp.servers.<name>.env]`

Each entry sets one environment variable for that server's process. The server
starts from the environment LocalPilot itself has; a configured entry then
replaces any inherited variable of the same name. Omit the table entirely and
inheritance behaves exactly as before.

There are three entry forms:

```toml
[mcp.servers.ask_google.env]
# 1. A plain string: an ordinary, non-sensitive value.
LOG_LEVEL = "info"

# 2. A reference to the credential store — the recommended way to supply a
#    credential. The config file holds only the alias.
GOOGLE_API_KEY = { credential = "google-api-key" }

# 3. A credential written literally into this file. An explicit escape hatch,
#    not the recommended path.
SERVICE_TOKEN = { value = "..." }
```

> **Do not put a credential in the plain-string form.** A plain string is treated
> as non-sensitive: it is not filtered out of what the server sends back. Use
> `{ credential = "..." }`, or `{ value = "..." }` if you must keep it in the
> file. Both object forms are masked in diagnostics and stripped from the
> server's responses; see [07-security-and-privacy.md](07-security-and-privacy.md).

Store a credential with `localpilot credential set <name>` (below) and reference
it by that name. If a referenced credential is not stored, that server is **not
started** and `localpilot doctor` reports which variable and alias need
attention.

Rules the loader enforces:

- Variable names must be a letter or underscore followed by letters, digits, or
  underscores.
- An entry may not set both `value` and `credential`, or neither. This is checked
  **after** config layers are merged, so two individually valid files cannot
  combine into a contradictory entry.
- Two names in one server that differ only by case are rejected. Windows treats
  them as one variable and Linux/macOS as two, so the configuration would mean
  different things on different platforms.
- Credential aliases may contain letters, digits, `.`, `-`, and `_`.

Config layers merge **per key**: an `env` table in the project file overrides
only the entries it names, leaving the user file's other entries in place.

> **Caveat on the environment layer.** An entry set through
> `LOCALPILOT_MCP__SERVERS__<server>__ENV__<VAR>` arrives **lower-cased**, so
> exporting `…__ENV__GOOGLE_API_KEY` sets `google_api_key`. On Windows that is
> the same variable; on Linux and macOS it is a different one. Prefer setting
> per-server environments in a config file.

### `localpilot credential`

Named credentials an `[mcp.servers.<name>.env]` entry references. These are
separate from provider API keys — `localpilot login` and `localpilot logout` are
unchanged, and a generic credential and a provider of the same name never
collide.

| Command | Effect |
| --- | --- |
| `localpilot credential set <name>` | Read one value from stdin and store it in the OS keychain, or a `0600` fallback file. The value is never taken as a command-line argument and never printed in full. |
| `localpilot credential list` | List stored credential names and where each is stored (`keychain` or `file`). Never shows values. |
| `localpilot credential delete <name>` | Remove the credential from every storage tier. |

There is deliberately no command to reveal, export, or copy a stored value.

### `[skills]`

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `autonomous_discovery` | bool | `false` | Register the `skill_list`/`skill_search`/`skill_load` tools so the model may list, discover, and read the installed SKILL.md package catalog on its own (the user-global baseline plus this workspace's trusted project overlay), **and** allow the skill-discovery lane to load a relevant, model-discoverable *installed* skill into a `/research` run (ADR-0099). Off by default, so a small local model never auto-injects a skill; an available/discovered match or a user-only skill stays report-only regardless. The deterministic `localpilot skills list \| show \| research` surface works regardless. |

Installed skill packages are advisory prompt modules under `.localpilot/skills/`
or `.agents/skills/`. The effective catalog is the user-global baseline
(`~/.localpilot/skills`, `~/.agents/skills`) — always present — overlaid by this
workspace's trusted project directories; an untrusted workspace contributes no
project overlay. See [05-tool-system.md](05-tool-system.md) §Project Skill
Discovery.

`localpilot doctor` reports a `skills:` block: whether `autonomous_discovery` is
on or off, the readable discoverable-package count and the hidden user-only
count. When the project overlay is not included it says which case applies —
`project overlay hidden (workspace untrusted)` for a confidently untrusted
workspace, or `project overlay hidden (workspace trust could not be evaluated)`
when trust is unknown (never a confident "untrusted"). In both cases only the
user-global baseline is counted and no project manifest is read. A catalog that
cannot be scanned at all is reported as `unreadable`, distinct from a real empty
`0`; and when some package entries are malformed the block adds `package entries
skipped as unreadable: N` (a count only) so a `0` is never mistaken for a clean
empty catalog. The full-screen `/settings` view shows a static `Installed
package discovery: on/off` row (config-only; it performs no catalog scan) with,
when off, guidance to `/skills list` or the `autonomous_discovery` switch.

### `[tools]`

The pull-discovery broker (ADR-0031): narrow each turn's advertised tool schemas
to a small working set and resolve a need to the right tool on demand. Every key
defaults so an absent `[tools]` block reproduces prior behaviour exactly — the
broker is off and the full tool set is advertised.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `broker` | bool | `false` | Enable the broker. Off advertises the full registry (the rollback path); on narrows advertised schemas to the working set and resolves/reveals on a miss. |
| `core` | array of string | `[]` | The core working set always advertised when the broker is on. Empty uses the built-in default (a lean read/edit/search/shell set). |
| `working_set_cap` | int | `24` | Maximum revealed tools retained before LRU eviction. |
| `score_floor` | int | `1` | Minimum resolution score to reveal; below it a miss is a clean "no match". |
| `marker` | bool | `false` | Enable the loose `NEED: <capability>` marker trigger. Off by default; the always-on failure-driven trigger does not need it. |
| `learning` | bool | `false` | Re-rank by past success, graduate hot tools into the always-advertised set, and record redacted resolution telemetry. Off keeps the broker working with mechanical freshness only. |
| `graduation_threshold` | int | `3` | Reveals of one tool before it graduates into the always-advertised set (when `learning`). |
| `readable_errors` | bool | `true` | When a tool call's arguments do not match the tool schema, hand the model a concise, schema-aware error (the offending field, the expected shape, and a valid example) instead of the raw deserializer string, so it can self-correct on the next turn. Set `false` to restore the raw message (the rollback). The raw detail is always kept in the logs/telemetry. |
| `repair` | `off`/`warn`/`on` | `off` | Conservative, schema-guided repair of a shape-invalid call's arguments (wrap a bare string as a one-element array, parse a stringified array/object of the right item type, unwrap a markdown autolink on a path field) — applied **only** on read-only / project-write tools, **never** on a destructive/external-write/irreversible/MCP tool or a content/command field, and only to the validator-reported fields. A repaired call carries a model-visible note and runs through the permission engine on the repaired input. `warn` applies the repair and logs it loudly (for vetting before any default change to `on`); `off` (the default) never rewrites arguments — a shape-invalid call gets the readable error instead. |
| `repair_learning` | bool | `false` | Offer the session's argument-repair patterns to LocalMind as aggregate, redacted, **review-gated** candidates at session close (which model needed which repair on which tool). Reuse-only: it stores no raw inputs/paths/content, writes no accepted memory, and adds no new store — a human promotes a candidate or it expires in review. Off by default. |
| `elide_seen_reads` | bool | `false` | Elide a `read_file` result whose file+range was already read this session and is unchanged since (same mtime **and** length): return a compact stub pointing at the earlier read instead of the full body, cutting context waste on read-heavy loops. Conservative — a changed file, a coarse-mtime same-length overwrite, or any unreadable stat always returns full content, never a stale stub; the model can re-page any range with `read_file` start_line/end_line. The elided read still records as a successful `read_file`, so nothing that depends on "was this read" changes. Off by default; in-memory, so a resumed session serves full content until it re-reads. |

**Migration:** these defaults reproduce prior behaviour, so an existing config
keeps working unchanged. Opt in with `[tools] broker = true`; see
[05-tool-system.md](05-tool-system.md) §Pull-Discovery Broker.

**Prefill lever.** With the broker off, every builtin tool's full JSON schema is
re-sent each turn — the dominant per-turn prefill weight. Turning the broker on
narrows the advertised set to a lean working set and reveals the rest on demand,
cutting that prefill substantially (measured ~35% of the tool-schema bytes on the
default builtin set — 21→12 advertised tools), which leaves more of a fixed token
budget for actual work. This is the recommended lever for a headless/benchmark run
under a tight budget: enable it per run with `[tools] broker = true`. Defaulting
the broker **on** is deferred pending a corpus ablation that measures the
solve-rate effect, not just the token saving. (The per-turn system prompt is
already small — under ~1k tokens — and compaction trims to the live transcript
rather than padding to `context_token_limit`, so the tool schemas are the lever
worth pulling.)

`readable_errors` defaults **on** — a pure message improvement with no behaviour
change beyond the text the model reads — so a shape-invalid tool call is answered
with an actionable, schema-aware correction by default.

### `[history]`

Durable prompt history for the interactive `chat` composer: submitted prompts are
persisted so Up/Down recall survives a restart (ADR-0040).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `persistence` | `"save-all"` \| `"none"` | `"save-all"` | `save-all` persists each submitted prompt and seeds recall at startup; `none` is a full opt-out — no read, no write, no file created. |

Recall is scoped to the current directory by default; **Ctrl-T** toggles a view of
every project's history. The store is one global `prompt-history.jsonl` under the
per-user directory beside this config file, mode `0600` on unix. Because prompts
are stored **raw** (not redacted — recall must be faithful), the opt-out and the
restrictive mode/location are the privacy controls; see
[07-security-and-privacy.md](07-security-and-privacy.md) §Prompt History At Rest.

### `[self_improvement]`

The **outward** half of the human-gated self-improvement loop (ADR-0053, extending
ADR-0034): the agent may author a **draft** issue/PR proposing an improvement, but
publishing one to an external repo is gated. Both keys ship **off**, so an absent
`[self_improvement]` block leaves the surface inert — nothing is publishable.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `false` | The explicit feature switch for the outward `propose-issue`/`propose-pr`/`emit-draft` commands. Off refuses them entirely. |
| `outward_targets` | array of string | `[]` | The allowlist of `owner/repo` targets a draft may be proposed for or published to. Empty → nothing is publishable even when `enabled = true`. A target outside this list is refused at propose time, before any draft is written. |

A draft is publishable only when `enabled = true` **and** its target is on
`outward_targets` (fail-closed: both are required). Even then, publishing is
**draft-only** (`gh issue create` / `gh pr create --draft`, never ready/merge),
**dry-run by default** (`emit-draft` prints the `gh` plan and publishes nothing
unless `--approve` is passed), and requires an explicit human approval — the
autonomous loop can author a draft but can never publish one. See
[07-security-and-privacy.md](07-security-and-privacy.md) §Outward Draft Emission.

### `[research]`

The `/research` mode and `localpilot research` subcommand (ADR-0060, amended
by ADR-0076). Local research (repo knowledge + accepted memory) is read-only
and never leaves the machine; the **web** half is on by default — disclosed,
allowlist-gated, and audited — and disableable per run (`--no-web`) or
globally (`[research.web].enabled = false`).

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Whether the research surface is usable at all. Local-only research is read-only and harmless; set `false` to disable the surface entirely. |
| `max_questions` | integer | `6` | The loop bound — the maximum sub-questions a single run may pursue. Per-run override: `--max-questions`. |
| `max_rounds` | integer | `3` | Maximum retrieval rounds (ADR-0078). Round 1 gathers for every sub-question; later rounds re-query only uncovered ones. `1` is the single-pass behaviour. Per-run overrides: `--rounds N`, `--quick` (= 1). |
| `per_source_evidence` | integer | `5` | Evidence snippets taken from each source per question per query; later rounds escalate this (×2, capped ×3) for stubborn questions. |
| `max_total_evidence` | integer | `120` | Hard cap on total evidence snippets across a run; hitting it is reported under "Retrieval notes". |
| `time_budget_secs` | integer | _(unset)_ | Optional wall-clock budget for the retrieval phase. Unset → no time budget (round/evidence caps still bound the run). Per-run override: `--time-budget SECS`. |
| `output_dir` | string | _(unset)_ | Directory for written report artefacts, relative to the project root. Unset → `.localpilot/research/`. |
| `ingest_report` | bool | `false` | When a report is written, also ingest it into LocalMind's documentation index (`doc_chunk`) so research output is semantically searchable and appears in the LocalMind UI's Docs/dashboard. Off by default — research output stays a local artefact unless you opt in. Ingest is best-effort (a failure warns, never fails the run) and idempotent. `localmind ingest docs .localpilot/research` does the same thing manually. |

#### `[research.web]`

The outbound web-egress gate. Web research is **on by default with open-web
reach** (ADR-0076 — a ratified, documented exception to the default-off rule
of `policies/remote-egress.md`; disclosure, audit, and kill switches hold
unchanged). `enabled = false` removes the entire outbound path regardless of
the other keys and cannot be overridden by any flag; `--no-web` skips the web
source for one run.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `enabled` | bool | `true` | Master switch for outbound web research. While `false`, no web request is ever made and the loop runs local-only — this is the kill switch no flag can override. |
| `allowlist` | array of string | `["*"]` | Domains that may be fetched (exact host or a subdomain). **Unset** takes the open-web default; an **explicitly empty** list (`allowlist = []`) means every host is skipped, so nothing is fetched — unset and empty are different statements. `*` matches every host; `*.example.com` matches `example.com` and any subdomain. |
| `disallowlist` | array of string | `[]` | Domains that are always blocked, even when the allowlist would permit them (`*` included). Checked **before** the allowlist, so a match is skipped outright. Same `*` / `*.example.com` patterns as `allowlist`. Empty → blocks nothing. Under the open-web default this is the main restriction tool. |
| `audit_log` | string | _(unset)_ | Path (relative to the project root) of the egress audit log recording every outbound request and skip. Unset → `.localpilot/research/egress-audit.log`. |

Every web-active run — subcommand and interactive `/research` alike — prints
an egress disclosure (posture, reach, off-switches, audit path) before any
request and records a per-session consent that is never persisted. See
[07-security-and-privacy.md](07-security-and-privacy.md) §Web Research Egress.

#### `[research.mcp]`

Designated MCP search tools that propose candidate URLs during web research
(ADR-0077). Nothing is auto-discovered: a tool is consulted only when named
here, as a `(server, tool)` pair referencing `[mcp.servers]`. Results are
leads only — extracted URLs pass the `[research.web]` gate and audit like any
other research fetch, and each search call is itself audited with the
redacted query.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `tools` | array of `{ server, tool }` | `[]` | The designated pairs, e.g. `tools = [{ server = "search", tool = "search" }]`. `server` is a key under `[mcp.servers]`; `tool` is the exact advertised tool name. Empty → no MCP server is consulted during research. |

See [mcp.md](mcp.md) §Research search tools.

#### `[research.render]`

The browser-rendering fallback for pages whose real content only appears after
JavaScript runs (single-page-app shells, iframe-embedded docs). Static HTTP
extraction stays the fast default and server-rendered pages never launch a
browser; rendering is an HTTP-first fallback that stays inside the
`[research.web]` allowlist/audit boundary. When a page's initial HTML shows a
render signal — an empty framework mount (`#root`/`#app`/`#__next`), very little
readable content after reduction, an iframe-only body, or an explicit `Loading…`
placeholder — research recovers an allowlisted iframe through the ordinary gated
path and records an explicit render outcome so a page that needed rendering is
never silently counted as complete.

| Key | Type | Default | Meaning |
| --- | --- | --- | --- |
| `mode` | `"auto"` \| `"off"` \| `"always"` | `"auto"` | `auto` renders/recovers only when a render signal fires; `off` is a complete kill switch (pure static extraction, no detection); `always` treats every fetched page as needing rendering (diagnostics / known dynamic sites). |

## Example

```toml
[provider]
default = "anthropic"

[providers.anthropic]
kind = "anthropic"
model = "claude-sonnet-4-6"
api_key_env = "ANTHROPIC_API_KEY"

[providers.local]
kind = "openai-compatible"
base_url = "http://localhost:8080/v1"
model = "qwen2.5-coder"

[harness]
mode = "agent"
test_command = "cargo test"

[context]
project_analysis = true

[docs]
lookup_policy = "evidence"

[compaction]
mode = "deterministic"

[permissions]
profile = "default"

[quota]
auto_resume = "ask"

[mcp.servers.files]
command = "my-mcp-file-server"
args = ["--root", "."]

[history]
persistence = "save-all"

[research]
enabled = true
max_questions = 6
# Also index written reports into LocalMind so they show up in its UI (off by default).
ingest_report = false
# Web research is on by default with open-web reach; carve out hosts with the
# disallowlist, or turn it off here (`--no-web` skips it for a single run).
[research.web]
enabled = true
disallowlist = ["reddit.com", "*.pinterest.com"]
```
