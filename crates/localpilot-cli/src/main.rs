use std::io::{self, Write};
use std::path::PathBuf;
use std::str::FromStr;

use clap::{Parser, Subcommand};
use futures::StreamExt;
use localpilot_config::{CliOverrides, ConfigPaths};
use localpilot_core::{Message, Role, SessionId};
use localpilot_llm::{ModelEvent, ModelRequest, ProviderRegistry};
use localpilot_store::Store;

mod context_inject;
mod doctor;
mod eval_cmd;
mod handoff_cmd;
mod harness_cmd;
mod ingest_cmd;
#[cfg(feature = "tui")]
mod key_input;
mod learning_cmd;
mod logging;
mod login_cmd;
mod mcp;
mod memory_cmd;
mod models_cmd;
mod propose_patch;
#[cfg(feature = "tui")]
mod repl;
mod rpc_cmd;
mod self_review_cmd;
mod session_cmd;
mod skills_cmd;
#[cfg(feature = "tui")]
mod trust;
mod update;

#[derive(Debug, Parser)]
#[command(name = "localpilot")]
#[command(about = "Provider-neutral coding-agent harness")]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report version, platform, config, providers, tools, and trust state.
    Doctor,
    /// Store an API key for a provider (bring-your-own-key): deep-link to the key
    /// page, paste, validate, and save it in the OS keychain (or a 0600 file).
    Login {
        /// Provider to log in: `anthropic`, `openai`, or a configured provider id.
        provider: String,
        /// Do not open the browser at the key-creation page (it is still printed).
        #[arg(long)]
        no_browser: bool,
        /// Skip the validation request and store the key without checking it.
        #[arg(long)]
        no_verify: bool,
    },
    /// Remove a provider's stored API key from the keychain / fallback file.
    Logout {
        /// Provider id whose stored credential to remove.
        provider: String,
    },
    /// List the models configured local servers actually have loaded.
    Models {
        /// Only query this configured provider id.
        #[arg(long)]
        provider: Option<String>,
    },
    /// Check the project repository for a newer release and optionally update.
    Update {
        /// Only report whether an update is available; do not install.
        #[arg(long)]
        check: bool,
    },
    /// Initialize project-local harness state (.localpilot.toml + .gitignore).
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
    /// LocalMind learning: closeout, review queue, memory.
    Learning {
        #[command(subcommand)]
        command: LearningCommand,
    },
    /// Project-local folder ingestion: preview, run, refresh, review, and clean up.
    Ingest {
        #[command(subcommand)]
        command: IngestCommand,
    },
    /// Search and package project-local ingested knowledge.
    Knowledge {
        #[command(subcommand)]
        command: KnowledgeCommand,
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
    /// Serve the Agent Client Protocol (for editors) on stdin/stdout.
    Acp {
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
    /// Drive the session runtime over stdin/stdout (newline-delimited JSON).
    Rpc {
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
        /// Continue the most recent session in this workspace.
        #[arg(long = "continue", conflicts_with = "resume")]
        continue_latest: bool,
        /// Resume the given session id.
        #[arg(long)]
        resume: Option<String>,
    },
    /// Run the agent headless on one problem and emit the capability scorecard
    /// (JSON) to stdout — the solver entry point for an external benchmark runner.
    Eval {
        /// The problem statement for the agent to solve in this workspace.
        problem: String,
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
        /// The harness arm label recorded on the scorecard (e.g. `full`, `baseline`).
        #[arg(long, default_value = "full")]
        arm: String,
        /// The task id recorded on the scorecard.
        #[arg(long, default_value = "eval")]
        task: String,
        /// Grading command (exit 0 = passed). Omit to emit an ungraded run for an
        /// external grader to score.
        #[arg(long)]
        test: Option<String>,
        /// Path to a gold unified diff, for the vs-gold ratio.
        #[arg(long)]
        gold_diff: Option<PathBuf>,
    },
    /// Inspect, resume, or export durable sessions in this workspace.
    Session {
        #[command(subcommand)]
        command: SessionCommand,
    },
    /// Project-local skills: list and read advisory skill modules.
    Skills {
        #[command(subcommand)]
        command: ProjectSkillsCommand,
    },
    /// Write a cross-context handoff, or check one before resuming work.
    Handoff {
        #[command(subcommand)]
        command: Option<HandoffCommand>,
    },
    /// Scan the repo for advisory health findings (read-only; writes nothing).
    SelfReview {
        /// Emit the machine-readable JSON report instead of the human summary.
        #[arg(long)]
        json: bool,
        /// Include the heuristic, low-confidence missing-test detector.
        #[arg(long)]
        missing_tests: bool,
        /// Fold in a model's harness-friction block read from this file.
        #[arg(long)]
        friction_file: Option<PathBuf>,
        /// Fold in measured friction from a captured run's capability scorecard
        /// JSON (its `process` block) read from this file.
        #[arg(long)]
        process_file: Option<PathBuf>,
        /// Print the friction audit prompt and exit (to run an audit, then feed
        /// its output back via --friction-file).
        #[arg(long)]
        audit_prompt: bool,
        /// Write-half subcommands: propose / promote / discard a patch for a finding
        /// (the gated self-improvement loop, ADR-0034). Omit for the read-only report.
        #[command(subcommand)]
        patch: Option<propose_patch::ProposePatchCommand>,
    },
}

#[derive(Debug, Subcommand)]
enum HandoffCommand {
    /// Write a handoff for the most recent session (the default action).
    Write {
        /// Optional objective for the next session; derived from the harness
        /// documents when omitted.
        objective: Option<String>,
    },
    /// Run the deterministic resume check for a handoff against the current repo.
    Resume {
        /// The handoff id (see the writer's output).
        id: String,
    },
}

#[derive(Debug, Subcommand)]
enum ProjectSkillsCommand {
    /// List the project's discovered skills.
    List,
    /// Print one skill's body by exact name (a deterministic load).
    Show {
        /// The skill name (see `skills list`).
        name: String,
    },
}

#[derive(Debug, Subcommand)]
enum SessionCommand {
    /// List this workspace's sessions, most recent first.
    List,
    /// Export a session as an inspectable, redacted JSON bundle.
    Export {
        /// The session id (see `session list`).
        id: String,
        /// Output file path.
        #[arg(long)]
        output: std::path::PathBuf,
    },
    /// Resume a session and run one prompt against it (print mode).
    Resume {
        /// The session id (see `session list`).
        id: String,
        /// The prompt text.
        #[arg(long)]
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
    /// Prune old sessions (and their orphaned tool-output) per the retention
    /// policy. Flags override the `[storage]` config for this run.
    Prune {
        /// Keep at most this many of the most recent sessions (0 = no limit).
        #[arg(long)]
        keep: Option<u64>,
        /// Drop sessions not updated within this many days (0 = no limit).
        #[arg(long)]
        older_than: Option<u64>,
        /// Report what would be removed without deleting anything.
        #[arg(long)]
        dry_run: bool,
    },
}

#[derive(Debug, Subcommand)]
enum MemoryCommand {
    /// Entry count and whether injection is enabled.
    Status,
    /// List all entries.
    Inspect,
    /// Show the memories used to answer the latest session's most recent turn,
    /// with provenance, confidence, epistemic status, contradictions, staleness.
    Used,
    /// Search entries by query.
    Search { query: String },
    /// Delete an entry by id.
    Delete { id: String },
    /// Disable memory injection for this project.
    Disable,
    /// Show a symbol's graph neighborhood, tests, and anchored lessons.
    Graph {
        /// Symbol name; use the qualified name when a plain name is ambiguous.
        symbol: String,
    },
    /// Write a redacted snapshot of the code graph to a local file.
    Export {
        /// Destination file path.
        path: std::path::PathBuf,
        /// Write HTML instead of JSON.
        #[arg(long)]
        html: bool,
    },
}

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

#[derive(Debug, Subcommand)]
enum IngestCommand {
    /// Preview candidate files, exclusions, and budgets.
    Preview,
    /// Run a full ingestion pass.
    Run,
    /// Show the current ingest job status.
    Status,
    /// Pause the current ingest job.
    Pause,
    /// Continue an incomplete job from the chunks already persisted.
    Resume,
    /// Cancel the current ingest job.
    Cancel,
    /// Refresh changed files only.
    Refresh,
    /// Delete derived ingestion state.
    Rebuild,
    /// Show skipped files and reasons from the latest manifest.
    Skipped,
    /// Add an explicit include rule.
    Include { path: PathBuf },
    /// Add an explicit exclude rule.
    Exclude { path: PathBuf },
    /// Remove derived records for a path or artifact id.
    Forget { target: String },
    /// List generated ingestion review items.
    Review,
    /// Queue an ingestion item for LocalMind review.
    Promote { id: String },
}

#[derive(Debug, Subcommand)]
enum KnowledgeCommand {
    /// Search ingested project knowledge.
    Search { query: String },
    /// Build a task-specific context pack.
    Pack { task: String },
}

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
    /// Back up the store, then delete every pending candidate (a one-time
    /// cleanup of an un-reviewed backlog). Decided items and accepted memory are
    /// untouched.
    Purge {
        /// Skip the confirmation prompt.
        #[arg(long)]
        yes: bool,
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
    /// Inspect or ratify the discovered quality gate (no provider needed).
    Gate {
        #[command(subcommand)]
        command: GateCommand,
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

#[derive(Debug, Subcommand)]
enum GateCommand {
    /// Show the discovered gate without writing anything.
    Propose,
    /// Write the discovered gate into `.localpilot.toml` (additions only).
    Ratify,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    if let Some(log_path) = logging::init(&cwd) {
        // The path goes to stderr (not the TUI's stdout) so the user knows where
        // to tail the run's log.
        eprintln!("localpilot: logging to {}", log_path.display());
    }
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
        Command::Models { provider } => {
            models_cmd::run(provider.as_deref()).await?;
        }
        Command::Login {
            provider,
            no_browser,
            no_verify,
        } => {
            login_cmd::login(
                &provider,
                login_cmd::LoginOptions {
                    no_browser,
                    no_verify,
                },
            )
            .await?;
        }
        Command::Logout { provider } => {
            login_cmd::logout(&provider)?;
        }
        Command::Rpc {
            model,
            provider,
            permission,
            bypass,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            rpc_cmd::run(
                model.as_deref(),
                provider.as_deref(),
                profile,
                rpc_cmd::WireProtocol::Native,
            )
            .await?;
        }
        Command::Acp {
            model,
            provider,
            permission,
            bypass,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            rpc_cmd::run(
                model.as_deref(),
                provider.as_deref(),
                profile,
                rpc_cmd::WireProtocol::Acp,
            )
            .await?;
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
                HarnessCommand::Gate { command } => {
                    let mut stdout = io::stdout().lock();
                    match command {
                        GateCommand::Propose => harness_cmd::gate_propose(&cwd, &mut stdout)?,
                        GateCommand::Ratify => harness_cmd::gate_ratify(&cwd, &mut stdout)?,
                    }
                    stdout.flush()?;
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
                MemoryCommand::Used => memory_cmd::used(&cwd, &mut stdout)?,
                MemoryCommand::Search { query } => {
                    memory_cmd::search(&cwd, &query, &mut stdout)?;
                }
                MemoryCommand::Delete { id } => memory_cmd::delete(&cwd, &id, &mut stdout)?,
                MemoryCommand::Disable => memory_cmd::disable(&cwd, &mut stdout)?,
                MemoryCommand::Graph { symbol } => {
                    memory_cmd::graph(&cwd, &symbol, &mut stdout)?;
                }
                MemoryCommand::Export { path, html } => {
                    memory_cmd::export(&cwd, &path, html, &mut stdout)?;
                }
            }
        }
        Command::Learning { command } => {
            use localpilot_localmind::ReviewVerdict;
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
                    ReviewCommand::Purge { yes } => {
                        learning_cmd::review_purge(&cwd, yes, &mut stdout)?;
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
        Command::Ingest { command } => {
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            match command {
                IngestCommand::Preview => ingest_cmd::preview(&cwd, &mut stdout)?,
                IngestCommand::Run => {
                    ingest_cmd::run(&cwd, localpilot_localmind::RunMode::Full, &mut stdout)?
                }
                IngestCommand::Status => ingest_cmd::status(&cwd, &mut stdout)?,
                IngestCommand::Pause => {
                    ingest_cmd::control(&cwd, ingest_cmd::ControlAction::Pause, &mut stdout)?
                }
                IngestCommand::Resume => ingest_cmd::resume(&cwd, &mut stdout)?,
                IngestCommand::Cancel => {
                    ingest_cmd::control(&cwd, ingest_cmd::ControlAction::Cancel, &mut stdout)?
                }
                IngestCommand::Refresh => {
                    ingest_cmd::run(&cwd, localpilot_localmind::RunMode::Refresh, &mut stdout)?
                }
                IngestCommand::Rebuild => ingest_cmd::rebuild(&cwd, &mut stdout)?,
                IngestCommand::Skipped => ingest_cmd::skipped(&cwd, &mut stdout)?,
                IngestCommand::Include { path } => {
                    ingest_cmd::rule(&cwd, ingest_cmd::RuleAction::Include, &path, &mut stdout)?;
                }
                IngestCommand::Exclude { path } => {
                    ingest_cmd::rule(&cwd, ingest_cmd::RuleAction::Exclude, &path, &mut stdout)?;
                }
                IngestCommand::Forget { target } => ingest_cmd::forget(&cwd, &target, &mut stdout)?,
                IngestCommand::Review => ingest_cmd::review(&cwd, &mut stdout)?,
                IngestCommand::Promote { id } => ingest_cmd::promote(&cwd, &id, &mut stdout)?,
            }
        }
        Command::Knowledge { command } => {
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            match command {
                KnowledgeCommand::Search { query } => {
                    ingest_cmd::knowledge_search(&cwd, &query, &mut stdout)?;
                }
                KnowledgeCommand::Pack { task } => {
                    ingest_cmd::knowledge_pack(&cwd, &task, &mut stdout)?;
                }
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
            continue_latest,
            resume,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            let resume = session_cmd::resolve_resume(continue_latest, resume.as_deref())?;
            session_cmd::print_mode(
                &prompt,
                &model,
                provider.as_deref(),
                profile,
                allow_writes,
                resume,
            )
            .await?;
        }
        Command::Eval {
            problem,
            model,
            provider,
            permission,
            bypass,
            arm,
            task,
            test,
            gold_diff,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            eval_cmd::run_eval(eval_cmd::EvalOptions {
                problem: &problem,
                model: &model,
                provider_id: provider.as_deref(),
                profile,
                arm: &arm,
                task: &task,
                test_command: test.as_deref(),
                gold_diff: gold_diff.as_deref(),
            })
            .await?;
        }
        Command::Session { command } => match command {
            SessionCommand::List => {
                let mut stdout = io::stdout().lock();
                session_cmd::list_sessions(&mut stdout)?;
                stdout.flush()?;
            }
            SessionCommand::Export { id, output } => {
                session_cmd::export_session(&id, &output)?;
                println!("exported {id} to {}", output.display());
            }
            SessionCommand::Resume {
                id,
                prompt,
                model,
                provider,
                permission,
                bypass,
                allow_writes,
            } => {
                let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
                let session = id.parse::<SessionId>()?;
                session_cmd::print_mode(
                    &prompt,
                    &model,
                    provider.as_deref(),
                    profile,
                    allow_writes,
                    Some(session),
                )
                .await?;
            }
            SessionCommand::Prune {
                keep,
                older_than,
                dry_run,
            } => {
                let mut stdout = io::stdout().lock();
                session_cmd::prune_sessions(keep, older_than, dry_run, &mut stdout)?;
                stdout.flush()?;
            }
        },
        Command::Skills { command } => {
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            match command {
                ProjectSkillsCommand::List => skills_cmd::list(&cwd, &mut stdout)?,
                ProjectSkillsCommand::Show { name } => skills_cmd::show(&cwd, &name, &mut stdout)?,
            }
            stdout.flush()?;
        }
        Command::Handoff { command } => {
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            match command {
                None | Some(HandoffCommand::Write { objective: None }) => {
                    handoff_cmd::write(&cwd, None, &mut stdout)?;
                }
                Some(HandoffCommand::Write {
                    objective: Some(objective),
                }) => {
                    handoff_cmd::write(&cwd, Some(&objective), &mut stdout)?;
                }
                Some(HandoffCommand::Resume { id }) => {
                    handoff_cmd::resume(&cwd, &id, &mut stdout)?;
                }
            }
            stdout.flush()?;
        }
        Command::SelfReview {
            json,
            missing_tests,
            friction_file,
            process_file,
            audit_prompt,
            patch,
        } => {
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            if let Some(cmd) = patch {
                propose_patch::dispatch(cmd, &mut stdout).await?;
            } else if audit_prompt {
                self_review_cmd::print_audit_prompt(&mut stdout)?;
            } else {
                self_review_cmd::run(
                    &cwd,
                    &self_review_cmd::SelfReviewArgs {
                        json,
                        missing_tests,
                        friction_file: friction_file.as_deref(),
                        process_file: process_file.as_deref(),
                    },
                    &mut stdout,
                )?;
            }
            stdout.flush()?;
        }
    }

    Ok(())
}

/// Bare `localpilot` with no subcommand. On a `tui`-enabled build it launches the
/// interactive REPL when a provider and model are resolvable; otherwise (and on
/// the default build) it prints the doctor report so a misconfigured or headless
/// environment still gets a useful, non-interactive result.
async fn run_default() -> anyhow::Result<()> {
    #[cfg(feature = "tui")]
    {
        let cwd = std::env::current_dir()?;
        if let Ok(config) =
            localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())
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
            eprintln!("a newer version is available: {tag} — run `localpilot update`");
        }
    }
    let mut stdout = io::stdout().lock();
    doctor::run(&mut stdout)?;
    stdout.flush()?;
    Ok(())
}

async fn ask(prompt: &str, model: &str, provider_id: Option<&str>) -> anyhow::Result<()> {
    let cwd = std::env::current_dir()?;
    let config = localpilot_config::load(&ConfigPaths::standard(&cwd), &CliOverrides::default())?;
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn login_and_logout_subcommands_parse() {
        // `login <provider>` with both opt-out flags.
        let cli = Cli::try_parse_from(["localpilot", "login", "anthropic", "--no-verify"]).unwrap();
        match cli.command {
            Some(Command::Login {
                provider,
                no_browser,
                no_verify,
            }) => {
                assert_eq!(provider, "anthropic");
                assert!(!no_browser);
                assert!(no_verify);
            }
            other => panic!("expected Login, got {other:?}"),
        }

        // `logout <provider>`.
        let cli = Cli::try_parse_from(["localpilot", "logout", "openai"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Logout { provider }) if provider == "openai"
        ));

        // The provider argument is required.
        assert!(Cli::try_parse_from(["localpilot", "login"]).is_err());
    }
}
