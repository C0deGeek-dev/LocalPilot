use std::io::{self, IsTerminal, Write};
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
mod output;
mod outward_cmd;
mod propose_patch;
#[cfg(feature = "tui")]
mod repl;
mod research;
mod rpc_cmd;
mod self_review_cmd;
mod session_cmd;
mod skill_discovery;
mod skills_cmd;
mod trust;
mod update;

#[derive(Debug, Parser)]
#[command(name = "localpilot")]
#[command(about = "Provider-neutral coding-agent harness")]
#[command(version = env!("LOCALPILOT_VERSION"))]
struct Cli {
    #[command(subcommand)]
    command: Option<Command>,
}

#[derive(Debug, Subcommand)]
enum Command {
    /// Report version, platform, config, providers, tools, and trust state.
    Doctor {
        /// Output format. Defaults to JSON when stdout is not a terminal
        /// (piped/redirected) and the human summary on a terminal (ADR-0048).
        #[arg(long, value_enum)]
        format: Option<output::OutputFormat>,
        /// Alias for `--format json`.
        #[arg(long)]
        json: bool,
    },
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
        /// Output format. Defaults to JSON when stdout is not a terminal
        /// (piped/redirected) and the human list on a terminal (ADR-0048).
        #[arg(long, value_enum)]
        format: Option<output::OutputFormat>,
        /// Alias for `--format json`.
        #[arg(long)]
        json: bool,
        /// Approve the network discovery request without prompting — required to
        /// query when running non-interactively (no TTY), so the command never
        /// silently skips.
        #[arg(long)]
        yes: bool,
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
        /// Pin the LocalMind store root explicitly, skipping the walk-up from the
        /// current directory. Use it when running from outside the project.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
        #[command(subcommand)]
        command: MemoryCommand,
    },
    /// LocalMind learning: closeout, review queue, memory.
    Learning {
        /// Pin the LocalMind store root explicitly, skipping the walk-up from the
        /// current directory. Use it when running from outside the project.
        #[arg(long, value_name = "PATH")]
        workspace: Option<PathBuf>,
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
    /// Research a topic across local sources and the web, writing a report and
    /// review-gated memory candidates. Web research is on by default —
    /// disclosed, allowlist-gated, and audited; disable per run with --no-web
    /// or globally with `[research.web].enabled = false`.
    Research {
        /// The topic or question to research.
        topic: String,
        /// Do not enqueue review-gated memory candidates from the findings.
        #[arg(long)]
        no_memory: bool,
        /// Do not write a report artefact to the research output directory.
        #[arg(long)]
        no_report: bool,
        /// Explicitly request web research. Web is already on by default, so
        /// this is a no-op kept for compatibility; it still cannot override
        /// `[research.web].enabled = false`.
        #[arg(long, conflicts_with = "no_web")]
        web: bool,
        /// Skip web research for this run: no outbound request is made and no
        /// candidate URL is proposed.
        #[arg(long)]
        no_web: bool,
        /// Override the maximum retrieval rounds for this run (config:
        /// `[research].max_rounds`, default 3). `1` is a single pass.
        #[arg(long, conflicts_with = "quick")]
        rounds: Option<usize>,
        /// Override the maximum sub-questions for this run (config:
        /// `[research].max_questions`, default 6).
        #[arg(long)]
        max_questions: Option<usize>,
        /// Wall-clock budget for the retrieval phase, in seconds (config:
        /// `[research].time_budget_secs`, unset by default).
        #[arg(long)]
        time_budget: Option<u64>,
        /// Quick mode: a single retrieval round (the pre-multi-round
        /// behaviour). Equivalent to `--rounds 1`.
        #[arg(long)]
        quick: bool,
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
        /// Permission profile (default | relaxed | bypass | unrestricted).
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
        /// Permission profile (default | relaxed | bypass | unrestricted).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
        /// Open the most recent session in this workspace instead of a fresh one.
        #[arg(long = "continue", conflicts_with = "resume")]
        continue_latest: bool,
        /// Open the session with this id or name (see `session list`).
        #[arg(long)]
        resume: Option<String>,
    },
    /// Model Context Protocol surfaces.
    #[command(subcommand)]
    Mcp(McpCommand),
    /// Launch the interactive terminal REPL (the TUI). Requires the `tui` build feature.
    #[cfg(feature = "tui")]
    Chat {
        /// Model name to request; defaults to the provider's configured model.
        #[arg(long)]
        model: Option<String>,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
        /// Permission profile (default | relaxed | bypass | unrestricted).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
        /// Open the most recent session in this workspace instead of a fresh one.
        #[arg(long = "continue", conflicts_with = "resume")]
        continue_latest: bool,
        /// Open the session with this id or name (see `session list`).
        #[arg(long)]
        resume: Option<String>,
    },
    /// Run the agent loop once non-interactively and print the answer (pipelines).
    ///
    /// This one-shot path *reads* accepted project memory (it injects relevant
    /// lessons into the turn) but deliberately does **not** close out — it never
    /// writes learning candidates, so a bare prompt leaves no project files. Use
    /// `harness`/`rpc`/`acp`, or an explicit `learning closeout`, when you want the
    /// run to learn. Pass `--self-review` for an advisory repo-health pass after the
    /// run.
    Print {
        /// The prompt text.
        prompt: String,
        /// Model name to request.
        #[arg(long)]
        model: String,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
        /// Permission profile (default | relaxed | bypass | unrestricted).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
        /// Allow the run to write to the workspace (off by default).
        #[arg(long)]
        allow_writes: bool,
        /// After the run, print an advisory `self-review` of the workspace to
        /// stderr (read-only; never edits or commits). Off by default.
        #[arg(long)]
        self_review: bool,
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
        /// Permission profile (default | relaxed | bypass | unrestricted).
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
        /// Verify-before-done is **on by default** for `eval`: a turn that would
        /// finalize re-runs a build/test verification first and continues on a
        /// failure, so the benchmark measures compiled+tested solves. This flag is
        /// accepted for back-compat but is now redundant (the gate is already on).
        #[arg(long)]
        verify: bool,
        /// Opt out of the default-on verify-before-done gate for this run, so the
        /// turn finalizes without a build/test check (the pre-default behaviour).
        #[arg(long, conflicts_with = "verify")]
        no_verify: bool,
        /// Verification command for the gate (a single command line). Overrides
        /// `[harness] verify_command` and stack detection. Ignored under
        /// `--no-verify`.
        #[arg(long)]
        verify_command: Option<String>,
        /// Learn from this run: after the turn, close the session out into
        /// LocalMind so it yields review-gated lesson candidates (good and
        /// anti-pattern, scope-routed to the machine-wide store). Off by default,
        /// so a plain `eval` stays a clean-room capability measurement that
        /// neither reads nor writes accumulated memory.
        #[arg(long)]
        learn: bool,
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
        /// Run the whole-repo teardown sweep: add the cleanup-audit detectors
        /// (dead/abandoned code, duplicate logic, over-engineering, redundant data
        /// access, plus tool-owned pointers). This is the on-demand path to the
        /// sweep the harness runs at completion under `[harness] teardown_sweep`.
        #[arg(long)]
        cleanup: bool,
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

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum ProjectSkillsCommand {
    /// List the effective skills (the global baseline overlaid by the project).
    List {
        /// Restrict the view to the user-global scope.
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Print one skill's body by exact name (a deterministic load).
    Show {
        /// The skill name (see `skills list`).
        name: String,
        /// Resolve the name in the user-global scope only.
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Manage skill source repositories (public HTTPS Git).
    Repo {
        #[command(subcommand)]
        command: SkillsRepoCommand,
    },
    /// Search cached source catalogs for installable skills (offline).
    Available {
        /// Optional query matched against skill names and descriptions.
        query: Option<String>,
        /// Restrict the view to the user-global scope.
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Install a managed skill, or every package of a source with `--all`.
    Install {
        /// The skill name to install (omit only together with `--all`).
        name: Option<String>,
        /// Disambiguate a name offered by several sources, or the source for `--all`.
        #[arg(long)]
        repo: Option<String>,
        /// Install every package of the `--repo` source (all-or-nothing).
        #[arg(long)]
        all: bool,
        /// Install into the user-global scope instead of the project.
        #[arg(short = 'g', long)]
        global: bool,
        /// Approve the mutation without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Remove a managed (LocalPilot-installed) skill.
    Delete {
        /// The installed skill name.
        name: String,
        /// Remove from the user-global scope instead of the project.
        #[arg(short = 'g', long)]
        global: bool,
        /// Approve the mutation without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Discover relevant skills — installed, available in a registered source, or
    /// in a newly found public repository — and save review proposals (read-only;
    /// registers and installs nothing).
    Research {
        /// The discovery query (required; multiple words are joined).
        #[arg(required = true, num_args = 1..)]
        query: Vec<String>,
        /// Search only the user-global catalog and default proposals to global scope.
        #[arg(short = 'g', long)]
        global: bool,
        /// Skip web discovery for this run (search local catalogs only).
        #[arg(long)]
        no_web: bool,
    },
}

#[derive(Debug, Subcommand, PartialEq, Eq)]
enum SkillsRepoCommand {
    /// Register a public HTTPS Git repository as a skill source (fetches one
    /// snapshot; installs nothing).
    Add {
        /// The public HTTPS repository URL.
        url: String,
        /// Register in the user-global scope instead of the project.
        #[arg(short = 'g', long)]
        global: bool,
        /// Approve the network fetch without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// Refresh one source (by id or URL), or every source when none is given.
    Refresh {
        /// The source id or URL; omit to refresh every source in scope.
        url: Option<String>,
        /// Operate on the user-global scope instead of the project.
        #[arg(short = 'g', long)]
        global: bool,
        /// Approve the network fetch without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
    /// List registered sources.
    List {
        /// Restrict the view to the user-global scope.
        #[arg(short = 'g', long)]
        global: bool,
    },
    /// Remove a source registration and its cache (installed skills remain).
    Delete {
        /// The source id or URL.
        url: String,
        /// Operate on the user-global scope instead of the project.
        #[arg(short = 'g', long)]
        global: bool,
        /// Approve the removal without an interactive prompt.
        #[arg(long)]
        yes: bool,
    },
}

/// A `Parser` wrapper so the interactive `/skills …` slash command parses its raw
/// arguments through the exact same command surface as `localpilot skills …`,
/// guaranteeing the two forms parse to the same operations (LocalHub#40). Only the
/// TUI slash path uses it.
#[cfg(feature = "tui")]
#[derive(Debug, Parser)]
#[command(name = "skills", no_binary_name = true)]
struct SkillsSlash {
    #[command(subcommand)]
    command: ProjectSkillsCommand,
}

#[derive(Debug, Subcommand)]
enum McpCommand {
    /// Serve this workspace's session runtime as an MCP server on
    /// stdin/stdout, so an MCP client (an agent host) can drive and steer a
    /// session through tools. Permission decisions stay in the engine; an
    /// unanswered ask is denied.
    Serve {
        /// Model name to request; defaults to the provider's configured model.
        #[arg(long)]
        model: Option<String>,
        /// Provider id; defaults to the configured default provider.
        #[arg(long)]
        provider: Option<String>,
        /// Permission profile (default | relaxed | bypass | unrestricted).
        #[arg(long)]
        permission: Option<String>,
        /// Shorthand for `--permission bypass`. Must be set explicitly.
        #[arg(long)]
        bypass: bool,
        /// Open the most recent session in this workspace instead of a fresh one.
        #[arg(long = "continue", conflicts_with = "resume")]
        continue_latest: bool,
        /// Open the session with this id or name (see `session list`).
        #[arg(long)]
        resume: Option<String>,
        /// Withhold the permission-reply tool: the client can watch and steer
        /// but never answer an ask, so every ask denies (watch-and-steer mode).
        #[arg(long)]
        no_approvals: bool,
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
    /// Give a session a name so it can be resumed by name (see `session resume`
    /// and `--resume`). Names are unique within the workspace.
    Name {
        /// The session id or its current name (see `session list`).
        id: String,
        /// The new name for the conversation.
        name: String,
    },
    /// Resume a session and run one prompt against it (print mode).
    Resume {
        /// The session id or name (see `session list`).
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
        /// Permission profile (default | relaxed | bypass | unrestricted).
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
    Search {
        query: String,
        /// Output format. Defaults to a JSON array when stdout is not a terminal
        /// (piped/redirected) and the human table on a terminal.
        #[arg(long, value_enum)]
        format: Option<output::OutputFormat>,
        /// Alias for `--format json`.
        #[arg(long)]
        json: bool,
    },
    /// Delete an entry by id.
    Delete { id: String },
    /// Disable memory injection for this project.
    Disable,
    /// Re-enable memory injection for this project (clears the disable flag).
    Enable,
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
    /// Seed curated lessons from a JSON pack directly into accepted memory.
    Seed {
        /// Path to a JSON seed pack: `{ "lessons": [ { "body": "...", ... } ] }`.
        #[arg(long)]
        file: PathBuf,
        /// Validate and count without writing anything.
        #[arg(long)]
        dry_run: bool,
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
    /// Export accepted memory to a portable, signed bundle file.
    Export {
        /// Destination file for the signed bundle.
        #[arg(long)]
        out: PathBuf,
        /// Which scopes to include: project, global, or both.
        #[arg(long, default_value = "both")]
        scope: String,
    },
    /// Import a signed bundle: verify, then (with --apply) enqueue for review.
    Import {
        /// Signed bundle file to import.
        input: PathBuf,
        /// Write imported entries as review candidates. Without it this is a dry
        /// run that reports what would change and writes nothing.
        #[arg(long)]
        apply: bool,
    },
    /// Search accepted memory.
    Search {
        /// Search query.
        query: String,
        /// Output format. Defaults to a JSON array (id, score, path, snippet,
        /// category) when stdout is not a terminal (piped/redirected) and the
        /// human table on a terminal.
        #[arg(long, value_enum)]
        format: Option<output::OutputFormat>,
        /// Alias for `--format json`.
        #[arg(long)]
        json: bool,
    },
    /// Skill drafts generated from accepted lessons.
    Skills {
        #[command(subcommand)]
        command: SkillsCommand,
    },
    /// Print the memory-change audit log.
    Audit,
    /// Freshness pass: flag stale / dead-weight / version-sensitive accepted
    /// memory for review. Dry-run by default (`--apply` writes); never deletes.
    Freshness {
        /// Which store(s) to groom: project, global, or both.
        #[arg(long, default_value = "both")]
        scope: String,
        /// Apply the flags (write). Without it, a dry run reports candidates only.
        #[arg(long)]
        apply: bool,
        /// Flag memory older than this many days.
        #[arg(long)]
        max_age_days: Option<i64>,
        /// Flag never-retrieved memory older than this many days.
        #[arg(long)]
        unused_grace_days: Option<i64>,
        /// Flag version-sensitive memory older than this many days.
        #[arg(long)]
        version_sensitive_min_age_days: Option<i64>,
        /// Cap on the flags emitted in one pass.
        #[arg(long)]
        max_flags: Option<usize>,
        /// Output format. Defaults to JSON when stdout is not a terminal.
        #[arg(long, value_enum)]
        format: Option<output::OutputFormat>,
        /// Alias for `--format json`.
        #[arg(long)]
        json: bool,
    },
    /// Memory lifecycle queues: flagged-for-review (stale), never-retrieved,
    /// most-used, and contradicted. Read-only.
    Lifecycle {
        /// Cap on the most-used section.
        #[arg(long, default_value_t = 10)]
        top: usize,
        /// Output format. Defaults to JSON when stdout is not a terminal.
        #[arg(long, value_enum)]
        format: Option<output::OutputFormat>,
        /// Alias for `--format json`.
        #[arg(long)]
        json: bool,
    },
    /// Opt-in source re-validation: ask the configured model whether version-
    /// sensitive accepted lessons are still current and flag "no longer true"
    /// ones for review. Network-touching and default-off — a preview contacts
    /// nothing; `--apply` contacts the configured model. Never deletes.
    Revalidate {
        /// Contact the configured model and flag for review. Without it, this is
        /// an offline preview that counts candidates and contacts nothing.
        #[arg(long)]
        apply: bool,
        /// Most version-sensitive lessons to sample in one pass.
        #[arg(long, default_value_t = 10)]
        sample: usize,
        /// Output format. Defaults to JSON when stdout is not a terminal.
        #[arg(long, value_enum)]
        format: Option<output::OutputFormat>,
        /// Alias for `--format json`.
        #[arg(long)]
        json: bool,
    },
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
        /// Run the pre-brief guidance assessment for this run (overrides
        /// `[harness.guidance] enabled`). The score is an inspectable signal,
        /// not proof the idea is fully specified.
        #[arg(long, conflicts_with = "no_guidance")]
        guidance: bool,
        /// Skip the pre-brief guidance assessment for this run (overrides
        /// `[harness.guidance] enabled`).
        #[arg(long)]
        no_guidance: bool,
        /// Below the guidance threshold, proceed to the brief anyway and let
        /// the model use its judgment for the open decisions (recorded in the
        /// intake record).
        #[arg(long)]
        assume_judgment: bool,
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
        /// Permission profile (default | relaxed | bypass | unrestricted).
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
        /// Permission profile (default | relaxed | bypass | unrestricted).
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

/// Resolve the LocalMind store root for a `learning`/`memory` command and log the
/// resolved root to stderr so the caller knows which store answered. An explicit
/// `--workspace` pins the root and skips the walk-up; otherwise the store is
/// resolved by walking up from `cwd` (git-style). The returned [`StoreRoot`]
/// preserves the found-vs-absent distinction so a read can stay read-only instead
/// of silently creating a second, empty store beside the cwd.
fn resolve_learning_store(
    workspace: Option<PathBuf>,
    cwd: &std::path::Path,
) -> localpilot_localmind::StoreRoot {
    use localpilot_localmind::StoreRoot;
    match workspace {
        Some(path) => {
            let found = localpilot_localmind::is_store_root(&path);
            eprintln!(
                "localmind: using store root {} (--workspace)",
                path.display()
            );
            if found {
                StoreRoot::Found(path)
            } else {
                StoreRoot::NotFound(path)
            }
        }
        None => {
            let resolved = localpilot_localmind::resolve_store_root(cwd);
            if let StoreRoot::Found(root) = &resolved {
                if root == cwd {
                    eprintln!("localmind: store root {}", root.display());
                } else {
                    eprintln!(
                        "localmind: resolved store root {} (walked up from {})",
                        root.display(),
                        cwd.display()
                    );
                }
            }
            resolved
        }
    }
}

/// Exit code returned when a `print` run stops because its output consumer closed
/// stdout (the reader went away). A clean, distinct terminal state — not the
/// process panic (101) the unchecked write macros used to take — so a wrapper can
/// tell "the reader left" from a real failure. Matches the POSIX SIGPIPE
/// convention (128 + SIGPIPE 13) that broken-pipe-aware tooling already expects.
const EXIT_OUTPUT_CONSUMER_GONE: u8 = 141;

/// Print the one-time "learning is on by default" transparency notice the first
/// time LocalPilot runs after the default flipped. Learning is **local-only**
/// (never leaves the machine), redacted, and review-gated, so this is disclosure,
/// not a consent gate. A marker under the per-user config dir suppresses it on
/// later runs; the notice goes to **stderr** so it never disturbs a piped
/// `print`/`eval`/`rpc` stdout. Best-effort: if the marker can't be persisted
/// (no base dir / write failure) the notice is skipped rather than shown every
/// run.
fn maybe_show_learning_notice() {
    let Some(marker) = localpilot_config::learning_notice_marker_path() else {
        return;
    };
    if marker.exists() {
        return;
    }
    // Persist the marker *before* printing, and only print if it persisted, so the
    // notice shows exactly once — never on every invocation.
    if let Some(parent) = marker.parent() {
        if std::fs::create_dir_all(parent).is_err() {
            return;
        }
    }
    if std::fs::write(&marker, b"shown\n").is_err() {
        return;
    }
    eprintln!(
        "localpilot: LocalMind learning is now ON by default — your sessions become \
         reviewed, machine-wide memory, so the agent gets better the more you use it. \
         It is LOCAL-ONLY (never leaves your machine), redacted, and review-gated (you \
         approve what is remembered; nothing is auto-applied). Opt out with \
         `[learning] enabled = false`, or manage it with `localpilot learning`."
    );
}

fn main() -> anyhow::Result<std::process::ExitCode> {
    // The clap command tree and the top-level command future are large; on Windows
    // the OS main thread's ~1 MiB default stack overflows building them in a debug
    // build (a `STATUS_STACK_OVERFLOW` before any work runs). Drive everything on a
    // worker thread with a generous stack so the binary behaves identically across
    // platforms (tier-1 parity, ADR-0007).
    const MAIN_STACK_SIZE: usize = 16 * 1024 * 1024;
    let worker = std::thread::Builder::new()
        .name("localpilot-main".to_string())
        .stack_size(MAIN_STACK_SIZE)
        .spawn(|| {
            let runtime = tokio::runtime::Builder::new_multi_thread()
                .enable_all()
                .build()?;
            let outcome = runtime.block_on(run());
            // Detached background work (the first-session knowledge ingest is
            // the big one) must never hold process exit hostage: a plain drop
            // of the runtime waits for lingering blocking tasks to finish.
            // Interrupted ingests resume on the next session open.
            runtime.shutdown_background();
            outcome
        })?;
    worker
        .join()
        .map_err(|_| anyhow::anyhow!("localpilot main thread panicked"))?
}

async fn run() -> anyhow::Result<std::process::ExitCode> {
    let cwd = std::env::current_dir().unwrap_or_else(|_| PathBuf::from("."));
    let cli = Cli::parse();
    // `chat` (and the bare `localpilot` default, which launches chat when a
    // provider is configured) hands the terminal to the raw-mode TUI, so the
    // default terminal log subscriber must not write into it mid-session.
    // File logging via LOCALPILOT_LOG is unaffected.
    #[cfg(feature = "tui")]
    let terminal_owned = matches!(cli.command, Some(Command::Chat { .. }) | None);
    #[cfg(not(feature = "tui"))]
    let terminal_owned = false;
    if let Some(log_path) = logging::init(&cwd, terminal_owned) {
        // The path goes to stderr (not the TUI's stdout) so the user knows where
        // to tail the run's log.
        eprintln!("localpilot: logging to {}", log_path.display());
    }
    maybe_show_learning_notice();

    let command = match cli.command {
        Some(command) => command,
        None => {
            return run_default()
                .await
                .map(|()| std::process::ExitCode::SUCCESS)
        }
    };

    // The terminal exit code, raised only by a print run whose consumer went away.
    let mut exit_code = std::process::ExitCode::SUCCESS;

    match command {
        Command::Doctor { format, json } => {
            let is_tty = io::stdout().is_terminal();
            let resolved = output::resolve_format(format, json, is_tty);
            let mut stdout = io::stdout().lock();
            doctor::run_with(&mut stdout, resolved).await?;
            stdout.flush()?;
        }
        Command::Models {
            provider,
            format,
            json,
            yes,
        } => {
            let is_tty = io::stdout().is_terminal();
            let resolved = output::resolve_format(format, json, is_tty);
            let stdin_is_tty = io::stdin().is_terminal();
            let outcome = models_cmd::run(provider.as_deref(), resolved, yes, stdin_is_tty).await?;
            if outcome.had_failure {
                exit_code = std::process::ExitCode::FAILURE;
            }
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
            continue_latest,
            resume,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            let resume = session_cmd::resolve_resume(continue_latest, resume.as_deref())?;
            rpc_cmd::run(
                model.as_deref(),
                provider.as_deref(),
                profile,
                rpc_cmd::WireProtocol::Native,
                resume,
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
                None,
            )
            .await?;
        }
        Command::Mcp(McpCommand::Serve {
            model,
            provider,
            permission,
            bypass,
            continue_latest,
            resume,
            no_approvals,
        }) => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            let resume = session_cmd::resolve_resume(continue_latest, resume.as_deref())?;
            rpc_cmd::run(
                model.as_deref(),
                provider.as_deref(),
                profile,
                rpc_cmd::WireProtocol::Mcp {
                    approvals: !no_approvals,
                },
                resume,
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
                    guidance,
                    no_guidance,
                    assume_judgment,
                } => {
                    let guidance_override = match (guidance, no_guidance) {
                        (true, _) => Some(true),
                        (_, true) => Some(false),
                        _ => None,
                    };
                    let mut stdout = io::stdout().lock();
                    let outcome = harness_cmd::intake(
                        &cwd,
                        &model,
                        provider.as_deref(),
                        &idea,
                        guidance_override,
                        assume_judgment,
                        &mut stdout,
                    )
                    .await?;
                    stdout.flush()?;
                    drop(stdout);
                    match outcome {
                        harness_cmd::IntakeOutcome::BriefWritten => println!("wrote brief.md"),
                        harness_cmd::IntakeOutcome::NeedsGuidance => {}
                    }
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
        Command::Memory { workspace, command } => {
            let cwd = std::env::current_dir()?;
            let resolution = resolve_learning_store(workspace, &cwd);
            let root = resolution.path();
            let mut stdout = io::stdout().lock();
            match command {
                MemoryCommand::Status => memory_cmd::status(root, &mut stdout)?,
                MemoryCommand::Inspect => memory_cmd::inspect(root, &mut stdout)?,
                MemoryCommand::Used => memory_cmd::used(root, &mut stdout)?,
                MemoryCommand::Search {
                    query,
                    format,
                    json,
                } => {
                    let is_tty = io::stdout().is_terminal();
                    let resolved = output::resolve_format(format, json, is_tty);
                    memory_cmd::search(
                        root,
                        resolution.is_found(),
                        &query,
                        resolved,
                        output::show_format_hint(resolved, is_tty),
                        &mut stdout,
                        &mut io::stderr(),
                    )?;
                }
                MemoryCommand::Delete { id } => memory_cmd::delete(root, &id, &mut stdout)?,
                MemoryCommand::Disable => memory_cmd::disable(root, &mut stdout)?,
                MemoryCommand::Enable => memory_cmd::enable(root, &mut stdout)?,
                MemoryCommand::Graph { symbol } => {
                    memory_cmd::graph(root, &symbol, &mut stdout)?;
                }
                MemoryCommand::Export { path, html } => {
                    memory_cmd::export(root, &path, html, &mut stdout)?;
                }
            }
        }
        Command::Learning { workspace, command } => {
            use localpilot_localmind::ReviewVerdict;
            let cwd = std::env::current_dir()?;
            let resolution = resolve_learning_store(workspace, &cwd);
            let root = resolution.path();
            let mut stdout = io::stdout().lock();
            match command {
                LearningCommand::Closeout { session } => {
                    learning_cmd::closeout(root, &session, &mut stdout)?;
                }
                LearningCommand::Review { command } => match command {
                    ReviewCommand::List => learning_cmd::review_list(root, &mut stdout)?,
                    ReviewCommand::Show { id } => {
                        learning_cmd::review_show(root, &id, &mut stdout)?;
                    }
                    ReviewCommand::Accept { id, reviewer, note } => {
                        learning_cmd::review_decide(
                            root,
                            &id,
                            ReviewVerdict::Accept,
                            &reviewer,
                            note,
                            &mut stdout,
                        )?;
                    }
                    ReviewCommand::Reject { id, reviewer, note } => {
                        learning_cmd::review_decide(
                            root,
                            &id,
                            ReviewVerdict::Reject,
                            &reviewer,
                            note,
                            &mut stdout,
                        )?;
                    }
                    ReviewCommand::Defer { id, reviewer, note } => {
                        learning_cmd::review_decide(
                            root,
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
                            root,
                            &id,
                            ReviewVerdict::Edit { replacement },
                            &reviewer,
                            note,
                            &mut stdout,
                        )?;
                    }
                    ReviewCommand::Purge { yes } => {
                        learning_cmd::review_purge(root, yes, &mut stdout)?;
                    }
                },
                LearningCommand::Seed { file, dry_run } => {
                    learning_cmd::seed(root, &file, dry_run, &mut stdout)?
                }
                LearningCommand::Promote { id } => learning_cmd::promote(root, &id, &mut stdout)?,
                LearningCommand::Export { out, scope } => {
                    learning_cmd::bundle_export(root, &scope, &out, &mut stdout)?;
                }
                LearningCommand::Import { input, apply } => {
                    learning_cmd::bundle_import(root, &input, apply, &mut stdout)?;
                }
                LearningCommand::Search {
                    query,
                    format,
                    json,
                } => {
                    let is_tty = io::stdout().is_terminal();
                    let resolved = output::resolve_format(format, json, is_tty);
                    learning_cmd::search(
                        root,
                        resolution.is_found(),
                        &query,
                        resolved,
                        output::show_format_hint(resolved, is_tty),
                        &mut stdout,
                        &mut io::stderr(),
                    )?;
                }
                LearningCommand::Skills { command } => match command {
                    SkillsCommand::Generate => learning_cmd::skills_generate(root, &mut stdout)?,
                    SkillsCommand::List => learning_cmd::skills_list(root, &mut stdout)?,
                    SkillsCommand::Show { id } => learning_cmd::skill_show(root, &id, &mut stdout)?,
                    SkillsCommand::Export { id, out } => {
                        learning_cmd::skill_export(root, &id, out, &mut stdout)?;
                    }
                },
                LearningCommand::Audit => learning_cmd::audit(root, &mut stdout)?,
                LearningCommand::Freshness {
                    scope,
                    apply,
                    max_age_days,
                    unused_grace_days,
                    version_sensitive_min_age_days,
                    max_flags,
                    format,
                    json,
                } => {
                    let is_tty = io::stdout().is_terminal();
                    let resolved = output::resolve_format(format, json, is_tty);
                    let params = localpilot_localmind::FreshnessParams {
                        max_age_days,
                        unused_grace_days,
                        version_sensitive_min_age_days,
                        max_flags,
                    };
                    learning_cmd::freshness(root, &params, &scope, apply, resolved, &mut stdout)?;
                }
                LearningCommand::Lifecycle { top, format, json } => {
                    let is_tty = io::stdout().is_terminal();
                    let resolved = output::resolve_format(format, json, is_tty);
                    learning_cmd::lifecycle(root, top, resolved, &mut stdout)?;
                }
                LearningCommand::Revalidate {
                    apply,
                    sample,
                    format,
                    json,
                } => {
                    let is_tty = io::stdout().is_terminal();
                    let resolved = output::resolve_format(format, json, is_tty);
                    learning_cmd::revalidate(
                        root,
                        sample,
                        apply,
                        resolved,
                        &mut stdout,
                        &mut io::stderr(),
                    )?;
                }
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
        Command::Research {
            topic,
            no_memory,
            no_report,
            web,
            no_web,
            rounds,
            max_questions,
            time_budget,
            quick,
        } => {
            let cwd = std::env::current_dir()?;
            let mut stdout = io::stdout().lock();
            let web_override = if no_web {
                Some(false)
            } else if web {
                Some(true)
            } else {
                None
            };
            match research::options_from_config(&cwd, !no_report, !no_memory)? {
                Some(mut options) => {
                    // Per-run flag overrides beat config; config beats defaults.
                    if quick {
                        options.max_rounds = 1;
                    }
                    if let Some(rounds) = rounds {
                        options.max_rounds = rounds.max(1);
                    }
                    if let Some(max_questions) = max_questions {
                        options.max_questions = max_questions.max(1);
                    }
                    if let Some(seconds) = time_budget {
                        options.time_budget = Some(std::time::Duration::from_secs(seconds));
                    }
                    research::run_research_command(
                        &cwd,
                        &topic,
                        &options,
                        web_override,
                        &mut stdout,
                    )
                    .await?;
                }
                None => writeln!(stdout, "research is disabled ([research].enabled = false)")?,
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
            continue_latest,
            resume,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            let resume = session_cmd::resolve_resume(continue_latest, resume.as_deref())?;
            repl::run_chat(model.as_deref(), provider.as_deref(), profile, resume).await?;
        }
        Command::Print {
            prompt,
            model,
            provider,
            permission,
            bypass,
            allow_writes,
            self_review,
            continue_latest,
            resume,
        } => {
            let profile = session_cmd::resolve_profile(permission.as_deref(), bypass);
            let resume = session_cmd::resolve_resume(continue_latest, resume.as_deref())?;
            let outcome = session_cmd::print_mode(
                &prompt,
                &model,
                provider.as_deref(),
                profile,
                allow_writes,
                self_review,
                resume,
            )
            .await?;
            if outcome.consumer_gone {
                exit_code = std::process::ExitCode::from(EXIT_OUTPUT_CONSUMER_GONE);
            }
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
            verify: _verify,
            no_verify,
            verify_command,
            learn,
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
                // Verify-before-done is on by default for `eval`: the benchmark
                // measures compiled+tested solves. `--no-verify` opts out,
                // reproducing the pre-default behaviour byte-for-byte. The legacy
                // `--verify` flag is redundant (the gate is already on).
                verify: !no_verify,
                verify_command: verify_command.as_deref(),
                learn,
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
            SessionCommand::Name { id, name } => {
                session_cmd::name_session(&id, &name)?;
                println!("named session {id} \"{name}\"");
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
                let session = session_cmd::resolve_session_ref(&id)?;
                let outcome = session_cmd::print_mode(
                    &prompt,
                    &model,
                    provider.as_deref(),
                    profile,
                    allow_writes,
                    false,
                    Some(session),
                )
                .await?;
                if outcome.consumer_gone {
                    exit_code = std::process::ExitCode::from(EXIT_OUTPUT_CONSUMER_GONE);
                }
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
            // Discovery is async (bounded web search); the rest of the skills
            // surface is synchronous. Both share the one command enum.
            let outcome = match command {
                ProjectSkillsCommand::Research {
                    query,
                    global,
                    no_web,
                } => {
                    skill_discovery::run_skill_research(
                        &cwd,
                        &query.join(" "),
                        global,
                        !no_web,
                        &mut stdout,
                    )
                    .await?
                }
                other => {
                    let stdin_is_tty = io::stdin().is_terminal();
                    skills_cmd::run(other, &cwd, stdin_is_tty, &mut stdout)?
                }
            };
            stdout.flush()?;
            if outcome.had_failure {
                exit_code = std::process::ExitCode::FAILURE;
            }
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
            cleanup,
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
                        cleanup,
                        friction_file: friction_file.as_deref(),
                        process_file: process_file.as_deref(),
                    },
                    &mut stdout,
                )?;
            }
            stdout.flush()?;
        }
    }

    Ok(exit_code)
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
                let profile = session_cmd::resolve_profile_from_config(&config);
                return repl::run_chat(None, None, profile, None).await;
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
    doctor::run(&mut stdout).await?;
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
    use localpilot_localmind::StoreRoot;

    #[test]
    fn workspace_override_pins_the_root_and_skips_the_walk_up() {
        // --workspace wins over the cwd walk-up: even with no store at the pinned
        // path, the resolver returns that path (NotFound), never an ancestor's.
        let dir = tempfile::tempdir().unwrap();
        let pinned = dir.path().join("pinned");
        std::fs::create_dir_all(&pinned).unwrap();
        let cwd = dir.path().join("elsewhere");
        std::fs::create_dir_all(&cwd).unwrap();

        let resolved = resolve_learning_store(Some(pinned.clone()), &cwd);
        assert_eq!(resolved, StoreRoot::NotFound(pinned.clone()));

        // With a store at the pinned path it resolves Found there.
        std::fs::write(
            pinned.join(".localmind.toml"),
            "[learning]\nenabled = true\n",
        )
        .unwrap();
        assert_eq!(
            resolve_learning_store(Some(pinned.clone()), &cwd),
            StoreRoot::Found(pinned)
        );
    }

    #[cfg(feature = "tui")]
    #[test]
    fn skills_slash_and_cli_parse_to_the_same_operation() {
        // LocalHub#40 acceptance: the `/skills …` slash form and the
        // `localpilot skills …` CLI form parse to identical operations, because
        // both drive the one clap command surface.
        fn cli(args: &[&str]) -> ProjectSkillsCommand {
            let parsed =
                Cli::try_parse_from(std::iter::once("localpilot").chain(args.iter().copied()))
                    .expect("cli parse");
            match parsed.command.expect("a command") {
                Command::Skills { command } => command,
                other => panic!("expected Skills, got {other:?}"),
            }
        }
        fn slash(raw: &str) -> ProjectSkillsCommand {
            let tokens: Vec<&str> = raw.split_whitespace().collect();
            SkillsSlash::try_parse_from(tokens)
                .expect("slash parse")
                .command
        }
        let cases = [
            (
                vec!["skills", "repo", "add", "https://github.com/o/r", "--yes"],
                "repo add https://github.com/o/r --yes",
            ),
            (vec!["skills", "repo", "list", "-g"], "repo list -g"),
            (
                vec!["skills", "install", "helper", "--repo", "src", "-g"],
                "install helper --repo src -g",
            ),
            (
                vec!["skills", "install", "--all", "--repo", "src"],
                "install --all --repo src",
            ),
            (vec!["skills", "available", "query"], "available query"),
            (
                vec!["skills", "delete", "helper", "--yes"],
                "delete helper --yes",
            ),
            (vec!["skills", "list"], "list"),
            (vec!["skills", "show", "helper", "-g"], "show helper -g"),
        ];
        for (cli_args, slash_raw) in cases {
            assert_eq!(
                cli(&cli_args),
                slash(slash_raw),
                "mismatch for `{slash_raw}`"
            );
        }
    }

    #[test]
    fn no_workspace_walks_up_from_cwd_to_the_root_store() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        std::fs::write(root.join(".localmind.toml"), "[learning]\nenabled = true\n").unwrap();
        let deep = root.join("a").join("b");
        std::fs::create_dir_all(&deep).unwrap();

        let resolved = resolve_learning_store(None, &deep);
        assert!(resolved.is_found());
        assert_eq!(
            resolved.path().canonicalize().unwrap(),
            root.canonicalize().unwrap()
        );
    }

    #[test]
    fn learning_and_memory_accept_a_workspace_override() {
        // The `--workspace` flag parses on both parent commands, ahead of the
        // subcommand (`localpilot learning --workspace <path> search <query>`).
        let cli = Cli::try_parse_from([
            "localpilot",
            "learning",
            "--workspace",
            "some/dir",
            "search",
            "q",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Learning {
                workspace: Some(_),
                ..
            })
        ));
        let cli =
            Cli::try_parse_from(["localpilot", "memory", "--workspace", "some/dir", "status"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Memory {
                workspace: Some(_),
                ..
            })
        ));
    }

    #[test]
    fn rpc_resume_flags_parse_and_conflict() {
        // Fresh session by default.
        let cli = Cli::try_parse_from(["localpilot", "rpc"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Rpc {
                continue_latest: false,
                resume: None,
                ..
            })
        ));
        // `--continue` opens the latest session.
        let cli = Cli::try_parse_from(["localpilot", "rpc", "--continue"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Rpc {
                continue_latest: true,
                ..
            })
        ));
        // `--resume` takes an id or name.
        let cli = Cli::try_parse_from(["localpilot", "rpc", "--resume", "review-run"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Rpc {
                resume: Some(_),
                ..
            })
        ));
        // The two are mutually exclusive, as on `chat`.
        assert!(
            Cli::try_parse_from(["localpilot", "rpc", "--continue", "--resume", "review-run"])
                .is_err()
        );
    }

    #[test]
    fn mcp_serve_parses_with_resume_and_approval_flags() {
        let cli = Cli::try_parse_from(["localpilot", "mcp", "serve"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Mcp(McpCommand::Serve {
                no_approvals: false,
                continue_latest: false,
                resume: None,
                ..
            }))
        ));
        let cli =
            Cli::try_parse_from(["localpilot", "mcp", "serve", "--continue", "--no-approvals"])
                .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Mcp(McpCommand::Serve {
                no_approvals: true,
                continue_latest: true,
                ..
            }))
        ));
        // Same resume exclusivity as `chat` and `rpc`.
        assert!(
            Cli::try_parse_from(["localpilot", "mcp", "serve", "--continue", "--resume", "x"])
                .is_err()
        );
    }

    #[test]
    fn print_self_review_flag_defaults_off_and_parses() {
        // Off by default: a plain `print` leaves the advisory cue disabled.
        let cli =
            Cli::try_parse_from(["localpilot", "print", "do a thing", "--model", "m"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Print {
                self_review: false,
                ..
            })
        ));
        // Opt-in with the flag.
        let cli = Cli::try_parse_from([
            "localpilot",
            "print",
            "do a thing",
            "--model",
            "m",
            "--self-review",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Print {
                self_review: true,
                ..
            })
        ));
    }

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

    #[test]
    fn eval_verify_flags_parse() {
        // Default: the verify gate is **on** for eval, so `no_verify` is false.
        let cli = Cli::try_parse_from(["localpilot", "eval", "fix it", "--model", "m"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Eval {
                no_verify: false,
                verify: false,
                ..
            })
        ));

        // `--no-verify` opts out of the default-on gate.
        let cli = Cli::try_parse_from([
            "localpilot",
            "eval",
            "fix it",
            "--model",
            "m",
            "--no-verify",
        ])
        .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Eval {
                no_verify: true,
                ..
            })
        ));

        // The legacy `--verify` flag still parses (now redundant — gate is on).
        let cli = Cli::try_parse_from(["localpilot", "eval", "fix it", "--model", "m", "--verify"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Eval { verify: true, .. })
        ));

        // `--verify` and `--no-verify` are mutually exclusive.
        assert!(Cli::try_parse_from([
            "localpilot",
            "eval",
            "fix it",
            "--model",
            "m",
            "--verify",
            "--no-verify",
        ])
        .is_err());

        // `--verify-command` carries an explicit command.
        let cli = Cli::try_parse_from([
            "localpilot",
            "eval",
            "fix it",
            "--model",
            "m",
            "--verify-command",
            "ctest",
        ])
        .unwrap();
        match cli.command {
            Some(Command::Eval { verify_command, .. }) => {
                assert_eq!(verify_command.as_deref(), Some("ctest"));
            }
            other => panic!("expected Eval, got {other:?}"),
        }
    }

    #[test]
    fn eval_learn_flag_parses_and_defaults_off() {
        // `--learn` opts the run into closing out into LocalMind (review-gated).
        let cli = Cli::try_parse_from(["localpilot", "eval", "fix it", "--model", "m", "--learn"])
            .unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Eval { learn: true, .. })
        ));

        // Default: a plain `eval` stays a clean-room measurement (no learning).
        let cli = Cli::try_parse_from(["localpilot", "eval", "fix it", "--model", "m"]).unwrap();
        assert!(matches!(
            cli.command,
            Some(Command::Eval { learn: false, .. })
        ));
    }
}
