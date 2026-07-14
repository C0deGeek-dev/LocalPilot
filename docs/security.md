# Security model (summary)

A short user-facing summary; the full specification is in
[07-security-and-privacy.md](07-security-and-privacy.md).

## Tools are the only path to side effects

The model cannot touch your machine except through builtin tools, and every tool
call passes through one permission engine before it runs. The model and the
harness cannot bypass that engine. MCP tools go through the same engine — they
are not a side channel.

## Permission profiles

- **default** — least privilege. Writes, deletes, shell commands, network, and
  secret-like reads require approval. This is the out-of-box behavior.
- **relaxed** — a user-defined allowlist auto-approves common safe actions; the
  rest still prompt.
- **bypass** — a launch mode that approves everything with no prompts. It must be
  set explicitly, is never the default, and is always shown in the footer/status.
  Bypass does **not** disable redaction, logging, or the workspace boundary.
- **unrestricted** — approves everything, out-of-workspace paths included, with
  no prompts. Never the default; it must be chosen explicitly and is surfaced in
  the footer in the strongest warning style. Redaction and logging stay on.

## The workspace boundary

File tools are confined to the workspace. The boundary is enforced by
canonicalizing paths and checking containment (handling `..`, symlinks, Windows
verbatim/8.3/case forms), not by string prefix matching. Writes outside the
workspace require explicit approval and are denied non-interactively.

Out-of-workspace **reads** are promptable rather than a dead end: every profile
(including `bypass`) can ask interactively, and `[permissions]
extra_read_roots` grants standing read-only access to the listed directories in
every profile, non-interactive runs included. Writes keep the workspace
boundary and secret-like reads keep their gate. The full model is in
[07-security-and-privacy.md](07-security-and-privacy.md).

## Secret redaction

Detected secrets (API keys, bearer tokens, private keys, passwords, cloud
credentials, credentialed connection strings) are redacted **before** anything is
logged, persisted, or exported. Detection is best-effort; the backstop is that
all stored data is inspectable plain files you can read and delete.

## Command classification

`run_shell` runs an argument list, never a shell string. Commands are classified
per platform (read-only, project-write, external-write, network, destructive,
privileged, unknown) and gated accordingly; destructive and privileged commands
are denied non-interactively.

## Quota wait/resume

A run paused on a provider limit only resumes when it is safe: at a step
boundary, with a clean workspace, no pending destructive approval, no
cancellation, and an unchanged provider configuration. Unattended (`global`)
resume must be enabled explicitly.

## Reporting

See [SECURITY.md](../SECURITY.md) for how to report issues.
