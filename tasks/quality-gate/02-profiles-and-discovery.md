# 02 — Toolchain profiles + discovery

## Goal
The fixed abstraction (built-in profiles) plus discovery that detects the stack,
probes available tools, and *proposes* a gate. No execution, no auto-adoption.

## Boxes

- [ ] **02.1** (agent) Define the profile abstraction: a `ToolchainProfile`
      (data + small trait) declaring default checks, finding interpretation, and
      auto-fixability — original code, the stack-neutral seam (§6.6).
- [ ] **02.2** (agent) Built-in **Rust** profile: fmt (step, auto_fix true),
      clippy (step, safe), test (phase), machete (phase), audit (phase, block).
      Commands are profile data, not engine literals.
- [ ] **02.3** (agent) Second built-in profile to prove generality (PowerShell:
      PSScriptAnalyzer; or Node: prettier/eslint/test). Pick in 00.6; record why.
- [ ] **02.4** (agent) Stack detection (marker files: `Cargo.toml`,
      `package.json`, `*.psd1`/`*.ps1`, …) + tool probing (is the tool on PATH?).
      Cross-platform PATH probe; no command executed beyond a version/help probe
      classified read-only.
- [ ] **02.5** (agent) Produce a *proposed* `Vec<CheckConfig>` from detected
      profile ∩ available tools. Mark each proposed check's command class; surface
      `destructive`/`privileged`/`network` for ratification (§6.5). Tests:
      detection per marker; proposal excludes absent tools; nothing runs.

## Hindsight checkpoint
- [ ] Captain Hindsight review recorded
- [ ] Verdict is `CLOSE`

## Progress log
