use std::io::{self, Write};

use clap::{Parser, Subcommand};

mod doctor;

#[derive(Debug, Parser)]
#[command(name = "unshackled")]
#[command(about = "Provider-neutral coding-agent harness")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report version, platform, config, providers, tools, and trust state.
    Doctor,
    /// Initialize project-local harness state.
    Init,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    tracing_subscriber::fmt::init();
    let cli = Cli::parse();

    match cli.command.unwrap_or(Command::Doctor) {
        Command::Doctor => {
            let mut stdout = io::stdout().lock();
            doctor::run(&mut stdout)?;
            stdout.flush()?;
        }
        Command::Init => {
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "initialized scaffold")?;
        }
    }

    Ok(())
}
