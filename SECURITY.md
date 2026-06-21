# Security Policy

## Supported Versions

No released versions exist yet.

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

