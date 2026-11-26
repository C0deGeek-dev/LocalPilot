# Troubleshooting & FAQ

Common problems and fixes. Entries match shipped behaviour at the current
`VERSION`.

> **Do not edit on github.com.** This wiki is generated from in-repo Markdown
> under `docs/wiki/` and synced one-way on every push to `main`. Edit the source
> in `docs/wiki/`; web edits are overwritten on the next sync.

## Start with `doctor`

```sh
localpilot doctor
```

It reports version, platform, resolved config, configured providers, available
tools, and trust state — the fastest way to see what LocalPilot thinks is set up.

## The model says it has no internet access

Chat web access is on by default (`[tools] web = "ask"`), but searching needs a
search tool. `localpilot doctor` shows a `chat web:` line: whether chat web is
on, where requests are recorded, and the search tools `web_search` would use.
With none set up, `fetch` still reads a page whose address you give it. To
search, run `localpilot search setup`: it lists the backends it can set up here
(a keyless one, a local SearXNG in Docker, or a search API with your own key)
and sets up the one you pick. The fullscreen session also offers it when a
search finds no backend. You can instead add a search-capable MCP server under
`[mcp.servers]` and name its search tool in `[research.mcp] tools`. The
`chat web:` line reads the configuration only and sends no search. The
`managed search backends` lines show each backend set up for you, and the
`mcp servers:` section shows whether a hand-added server starts.

Without any search backend, `/research` makes no web search or page fetch. It
reports a "local knowledge review" and says why.
Only the fullscreen session can ask for web consent; a piped run has no web
access.

## Windows: "could not find a DLL it needs"

Llama.cpp's Windows builds load the Microsoft Visual C++ Redistributable
(x64), which a fresh Windows does not have; LocalX releases up to v6.0.0 need
it for their own tools too. The installer offers it when it is missing. To
install it yourself, download
https://aka.ms/vs/17/release/vc_redist.x64.exe from Microsoft, run it, then
run the command again (`localx install all` or `localx setup`).

## `localx setup` stopped at a step

Setup prints the step that failed, the log to read (each run keeps its own
folder under `localx/logs/` in the LocalX data directory) and the command to
run it again. Steps look before they act, so running `localx setup` again is
safe. `--yes` never replaces a different default provider; that takes
`--replace-provider`.

## The local model is not usable

When the LocalBox model LocalPilot uses is down, a session asks LocalBox to
start the same model again. If that fails, the error names the stage:

- **start** — LocalBox could not start it. Common reasons: no saved launch
  for that port (a model adopted before LocalBox saved serves, or served with
  options a replay cannot repeat: run `localx setup` once, or
  `localbox serve <model>` once), or the
  port is already in use. LocalPilot and `serve --again` leave a program on
  the port alone: close or reconfigure it, then retry. When the message says
  LocalBox already serves another model there, `localbox stop` frees it (it
  stops every LocalBox model server, not only that one).
- **readiness** — the model was started but its address did not answer with
  that model in time. Read the start log it names, and `localbox log`.
- **first reply** — the model was reachable but the first request failed.

Each message ends with the exact retry (`localbox serve --again --proxy-port
<port> --expect-model <model>`), `localbox status` to check, and the fallback
(`localx setup`, or `--provider <name>` for another provider). Detail:
[providers.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/providers.md#using-a-localbox-local-model).

## `chat` says the feature isn't available

The interactive REPL needs the `tui` build feature:

```sh
cargo build -p localpilot --features tui
```

## The submodule is empty after cloning

The LocalMind learning engine is a git submodule. In an existing clone:

```sh
git submodule update --init --recursive
```

## No models listed / provider errors

Confirm `.localpilot.toml` has a `[provider] default` and a matching
`[providers.<id>]` block, then check what a local server actually has loaded:

```sh
localpilot models
```

Provider setup detail:
[providers.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/providers.md).

## "provider stopped at max_tokens while streaming `write_file` arguments"

The model ran out of output budget partway through a tool call. `max_tokens`
covers the whole reply — any explanation the model writes first *and* the tool
arguments — so a long preamble followed by a large file write can hit the cap
even at 16384. LocalPilot discards the truncated call (it is never half-applied)
and retries a file write once in smaller pieces; if the cap is hit again the
turn stops with the same warning. Ask for a shorter answer or a chunked write,
or raise the provider's `max_tokens`
([providers.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/providers.md)).

## A tool action was blocked

That's the permission engine doing its job — risky actions need explicit approval
and `bypass` is never the default. Permission profiles (`default` / `relaxed` /
`bypass`) are set per run; see
[07-security-and-privacy.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/07-security-and-privacy.md).

## Proposal validation or promotion is blocked

Inspect `localpilot self-review evidence --id <proposal>` and a fresh `preflight`
for that proposal. A new source/plan/toolchain or private host configuration can
invalidate old evidence. Missing original base/scope metadata in an old proposal
requires regeneration; an old narrative pass is not completed validation.
ReadOnly/Deny and project trust still apply even after execution consent.

If an interrupted intent is reported, use `localpilot selfimprove recovery` and
then `recovery --nonce <uuid>` to inspect it. Check remaining processes, host
effects and any remote publication before acknowledging with the exact displayed
review, reviewer and inspection note. Recovery never retries the interrupted
action or certifies validation. See the
[recovery contract and command flags](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/07-security-and-privacy.md#self-improvement-patch-generation).

Do not delete the authority directory, lock or journal to clear a refusal. A
missing/corrupt lock or journal, changed source/state, or older promotion without
recorded checkpoint proof needs manual investigation. Stop older LocalPilot
processes before adoption; they cannot safely co-write the new authority store.
Keep the existing data for inspection. A slow preview may be hashing source,
toolchain and host configuration; execution deadlines are not runtime estimates.

## Transcript scrolling in the REPL

The full-screen chat host runs in an alternate screen buffer and owns timeline
scrolling: use the mouse wheel, the scrollbar, PageUp/PageDown, or the arrow
keys to move through history. Selection and copy are handled inside the
application (the mouse is captured), and the composer and status line stay
pinned at the bottom.

## Ingesting a folder looks stuck

A first `/ingest run` (or the automatic first-use index build) walks and chunks
the whole workspace, which takes time on a large repo. The REPL is not hung: it
shows a working spinner plus stage notices — discovering, files to parse,
parsed *N*/*total*, indexing, writing — and a final `ingestion completed:
… file(s), … chunk(s)` line. Press `Ctrl-C` to pause; the chunks written so far
are kept, and `/ingest resume` (or `localpilot ingest resume`) continues from
there instead of restarting.
