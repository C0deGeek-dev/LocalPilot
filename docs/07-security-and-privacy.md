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

When opening a directory for the first time, LocalPilot should ask whether the
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

## Retention

The project-local `.localpilot/` state is bounded by a retention policy so it
cannot grow without limit (ADR-0024). A conservative cap is on by default —
`[storage]`: `max_sessions = 100`, `max_age_days = 90`, `auto_prune = true` —
pruning the oldest session transcripts/event logs and any tool-output snapshot no
surviving session references. Because deleting history is sensitive, cleanup is
best-effort and silent at chat startup, every limit is configurable, `0`/`false`
disables it, and `localpilot session prune --dry-run` reports what would be
removed without deleting. Cache and provider metadata are out of scope.

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

Wrapper commands are never auto-allowed. A shell or interpreter invocation that
executes an embedded command the classifier cannot see into — `bash`/`sh`/
`zsh`/`dash`/`ksh -c …`, `env`-prefixed commands, `xargs`, `nohup`, `timeout`,
interpreter `-c`/`-e` one-liners (python, node, perl, ruby), and their
equivalents reachable on Windows (git-bash, WSL) — classifies as `unknown` at
best, on every platform. Destructive flag forms of otherwise project-write
commands escalate: `git reset --hard`, `git clean -f`, and `git checkout`/
`git restore` against pathspecs classify as `destructive`, so a raw shell
command never faces a weaker gate than the purpose-built tool for the same
effect.

## Discovered Tooling

The harness quality gate discovers language-specific check commands from the
project toolchain (ADR-0009). Discovery is untrusted input and must not become
execution by itself:

- Discovery *proposes* a gate; the user *ratifies* it into the project's
  local `.localpilot.toml` (local-only, ADR-0012). Nothing discovered runs
  before ratification.
- Ratified check and fix commands are still classified and mediated by the
  permission engine and shell policy above — ratification records intent, it does
  not grant a standing bypass.
- A non-interactive harness run executes only the ratified gate; a newly
  discovered tool is proposed for the next ratification, never auto-run.
- Auto-fix commands are `project-write` (or higher) and follow the same default
  decisions as any other write.
- A discovered command that classifies as `destructive`, `privileged`, or
  `network` is surfaced with its class at ratification time, not silently
  accepted into the gate.

## Permission Profiles

The permission engine is configurable so users can trade safety for speed
deliberately. Profiles apply in both agent mode and harness mode.

- `default`: least privilege. Risky actions (writes, deletes, shell, network,
  secret-like reads) require approval. This is the out-of-box behavior.
- `relaxed`: a user-defined allowlist auto-approves common safe actions; the rest
  still prompt.
- `bypass`: a launch mode that approves everything with no prompts, equivalent to
  running fully localpilot.

Rules:

- **The allowlist is floor-aware.** Under `relaxed`, an allowlisted tool is
  auto-approved only for low-risk effects: read-only, project-write, and
  network command classes, in-workspace non-secret file reads, and
  in-workspace writes. This includes non-interactive runs — it is how the
  ratified quality gate executes headless (ADR-0009). Destructive,
  privileged, unknown, and external-write commands, secret-like reads, and
  out-of-workspace paths keep their gate regardless of the allowlist, in
  every mode. Allowlisting `run_shell` stops prompt fatigue for routine
  commands; it does not grant `sudo` or `rm -rf`.
- `bypass` is never the default. It must be set explicitly, through a launch flag
  or config, and the active profile is always shown in the footer/status output.
- `bypass` does not silently disable redaction or logging; disabling those
  requires separate explicit settings.
- **Bypass keeps the workspace boundary for path effects only.** The file
  tools' read/write effects carry path information, and an out-of-workspace
  path is still denied under bypass. Shell commands carry no path
  information: bypass auto-allows every command class, and a command's own
  file access is not contained (its working directory is the workspace root,
  nothing more). Treat bypass as full shell access for the model.
- Harness rule verdicts still apply on top of the permission profile. A profile
  controls prompting, not the harness correctness gates.

Bypass removes the main safety net against model-initiated destructive actions.
It should be used only in disposable or sandboxed environments.

## Reliability Contract — Permission Invariants

These invariants are the permission half of the reliability contract
(ADR-0010): the explicit guarantees that make unattended operation
trustworthy. Each is pinned by a named test; a change that breaks the test is
a contract change and needs an ADR, not a patch.

1. **No command reachable via `run_shell` faces a weaker gate than the
   equivalent builtin tool.** Destructive flag/pathspec forms of git commands
   classify `destructive`, matching the purpose-built `git_restore`.
   Enforced by `destructive_git_flags_escalate_past_project_write`
   (`localpilot-sandbox`).
2. **Allowlists never lift destructive, privileged, or unknown gating.** The
   relaxed-profile allowlist relaxes *ask* to *allow* only below the risk
   floor. Enforced by
   `allowlist_never_lifts_destructive_privileged_or_unknown_commands` and
   `allowlist_never_lifts_secret_reads_or_out_of_workspace_paths`
   (`localpilot-sandbox`), and end-to-end by
   `allowlisted_run_shell_still_prompts_for_destructive_commands`
   (`localpilot-tools`).
3. **Wrapper commands never classify below `unknown`** on any platform.
   Enforced by `shell_wrappers_never_classify_below_unknown_on_any_platform`
   and the `wrappers_are_never_read_only` property (`localpilot-sandbox`).
4. **Approval prompts state what is being approved.** Every tool with side
   effects supplies the concrete target (command line, path, query) in the
   prompt detail. Enforced by
   `run_shell_approval_prompt_shows_the_full_command_line`
   (`localpilot-tools`).

The loop half of the contract (tool-result pairing, transcript fidelity) lives
in [`docs/06`](06-harness-spec.md) §Reliability Contract.

## Platform Policy (All Tier-1)

Windows, Linux, and macOS are all first-class, tier-1 platforms. Shell and
filesystem policy must be explicit for both Windows and POSIX, and behavior
parity across the three is a release requirement. The subsections below split
the platform-specific rules; neither side is a degraded fallback.

### Windows

- classify PowerShell, `cmd.exe`, and direct executable invocations separately
- normalize drive-letter, UNC, symlink, junction, and long-path forms
- treat registry writes as privileged local effects
- detect destructive PowerShell commands such as `Remove-Item -Recurse`
- avoid string-built shell commands for filesystem operations
- prefer native Rust filesystem APIs for tool operations
- test path escapes with `..`, drive roots, UNC paths, junctions, and symlinks

### Linux and macOS (POSIX)

- normalize symlinks before write/delete decisions
- detect destructive shell patterns such as `rm -rf`
- treat privilege escalation commands (`sudo`, `doas`) as privileged
- distinguish workspace-local writes from external writes
- test path escapes with `..`, absolute roots, and symlinks

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

LocalPilot is a coding tool. It should not ship prompts or affordances aimed at:

- malware creation
- credential theft
- phishing
- evasion
- unauthorized access

The permission engine is a local safety layer, not a replacement for provider
usage policies.
