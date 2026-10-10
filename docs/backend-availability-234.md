# Missing backend handling: LocalHub #234

## Investigation and resolution

The earlier fix (`7ebcc2ae`, ADR-0222) stopped consecutive unavailable-backend
observations regardless of query arguments. It did not gate advertisement or
prevent interleaved queries. Controlled offline replays on the unmodified runtime
confirmed the gap with the real `KnowledgeSearch` and temporary workspaces:

| Scripted trace | Before | After |
| --- | --- | --- |
| 123 distinct consecutive knowledge queries against no index | 3 actual queries, `NoProgress` | 3 preflight refusals, no queries, `NoProgress` |
| 123 knowledge queries alternating with advancing fixture polls | 246 calls, `Done`; 123 useless queries | 123 preflight refusals and 123 polls, `Done`; no knowledge queries |
| 123 knowledge queries alternating with real empty text searches | 246 calls, `Done`; 123 useless queries | 123 preflight refusals and 123 text searches, `Done`; no knowledge queries |

The replay explicitly allows 300 calls to isolate detection from the hard
budget. This is a mechanism comparison, not the exact historical 72-run trace or
a live model quality/latency measurement. A scripted model ignores hiding and
feedback; a model that switches to another source can finish normally after a
refusal. Tests verify that behavior too.

## Implementation

ADR-0225 adds a fresh, side-effect-free absence probe with no authoritative
absence by default. `KnowledgeSearch` reuses ingest/span index-presence checks.
Present/corrupt indexes remain eligible, as do ordinary empty search results.
Only knowledge search currently implements a probe; unknown server/MCP status
is not guessed from result text or blocked permanently.

The session refreshes provider advertisement, broker search/load/ranking/reveal
and stable/revealed tiers from probes. Catalog, core, graduation and working-set
membership survive absence, permitting recovery without re-registration.
Each stale call rechecks the backend before query execution. Refused attempts
have explicit not-executed feedback and distinct attempt-stop accounting; actual
invocations remain observed under their existing guards. Only three consecutive
same-backend stale attempts stop the turn. Another tool attempt, recovery, user
steering or a fresh turn clears the counter. Interleaved work is not stopped
merely because this backend stays absent; ordinary guards and budgets still
bound the turn. Ordinary permission gates still apply after recovery.

The broker snapshot is refreshed by the session before requests, request/marker
reveals and model calls, including discovery tools. Direct host users of the
broker must supply their own fresh snapshot. No absence cache or persisted
availability is added. Filesystem snapshots cannot lock external backend state;
post-invocation typed absence remains the fallback if availability changes
between checking and execution.

## Regression evidence

`backend_availability` exercises zero invocation counts, useful work after a
stale refusal, per-turn reset, absent broker search/load/core/graduation, and real
index creation restoring advertisement and successful knowledge retrieval,
including within one response and with the broker enabled or disabled.
`repeat_guard` pins consecutive stale stops, unaffected interleaved work and actual-call
controls. Knowledge-tool tests compare probe eligibility for absent,
present/corrupt and newly created indexes. Broker unit tests mask core,
graduated and revealed tools and restore their original tiers.
