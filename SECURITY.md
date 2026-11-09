# Security Policy

## Supported Versions

Security fixes land on the latest release train (the version in the top-level
`VERSION` file). Earlier trains are not maintained — upgrade to the current
release.

| Version | Supported |
| --- | --- |
| latest release train | ✅ |
| earlier trains | ❌ |

## Reporting Security Issues

Until a public security contact exists, report issues privately to the repository
owner. Do not open public issues for vulnerabilities involving credential
exposure, command execution, sandbox bypass, or provider authentication.

## Security Scope

Security-sensitive areas:

- shell command execution
- file read/write tools
- workspace boundary checks
- secret redaction
- provider authentication
- transcript persistence
- MCP server integration
- web research egress
- the opt-in remote gateway, paired devices and the phone companion app
- phone notifications (Web Push)

## Security Defaults

- No remote telemetry by default.
- Non-interactive mode denies risky actions by default.
- Writes outside the workspace require explicit approval.
- Secret-like files require explicit approval.
- Provider API keys must come from environment variables or secure storage
  (`localpilot login` stores them in the OS keychain, or a `0600` per-user file).
  Bring-your-own-key only: no "sign in with Claude/ChatGPT" and no use of
  subscription credentials (ADR-0042).
- Logs must redact secrets; a stored key is never logged or echoed in full.
- `localpilot pair` applies one explicitly selected profile to two independent
  permission engines. Approval and question dialogs identify their originating
  peer; abort or terminal shutdown denies/dismisses outstanding asks and cancels
  both sessions.
- Pair collaboration never applies, commits, merges, or publishes an agreed
  candidate automatically.
- LocalMind inference is loopback-only, enforced at the request. `local_only`
  constrains the inference endpoint as well as the storage scope: only literal
  loopback destinations are permitted, no name resolution is performed (a
  hostname that resolves to loopback when checked can resolve elsewhere when
  connected), and there is no configuration in which the constraint is off.
  See [docs/07-security-and-privacy.md](docs/07-security-and-privacy.md)
  §LocalMind Inference Egress.
- Remote access is off by default. `localpilot remote gateway` runs only when
  `[remote] enabled` is set in the user config (a project file cannot set it),
  binds loopback only, and is reached through an HTTPS component the user runs
  on the same computer. There is no LocalX server, relay or account. Every
  change needs the exact advertised `Origin`; reads refuse another origin or
  site. A paired device's credential lives only in a `__Host-` `Secure`
  `HttpOnly` `SameSite=Strict` cookie, the workspace server decides every
  action, and a device can be revoked from the computer or by itself. Pairing
  codes are single-use, short-lived and bound to one workspace. A stolen,
  unlocked phone acts as that device until it is revoked. See
  [docs/07-security-and-privacy.md](docs/07-security-and-privacy.md)
  §Paired Devices and ADR-0269.
- Phone notifications are off by default (`[remote.push] enabled`) and
  content-free; the push service still sees delivery metadata.
- Web research is off by default. The research loop is local-only unless the
  operator opts in for that run with the headless `localpilot research --web`
  flag, which discloses what egresses, fetches only allowlisted domains (others
  are skipped and logged), sends only the redacted sub-question, and audits every
  request. `[research.web] enabled = false` is the kill switch a runtime opt-in
  cannot override. See
  [docs/07-security-and-privacy.md](docs/07-security-and-privacy.md) §Web Research
  Egress.
