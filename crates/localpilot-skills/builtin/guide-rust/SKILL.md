## Rust and Cargo
- Read the package manifest, the workspace membership and any toolchain pin (`rust-toolchain.toml`) before choosing a command.
- Work on the affected package and target (`-p <package>`, `--bin`, `--lib`, `--test`), with the features the project requires.
- The project's configured checks are authoritative over generic ones.
- For a defect, write or run a focused test that fails before the change and passes after it.
- `cargo check` and `cargo build` show that code compiles, not that it behaves. A filtered `cargo test` that reports 0 tests ran checked nothing.
- Record the package, target and features you used.
