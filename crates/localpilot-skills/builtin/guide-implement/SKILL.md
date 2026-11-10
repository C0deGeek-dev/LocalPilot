## Procedure
- Name the behaviour that is asked for and the component it belongs in.
- Reuse what is there: the component's structure, naming, error handling, dependencies and test style. Do not add a parallel way of doing the same thing.
- Make one coherent change. Keep unrelated cleanup out of it.
- Then look for evidence that fits the behaviour: a test that exercises it, the program's actual output, or a request to the running service. Use the checks the project already has.
- Report what changed and what was actually checked. A successful build is not a behaviour check.
