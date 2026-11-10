use std::process::ExitCode;

/// Print 1..=N for `--count N`.
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let count = match args.as_slice() {
        [flag, value] if flag == "--count" => value.parse::<i64>().unwrap_or(0),
        _ => 0,
    };
    for n in 1..=count {
        println!("{n}");
    }
    ExitCode::SUCCESS
}
