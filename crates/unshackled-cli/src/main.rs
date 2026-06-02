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
mod harness_cmd;
mod memory_cmd;
#[cfg(feature = "tui")]
mod repl;
mod session_cmd;

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
    /// Initialize project-local harness state (.unshackled.toml + .gitignore).
    Init {
        /// Also initialize a git repository if one is not present.
        #[arg(long)]
        git: bool,
    },
    /// Harness subcommands (rule-enforced operating mode).
    Harness {
        #[command(subcommand)]
        command: HarnessCommand,
    },
    /// Local project memory: inspect, search, delete, disable.
    Memory {
        #[command(subcommand)]
        command: MemoryCommand,
    },
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
    /// Launch the interactive terminal REPL (the TUI). Requires the `tui` build feature.
    #[cfg(feature = "tui")]
    Chat {
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
        /// Permission profile (default | relaxed | bypass).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
    },
    /// Run the agent loop once non-interactively and print the answer (pipelines).
    Print {
        /// The prompt text.
        prompt: String,
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
        /// Permission profile (default | relaxed | bypass).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
        /// Allow the run to write to the workspace (off by default).
        #[arg(long)]
        allow_writes: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Entry count and whether injection is enabled.
    Status,
    /// List all entries.
    Inspect,
    /// Search entries by query.
    Search { query: String },
    /// Delete an entry by id.
    Delete { id: String },
    /// Disable memory injection for this project.
    Disable,
}

#[derive(Debug, Subcommand)]
enum HarnessCommand {
    /// Read-only summary of the harness state (works without a provider).
    Status,
    /// Turn a rough idea into brief.md.
    Intake {
        /// The idea to develop into a brief.
        #[arg(long)]
        idea: String,
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Turn brief.md into a PROGRESS.md plan.
    Plan {
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Add a feature to the existing brief and plan (no provider needed).
    Feature {
        /// The feature description.
        description: String,
    },
    /// Work the plan: run incomplete steps, committing each. (resume)
    Resume {
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
        /// Permission profile (default | relaxed | bypass).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
    },
    /// Continue a run that paused on a provider quota/rate limit, if now safe.
    WaitResume {
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
        /// Permission profile (default | relaxed | bypass).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
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
        Command::Init { git } => {
            let summary = harness_cmd::init(&std::env::current_dir()?, git)?;
            let mut stdout = io::stdout().lock();
            writeln!(stdout, "{summary}")?;
        }
        Command::Harness { command } => {
            let cwd = std::env::current_dir()?;
            match command {
                HarnessCommand::Status => {
                    let mut stdout = io::stdout().lock();
                    harness_cmd::status(&cwd, &mut stdout)?;
                    stdout.flush()?;
                }
                HarnessCommand::Intake {
                    idea,
                    model,
                    provider,
                } => {
                    harness_cmd::intake(&cwd, &model, provider.as_deref(), &idea).await?;
                    println!("wrote brief.md");
                }
                HarnessCommand::Plan { model, provider } => {
                    harness_cmd::plan(&cwd, &model, provider.as_deref()).await?;
                    println!("wrote PROGRESS.md");
                }
                HarnessCommand::Feature { description } => {
                    harness_cmd::feature(&cwd, &description)?;
                    println!("appended feature to brief.md and PROGRESS.md");
                }
                HarnessCommand::Resume {
                    model,
                    provider,
                    permission,
                    bypass,
                } => {
                    let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
                    let mut stdout = io::stdout().lock();
                    harness_cmd::resume(&cwd, &model, provider.as_deref(), profile, &mut stdout)
                        .await?;
                }
                HarnessCommand::WaitResume {
                    model,
                    provider,
                    permission,
                    bypass,
                } => {
                    let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
                    let mut stdout = io::stdout().lock();
                    harness_cmd::wait_resume(
                        &cwd,
                        &model,
                        provider.as_deref(),
                        profile,
                        &mut stdout,
                    )
                    .await?;
                }
            }
        }
        Command::Memory { command } => {
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            match command {
                MemoryCommand::Status => memory_cmd::status(&cwd, &mut stdout)?,
                MemoryCommand::Inspect => memory_cmd::inspect(&cwd, &mut stdout)?,
                MemoryCommand::Search { query } => {
                    memory_cmd::search(&cwd, &query, &mut stdout)?;
                }
                MemoryCommand::Delete { id } => memory_cmd::delete(&cwd, &id, &mut stdout)?,
                MemoryCommand::Disable => memory_cmd::disable(&cwd, &mut stdout)?,
            }
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
        #[cfg(feature = "tui")]
        Command::Chat {
            model,
            provider,
            permission,
            bypass,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            repl::run_chat(&model, provider.as_deref(), profile).await?;
        }
        Command::Print {
            prompt,
            model,
            provider,
            permission,
            bypass,
            allow_writes,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            session_cmd::print_mode(&prompt, &model, provider.as_deref(), profile, allow_writes)
                .await?;
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
