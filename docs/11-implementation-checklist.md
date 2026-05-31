# Implementation Checklist

## Foundation

- [ ] Replace repository URL placeholders.
- [ ] Add GitHub Actions.
- [ ] Add `cargo-deny` CI step.
- [ ] Add `cargo-audit` CI step.
- [ ] Add changelog.
- [ ] Add contributor guide.

## Core

- [ ] Add typed IDs for sessions, turns, and tool calls.
- [ ] Add message metadata.
- [ ] Add thinking/reasoning content/event model.
- [ ] Persist and replay reasoning signatures/provider metadata when required.
- [ ] Add usage accounting model.
- [ ] Add structured error hierarchy.

## Config

- [ ] Implement config loading.
- [ ] Implement user config path resolution.
- [ ] Implement project config path resolution.
- [ ] Implement env overrides.
- [ ] Implement config diagnostics.

## Providers

- [ ] Implement provider trait fully.
- [ ] Implement local OpenAI-compatible provider.
- [ ] Implement official hosted provider.
- [ ] Implement provider registry.
- [ ] Implement mock provider for tests.
- [ ] Add stream parser tests.
- [ ] Add quota/rate-limit classification.
- [ ] Add reset-window metadata model.
- [ ] Add provider capability declarations.
- [ ] Add reasoning/thinking event translation.
- [ ] Add reasoning round-trip tests for tool-use loops.

## Tools

- [ ] Implement tool registry.
- [ ] Implement path policy.
- [ ] Implement read tool.
- [ ] Implement write tool.
- [ ] Implement edit tool.
- [ ] Implement search tool.
- [ ] Implement shell tool.
- [ ] Implement git tools.
- [ ] Add approval interface.

## Modes and Permissions

- [ ] Implement agent mode loop (no rule engine).
- [ ] Implement harness mode entry paths (ground-up, single task, adopt existing).
- [ ] Implement mode switching at safe boundaries.
- [ ] Implement permission profiles (default, relaxed, bypass).
- [ ] Implement bypass launch flag with active-profile indicator.

## Harness

- [ ] Implement brief parser.
- [ ] Implement brief renderer.
- [ ] Implement progress parser.
- [ ] Implement progress renderer.
- [ ] Implement status command.
- [ ] Implement intake.
- [ ] Implement planner.
- [ ] Implement rules.
- [ ] Implement resume loop.
- [ ] Implement attempt logs.
- [ ] Implement replan loop.
- [ ] Implement context compaction strategy.
- [ ] Implement worker-loop trace events.
- [ ] Implement wait/resume after provider quota reset.
- [ ] Add unattended-resume safety gates.

## TUI

- [ ] Pick TUI crate stack.
- [ ] Implement message list.
- [ ] Implement input box.
- [ ] Implement streaming render.
- [ ] Implement approval modal.
- [ ] Implement status line.
- [ ] Implement always-visible footer stats.
- [ ] Implement optional thinking/reasoning side panel.
- [ ] Implement narrow-terminal panel collapse.
- [ ] Implement slash commands.

## Recovery

- [ ] Detect empty responses.
- [ ] Detect repeated-token loops.
- [ ] Detect slash floods.
- [ ] Skip false positives inside fenced code/log/base64 contexts.
- [ ] Detect malformed tool calls.
- [ ] Implement recovery retry ladder.
- [ ] Add hard budget for repair prompts.
- [ ] Persist recovery diagnostics.

## Skills

- [ ] Define skill manifest.
- [ ] Define trigger semantics.
- [ ] Implement project skill loading.
- [ ] Implement user skill loading.
- [ ] Implement skill validation.
- [ ] Implement skill permission declarations.
- [ ] Implement generated skill drafts.
- [ ] Add skill suggestion cooldowns.

## Memory

- [ ] Define flat local memory store format.
- [ ] Implement project memory.
- [ ] Add graph layer only after flat store is useful.
- [ ] Implement explicit global-memory consent.
- [ ] Implement memory inspect command.
- [ ] Implement memory delete command.
- [ ] Implement memory opt-out.
- [ ] Add memory redaction.
- [ ] Add memory relevance threshold and token cap.

## MCP

- [ ] Implement MCP client.
- [ ] Discover MCP tools.
- [ ] Route MCP tool calls through permission checks.
- [ ] Read MCP resources.
- [ ] Persist server configs and show server health.

## Evals

- [ ] Define golden-task fixture format.
- [ ] Add fake-provider eval runner.
- [ ] Track task success rate.
- [ ] Track turn/tool/token counts.
- [ ] Add optional live-provider eval mode.

## Store

- [ ] Implement transcript format.
- [ ] Implement atomic writes.
- [ ] Implement session index.
- [ ] Implement redaction.
- [ ] Implement export command.
- [ ] Persist memory store.
- [ ] Persist skill drafts.
- [ ] Persist quota wait/resume records.

## Security

- [ ] Implement secret detection.
- [ ] Implement command classification.
- [ ] Implement Windows PowerShell/cmd command classification.
- [ ] Implement POSIX shell command classification.
- [ ] Add Windows path escape tests.
- [ ] Implement workspace trust prompts.
- [ ] Implement non-interactive denial policy.
- [ ] Add destructive command tests.

## Release

- [ ] Add install docs.
- [ ] Add alpha release checklist.
- [ ] Run clean-room audit.
- [ ] Tag `v0.1.0-alpha.1`.
