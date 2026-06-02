# 04 — Tools and Sandbox / Permissions

## Goal
> Phase 3 (`docs/03`) + the security/permission layer (`docs/05`, `docs/07`).
> Tools are the only path from model output to local side effects; every tool
> call passes schema validation → permission policy → execution → result
> normalization. Implement the eight builtin tools, the path/containment policy,
> per-OS command classification, and the permission engine with three profiles.
> Local side effects live only in `unshackled-tools`; permission decisions live
> only in `unshackled-sandbox` (`docs/13` §2). Windows + POSIX are equal tier-1
> (ADR-0007).

## Boxes
> ID = `04.<box-number>`. All agent-owned.

- [x] **04.1** (agent) Implement the tool registry holding
      `Vec<Box<dyn Tool>>` (object-safe trait, `docs/05`/`docs/13` §6) with
      dispatch by name; the model cannot call a tool outside the registry
      (safety invariant, `docs/05`). (Verified: registry dispatch test; unknown
      tool returns a typed error as data, not a panic.)
- [x] **04.2** (agent) Implement **typed-struct → JSON Schema** generation per
      tool with `schemars` so schema and deserialized input cannot drift
      (`docs/13` §3). One generation path owned by `unshackled-tools`. (Verified:
      schema snapshot test per tool; bad input deserializes to a typed error.)
- [x] **04.3** (agent) Implement workspace **path policy** in
      `unshackled-sandbox`: canonicalize + normalized `starts_with` containment,
      handling Windows `\\?\` verbatim prefixes, case-insensitivity, 8.3 short
      names, ADS (`file.txt:stream`), drive roots, UNC, junctions, symlinks; and
      POSIX `..`, absolute roots, symlinks (`docs/07` Platform Policy, `docs/13`
      §7). A naive string `starts_with` is a security bug. (Verified: proptest +
      enumerated path-escape tests for both OS families, `#[cfg]`-gated and run
      on their OS.)
- [x] **04.4** (agent) Implement `read_file` (`docs/05`): UTF-8 from a workspace
      path; deny paths outside workspace unless approved; deny secret-like files
      by default; line ranges; capped output with explicit truncation marker.
      (Verified: `docs/08` Tools tests — read in workspace; deny read outside
      workspace.)
- [x] **04.5** (agent) Implement `write_file` (`docs/05`): require approval for
      overwrite until trust established; create parent dirs only inside
      workspace; preserve newline style; temp-then-rename atomic write
      (`docs/13` §5). (Verified: write in workspace; deny write outside
      workspace; overwrite prompts.)
- [x] **04.6** (agent) Implement `edit_file` (`docs/05`): reject ambiguous
      edits; require exact old-text match (or AST-aware op); produce a diff for
      approval when interactive. (Verified: `docs/08` Tools tests — edit exact
      match; reject ambiguous edit.)
- [x] **04.7** (agent) Implement `list_files` (respect ignore files, cap count,
      hidden only when requested) and `search_text` (ripgrep when available,
      respect ignore files, cap matches, never traverse outside workspace
      without approval) (`docs/05`). Use native Rust FS APIs, not string-built
      shell (`docs/07` Windows). (Verified: ignore-file + cap + containment
      tests.)
- [x] **04.8** (agent) Implement command **risk classification** for `run_shell`
      shared across OSes, with per-OS rule sets: read-only, project-write,
      external-write, network, destructive, privileged, unknown (`docs/07` Shell
      Policy). (Verified: classification unit tests + proptest on adversarial
      command strings.)
- [x] **04.9** (agent) Implement **Windows** command classification (`docs/07`
      Windows, `docs/13` §7): classify PowerShell / `cmd.exe` / direct exe
      separately; detect destructive PowerShell (`Remove-Item -Recurse`); treat
      registry writes as privileged; `%VAR%` syntax; no string-built FS commands.
      (Verified: `#[cfg(windows)]` tests for destructive + privileged detection,
      run on Windows CI.)
- [x] **04.10** (agent) Implement **POSIX** command classification (`docs/07`
      POSIX): detect `rm -rf`-class destructive patterns; treat `sudo`/`doas` as
      privileged; `$VAR` syntax; distinguish workspace-local vs external writes;
      do not hardcode `/bin/sh`. (Verified: `#[cfg(unix)]` tests run on
      Linux+macOS CI.)
- [x] **04.11** (agent) Implement `run_shell` execution (`docs/05`): argument
      lists not shell strings (`docs/13` §7); timeout; stdout/stderr captured
      separately; never chain destructive commands built from untrusted path
      lists; result bounded + redacted. (Verified: `docs/08` Tools tests — shell
      read-only allowed; shell destructive denied in non-interactive mode.)
- [x] **04.12** (agent) Implement `git_status` (read-only, allowed by default in
      workspace) and `git_commit` (pre-commit rules must pass — interface hook
      for subject 06; message must not contain secrets; only intended files)
      (`docs/05`). (Verified: git_status round-trips on a temp repo; git_commit
      rejects a secret-bearing message.)
- [x] **04.13** (agent) Implement the **permission engine** in
      `unshackled-sandbox`: decision `Allow`/`Ask`/`Deny` from inputs (tool name,
      normalized path, command class, workspace trust, interactive vs
      non-interactive, user policy, harness rule state) and the
      interactive/non-interactive default table (`docs/07` Shell Policy table,
      `docs/05` Permission Model). Approval interface (trait) for prompting,
      with a scriptable test impl. The model and harness MUST NOT bypass it
      (safety invariants). (Verified: scripted-decision tests for each class ×
      interactivity; a "harness cannot bypass" test.)
- [x] **04.14** (agent) Implement the three **permission profiles** (`docs/07`,
      `docs/01`): `default` (least privilege), `relaxed` (user-defined allowlist
      auto-approves common safe actions; rest prompt), `bypass` (launch mode,
      no prompts, never default, must be set explicitly, always shown in
      footer/status). Per-profile behavior must be explicit (`docs/07`
      Permission Profiles): under `default` and `relaxed` the `secret_file_guard`
      **prompts** before reading/editing secret-like files (`.env`, private keys,
      credential stores, token-bearing cloud config); under `bypass` nothing
      prompts (it approves everything). `bypass` does NOT disable redaction,
      logging, or the workspace boundary — those stay on unless separately and
      explicitly disabled. Workspace-trust prompt on first open (`docs/07`
      Workspace Trust). (Verified: profile-selection tests; under default/relaxed
      a secret-like read prompts; under bypass the same read does not prompt yet
      a test asserts bypass still redacts output AND still enforces the workspace
      boundary.)

## Progress log
> One line per slice. Date · slice · box IDs · what shipped · how verified.

- 2026-06-02 · slice 1 · 04.3, 04.8–04.10, 04.13 · `unshackled-sandbox`: workspace
  path containment (lexical `..` normalization + existing-prefix canonicalize for
  symlink/8.3/case/verbatim safety; naive `starts_with` avoided); per-OS command
  risk classification (`classify_posix`/`classify_windows` pure + cfg `classify`)
  across read-only/project-write/external-write/network/destructive/privileged/
  unknown incl. PowerShell `Remove-Item -Recurse`, registry-as-privileged,
  `sudo`/`doas`, `rm -rf`; permission engine + 3 profiles + `Approver` with the
  `docs/07` class×interactivity table, untrusted-floor, and bypass that keeps the
  workspace boundary. Verified: 18 tests — path-escape (incl. unix symlink /
  windows root cfg-gated), classification (+adversarial proptest), per-class
  decisions, secret reads prompt under default/relaxed, bypass denies
  out-of-workspace + a harness-cannot-bypass test; clippy(-D)/fmt clean.
- 2026-06-02 · slice 2 · 04.1, 04.2, 04.4–04.7, 04.11, 04.12, 04.14 ·
  `unshackled-tools`: object-safe `Tool` trait + `schemars` schema generation,
  registry with one permission-gated `dispatch` (authorizes every effect, then
  redacts every output — the single path to side effects) returning failures as
  data. Eight builtins: `read_file` (line ranges, cap, outside-denied),
  `write_file` (newline-preserving atomic temp-then-rename), `edit_file`
  (exact-unique match, rejects ambiguous), `list_files`/`search_text`
  (ignore-file-aware, capped, contained), `run_shell` (arg-list, timeout,
  separate stdout/stderr), `git_status`/`git_commit` (secret-bearing message
  rejected). Verified: 12 tests incl. the `docs/08` Tools MVP set, schema
  snapshot, and a bypass-still-redacts-and-keeps-boundary test; clippy(-D)/fmt/
  deny green. Note: pinned `globset 0.4.16` (0.4.18 needs `edition2024`).
