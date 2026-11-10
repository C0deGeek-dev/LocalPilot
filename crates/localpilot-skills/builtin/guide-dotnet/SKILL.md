## .NET
- Find the project the change belongs to and its target framework. Check `global.json` for the SDK the repository pins; do not assume the newest.
- Build and test the intended project or solution, not whichever one is found first.
- Use the test runner the test project references (xUnit, NUnit or MSTest) through `dotnet test` on that project.
- A test run that discovers no tests, or tests in another project passing, does not verify this change.
- Do not add packages or change the SDK version unless the task needs it, and say so when it does.
