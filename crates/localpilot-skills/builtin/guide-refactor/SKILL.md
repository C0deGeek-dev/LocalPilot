## Procedure
- Write down the behaviour that must not change and the structural change that is wanted.
- Find the callers and the public contract of what you move or rename, including configuration keys, serialized formats and command-line names.
- Run the relevant tests before you start, so a failure afterwards means something.
- Change structure only. If you find a bug, report it and keep its fix separate unless the user asked for both.
- Run the same tests afterwards and compare. Say plainly if a behaviour did change.
