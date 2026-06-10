# Lessons — NextPhase Plan

> Append during the run, the moment a slice teaches something — not at the
> gate. Disposable run-notes; durable lessons migrate to the permanent
> `tasks/lessons.md` at the plan's §7 gate.

- 2026-06-10 (subject 00): verify a review's line references *and its claims*
  before scoping fixes — review §2.2 said POSIX `env rm -rf` classifies
  Unknown, but `env` is actually in the classifier's read-only list, so it
  auto-Allows. The fix scope (02.3) grew: remove `env` from read-only, don't
  just add wrapper detection.
- 2026-06-10 (subject 00): Anthropic publishes reset times as RFC 3339 in
  `anthropic-ratelimit-*-reset` headers; the adapter currently never fills
  `reset_at`. Fix alongside the OpenAI duration-string parse (01.5) so both
  adapters surface machine-readable resets.
- 2026-06-10 (subject 03): never `#[serde(flatten)]` a tagged enum into an
  envelope that has its own `id`-style fields — a variant field with the same
  name silently collides at the JSON level and corrupts the envelope. Nest the
  payload under a named field; the roundtrip test caught it immediately.
- 2026-06-10 (subject 03): deriving event metadata from the payload
  (`origin_for(&message)`) instead of passing it at every call site removes a
  whole class of emission-site drift — the derivation test then proves a
  structural property, not call-site discipline.
- 2026-06-10 (subject 02): tightening a permission rule can break a feature
  that *depends* on the looser rule — the ratified quality gate (ADR-0009)
  runs headless precisely because the allowlist lifts a non-interactive
  low-risk deny. Before narrowing a security primitive, grep for callers that
  treat the current behavior as a grant mechanism (here:
  `QUALITY_CHECK_TOOL`). The behavior test caught it; design review should
  have first.
