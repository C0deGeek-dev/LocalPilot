# Security and Privacy

## Security Model

The model is untrusted. Tool inputs are untrusted. Provider outputs are
untrusted. User-approved policy is trusted.

## Local Effects

Local side effects include:

- file writes
- file deletes
- shell commands
- package installs
- git mutations
- network access
- credential reads

Every local side effect must be mediated by the tool runtime and permission
engine.

## Workspace Trust

When opening a directory for the first time, Unshackled should ask whether the
workspace is trusted.

Trusted means:

- read normal project files
- run low-risk commands
- use configured tools

Trusted does not mean:

- read secrets without approval
- run destructive commands without approval
- write outside workspace without approval

## Secret Redaction

Redact:

- API keys
- bearer tokens
- private keys
- passwords
- cloud credentials
- connection strings with credentials

Redaction applies to:

- logs
- transcripts
- tool outputs
- error messages
- memory entries

Secret detection is best-effort. Inspect/delete controls are the backstop; the
product must not promise perfect secret filtering.

## Shell Policy

Commands are classified as:

- read-only
- project-write
- external-write
- network
- destructive
- privileged
- unknown

Default decisions:

| Class | Interactive | Non-interactive |
| --- | --- | --- |
| read-only | allow | allow |
| project-write | ask | deny |
| external-write | ask | deny |
| network | ask | deny |
| destructive | ask with explicit warning | deny |
| privileged | ask with explicit warning | deny |
| unknown | ask | deny |

## Permission Profiles

The permission engine is configurable so users can trade safety for speed
deliberately. Profiles apply in both agent mode and harness mode.

- `default`: least privilege. Risky actions (writes, deletes, shell, network,
  secret-like reads) require approval. This is the out-of-box behavior.
- `relaxed`: a user-defined allowlist auto-approves common safe actions; the rest
  still prompt.
- `bypass`: a launch mode that approves everything with no prompts, equivalent to
  running fully unshackled.

Rules:

- `bypass` is never the default. It must be set explicitly, through a launch flag
  or config, and the active profile is always shown in the footer/status output.
- `bypass` does not silently disable redaction, logging, or the workspace
  boundary; disabling those requires separate explicit settings.
- Harness rule verdicts still apply on top of the permission profile. A profile
  controls prompting, not the harness correctness gates.

Bypass removes the main safety net against model-initiated destructive actions.
It should be used only in disposable or sandboxed environments.

## Windows Tier-1 Policy

Windows is a first-class platform. Shell and filesystem policy must be explicit
for both Windows and POSIX.

Windows requirements:

- classify PowerShell, `cmd.exe`, and direct executable invocations separately
- normalize drive-letter, UNC, symlink, junction, and long-path forms
- treat registry writes as privileged local effects
- detect destructive PowerShell commands such as `Remove-Item -Recurse`
- avoid string-built shell commands for filesystem operations
- prefer native Rust filesystem APIs for tool operations
- test path escapes with `..`, drive roots, UNC paths, junctions, and symlinks

POSIX requirements:

- normalize symlinks before write/delete decisions
- detect destructive shell patterns such as `rm -rf`
- treat privilege escalation commands as privileged
- distinguish workspace-local writes from external writes

## Network Policy

The core app may call configured model providers. Tools need separate approval
for arbitrary network commands.

Provider clients must:

- use TLS for hosted APIs
- redact auth headers in logs
- expose request IDs when providers return them
- avoid logging raw prompts by default

## Quota Wait/Resume Safety

Automatic quota wait/resume is allowed only when it honors the provider's
documented retry contract and the user's explicit policy.

Safety gates:

- resume only at harness step boundaries
- never resume while a destructive approval is pending
- never resume after user cancellation
- never resume with unrelated dirty workspace state
- re-probe the provider after the timer instead of trusting local wall-clock time
- use bounded backoff with jitter when reset metadata is approximate
- record pause/resume reasons in local state
- do not present the feature as bypassing or outsmarting limits

## Telemetry

Default: no remote telemetry.

Allowed:

- local logs
- local performance timings
- user-exported debug bundles after review

If remote telemetry is ever added:

- it must be opt-in
- schema must be public
- redaction must happen before upload
- no prompts or source code by default

## Supply Chain

Required before public release:

- `cargo audit`
- `cargo deny`
- dependency license review
- release artifact reproducibility notes

## Abuse Resistance

Unshackled is a coding tool. It should not ship prompts or affordances aimed at:

- malware creation
- credential theft
- phishing
- evasion
- unauthorized access

The permission engine is a local safety layer, not a replacement for provider
usage policies.
