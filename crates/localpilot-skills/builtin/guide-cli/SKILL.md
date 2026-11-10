## Command-line programs
- Behaviour is what the built executable does with real arguments: its exit code, its standard output and its standard error.
- Check the actual executable, not only an internal helper function.
- Cover the invalid inputs the change is about as well as a valid one, and check that valid output did not change.
- Interactive terminal behaviour needs a real terminal session; buffered command output does not show it.
