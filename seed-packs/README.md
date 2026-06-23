# Seed packs

Opt-in, curated best-practice lesson packs you can seed into a project's
LocalMind accepted memory so the model is reminded of them at the start of each
turn. **Nothing here is auto-loaded** — you choose to seed it.

```sh
# point-lessons → injected into context (FTS-retrieved, capped per turn)
localpilot learning seed --file seed-packs/coding-lessons.json
localpilot learning seed --file seed-packs/research-lessons.json   # --dry-run to preview

# long-form references → reachable when the model calls knowledge_search
localpilot ingest run     # ingests *.md including these references
```

Seeding is idempotent: a lesson whose body already exists is skipped. Remove a
seeded lesson with `localpilot memory delete <id>`; toggle injection for the
whole project with `localpilot memory disable` / `localpilot memory enable`.

A lesson whose `tags` include `"rule-cue"` is **promoted to an always-on rule
cue**: instead of being retrieved by relevance, it is injected every turn as
terse, always-present guidance (and excluded from the relevance block so it is
never injected twice). Use it sparingly, for a few behaviour-critical rules a
weak model should always see. It stays advisory — a cue the model reads, not an
enforced gate.

The lessons are general, transferable coding and research practices — written for
this repository, not copied from anywhere. Treat them as defaults to adapt, not
laws.
