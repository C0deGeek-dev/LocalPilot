use std::process::ExitCode;

/// Print 1..=N for `--count N`; anything but a whole number of zero or more
/// is a usage error (exit 2).
fn main() -> ExitCode {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let value = match args.as_slice() {
        [flag, value] if flag == "--count" => value,
        _ => {
            eprintln!("usage: counter --count N");
            return ExitCode::from(2);
        }
    };
    let Ok(count) = value.parse::<u64>() else {
        eprintln!("invalid count: {value}");
        return ExitCode::from(2);
    };
    for n in 1..=count {
        println!("{n}");
    }
    ExitCode::SUCCESS
}
