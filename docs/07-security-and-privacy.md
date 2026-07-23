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

## Prompt History At Rest

The interactive composer persists submitted prompts so Up/Down recall survives a
restart (ADR-0040). This store has a **different** posture from transcripts, and
the difference is deliberate:

- **Stored raw, not redacted.** Transcripts and tool outputs are redacted before
  write (see Secret Redaction); prompt-history entries are not. An entry exists
  only to be recalled verbatim into the composer — redacting it would recall
  `[REDACTED]` and defeat the feature. A prompt can therefore contain a secret the
  user typed, in cleartext, on disk.
- **The controls instead are:** an opt-out, a restrictive file mode, a per-user
  location, and a bounded size.
  - **Opt-out:** `[history] persistence = "none"` disables it entirely — no read at
    startup, no write on submit, no file created. The default is `save-all`. See
    [configuration.md](configuration.md) §`[history]`.
  - **Location:** a single global file
    (`prompt-history.jsonl`) under the per-user directory beside `config.toml`
    (`%APPDATA%/localpilot` on Windows, `$XDG_CONFIG_HOME`/`~/.config/localpilot`
    elsewhere), never the project-local `.localpilot/`.
  - **File mode:** `0600` (owner read/write) on unix. Windows has no exact
    equivalent; the per-user profile directory's own ACL is the protection there.
    Tier-1 parity (ADR-0007) is behaviour parity — load, append, project-filter,
    and opt-out are identical on all three platforms; only the filesystem
    permission mechanism differs.
  - **Bound:** the file is trimmed to a maximum entry count on write, so it cannot
    grow without limit.
- **To disable and purge:** set `persistence = "none"` and delete the
  `prompt-history.jsonl` file.

Recall is scoped to the current directory by default (each record is tagged with
the directory it was submitted in); Ctrl-T toggles a view of every project's
prompts.

## Stored API Credentials At Rest

`localpilot login <provider>` stores a provider API key so a logged-in user needs
no environment variable (ADR-0042). The posture:

- **Bring-your-own-key only — no subscription tokens (blocking).** The key is one
  the user creates in the provider's own dashboard. No code path obtains, stores,
  or routes Claude Free/Pro/Max or ChatGPT Plus/Pro *subscription* credentials,
  and there is no "sign in with Claude/ChatGPT" flow. Neither provider offers a
  sanctioned OAuth flow that mints a standard API key for a third-party client,
  and routing a third party's users through subscription credentials is a terms
  violation; BYOK is the only sanctioned path.
- **Where it is stored.** The OS keychain when available — currently the Windows
  Credential Manager (built with the `keychain` feature). On macOS and Linux, and
  on any host without a keychain backend, a `0600` file (`credentials.json`) under
  the per-user directory beside `config.toml`. (The macOS/Linux native keychains
  are held back by an MSRV-incompatible dependency; see ADR-0042.) The key never
  enters the repo or a config file.
- **Best-effort, never blocking.** A keychain that is absent or locked is a miss,
  not an error: the store falls back to the file and resolution falls through to
  the environment, so startup and a live session never depend on keychain
  availability.
- **Secret discipline.** The pasted key is wrapped in `Secret` immediately, never
  logged or echoed in full (only a masked head/tail), and the `Secret` type
  refuses serialization, so the only places a key leaves the wrapper are the
  audited keychain/file writes. The fallback file is `0600` on unix (the per-user
  directory ACL on Windows — tier-1 parity is behaviour parity, ADR-0007).
- **Resolution precedence:** stored credential (keychain → file) → `api_key_env`
  environment variable → config. `localpilot doctor` reports the resolved *source*
  (`keychain` / `file` / `env` / `not set`), never the secret.
- **To remove:** `localpilot logout <provider>` deletes it from every tier.

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
best, on every platform. The Windows shells are no exception: a `cmd /c …`,
`powershell`/`pwsh -Command …`, `-EncodedCommand`, or `-File` invocation carries
an inline command or script the substring classifier cannot read, so it
classifies as `unknown` (gated) rather than trusting a coincidental keyword — a
`cmd /c "echo data > secrets"` is a write, not the read its `echo` looks like.
Independently, any argument carrying an output redirection (`>`/`>>`) lifts a
read-looking command to at least `project-write`, so a redirection can never be
auto-allowed as a read. Destructive flag forms of otherwise project-write
commands escalate: `git reset --hard`, `git clean -f`, and `git checkout`/
`git restore` against pathspecs classify as `destructive`, so a raw shell
command never faces a weaker gate than the purpose-built tool for the same
effect.

A shell command carries no contained path, so a `read-only` command
(`cat`/`type`/`head`) could otherwise read a secret-bearing or out-of-workspace
file and pull it into model context with no prompt. Each non-flag path argument
of a read-only command is therefore inspected: one that is secret-like (the same
table the file tools use — `.env`, `*.pem`, `~/.ssh/…`, `.aws/credentials`, …) or
that resolves outside the workspace adds an explicit read effect, so it faces the
same prompt the `read_file` tool would. The check is best-effort and
conservative — ordinary in-workspace reads add no prompt.

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
  running fully localpilot. The single exception is an out-of-workspace path,
  which prompts (see the boundary rule below).
- `unrestricted`: a launch mode that approves everything — out-of-workspace
  paths included — with no prompts at all. The user explicitly accepts full
  responsibility. Like `bypass` it is never the default, must be set explicitly
  (`--permission unrestricted`, the `/unrestricted` slash command, or
  `[permissions] profile`), is always surfaced in the footer/status output, and
  does not disable redaction or logging (ADR-0070).

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
- **Bypass keeps the workspace boundary for path effects only — as a prompt,
  never a dead end.** The file tools' read/write effects carry path
  information, and an out-of-workspace path under bypass prompts in an
  interactive session and is denied non-interactively — exactly the `default`
  gate, so bypass is never *weaker* than `default` (ADR-0070; it previously
  hard-denied with no way to approve). Shell commands carry no path
  information: bypass auto-allows every command class, and a command's own
  file access is not contained (its working directory is the workspace root,
  nothing more). Treat bypass as full shell access for the model.
- **Standing read grants: `[permissions] extra_read_roots`.** Directories
  listed there (absolute paths, canonicalized at startup) are treated like
  in-workspace paths for *read* effects only, in every profile and in
  non-interactive runs — the config-file counterpart of approving the same
  read prompt every session. Writes under a granted root keep the workspace
  boundary, and secret-like reads keep their own gate. A listed directory
  that cannot be canonicalized (typically: it does not exist) is reported at
  startup and skipped, never silently widened. A denied out-of-workspace
  access names this key, the interactive prompt, and `--permission
  unrestricted` in its error, so the denial is actionable (ADR-0070).
- **The containment root and the spawn working directory are distinct
  spellings of the same directory.** The sandbox canonicalizes the workspace
  root to a verbatim extended-length path (`\\?\…` on Windows); that verbatim
  form is the security boundary — every contained-path check (`starts_with`)
  uses it, and it is never weakened. A child process cannot use a verbatim path
  as its working directory, so spawns use a de-verbatim equivalent of the *same*
  directory (`dunce::simplified`, which keeps the verbatim form whenever it
  cannot be safely shortened). De-verbatim never widens containment: it changes
  only the cwd spelling handed to a child, not the boundary the path checks
  enforce.
- Harness rule verdicts still apply on top of the permission profile. A profile
  controls prompting, not the harness correctness gates.

Bypass removes the main safety net against model-initiated destructive actions,
and unrestricted additionally removes the workspace boundary for the file
tools. Both should be used only in disposable or sandboxed environments, or
where the user has deliberately accepted the risk.

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

## Self-Improvement Patch Generation

The self-improvement loop's write half (ADR-0034) is built so the human gate is
structural, not a convention:

- **No main-branch write without a human.** A proposed change is produced inside
  an isolated git worktree on its own branch; the only operation that writes
  outside that worktree (promotion onto the main branch) requires an approval
  token that authorizes exactly that patch. The token's only constructor is an
  explicit human-confirmation call — the autonomous loop has no path to mint one,
  so it can never self-merge.
- **Conservative promotion.** Promotion refuses a dirty target working tree,
  fast-forwards only (never silently creates a merge or resolves conflicts), and
  **never pushes**.
- **No shell, no network in the git surface.** Every git invocation passes its
  arguments as an argv array directly to `git` — there is no shell and no string
  interpolation of model input, so an edit path or branch name can never become
  another command. No `push`/`fetch`/`pull`/remote subcommand appears anywhere in
  the patch-generation crate.
- **Path containment.** Edit paths are joined under the worktree with a guard
  that rejects absolute paths, `..` traversal, and drive prefixes; an edit can
  only land strictly inside the worktree.
- **Scope-bound.** A proposal may touch only the files the finding named; both
  the declared edits and the produced diff are checked against that set.

## Outward Draft Emission

The loop's **outward** half (ADR-0053) lets the agent author a **draft** issue/PR
describing a proposed improvement and, only with an explicit human approval,
publish it to an allowlisted repo as a draft. It carries the same structural gate
as patch promotion:

- **No publish without a human.** Authoring or persisting a draft mints no
  approval token and touches no network. The only operation that yields a runnable
  publish plan requires the same value-typed approval token used to promote a
  patch, and that token's sole constructor is the explicit `--approve` path. The
  autonomous loop has no path to mint one, so it can propose a draft but never
  publish — a standing test pins this.
- **Default-off, fail-closed allowlist.** A draft can only be built (let alone
  published) when `[self_improvement] enabled = true` **and** its `owner/repo`
  target is on the `outward_targets` allowlist. Both ship off; an un-allowlisted or
  disabled target is refused at propose time, before any draft is written, and the
  allowlist is re-checked again at emit time.
- **Draft-only, never promote.** Publication runs `gh issue create` /
  `gh pr create --draft` only. The constructed argv can never carry `ready`,
  `merge`, `--web`, or an edit/comment/close on an existing item — the builder
  cannot produce them and a test asserts it. There is no path to mark a PR ready,
  merge it, or comment on others' issues.
- **Dry-run by default.** Without `--approve`, `emit-draft` prints the exact `gh`
  plan it *would* run and publishes nothing. The human reviews the plan (and the
  resolved `gh` account, surfaced from a `gh auth status` preflight) before
  approving.
- **Redacted, locally inspectable.** The draft title/body are redacted with the
  shared workspace redactor at construction, so a secret never reaches the
  project-local `.localpilot/outward/` store even before publish. The body carries
  the change provenance (the finding, its source, the rationale) so a published
  draft is traceable. Every emit appends a redacted, token-free lifecycle event
  (proposed → approved → published, with the resulting URL).
- **No shell.** The `gh` arguments are passed as an argv array, never a shell
  string, so a redacted title or body can never become another command.

## Web Research Egress

Research (ADR-0060, amended by ADR-0076) gathers from the repo's ingested
knowledge and accepted memory — read-only, on-machine — **and the web, on by
default**: a local model's parametric memory cannot carry a research run, so
reach is the default and the boundary does the protecting. This is a ratified,
documented exception to the default-off rule of the ecosystem remote-egress
policy (`policies/remote-egress.md`); the policy's other four rules hold by
construction:

- **Disableable, twice.** `--no-web` skips the web source for one run — no
  fetch, and no URL-proposal model call. `[research.web] enabled = false`
  removes the entire outbound path and is the absolute kill switch — no flag
  can override it (the consent grant is a no-op against config-off, enforced
  and tested).
- **Loud disclosure on every web-active run, both surfaces.** The subcommand
  and interactive `/research` print the same egress disclosure before any
  request: the default-on posture and both off-switches, what is sent (only
  the redacted sub-question), the effective reach, blocked domains, and the
  audit-log path. The per-session consent is recorded after the disclosure
  and is **never persisted**.
- **Allowlist-gated.** Each candidate URL's host is parsed with a real URL
  parser and checked against `[research.web] allowlist`. Unset means `["*"]`
  — the open web — while an **explicitly** empty list means every host needs
  confirmation, so nothing is fetched (unset and empty are different user
  statements). An allowed host is fetched (bounded bytes and timeouts); every
  other host is **skipped and logged**, never fetched. `disallowlist` beats
  the allowlist, `*` included (ADR-0068).
- **Redirects are never followed.** The fetch client uses a no-redirect policy,
  so an allowlisted host that returns a 3xx cannot bounce the request to a
  non-allowlisted (or internal) host. A redirect is audited
  (`decision=redirect-not-followed`) and yields no evidence — the allowlist is a
  true egress boundary, not just a first-hop check.
- **Only the sub-question reaches search servers and web hosts.** The
  outbound text is the sub-question passed through the shared workspace
  redactor — never gathered evidence, file contents, or memory. The redactor
  is a second guard over the topic the user typed. This holds for designated
  MCP search tools too (ADR-0077): a search call sends the redacted query
  only, is audited like a fetch, and its results are candidate URLs that
  still pass this gate — a search result never becomes evidence directly.
  One deliberate carve-out (ADR-0087): bounded *fetched web content* — public
  pages this run just downloaded, nothing from the workspace — is sent to the
  **user's own configured model** for a strict relevance classification. That
  is the same model that already sees the session; no new destination exists,
  and a rejected page is audited (`rejected-low-relevance`).
- **Auditable.** When active, every outbound request and every skip appends one
  line to the audit log (`[research.web] audit_log`, default
  `.localpilot/research/egress-audit.log`): the decision, host, URL, and the
  redacted sub-question — metadata and the redacted question only, never content.
- **Findings stay review-gated.** Web-derived findings flow through the same
  provenance cross-check and review queue as local ones; nothing a fetch produced
  is written to accepted memory without human promotion.

A sample audit line:

```
decision=allowed host=docs.rs url=https://docs.rs/tokio question=how does tokio schedule tasks
```

### Two different network surfaces

Research web egress (above) is the **allowlisted, audited, redacted** surface —
on by default since ADR-0076, disclosed on every run, and disableable per run
(`--no-web`) or globally (`enabled = false`). It is *not* the only way the
agent can touch the network, and the others are deliberately looser — know the
difference:

- **The builtin `fetch` tool** carries `Effect::Network` and no host allowlist:
  under the `relaxed` permission profile an allowlisted `fetch` reaches **any**
  host without the research path's per-host audit. It is a general fetch, gated
  by the permission engine, not by the research allowlist.
- **MCP tool servers** are gated as `Effect::Network` when invoked, but a stdio
  MCP server is a local process that can do anything its own code does — the gate
  covers *invoking the tool*, not what the server then reaches. Trust an MCP
  server as you would any dependency you run.

So "may the agent touch the network?" has two honest answers: the research path
is a true audited allowlist boundary; `fetch`/MCP are permission-gated but
host-agnostic. Choose the profile and allowlist accordingly.

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
