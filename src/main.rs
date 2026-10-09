//! memd — universal local memory daemon for LLMs, backed by Meilisearch.
//!
//! A single always-on service that turns a local Meilisearch instance into a
//! persistent, time-aware, semantically-searchable memory layer shared across
//! every LLM tool, exposed through one stable MCP server.

mod agents;
mod cli;
mod config;
mod crawler;
mod daemon;
mod history;
mod knowledge;
mod launchd;
mod logging;
mod mcp;
mod meili;
mod memory;
mod paths;
mod update;

use clap::{Args, Parser, Subcommand};

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
    /// Ensure the daemon is running (fast, silent; used by the SessionStart hook).
    Ensure,
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
        /// Only memories that mention this entity (name or id).
        #[arg(long)]
        entity: Option<String>,
        /// Only records with this status.
        #[arg(long)]
        status: Option<String>,
        /// Only entities of this kind.
        #[arg(long = "kind-of")]
        kind_of: Option<String>,
    },
    /// Show what memd knows about an entity, or add one (`memd entity add`).
    Entity(EntityArgs),
    /// Declare a relation: `memd relate "Lumen" part_of "Meilisearch Lab"`.
    Relate {
        subject: String,
        predicate: String,
        object: String,
        /// Optional note on the relation.
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove a relation.
    Unrelate {
        subject: String,
        predicate: String,
        object: String,
    },
    /// Forget (delete) a memory by id.
    Forget {
        /// The memory id.
        id: String,
    },
    /// Show the history of memory changes (created, updated, deleted, crawled).
    History {
        /// Filter by action: create, update, delete, or crawl.
        #[arg(long)]
        action: Option<String>,
        /// Filter by memory type.
        #[arg(long, value_name = "TYPE")]
        r#type: Option<String>,
        /// Filter by scope.
        #[arg(long)]
        scope: Option<String>,
        /// Only events since this unix timestamp or relative span (e.g. 30d, 12h).
        #[arg(long)]
        since: Option<String>,
        /// Maximum number of events.
        #[arg(long, default_value_t = 20)]
        limit: usize,
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
    /// Print relevant memories for a session-start hook. Reads the hook's JSON
    /// payload on stdin (when piped) to pick up the working directory.
    Context {
        /// Project scope (defaults to `cwd` from the hook payload, else the
        /// current directory).
        #[arg(long)]
        scope: Option<String>,
        /// The agent invoking the hook (claude-code, codex, gemini-cli, …).
        #[arg(long)]
        agent: Option<String>,
        /// Output format: `text` (plain markdown on stdout) or `json`
        /// (`hookSpecificOutput.additionalContext`, as Gemini CLI expects).
        #[arg(long, default_value = "text")]
        format: String,
        /// Optional query to rank by relevance instead of recency.
        #[arg(long)]
        query: Option<String>,
        /// Maximum memories to surface.
        #[arg(long, default_value_t = 8)]
        limit: usize,
    },
    /// Conservatively capture a finished session turn (for a Stop /
    /// AfterAgent hook). Reads the hook JSON payload on stdin.
    Capture {
        /// The agent invoking the hook (claude-code, codex, gemini-cli, …).
        #[arg(long)]
        agent: Option<String>,
    },
    /// Inject/remove memd usage directives in agent instruction files.
    Directives {
        #[command(subcommand)]
        action: DirectivesAction,
    },
    /// Install/remove memd's Claude Code skills (~/.claude/skills/).
    Skills {
        #[command(subcommand)]
        action: SkillsAction,
    },
    /// One-command install: relocate the binary, start the daemon, register
    /// with detected agents, install directives, and wire up hooks.
    Setup {
        /// Skip wiring the session-start / end-of-turn hooks.
        #[arg(long)]
        no_hooks: bool,
        /// Explicit, comma-separated list of agent ids to connect
        /// (claude-code, codex, gemini-cli, cursor, windsurf, cline, zed)
        /// instead of the interactive picker. Agents not listed are left as
        /// they are.
        #[arg(long, value_delimiter = ',')]
        agents: Option<Vec<String>>,
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

#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct EntityArgs {
    #[command(subcommand)]
    action: Option<EntityAction>,
    /// Entity name, alias or id.
    name: Option<String>,
    /// 1, or 2 to include the neighbours' relations.
    #[arg(long, default_value_t = 1)]
    depth: u8,
}

#[derive(Subcommand)]
enum EntityAction {
    /// Create or update an entity.
    Add {
        name: String,
        /// company, team, person, project, product, service, customer, concept.
        #[arg(long)]
        kind: String,
        #[arg(long = "desc")]
        description: Option<String>,
        #[arg(long, value_delimiter = ',')]
        alias: Vec<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        scope: Option<String>,
    },
}

#[derive(Subcommand)]
enum CrawlAction {
    /// Run a one-off crawl of the configured roots.
    Run {
        /// Drop every crawled document first and rebuild from scratch.
        #[arg(long)]
        reset: bool,
    },
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

#[derive(Subcommand)]
enum SkillsAction {
    /// Write memd's skills into ~/.claude/skills/.
    Install,
    /// Remove memd's skills from ~/.claude/skills/.
    Uninstall,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = Cli::parse();
    match cli.command {
        Command::Up { foreground } => cli::up(foreground).await,
        Command::Down => cli::down().await,
        Command::Ensure => cli::ensure().await,
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
            entity,
            status,
            kind_of,
        } => {
            cli::search(
                query,
                r#type,
                since,
                semantic_ratio,
                limit,
                entity,
                status,
                kind_of,
            )
            .await
        }
        Command::Entity(args) => match args.action {
            Some(EntityAction::Add {
                name,
                kind,
                description,
                alias,
                owner,
                status,
                url,
                scope,
            }) => cli::entity_add(name, kind, description, alias, owner, status, url, scope).await,
            None => match args.name {
                Some(name) => cli::entity_show(name, args.depth).await,
                None => anyhow::bail!(
                    "usage: memd entity <name> | memd entity add <name> --kind <kind>"
                ),
            },
        },
        Command::Relate {
            subject,
            predicate,
            object,
            note,
        } => cli::relate(subject, predicate, object, note).await,
        Command::Unrelate {
            subject,
            predicate,
            object,
        } => cli::unrelate(subject, predicate, object).await,
        Command::Forget { id } => cli::forget(id).await,
        Command::History {
            action,
            r#type,
            scope,
            since,
            limit,
        } => cli::history(action, r#type, scope, since, limit).await,
        Command::Crawl { action } => match action {
            CrawlAction::Run { reset } => cli::crawl_run(reset).await,
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
            agent,
            format,
            query,
            limit,
        } => cli::context(scope, agent, &format, query, limit).await,
        Command::Capture { agent } => cli::capture(agent).await,
        Command::Directives { action } => match action {
            DirectivesAction::Install => cli::directives_install(),
            DirectivesAction::Uninstall => cli::directives_uninstall(),
        },
        Command::Skills { action } => match action {
            SkillsAction::Install => cli::skills_install(),
            SkillsAction::Uninstall => cli::skills_uninstall(),
        },
        Command::Setup { no_hooks, agents } => cli::setup(no_hooks, agents).await,
        Command::Doctor { fix } => cli::doctor(fix).await,
        Command::Update { check } => cli::update(check).await,
    }
}
