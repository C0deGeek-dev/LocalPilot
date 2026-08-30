---
name: clean-room-guard
description: >-
  Apply this repository's clean-room provenance rules. Use before consulting the
  read-only behavior reference, before writing prompts/identifiers/UI copy, and
  before opening any PR — to know what may be copied (nothing proprietary), when
  a provenance note is required, and what framing is prohibited.
---

# clean-room guard

This repository must be independently authored. The authoritative policy is
[`docs/00-clean-room.md`](../../../docs/00-clean-room.md) and ADR-0004 / ADR-0005
in [`docs/10-decisions.md`](../../../docs/10-decisions.md). Read them; this skill
only routes you to the contract and the must-do steps.

## Before you copy anything — don't

Never copy, translate, port, summarize, or mechanically transform proprietary or
leaked source, bundled prompts, private/undocumented endpoint payloads, hidden
feature flags, vendor branding/UI copy, or internal identifiers. Adapters for
private consumer-product endpoints are prohibited — official public APIs or
local servers the user runs, only.

## Using the read-only behavior reference

A local working implementation may exist at the path named in
[`AGENTS.md`](../../../AGENTS.md). It is a **read-only behavior reference** for
workflows, command behavior, config shape, and user-facing edge cases **only
when this repo's own docs are silent**. It is never a source of code, prompts,
tests, identifiers, UI copy, or private endpoint details.

When a change was informed by it, add this provenance note to the PR:

```text
Behavior cross-checked against a local read-only behavior reference.
Implementation, prompts, tests, and API details are original to this repository.
```

## PR provenance checklist (from docs/00)

- Is this original code?
- Is public documentation cited for any external API?
- Official APIs or local servers only — no private/undocumented endpoints?
- No vendor branding or private implementation names?
- Prompts authored here, not copied?
- Tests derived from this repo's spec, not another implementation?

## Prohibited framing

Not "a free build of", "a fork of", "a replacement for <vendor CLI>", "an
unlocked version of", or "a redistribution of exposed source". Provider names
appear only as compatibility statements, never as product identity.
