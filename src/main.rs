//! memd — universal local memory daemon for LLMs, backed by Meilisearch.
//!
//! A single always-on service that turns a local Meilisearch instance into a
//! persistent, time-aware, semantically-searchable memory layer shared across
//! every LLM tool, exposed through one stable MCP server.

mod cli;
mod config;
mod crawler;
mod daemon;
mod launchd;
mod mcp;
mod meili;
mod memory;
mod paths;
mod update;

use clap::{Parser, Subcommand};

/// memd — universal local memory daemon for LLMs, backed by Meilisearch.
#[derive(Parser)]
#[command(name = "memd", version, about, long_about = None)]
struct Cli {
    #[command(subcommand)]
    command: Command,
}

#[derive(Subcommand)]
enum Command {
    /// Start the daemon (Meilisearch + crawler + MCP); installs the service if needed.
    Up {
        /// Run in the foreground instead of detaching.
        #[arg(long)]
        foreground: bool,
    },
    /// Stop the running daemon.
    Down,
    /// Show daemon + Meilisearch health, index stats, and last crawl.
    Status,
    /// Tail the daemon logs.
    Logs {
        /// Follow the log output.
        #[arg(short, long)]
        follow: bool,
    },
    /// Run the daemon process in the foreground (used by the service runner).
    Serve,
    /// Add a memory manually.
    Add {
        /// The memory text. Omit when using --file.
        text: Option<String>,
        /// Annotate an important file at this path instead of free text.
        #[arg(long)]
        file: Option<String>,
        /// Note/description for a file annotation.
        #[arg(long)]
        note: Option<String>,
        /// Memory type (fact, preference, decision, task, project_overview, ...).
        #[arg(long, value_name = "TYPE")]
        r#type: Option<String>,
        /// Comma-separated tags.
        #[arg(long)]
        tags: Option<String>,
        /// Scope (global or a path prefix).
        #[arg(long)]
        scope: Option<String>,
        /// Short title.
        #[arg(long)]
        title: Option<String>,
    },
    /// Search memories.
    Search {
        /// The query text.
        query: String,
        /// Filter by type.
        #[arg(long, value_name = "TYPE")]
        r#type: Option<String>,
        /// Only memories created since this RFC3339 timestamp or unix seconds.
        #[arg(long)]
        since: Option<String>,
        /// Semantic ratio (0.0 keyword .. 1.0 vector).
        #[arg(long)]
        semantic_ratio: Option<f32>,
        /// Maximum number of results.
        #[arg(long, default_value_t = 10)]
        limit: usize,
    },
    /// Forget (delete) a memory by id.
    Forget {
        /// The memory id.
        id: String,
    },
    /// Manage passive ingestion (the crawler).
    Crawl {
        #[command(subcommand)]
        action: CrawlAction,
    },
    /// MCP integration.
    Mcp {
        /// Run a stdio bridge for stdio-only clients.
        #[arg(long)]
        stdio: bool,
    },
    /// launchd service integration.
    Service {
        #[command(subcommand)]
        action: ServiceAction,
    },
    /// Print relevant memories as markdown (for a SessionStart hook).
    Context {
        /// Bias toward memories in this scope (e.g. the project path).
        #[arg(long)]
        scope: Option<String>,
        /// Optional query to rank by relevance instead of recency.
        #[arg(long)]
        query: Option<String>,
        /// Maximum memories to surface.
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Conservatively capture a finished session turn (for a Stop hook).
    /// Reads the hook JSON payload on stdin.
    Capture,
    /// Inject/remove memd usage directives in agent instruction files.
    Directives {
        #[command(subcommand)]
        action: DirectivesAction,
    },
    /// One-command install: relocate the binary, start the daemon, register
    /// with detected agents, install directives, and wire up hooks.
    Setup {
        /// Skip wiring the Claude Code SessionStart/Stop hooks.
        #[arg(long)]
        no_hooks: bool,
    },
    /// Diagnose Meilisearch / model / config issues.
    Doctor {
        /// Attempt to repair detected issues (e.g. reset a version-mismatched db).
        #[arg(long)]
        fix: bool,
    },
    /// Check GitHub for updates to memd and the managed engine; apply them.
    Update {
        /// Only report what would be updated.
        #[arg(long)]
        check: bool,
    },
}

#[derive(Subcommand)]
enum CrawlAction {
    /// Run a one-off crawl of the configured roots.
    Run,
    /// Show crawl status.
    Status,
    /// Print the active crawl configuration.
    Config,
}

#[derive(Subcommand)]
enum ServiceAction {
    /// Install the launchd service (macOS).
    Install,
    /// Uninstall the launchd service.
    Uninstall,
}

#[derive(Subcommand)]
enum DirectivesAction {
    /// Write the managed memd block into known agent instruction files.
    Install,
    /// Remove the managed memd block from agent instruction files.
    Uninstall,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Up { foreground } => cli::up(foreground).await,
        Command::Down => cli::down().await,
        Command::Status => cli::status().await,
        Command::Logs { follow } => cli::logs(follow).await,
        Command::Serve => daemon::serve().await,
        Command::Add {
            text,
            file,
            note,
            r#type,
            tags,
            scope,
            title,
        } => cli::add(text, file, note, r#type, tags, scope, title).await,
        Command::Search {
            query,
            r#type,
            since,
            semantic_ratio,
            limit,
        } => cli::search(query, r#type, since, semantic_ratio, limit).await,
        Command::Forget { id } => cli::forget(id).await,
        Command::Crawl { action } => match action {
            CrawlAction::Run => cli::crawl_run().await,
            CrawlAction::Status => cli::crawl_status().await,
            CrawlAction::Config => cli::crawl_config().await,
        },
        Command::Mcp { stdio } => {
            if stdio {
                mcp::run_stdio().await
            } else {
                anyhow::bail!(
                    "`memd mcp` requires --stdio; the HTTP MCP endpoint is served by the daemon (`memd up`)"
                )
            }
        }
        Command::Service { action } => match action {
            ServiceAction::Install => launchd::install(&cli::resolve_memd_exe()),
            ServiceAction::Uninstall => launchd::uninstall(),
        },
        Command::Context {
            scope,
            query,
            limit,
        } => cli::context(scope, query, limit).await,
        Command::Capture => cli::capture().await,
        Command::Directives { action } => match action {
            DirectivesAction::Install => cli::directives_install(),
            DirectivesAction::Uninstall => cli::directives_uninstall(),
        },
        Command::Setup { no_hooks } => cli::setup(no_hooks).await,
        Command::Doctor { fix } => cli::doctor(fix).await,
        Command::Update { check } => cli::update(check).await,
    }
}
