# Manual actions — agent-mode plan

> Mirror of every human-owned (non-`agent`) box. Keep in sync with the subject
> files. Status: `TODO` | `DONE` | `DEFERRED` (a deferral needs a rationale).

| Box ID | Owner | Action | Source subject | Status | Deferral rationale |
|---|---|---|---|---|---|
| 05.5 | release-engineer | Run the live eval once against a capable hosted model (real key, never committed); record the scorecard. | 05 | DEFERRED | Deferred by AgentMode D005; continue as normal development validation when credentials and model budget are available. |
| 05.6 | release-engineer | Run the live eval against a capable local model (≥ Q4 via a local server or the gateway); record the scorecard. | 05 | DEFERRED | Deferred by AgentMode D005; continue when a capable local model or gateway is available. |
| 05.7 | release-engineer | Run representative maturity scenarios against the mature fork and Rust agent mode as black-box products; compare outcomes only. | 05 | DEFERRED | Deferred by AgentMode D005; run during dogfood/maturity work, comparing outcomes only under the clean-room rules. |
