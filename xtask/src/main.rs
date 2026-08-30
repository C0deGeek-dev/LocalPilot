#![forbid(unsafe_code)]

use std::env;
use std::ffi::OsStr;
use std::process::{Command, ExitCode};

fn main() -> ExitCode {
    match env::args().nth(1).as_deref() {
        Some("ci") => run_ci(),
        _ => {
            eprintln!("usage: cargo ci");
            ExitCode::FAILURE
        }
    }
}

fn run_ci() -> ExitCode {
    let steps: &[(&str, &[&str])] = &[
        ("fmt", &["fmt", "--check"]),
        (
            "clippy",
            &[
                "clippy",
                "--workspace",
                "--all-targets",
                "--",
                "-D",
                "warnings",
            ],
        ),
        ("test", &["nextest", "run", "--workspace"]),
        ("check", &["check", "--workspace"]),
    ];

    for (name, args) in steps {
        eprintln!("cargo ci: running {name}");
        if !run("cargo", args) {
            return ExitCode::FAILURE;
        }
    }

    ExitCode::SUCCESS
}

fn run(program: &str, args: &[&str]) -> bool {
    let status = Command::new(program)
        .args(args.iter().map(OsStr::new))
        .status();

    matches!(status, Ok(status) if status.success())
}
