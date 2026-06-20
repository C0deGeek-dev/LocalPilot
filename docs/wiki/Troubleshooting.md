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

## A tool action was blocked

That's the permission engine doing its job — risky actions need explicit approval
and `bypass` is never the default. Permission profiles (`default` / `relaxed` /
`bypass`) are set per run; see
[07-security-and-privacy.md](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/07-security-and-privacy.md).

## Transcript scrolling in the REPL

The REPL stays in your terminal's normal screen buffer and does not capture the
mouse, so scroll the transcript exactly as you scroll any other terminal output
(mouse wheel, scrollbar, or your terminal's scroll keys). Finished history is
printed into native scrollback while the live input box and status line stay
pinned at the bottom.

## Ingesting a folder looks stuck

A first `/ingest run` (or the automatic first-use index build) walks and chunks
the whole workspace, which takes time on a large repo. The REPL is not hung: it
shows a working spinner plus stage notices — discovering, files to parse,
parsed *N*/*total*, indexing, writing — and a final `ingestion completed:
… file(s), … chunk(s)` line. Press `Ctrl-C` to pause; the chunks written so far
are kept, and `/ingest resume` (or `localpilot ingest resume`) continues from
there instead of restarting.
