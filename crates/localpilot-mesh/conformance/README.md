# Conformance suite

A language-neutral description of how a pair-programming mailbox must behave,
as fixtures any implementation can be run against. `pair.py` is the reference
implementation and passes every fixture. `run.py` is its runner, and
`self-test.py` runs it.

```sh
python run.py                         # every fixture against pair.py
python run.py --profile two-party     # the Claude+Codex-only profile
python run.py fixtures/<name>.json    # one fixture
python run.py --impl localpilot=<cmd> # mixed run: that role's steps use <cmd>
```

The exit code is 0 only when every selected fixture passes.

## Two layers

- **STATE** binds every implementation. After **every** command step (`state`)
  and at the end (`final_state`), the normalised mailbox tree must match: every
  file under `.pair-programming/`, JSON parsed, journals one entry per line.
  After concurrent writes the exact tree is not deterministic, so a `parallel`
  step's state contract is its invariants plus exact journal counts. No exact
  state check may follow a `parallel` step, and such a fixture has no
  `final_state`.
- **CLI** binds only an implementation that claims the `cli` profile, i.e. one
  that is a drop-in for `pair.py` on the command line. It covers each step's
  exit code, stdout and stderr.

A native library (for example LocalPilot's) maps each step's arguments onto its
own API and must pass STATE. It passes CLI only if it ships a compatible
command line.

## Fixture format

```json
{
  "id": "two-party-handoff",
  "profiles": ["two-party"],
  "layers": ["cli", "state"],
  "setup": {"vcs": "git"},
  "provenance": {"build": "<commit>:pair.py", "sha12": "...", "note": "..."},
  "steps": [ ... ],
  "final_state": { ... }
}
```

A step is exactly one of:

- `{"cmd": [...], "rc": 0, "stdout": "...", "stderr": "...", "state": {...}}`:
  run a public command. The runner alone sets `--repo <fixture root>`, and
  replaces `<SID>` in an argument with the active session's id. It
  refuses a step that does not start with a subcommand, and any argument that is
  `--repo` or a prefix argparse would expand to it (for example `--rep=`), even
  as another option's value. `compare` may be:
  - `"entries"` for `transcript`, compared as a set of entries;
  - `"blocks"` for peer mail from several senders, compared as a set of
    messages plus each sender's own order.

  Mail from different senders is ordered by a one-second timestamp and then by
  role, so how they interleave may differ between runs. Each sender's own order
  may not.
- `{"raw": {"op": ..., "path": ..., "data": ...}}`: a direct file operation for
  crash-point and malformed-file setups that no command can produce. `op` is
  one of `append_bytes`, `write_file`, `truncate`, `remove`, `make_lock`.
  `data_hex` in place of `data` writes exact bytes, for damage text cannot
  express, such as a multi-byte character cut in half. `make_lock` with
  `age_s` backdates the lock file's modification time by that many seconds, to
  model a lock left by a dead process.
  `json_set` (with `key` and `value`) sets one top-level key of an existing
  JSON object file, as a newer build or damage would leave it.
  `path` is relative to `.pair-programming/`, and `<SID>` stands for the active
  session's id. `"expect_refused": true` asserts that the runner refuses the
  step.
- `{"parallel": [[...], ...], "invariants": [...], "journal_counts": {...}}`:
  run the commands at once and check only invariants, never exact output. The
  invariants are:
  - `rc_zero`: every command succeeded;
  - `no_invalid_lines`: every journal line is a JSON object with a
    terminator;
  - `journals_contiguous`: each journal holds seq 1..n exactly once.

  `journal_counts` gives the number of records each role's journal must hold.
  A key such as `receipts/codex` or `pushes/claude` counts that facts file
  instead. `receipts_unique` requires one receipt per `(msg_id, generation)`.
  A `parallel` step may carry `env` as well.

A `cmd` step may also carry:

- `capture`, `{NAME: regex}`: the first group of each regex, matched against
  the step's raw stdout, is kept as `NAME`;
- `env`, `{VAR: "...${NAME}..."}`: passed to the step, with captured values
  substituted.

Only `PAIR_ENDPOINT_TOKEN` may be set this way, never `PAIR_REPO`. Tokens and
other 64-hex values normalise to `<HEX64>`.

### Normalisation

Only what differs between two runs of the same build is replaced: the fixture
root (`<REPO>`), session ids (`<SID>`), times (`<TS>`), unit-id suffixes
(`1-<U>`, and `#1` where shown), and commit ids (`<SHA>`). The base commit uses
a fixed date, so it is the same on every run. Line endings compare as `\n`.
In the STATE layer the session the pointer names is `<SID>`; any other
session in the mailbox is `<SID-2>`, `<SID-3>`, ordered by its `created_at`
and work unit, so two sessions never merge under one key.

### Containment

Every fixture runs in a fresh repository under the system temp directory, which
is deleted afterwards. The runner refuses to start if that directory is inside
the invoking directory or a git work tree. It never passes `PAIR_REPO` to a
step, and no step can name its own `--repo`.

A fixture that claims a layer must carry that layer's checks: `rc`, `stdout` and
`stderr` for CLI, and `state` plus `final_state` for STATE. Otherwise it is
rejected before it runs.

Before any file I/O, a `raw` path is refused when it:

- is absolute, has a drive or UNC prefix, is empty or holds a NUL;
- has a `..` segment;
- crosses a symlink, junction or other reparse point;
- resolves outside the mailbox.

`remove` and `truncate` refuse directories.

## Adding a fixture

1. Write the steps only (commands with no expected values) in
   `fixtures/<id>.json`, with `profiles` and `layers`.
2. Capture the expected values from the reference:
   `python run.py --capture fixtures/<id>.json`. This fills `rc`, `stdout`,
   `stderr`, `final_state` and `provenance`.
3. **Read every captured value** and confirm it is the behaviour the protocol
   intends, not merely what the build did. A capture is never regenerated with a
   build under test; the expected values change only by a reviewed edit.
4. Run the fixture three times. It must pass every time.

A deliberate behaviour change keeps both the old and the new expectation, so
the change stays visible in review:

- `python run.py --capture-legacy <old pair.py> --legacy-label <commit>:pair.py
  <fixtures>` records, for each step where the older build behaves
  differently, a `legacy` block with its `rc`, `stdout` and `stderr`. It also
  sets the fixture's `legacy_build`. A crash is stored as the exception's last
  line only (`"stderr_compare": "last_line"`), since the traceback frames name
  the old build's path.
- `python run.py --check-legacy <old pair.py> <fixtures>` runs each fixture's
  legacy view (CLI layer only) against that build. `self-test.py` does this for
  every `legacy_build`, taking the old file from git history, so the recorded
  old behaviour is proven, not only asserted.

## The participant profile

An implementation that joins sessions but does not create or retire them (for
example LocalPilot's native mailbox) claims the **participant profile**:

```sh
python run.py --participant codex=<command> --participant localpilot=<command>
```

- Each named role's steps, and every step that names no role (`status`,
  `transcript`), run on `<command>`. The other roles run on the reference.
- A fixture is selected only when those steps use the participant operations
  (`participant.json` lists them). Every other fixture is reported as
  `SKIP <id>: <reason>`, and the run ends with
  `SELECTED s / TOTAL t / SKIPPED k / FAILED f`.
- `participant.json` also lists the **mandatory** fixtures. It is generated
  from the selection rule and checked in, so a change to it is reviewed. A
  mandatory fixture that is skipped or not run fails the run
  (`MANDATORY_NOT_RUN <id>`).
- Checking uses the STATE layer plus **obs** instead of the full CLI layer:
  - the exit code of every step;
  - peer mail from `watch` and `peek`, compared as blocks;
  - the `ENDPOINT`, `ENDPOINT_TOKEN`, `ACCEPTED` and `PUSH_RECORDED` lines;
  - for a `status` that succeeds, a required subset: the `SESSION`,
    `AUTHORITY`, `PAUSE`, `ENDPOINT`, `JOURNAL_INVALID` and `WAITING` lines,
    and one health line per participant. For a `status` that refuses, the
    stdout lines it printed before refusing are not part of the contract;
    its exit code and any `REFUSED` or `WRITE_DENIED` line still are;
  - any `REFUSED` or `WRITE_DENIED` line.

  A self-test proves that an implementation with correct state but broken
  output fails.

## Vendoring

`python vendor.py <dest>` copies the runner, README, `participant.json`,
fixtures and a pinned, test-only copy of the reference implementation
(`reference/pair.py`, which the runner uses when present; `--reference`
overrides it) into another repository with `MANIFEST.json` (the source commit and
the sha256 of each file). `python vendor.py --check <dest>` reports any
changed, missing or unlisted file.

## Claiming conformance

An implementation conforms to a profile when every fixture tagged with that
profile passes on the STATE layer. It also passes CLI if it claims the `cli`
profile. Record the suite's commit and the command in the claim. The
`two-party` profile is the Claude+Codex-only behaviour, and it must stay green
in every repository that implements the protocol.

A copy of the fixtures vendored into another repository carries a manifest: the
source commit and the sha256 of each file. It fails its own check when the
manifest and the files disagree.
