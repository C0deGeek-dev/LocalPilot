# Security Policy

## Supported Versions

Security fixes land on the latest release train. Earlier trains are not
maintained — upgrade to the current minor.

| Version | Supported |
| --- | --- |
| 1.2.x   | ✅        |
| < 1.2   | ❌        |

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
- Web research is off by default. The research loop is local-only unless the
  operator opts in for that run with the headless `localpilot research --web`
  flag, which discloses what egresses, fetches only allowlisted domains (others
  are skipped and logged), sends only the redacted sub-question, and audits every
  request. `[research.web] enabled = false` is the kill switch a runtime opt-in
  cannot override. See
  [docs/07-security-and-privacy.md](docs/07-security-and-privacy.md) §Web Research
  Egress.

