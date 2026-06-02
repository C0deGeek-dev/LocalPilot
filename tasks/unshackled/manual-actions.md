# Manual Actions — Human-Owned Boxes

> Mirror of every non-`agent` box from the subject files (§5 of
> `tasks/Unshackled-Plan.md`). Keep in sync with the owning subject. Status is
> one of `TODO`, `DONE`, `DEFERRED` (a deferral needs a rationale). Owner is a
> non-`agent` role from the §5 enum.

| Box ID | Owner | Action | Source subject | Status | Deferral rationale |
|---|---|---|---|---|---|
| 00.6 | tech-lead | Decide MCP posture for the run (default: start without MCP; add OpenAI Docs MCP at subject 03 only after audit). Record in Decision log. | 00 | DONE | Adopted documented default; recorded as D003 (start without MCP, revisit at 03 after audit). |
| 00.7 | tech-lead | Security-review every authored repo skill `SKILL.md` + bundled scripts before the repo is trusted; confirm no broad tool allowlists. | 00 | DONE | Agent audit: 6 skills authored, zero `allowed-tools`/`allowed_tools` keys, no bundled scripts; bodies only route to specs. Owner may re-confirm before trusting. |
| 00.8 | release-engineer | Confirm credentials policy: `.env` gitignored, env-var → key mapping documented, no real keys in CI. | 00 | DONE | `git check-ignore` confirms `.env`/`.env.local` ignored (`!.env.example` excepted). Keys: `UNSHACKLED_LOCAL_API_KEY`, `OPENAI_API_KEY` (per `.env.example`). CI carries no keys; live tests stay opt-in behind `UNSHACKLED_LIVE_TESTS`. |
| 03.13 | tech-lead | Choose the first official hosted provider (official public API, ADR-0004) and confirm its public-docs source. | 03 | DONE | OpenAI official Chat Completions API (D009). Public docs: https://platform.openai.com/docs/api-reference/chat. Provenance note in `crates/unshackled-llm/src/openai.rs`. |
| 03.14 | release-engineer | Provide local-only credentials for the opt-in live provider test (`UNSHACKLED_LIVE_TESTS`); never commit keys. | 03 | DEFERRED | Opt-in live test scaffolded (`crates/unshackled-llm/tests/live.rs`); skips unless `UNSHACKLED_LIVE_TESTS` + `OPENAI_API_KEY` set. No key supplied this session; run once before alpha (subject 09 release checklist). |
| 06.18 | tech-lead | Review intake + planner prompts and rule verdict severities (block vs warn) for correctness + clean-room provenance before locking. | 06 | TODO | |
| 07.16 | tech-lead | Security-review MCP permission integration + any shipped/sample skill manifests; confirm no permission bypass / broad allowlists. | 07 | TODO | |
| 09.10 | tech-lead | Human clean-room audit sign-off (provenance, not just correctness) before the §7 clean-room gate is ticked. | 09 | TODO | |
| 09.11 | release-engineer | Run the full `docs/09` release checklist before tagging. | 09 | TODO | |
| 09.12 | release-engineer | Confirm Public-Alpha Criteria and tag `v0.1.0-alpha.1`. | 09 | TODO | |
| 09.13 | release-engineer | Set up the nightly channel build from main. | 09 | TODO | |
