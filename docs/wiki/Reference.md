# Reference

This wiki does not duplicate the in-repo specification. Reference material is
indexed in [`docs/README.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/README.md),
which maps every area to its owning doc.

> **Do not edit on github.com.** This wiki is generated from in-repo Markdown
> under `docs/wiki/` and synced one-way on every push to `main`. Edit the source
> in `docs/wiki/`; web edits are overwritten on the next sync.

## LocalMind CLI contracts

- **Search output format** — non-terminal stdout returns JSON by default, a
  terminal returns the human table, and `--format human|json` overrides either way
  (ADR-0048). See
  [`docs/configuration.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/configuration.md#project-context-files).
- **Store resolution** — `learning`/`memory` walk up to the nearest ancestor
  `.localmind` store; `--workspace <path>` pins it. See
  [`docs/localmind-integration.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/localmind-integration.md#store-resolution).

## Research

- **Research surface** — `/research` (interactive) and `localpilot research`
  (headless) drive one local-first loop that writes a report and review-gated
  memory candidates (ADR-0060). Configure it under `[research]`; see
  [`docs/configuration.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/configuration.md).
- **Web egress** — off by default; reachable only via the headless
  `localpilot research --web` opt-in (disclosed, allowlist-only, audited,
  disableable). See
  [`docs/07-security-and-privacy.md`](https://github.com/C0deGeek-dev/LocalPilot/blob/main/docs/07-security-and-privacy.md).
