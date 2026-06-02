use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use unshackled_core::SessionId;
use unshackled_store::Store;

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
    /// Export a session transcript as a redacted, inspectable bundle.
    Export {
        /// Session id to export.
        #[arg(long)]
        session: String,
        /// Destination file for the bundle.
        #[arg(long)]
        out: PathBuf,
    },
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
        Command::Export { session, out } => {
            let session_id = SessionId::from_str(&session)
                .map_err(|e| anyhow::anyhow!("invalid session id '{session}': {e}"))?;
            let store = Store::open(&std::env::current_dir()?);
            store.export_session(session_id, &out)?;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "exported session {session_id} to {}", out.display())?;
        }
    }

    Ok(())
}
