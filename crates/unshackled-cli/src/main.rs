use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use futures::StreamExt;
use unshackled_config::{CliOverrides, ConfigPaths};
use unshackled_core::{Message, Role, SessionId};
use unshackled_llm::{ModelEvent, ModelRequest, ProviderRegistry};
use unshackled_store::Store;

mod context_inject;
mod doctor;
mod harness_cmd;
#[cfg(feature = "learning")]
mod learning_cmd;
mod mcp;
mod memory_cmd;
#[cfg(feature = "tui")]
mod repl;
mod session_cmd;
#[cfg(feature = "tui")]
mod trust;
mod update;

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
    /// Check the project repository for a newer release and optionally update.
    Update {
        /// Only report whether an update is available; do not install.
        #[arg(long)]
        check: bool,
    },
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
    /// LocalMind learning: closeout, review queue, memory. Requires the `learning` feature.
    #[cfg(feature = "learning")]
    Learning {
        #[command(subcommand)]
        command: LearningCommand,
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
        /// Model name to request; defaults to the provider's configured model.
        #[arg(long)]
        model: Option<String>,
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

#[cfg(feature = "learning")]
#[derive(Debug, Subcommand)]
enum LearningCommand {
    /// Close out a session: extract candidate lessons and enqueue them for review.
    Closeout {
        /// Session id to close out.
        #[arg(long)]
        session: String,
    },
    /// Review queue: list, show, and decide on candidate lessons.
    Review {
        #[command(subcommand)]
        command: ReviewCommand,
    },
    /// Promote an accepted review item into durable memory.
    Promote {
        /// Review item id.
        id: String,
    },
    /// Search accepted memory.
    Search {
        /// Search query.
        query: String,
    },
    /// Skill drafts generated from accepted lessons.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Print the memory-change audit log.
    Audit,
}

#[cfg(feature = "learning")]
#[derive(Debug, Subcommand)]
enum SkillsCommand {
    /// Generate disabled skill drafts from accepted review items.
    Generate,
    /// List generated skill drafts.
    List,
    /// Inspect a skill draft.
    Show {
        /// Skill draft id.
        id: String,
    },
    /// Export a skill draft's Markdown body to a file or stdout.
    Export {
        /// Skill draft id.
        id: String,
        /// Destination file; prints to stdout when omitted.
        #[arg(long)]
        out: Option<PathBuf>,
    },
}

#[cfg(feature = "learning")]
#[derive(Debug, Subcommand)]
enum ReviewCommand {
    /// List the review queue.
    List,
    /// Inspect one review item.
    Show {
        /// Review item id.
        id: String,
    },
    /// Accept a review item.
    Accept {
        /// Review item id.
        id: String,
        /// Reviewer name recorded in the audit log.
        #[arg(long, default_value = "user")]
        reviewer: String,
        /// Optional review note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Reject a review item.
    Reject {
        /// Review item id.
        id: String,
        /// Reviewer name recorded in the audit log.
        #[arg(long, default_value = "user")]
        reviewer: String,
        /// Optional review note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Defer a review item (keep temporary).
    Defer {
        /// Review item id.
        id: String,
        /// Reviewer name recorded in the audit log.
        #[arg(long, default_value = "user")]
        reviewer: String,
        /// Optional review note.
        #[arg(long)]
        note: Option<String>,
    },
    /// Edit a review item's summary before accepting it.
    Edit {
        /// Review item id.
        id: String,
        /// Replacement summary.
        #[arg(long)]
        replacement: String,
        /// Reviewer name recorded in the audit log.
        #[arg(long, default_value = "user")]
        reviewer: String,
        /// Optional review note.
        #[arg(long)]
        note: Option<String>,
    },
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

    let command = match cli.command {
        Some(command) => command,
        None => return run_default().await,
    };

    match command {
        Command::Doctor => {
            let mut stdout = io::stdout().lock();
            doctor::run(&mut stdout)?;
            stdout.flush()?;
        }
        Command::Update { check } => {
            let mut stdout = io::stdout().lock();
            update::run(check, &mut stdout).await?;
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
        #[cfg(feature = "learning")]
        Command::Learning { command } => {
            use unshackled_localmind::ReviewVerdict;
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            match command {
                LearningCommand::Closeout { session } => {
                    learning_cmd::closeout(&cwd, &session, &mut stdout)?;
                }
                LearningCommand::Review { command } => match command {
                    ReviewCommand::List => learning_cmd::review_list(&cwd, &mut stdout)?,
                    ReviewCommand::Show { id } => {
                        learning_cmd::review_show(&cwd, &id, &mut stdout)?;
                    }
                    ReviewCommand::Accept { id, reviewer, note } => {
                        learning_cmd::review_decide(
                            &cwd,
                            &id,
                            ReviewVerdict::Accept,
                            &reviewer,
                            note,
                            &mut stdout,
                        )?;
                    }
                    ReviewCommand::Reject { id, reviewer, note } => {
                        learning_cmd::review_decide(
                            &cwd,
                            &id,
                            ReviewVerdict::Reject,
                            &reviewer,
                            note,
                            &mut stdout,
                        )?;
                    }
                    ReviewCommand::Defer { id, reviewer, note } => {
                        learning_cmd::review_decide(
                            &cwd,
                            &id,
                            ReviewVerdict::Defer,
                            &reviewer,
                            note,
                            &mut stdout,
                        )?;
                    }
                    ReviewCommand::Edit {
                        id,
                        replacement,
                        reviewer,
                        note,
                    } => {
                        learning_cmd::review_decide(
                            &cwd,
                            &id,
                            ReviewVerdict::Edit { replacement },
                            &reviewer,
                            note,
                            &mut stdout,
                        )?;
                    }
                },
                LearningCommand::Promote { id } => learning_cmd::promote(&cwd, &id, &mut stdout)?,
                LearningCommand::Search { query } => {
                    learning_cmd::search(&cwd, &query, &mut stdout)?;
                }
                LearningCommand::Skills { command } => match command {
                    SkillsCommand::Generate => learning_cmd::skills_generate(&cwd, &mut stdout)?,
                    SkillsCommand::List => learning_cmd::skills_list(&cwd, &mut stdout)?,
                    SkillsCommand::Show { id } => learning_cmd::skill_show(&cwd, &id, &mut stdout)?,
                    SkillsCommand::Export { id, out } => {
                        learning_cmd::skill_export(&cwd, &id, out, &mut stdout)?;
                    }
                },
                LearningCommand::Audit => learning_cmd::audit(&cwd, &mut stdout)?,
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
            repl::run_chat(model.as_deref(), provider.as_deref(), profile).await?;
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

/// Bare `unshackled` with no subcommand. On a `tui`-enabled build it launches the
/// interactive REPL when a provider and model are resolvable; otherwise (and on
/// the default build) it prints the doctor report so a misconfigured or headless
/// environment still gets a useful, non-interactive result.
async fn run_default() -> anyhow::Result<()> {
    #[cfg(feature = "tui")]
    {
        let cwd = std::env::current_dir()?;
        if let Ok(config) =
            unshackled_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())
        {
            if config.resolve_model(None).is_some() {
                let profile = session_cmd::resolve_profile(None, false);
                return repl::run_chat(None, None, profile).await;
            }
        }
    }
    // Doctor fallback: surface a cached update notice on stderr (the REPL shows
    // it in its header on the chat path above).
    if let Ok(cwd) = std::env::current_dir() {
        if let Some(tag) = update::cached_notice(&cwd).await {
            eprintln!("a newer version is available: {tag} — run `unshackled update`");
        }
    }
    let mut stdout = io::stdout().lock();
    doctor::run(&mut stdout)?;
    stdout.flush()?;
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
