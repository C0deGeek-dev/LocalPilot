use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use futures::StreamExt;
use unshackled_config::{CliOverrides, ConfigPaths};
use unshackled_core::{Message, Role, SessionId};
use unshackled_llm::{ModelEvent, ModelRequest, ProviderRegistry};
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
    /// Send a single prompt to a provider and stream the text answer.
    Ask {
        /// The prompt text.
        prompt: String,
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
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
        Command::Ask {
            prompt,
            model,
            provider,
        } => {
            ask(&prompt, &model, provider.as_deref()).await?;
        }
    }

    Ok(())
}

async fn ask(prompt: &str, model: &str, provider_id: Option<&str>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = unshackled_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
    let registry = ProviderRegistry::from_config(&config)?;
    let provider = match provider_id {
        Some(id) => registry
            .get(id)
            .ok_or_else(|| anyhow::anyhow!("provider '{id}' is not configured"))?,
        None => registry
            .default_provider()
            .ok_or_else(|| anyhow::anyhow!("no default provider is configured"))?,
    };

    let request = ModelRequest::new(model, vec![Message::text(Role::User, prompt)]);
    let mut stream = provider.stream(request).await?;

    let mut stdout = io::stdout().lock();
    while let Some(event) = stream.next().await {
        match event? {
            ModelEvent::TextDelta(text) => {
                write!(stdout, "{text}")?;
                stdout.flush()?;
            }
            ModelEvent::Done => break,
            _ => {}
        }
    }
    writeln!(stdout)?;
    Ok(())
}
