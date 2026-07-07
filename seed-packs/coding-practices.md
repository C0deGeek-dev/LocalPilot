# Coding practices reference

Long-form companion to `coding-lessons.json`. Ingest it (`localpilot ingest run`)
to make it reachable when the model calls `knowledge_search`. Headings are phrased
the way a model would query them.

## How to debug a wrong result

Start by reproducing the failure and recording the exact command, environment,
and output. Form two or three concrete hypotheses for the cause, then run the
single cheapest test that tells them apart — the same input on a different
backend, with randomness off, or on a smaller case — before changing any code.
Read the first error in a failed build, not the last, because later errors are
usually cascades. If a change "should work" but does not, question the
environment (driver, runtime, version), not only the code: a correct program on a
broken substrate still fails. Do not conclude from correlation; rule out the
confound, and do not present a guess as a finding until the discriminating test
has run.

## How to write and change code

Match the surrounding file's style, naming, and altitude so new code reads as if
the original author wrote it. Prefer guard clauses and small extracted helpers
over deep nesting; a branch-heavy function is a signal to simplify. Keep changes
small and cohesive, and when a change crosses a shared boundary such as a
submodule or a generated interface, rebuild the consumer — code that compiles in
isolation can still break its caller. Make safety structural rather than
procedural: encode an invariant in the type system so the compiler enforces it,
instead of relying on a reviewer to notice.

## How to test

A test pins one observable behaviour and prevents a named regression; if you
cannot name the bug it prevents, delete it. Pin the contract, not implementation
details like private call shapes or exact log strings, so a behaviour-preserving
refactor keeps the tests green. Coverage percentage is a smell detector that
flags untested paths, not a target to chase.

## How to use tools well

Check whether a target already exists or is already running before creating or
launching your own — inspect the directory, hit the health endpoint, or list the
processes. Keep tool-call arguments valid and minimal; prefer the smallest first
call that makes progress over a large speculative one, and split a write that is
too big for a single call — and decompose a large implementation into several
small, modular files instead of one huge file. Verify the exit code of the
command you care about, not a pipe's tail.

## How to finish a unit of work

Checkpoint after each coherent unit: build, test, commit, push. Update the
user-facing docs (README, CHANGELOG, configuration reference) in the same change
as the behaviour they describe. Unpushed work is not done — someone must be able
to resume from it.
