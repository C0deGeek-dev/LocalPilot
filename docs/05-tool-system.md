# Tool System

## Purpose

Tools are the only path from model output to local side effects. Every tool call
must pass through schema validation, permission policy, execution, and result
normalization.

## Tool Trait

```rust
#[async_trait]
pub trait Tool: Send + Sync {
    fn name(&self) -> &str;
    fn description(&self) -> &str;
    fn schema(&self) -> serde_json::Value;
    fn effects(&self, invocation: &ToolInvocation) -> ToolEffects;
    async fn invoke(&self, invocation: ToolInvocation) -> anyhow::Result<ToolOutput>;
}
```

Builtin tools normally return static string literals. Dynamically discovered
tools, such as MCP tools, may return borrowed metadata from owned registry
entries; dynamic metadata must not be forced into a static lifetime.

## Builtin Tools

### `read_file`

Reads UTF-8 text from a workspace path.

Rules:

- deny paths outside workspace unless approved
- deny secret-like files by default
- support line ranges
- cap output size

### `write_file`

Writes a new file or replaces an existing file.

Rules:

- require approval for overwrite until trust is established
- create parent directories only inside workspace
- preserve newline style where possible

### `edit_file`

Applies structured edits.

Rules:

- reject ambiguous edits
- require exact old text or AST-aware operation
- show diff before approval when interactive

### `list_files`

Lists files under a workspace path.

Rules:

- respect ignore files
- cap result count
- include hidden files only when requested

### `search_text`

Searches text using ripgrep when available.

Rules:

- respect ignore files by default
- cap matches
- never traverse outside workspace without approval

### `multi_edit`

Applies several exact-text replacements to one file atomically; rejects
missing or ambiguous context before writing anything.

### `apply_patch`

Applies a structured multi-file patch: create, update (exact-match hunks), and
delete operations, expressed as typed JSON generated from the input schema.

Rules:

- the whole patch is validated against the current tree before any write;
  a rejected hunk fails the call with an operation- and hunk-named error
- every touched path passes the same workspace containment as `write_file`
- the approval prompt previews the operation list

### `find_files`

Finds files by name pattern, respecting ignore files; capped results.

### `read_tool_output`

Reads back the full retained output of an earlier tool call that was truncated
in context, by its retention id, optionally a line range. No new side effect:
the output was captured under the permission decision that produced it.

### `run_shell`

Runs a shell command.

Rules:

- classify command risk
- approve writes, deletes, network, package installs, and privileged commands
- set timeout
- capture stdout/stderr separately
- never chain destructive commands generated from untrusted path lists

### quality-gate checks

The harness quality gate ([`docs/06`](06-harness-spec.md)) runs its ratified
`[[harness.checks]]` commands through `run_shell` — not a side channel. A check
command is classified, permission-checked, timed, and captured like any other
shell command; ratification at setup records the command, it does not exempt it
from the engine. Auto-fix invocations (`fix_command`) are project-write side
effects and are mediated the same way.

### `git_status`

Reads repository state.

Rules:

- read-only
- allowed by default inside workspace

### `git_commit`

Creates commits for harness steps.

Rules:

- pre-commit rules must pass
- message must not contain secrets
- include only intended files

## Permission Model

Decision:

- `Allow`: run immediately
- `Ask`: prompt user
- `Deny`: block and return model-visible error

Inputs:

- tool name
- normalized path
- command classification
- workspace trust
- interactive/non-interactive mode
- user policy
- harness rule state

## Result Model

Tool result text must be:

- bounded
- deterministic enough for tests
- explicit about truncation
- free of secrets where redaction is possible

Oversized output is bounded at the dispatch chokepoint after redaction: the
head and tail stay in context with an explicit truncation note, and the full
redacted output spills to the retention store under the call id, where
`read_tool_output` can fetch it.

## Tool Gates

Dispatch accepts an ordered chain of tighten-only gates consulted *after* the
permission engine. A gate may block a call with a model-visible reason; it can
never grant what the engine refused. The permission engine is the always-on
first link of the chain and is not removable. Hosts register gates through the
session runtime's hook fabric (see [`docs/extending.md`](extending.md)).

## Safety Invariants

- The model cannot execute a tool outside the registry.
- The model cannot bypass permission policy.
- The harness cannot bypass permission policy.
- A gate can only tighten, never loosen, a permission outcome.
- Tool outputs are stored only after redaction.
- A failed tool call is represented as data, not a process crash.
- A cancelled tool execution is aborted (child processes killed), answered
  with a synthesized error result, and recorded in the session event log.
