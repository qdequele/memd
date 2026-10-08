//! CLI command implementations. These talk to the running daemon's Meilisearch
//! instance (started by `memd up`); commands that need it fail with a helpful
//! hint when the daemon is down.

use crate::config::Config;
use crate::memory::model::now_secs;
use crate::memory::{
    GetRequest, MemoryService, MemoryType, ProjectionOptions, SaveRequest, Source,
};
use crate::{crawler, launchd, meili, paths};
use anyhow::{Context, Result, bail};
use serde_json::Value;
use std::path::{Path, PathBuf};
use std::time::Duration;

/// What `memd up` should do given the two health signals. The MCP endpoint
/// answering is not enough: a wedged daemon (process alive, Meilisearch
/// unhealthy) must be restarted, not reported as "already running".
#[derive(Debug, PartialEq)]
enum UpAction {
    AlreadyRunning,
    RestartWedged,
    Start,
}

fn up_action(mcp_up: bool, meili_up: bool) -> UpAction {
    match (mcp_up, meili_up) {
        (true, true) => UpAction::AlreadyRunning,
        (true, false) => UpAction::RestartWedged,
        (false, _) => UpAction::Start,
    }
}

/// Start the daemon. In foreground mode this *is* the daemon; otherwise it
/// installs the launchd service (macOS) or spawns a detached process.
pub async fn up(foreground: bool) -> Result<()> {
    if foreground {
        return crate::daemon::serve().await;
    }

    let cfg = Config::load_or_init()?;
    let svc = MemoryService::from_config(&cfg);
    match up_action(daemon_healthy(&cfg).await, svc.client().is_healthy().await) {
        UpAction::AlreadyRunning => {
            println!(
                "memd is already running (MCP on http://{}:{}).",
                cfg.mcp.host, cfg.mcp.port
            );
            return Ok(());
        }
        UpAction::RestartWedged => {
            println!("memd is running but Meilisearch is not responding — restarting the daemon…");
            down().await?;
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        UpAction::Start => {}
    }

    if cfg!(target_os = "macos") {
        launchd::install(&resolve_memd_exe())?;
    } else {
        spawn_detached()?;
        println!("Started memd daemon in the background.");
    }

    // Wait for both the MCP endpoint and Meilisearch to come up.
    print!("Waiting for daemon to become healthy");
    for _ in 0..120 {
        if daemon_healthy(&cfg).await && svc.client().is_healthy().await {
            println!(
                "\nmemd is up. MCP: http://{}:{}/mcp",
                cfg.mcp.host, cfg.mcp.port
            );
            return Ok(());
        }
        print!(".");
        use std::io::Write;
        let _ = std::io::stdout().flush();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    bail!("daemon did not become healthy in time; check `memd logs`")
}

/// Best-effort: make sure the daemon is running, fast and silent. Used by the
/// Claude Code SessionStart hook so memory keeps working even if the daemon
/// died or was never started. Prints nothing to stdout (the hook injects its
/// output into the session) and never errors out the caller — a failure here
/// must not disrupt a session start.
pub async fn ensure() -> Result<()> {
    let Ok(cfg) = Config::load_or_init() else {
        return Ok(());
    };
    if daemon_healthy(&cfg).await {
        return Ok(());
    }
    if let Err(e) = ensure_running(&cfg).await {
        eprintln!("memd ensure: could not start daemon: {e:#}");
    }
    Ok(())
}

/// Start the daemon without `up`'s long readiness wait: kick the launchd service
/// (or spawn a detached process), then poll briefly so a following `context`
/// call in the same hook can already see it healthy. Silent on stdout.
async fn ensure_running(cfg: &Config) -> Result<()> {
    if cfg!(target_os = "macos") {
        if launchd::is_installed() {
            let _ = launchd::load();
        } else {
            launchd::install(&resolve_memd_exe())?;
        }
    } else {
        spawn_detached()?;
    }
    for _ in 0..10 {
        if daemon_healthy(cfg).await {
            break;
        }
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    Ok(())
}

/// Stop the daemon (launchd unload, or kill the recorded pid).
pub async fn down() -> Result<()> {
    if launchd::is_installed() {
        launchd::uninstall()?;
        return Ok(());
    }
    let pid_path = paths::pid_file()?;
    if let Ok(pid_str) = std::fs::read_to_string(&pid_path) {
        if let Ok(pid) = pid_str.trim().parse::<i32>() {
            #[cfg(unix)]
            unsafe {
                libc_kill(pid, 15);
            }
            println!("Sent stop signal to memd (pid {pid}).");
        }
        let _ = std::fs::remove_file(&pid_path);
    } else {
        println!("memd does not appear to be running.");
    }
    Ok(())
}

/// Report daemon, Meilisearch, index, and crawl status.
pub async fn status() -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = MemoryService::from_config(&cfg);

    let ms_up = svc.client().is_healthy().await;
    println!("Meilisearch: {}", health_label(ms_up));
    if ms_up {
        if let Ok(v) = svc.client().version().await {
            println!("  version:   {v} (pinned {})", cfg.meilisearch.version);
        }
        println!("  url:       {}", cfg.meili_url());
        if let Ok(stats) = svc.stats(&[]).await {
            let count = stats
                .get("numberOfDocuments")
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            println!("  memories:  {count}");
        }
        if let Some(events) = svc.events().count().await {
            println!("  events:    {events}");
        }
    }

    let mcp_up = daemon_healthy(&cfg).await;
    println!(
        "MCP endpoint: {} (http://{}:{}/mcp)",
        health_label(mcp_up),
        cfg.mcp.host,
        cfg.mcp.port
    );
    println!(
        "Service:      {}",
        if launchd::is_installed() {
            "installed (launchd)"
        } else {
            "not installed"
        }
    );

    let upd = crate::update::UpdateState::load();
    if upd.last_check_secs > 0 {
        println!(
            "Updates:      last check {} — {}",
            format_ts(upd.last_check_secs),
            upd.last_result
        );
    } else {
        println!("Updates:      never checked (daily check runs in the daemon)");
    }

    println!("Agents:");
    for agent in crate::agents::registry() {
        let line = match agent.status() {
            crate::agents::AgentStatus::Configured => {
                format!("configured ({})", agent.transport())
            }
            crate::agents::AgentStatus::Detected => "detected — not configured".to_string(),
            crate::agents::AgentStatus::NotDetected => "not detected".to_string(),
        };
        println!("  {:<13} {}", agent.name, line);
    }

    match crawler::last_summary() {
        Some(s) => println!(
            "Last crawl:   {} indexed, {} skipped, {} deleted, {} errors (at {})",
            s.indexed, s.skipped, s.deleted, s.errors, s.finished_at
        ),
        None => println!("Last crawl:   never"),
    }
    Ok(())
}

/// Print (and optionally follow) the daemon log file.
pub async fn logs(follow: bool) -> Result<()> {
    let path = paths::log_file()?;
    if !path.exists() {
        println!("No log file yet at {}", path.display());
        return Ok(());
    }
    if follow {
        let mut cmd = std::process::Command::new("tail");
        cmd.arg("-f").arg(&path);
        let status = cmd.status().context("running tail -f")?;
        std::process::exit(status.code().unwrap_or(0));
    } else {
        let contents = std::fs::read_to_string(&path)?;
        print!("{contents}");
    }
    Ok(())
}

/// Add a memory (free text or a file annotation).
#[allow(clippy::too_many_arguments)]
pub async fn add(
    text: Option<String>,
    file: Option<String>,
    note: Option<String>,
    ty: Option<String>,
    tags: Option<String>,
    scope: Option<String>,
    title: Option<String>,
) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = require_daemon(&cfg).await?;

    let parsed_type = match &ty {
        Some(s) => Some(MemoryType::parse(s).ok_or_else(|| anyhow::anyhow!("unknown type: {s}"))?),
        None => None,
    };
    let tags_vec = tags
        .map(|t| {
            t.split(',')
                .map(|s| s.trim().to_string())
                .filter(|s| !s.is_empty())
                .collect()
        })
        .unwrap_or_default();

    let req = if let Some(path) = file {
        // File annotation.
        let abs = crate::config::expand_tilde(&path);
        let abs_str = abs.to_string_lossy().to_string();
        let note = note.unwrap_or_else(|| format!("Important file: {abs_str}"));
        SaveRequest {
            content: format!("{note}\n\nPath: {abs_str}"),
            title: title.or_else(|| Some(format!("Annotation: {abs_str}"))),
            r#type: Some(parsed_type.unwrap_or(MemoryType::FileAnnotation)),
            tags: tags_vec,
            scope,
            source: Some(Source::Cli),
            source_path: Some(abs_str),
            source_client: None,
        }
    } else {
        let content =
            text.ok_or_else(|| anyhow::anyhow!("provide memory text or --file <path>"))?;
        SaveRequest {
            content,
            title,
            r#type: parsed_type,
            tags: tags_vec,
            scope,
            source: Some(Source::Cli),
            source_path: None,
            source_client: None,
        }
    };

    let id = svc.save(req).await?;
    println!("Saved memory {id}");
    Ok(())
}

/// Search memories and print ranked results.
pub async fn search(
    query: String,
    ty: Option<String>,
    since: Option<String>,
    semantic_ratio: Option<f32>,
    limit: usize,
) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = require_daemon(&cfg).await?;

    let parsed_type = match &ty {
        Some(s) => Some(MemoryType::parse(s).ok_or_else(|| anyhow::anyhow!("unknown type: {s}"))?),
        None => None,
    };
    let since_ts = match since {
        Some(s) => Some(parse_since(&s)?),
        None => None,
    };

    let req = GetRequest {
        query,
        limit: Some(limit),
        offset: None,
        r#type: parsed_type,
        scope: None,
        since: since_ts,
        until: None,
        semantic_ratio,
        extra_filters: Vec::new(),
    };
    // CLI shows a plain snippet; disable HTML highlight tags.
    let opts = ProjectionOptions {
        highlight: false,
        ..ProjectionOptions::search_default()
    };
    let result = svc.get(req, &opts).await?;
    if result.hits.is_empty() {
        println!("No memories found.");
        return Ok(());
    }
    let field = |row: &serde_json::Value, k: &str| {
        row.get(k)
            .and_then(|v| v.as_str())
            .unwrap_or("")
            .to_string()
    };
    for (i, m) in result.hits.iter().enumerate() {
        let title = {
            let t = field(m, "title");
            if t.is_empty() {
                "(untitled)".to_string()
            } else {
                t
            }
        };
        println!("\n{}. [{}] {}", i + 1, field(m, "type"), title);
        println!(
            "   id: {}  scope: {}  source: {}",
            field(m, "id"),
            field(m, "scope"),
            field(m, "source")
        );
        let snippet: String = field(m, "content").chars().take(200).collect();
        println!("   {}", snippet.replace('\n', " "));
    }
    if result.estimated_total as usize > result.hits.len() {
        println!("\n… {} total matches.", result.estimated_total);
    }
    Ok(())
}

/// Delete a memory by id.
pub async fn forget(id: String) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = require_daemon(&cfg).await?;
    if svc.forget(&id, Source::Cli).await? {
        println!("Forgot memory {id}");
    } else {
        println!("No memory with id {id}");
    }
    Ok(())
}

/// Show the history of memory changes (most recent first).
pub async fn history(
    action: Option<String>,
    ty: Option<String>,
    scope: Option<String>,
    since: Option<String>,
    limit: usize,
) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = require_daemon(&cfg).await?;

    let parsed_action = match &action {
        Some(s) => Some(crate::history::EventAction::parse(s).ok_or_else(|| {
            anyhow::anyhow!("unknown action: {s} (use create/update/delete/crawl)")
        })?),
        None => None,
    };
    let since_ts = match since {
        Some(s) => Some(parse_since(&s)?),
        None => None,
    };

    let query = crate::history::EventQuery {
        action: parsed_action,
        r#type: ty,
        scope,
        since: since_ts,
        limit,
    };
    let events = svc.history(&query).await?;
    if events.is_empty() {
        println!("No history yet.");
        return Ok(());
    }
    for ev in &events {
        println!("{}", crate::history::render_event(ev));
    }
    Ok(())
}

/// Run a one-off crawl.
pub async fn crawl_run(reset: bool) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = require_daemon(&cfg).await?;
    let knowledge = crate::agents::knowledge_roots();
    if reset {
        println!("Dropping every crawled document, then rebuilding…");
    }
    println!(
        "Crawling {} project root(s) + {} agent knowledge root(s)...",
        cfg.crawler.roots.len(),
        knowledge.len()
    );
    let summary = crawler::scan(&cfg, &svc, reset).await?;
    println!(
        "Done: {} scanned, {} indexed, {} skipped, {} deleted, {} errors.",
        summary.scanned, summary.indexed, summary.skipped, summary.deleted, summary.errors
    );
    Ok(())
}

/// Show the last crawl summary.
pub async fn crawl_status() -> Result<()> {
    match crawler::last_summary() {
        Some(s) => println!(
            "Last crawl: {} scanned, {} indexed, {} skipped, {} deleted, {} errors (at {}).",
            s.scanned, s.indexed, s.skipped, s.deleted, s.errors, s.finished_at
        ),
        None => println!("No crawl has run yet."),
    }
    Ok(())
}

/// Print the active crawl configuration.
pub async fn crawl_config() -> Result<()> {
    let cfg = Config::load_or_init()?;
    println!("Roots:        {:?}", cfg.crawler.roots);
    println!("Exclude dirs: {:?}", cfg.crawler.exclude_dirs);
    println!("Deny globs:   {:?}", cfg.crawler.deny_globs);
    println!("Max bytes:    {}", cfg.crawler.max_file_bytes);
    println!("Reconcile:    every {}s", cfg.crawler.reconcile_secs);
    let knowledge = crate::agents::knowledge_roots();
    println!("Agent knowledge roots (always indexed, scoped to their project):");
    if knowledge.is_empty() {
        println!("  (none found)");
    }
    for k in knowledge {
        println!("  {}", k.display());
    }
    Ok(())
}

/// Diagnose Meilisearch / model / config issues. With `fix`, attempt repairs
/// (currently: reset a database created by a different Meilisearch version).
pub async fn doctor(fix: bool) -> Result<()> {
    let cfg = Config::load_or_init()?;
    println!("memd doctor\n-----------");
    println!("Config:        {}", paths::config_file()?.display());
    println!("Data dir:      {}", paths::data_dir()?.display());
    println!(
        "Installed bin: {}",
        match paths::installed_bin() {
            Ok(p) if p.exists() => p.display().to_string(),
            _ => "not installed (run `memd setup`)".to_string(),
        }
    );

    let bin = meili::binary_path(&cfg.meilisearch.version)?;
    println!(
        "MS binary:     {} [{}]",
        bin.display(),
        if bin.exists() {
            "present"
        } else {
            "missing — will download on `memd up`"
        }
    );

    // Detect a database created by a different Meilisearch version.
    if let Some(db_ver) = meili::db_version()? {
        let pinned = cfg.meilisearch.version.trim_start_matches('v');
        if db_ver != pinned {
            println!("Database:      version {db_ver} != pinned {pinned} — MISMATCH");
            if fix {
                fix_db_mismatch().await?;
            } else {
                println!(
                    "               run `memd doctor --fix` to reset the database (backs it up first)."
                );
            }
        } else {
            println!("Database:      version {db_ver} (matches pinned)");
        }
    }

    let svc = MemoryService::from_config(&cfg);
    let ms_up = svc.client().is_healthy().await;
    println!("Meilisearch:   {}", health_label(ms_up));
    if ms_up {
        match svc.stats(&[]).await {
            Ok(_) => println!("Index:         memories index reachable"),
            Err(e) => println!("Index:         ERROR — {e} (run `memd up` to configure)"),
        }
        if fix {
            // The engine keeps a history of every task; prune anything older
            // than a week so the task queue stays small.
            let week_ago = now_secs() - 7 * 86_400;
            match svc.client().prune_tasks(week_ago).await {
                Ok(()) => println!("Tasks:         pruned finished tasks older than 7 days"),
                Err(e) => println!("Tasks:         could not prune — {e}"),
            }
        }
    }
    println!(
        "Embedder:      {} / {}",
        cfg.embedder.source, cfg.embedder.model
    );
    println!(
        "MCP endpoint:  {}",
        if daemon_healthy(&cfg).await {
            "up"
        } else {
            "down"
        }
    );
    println!(
        "Service:       {}",
        if launchd::is_installed() {
            "installed"
        } else {
            "not installed"
        }
    );
    Ok(())
}

/// One-command install. Idempotent: safe to re-run after upgrades.
pub async fn setup(no_hooks: bool, agents: Option<Vec<String>>) -> Result<()> {
    println!("memd setup\n----------");

    // 1. Relocate the binary to a stable path so the service/hooks don't point
    //    into a build tree.
    let installed = install_self()?;

    // 2. Ensure config exists (generates a local master key on first run).
    let cfg = Config::load_or_init()?;
    println!("✓ config: {}", paths::config_file()?.display());

    // 3. Start the daemon as a managed service.
    if cfg!(target_os = "macos") {
        if daemon_healthy(&cfg).await {
            // Re-point the service at the (possibly new) installed binary.
            launchd::install(&installed)?;
        } else {
            launchd::install(&installed)?;
        }
        println!("✓ launchd service installed");
    } else {
        if !daemon_healthy(&cfg).await {
            spawn_detached()?;
        }
        println!("✓ daemon started (no launchd on this platform — see README for init setup)");
    }

    // 4. Wait for health.
    print!("  waiting for daemon");
    use std::io::Write;
    for _ in 0..120 {
        if daemon_healthy(&cfg).await {
            break;
        }
        print!(".");
        let _ = std::io::stdout().flush();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    println!();
    if daemon_healthy(&cfg).await {
        println!(
            "✓ daemon healthy (MCP on http://{}:{})",
            cfg.mcp.host, cfg.mcp.port
        );
    } else {
        println!("⚠ daemon did not become healthy yet — check `memd logs`");
    }

    // 5–7. Pick agents and converge their memd wiring (MCP + directives + hooks).
    configure_agents(&installed, &cfg, no_hooks, agents)?;

    println!(
        "\nmemd is set up. Open a new agent session (or `/hooks` in Claude Code) to activate."
    );
    Ok(())
}

/// Interactive agent selection + convergence. On a TTY, show a multi-select
/// pre-checked with already-configured agents (sync semantics: unchecking a
/// configured agent removes memd from it). Off a TTY, configure every detected
/// agent and remove nothing.
fn configure_agents(
    installed: &Path,
    cfg: &Config,
    no_hooks: bool,
    explicit: Option<Vec<String>>,
) -> Result<()> {
    use std::io::IsTerminal;

    let agents = crate::agents::registry();
    let statuses: Vec<_> = agents.iter().map(|a| a.status()).collect();
    let url = crate::agents::mcp_url(cfg);
    let interactive = std::io::stdin().is_terminal() && explicit.is_none();

    // Determine the desired selection (one bool per agent).
    let desired: Vec<bool> = if let Some(ids) = explicit {
        // Scripted: connect exactly the listed agents; leave the rest alone.
        for id in &ids {
            if !agents.iter().any(|a| a.id == id) {
                bail!(
                    "unknown agent id `{id}` (expected one of: {})",
                    agents.iter().map(|a| a.id).collect::<Vec<_>>().join(", ")
                );
            }
        }
        agents
            .iter()
            .zip(&statuses)
            .map(|(a, s)| {
                ids.iter().any(|id| id == a.id)
                    || matches!(s, crate::agents::AgentStatus::Configured)
            })
            .collect()
    } else if interactive {
        let items: Vec<String> = agents
            .iter()
            .zip(&statuses)
            .map(|(a, s)| {
                let tag = match s {
                    crate::agents::AgentStatus::Configured => "configured",
                    crate::agents::AgentStatus::Detected => "detected",
                    crate::agents::AgentStatus::NotDetected => "not detected",
                };
                format!("{} ({tag})", a.name)
            })
            .collect();
        let checked: Vec<bool> = statuses
            .iter()
            .map(|s| matches!(s, crate::agents::AgentStatus::Configured))
            .collect();
        match dialoguer::MultiSelect::new()
            .with_prompt("Select agents to connect to memd (space toggles, enter confirms)")
            .items(&items)
            .defaults(&checked)
            .interact_opt()?
        {
            Some(picked) => (0..agents.len()).map(|i| picked.contains(&i)).collect(),
            None => {
                println!("• agent setup skipped");
                return Ok(());
            }
        }
    } else {
        // Non-TTY: desired = detected (or already configured).
        statuses
            .iter()
            .map(|s| !matches!(s, crate::agents::AgentStatus::NotDetected))
            .collect()
    };

    for ((agent, status), want) in agents.iter().zip(&statuses).zip(&desired) {
        match crate::agents::plan_action(status, *want, interactive) {
            crate::agents::Action::Configure => match agent.configure(installed, &url, !no_hooks) {
                Ok(_) => println!("✓ {}: configured ({})", agent.name, agent.transport()),
                Err(e) => println!("⚠ {}: {e}", agent.name),
            },
            crate::agents::Action::Remove => match agent.remove() {
                Ok(_) => println!("• {}: removed", agent.name),
                Err(e) => println!("⚠ {}: {e}", agent.name),
            },
            crate::agents::Action::Skip => {}
        }
    }
    Ok(())
}

/// Print relevant memories as markdown, for injection by a SessionStart hook.
/// Stays silent (and exits 0) if the daemon is down, so it never disrupts a
/// session start.
/// Read the hook payload from stdin when it is piped (hooks always pipe a JSON
/// object); never block on an interactive terminal.
fn read_hook_payload() -> Value {
    use std::io::{IsTerminal, Read};
    if std::io::stdin().is_terminal() {
        return Value::Null;
    }
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    serde_json::from_str(input.trim()).unwrap_or(Value::Null)
}

/// Print the memories a new session should start with. Agent-agnostic: every
/// agent's session-start hook calls this, passing its working directory via
/// `--scope` or the hook payload's `cwd`.
///
/// What gets injected, newest first, within the project's scope chain
/// (project → parents → global):
/// - everything agents and the user saved (MCP / CLI), and
/// - crawled agent memory files that live *outside* the project directory
///   (another agent's notes about this project), but never files the agent
///   already reads from the project itself (README, CLAUDE.md, …).
pub async fn context(
    scope: Option<String>,
    agent: Option<String>,
    format: &str,
    query: Option<String>,
    limit: usize,
) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = MemoryService::from_config(&cfg);
    if !svc.client().is_healthy().await {
        return Ok(());
    }
    let payload = read_hook_payload();
    let scope = scope
        .filter(|s| !s.trim().is_empty())
        .or_else(|| {
            payload
                .get("cwd")
                .and_then(|c| c.as_str())
                .map(String::from)
        })
        .or_else(|| {
            std::env::current_dir()
                .ok()
                .map(|p| p.to_string_lossy().to_string())
        });
    let scope = crate::memory::service::normalize_scope(scope.as_deref());

    let opts = ProjectionOptions {
        include_content: false,
        crop_length: Some(40),
        highlight: false,
        facets: Vec::new(),
    };

    // Crawled project files are on disk where the agent already looks; only
    // inject crawled *memory* files kept outside the project (e.g. another
    // agent's notes about it). Needs `STARTS WITH`; fall back to "no crawled
    // docs at all" if the engine rejects it.
    let rich_filter = if scope == "global" {
        "source != 'crawler'".to_string()
    } else {
        format!(
            "(source != 'crawler' OR (type IN ['fact', 'agent_instruction'] AND NOT source_path STARTS WITH '{}/'))",
            scope.replace('\'', "\\'")
        )
    };
    let simple_filter = "source != 'crawler'".to_string();

    let mut result = None;
    for extra in [rich_filter, simple_filter] {
        let req = GetRequest {
            query: query.clone().unwrap_or_default(),
            limit: Some(limit),
            scope: Some(scope.clone()),
            extra_filters: vec![extra],
            ..Default::default()
        };
        let r = if query.is_some() {
            svc.get(req, &opts).await
        } else {
            svc.list_with(&req, limit, &opts).await
        };
        match r {
            Ok(r) => {
                result = Some(r);
                break;
            }
            Err(e) if e.to_string().contains("STARTS WITH") => continue,
            Err(e) => return Err(e),
        }
    }
    let Some(result) = result else { return Ok(()) };
    if result.hits.is_empty() {
        return Ok(());
    }

    let mut out = String::new();
    out.push_str("## Memory (memd)\n");
    out.push_str(&format!(
        "Long-term memory shared across every LLM tool on this machine (scope: `{scope}`). \
         Save durable facts with `save_memory`; call `read_memory(<id>)` for the full text.\n\n"
    ));
    for row in &result.hits {
        let ty = row_str(row, "type");
        let title = {
            let t = row_str(row, "title");
            if t.is_empty() {
                "(untitled)".to_string()
            } else {
                t
            }
        };
        let snippet: String = row_str(row, "content")
            .replace('\n', " ")
            .chars()
            .take(160)
            .collect();
        let id = row_str(row, "id");
        let origin = match row_str(row, "source").as_str() {
            "crawler" => " (file)",
            _ => "",
        };
        if snippet.is_empty() {
            out.push_str(&format!("- **[{ty}]** {title}{origin} `id:{id}`\n"));
        } else {
            out.push_str(&format!(
                "- **[{ty}]** {title}{origin} — {snippet} `id:{id}`\n"
            ));
        }
    }
    let _ = agent; // reserved for per-agent phrasing

    match format {
        "json" => {
            // Gemini CLI (and any client expecting the hook JSON protocol).
            let v = serde_json::json!({
                "hookSpecificOutput": {
                    "hookEventName": "SessionStart",
                    "additionalContext": out,
                }
            });
            println!("{}", serde_json::to_string(&v)?);
        }
        _ => print!("{out}"),
    }
    Ok(())
}

/// The last exchange of a session, as an end-of-turn hook sees it.
struct Turn {
    user: String,
    assistant: Option<String>,
}

/// Pull the final user/assistant exchange out of a hook payload, whichever
/// agent produced it:
/// - Gemini CLI `AfterAgent`: `prompt` + `prompt_response` inline;
/// - Codex `Stop`: `last_assistant_message` + a JSONL transcript;
/// - Claude Code `Stop`: a JSONL transcript.
fn turn_from_payload(payload: &Value) -> Option<Turn> {
    if let Some(prompt) = payload.get("prompt").and_then(|v| v.as_str()) {
        return Some(Turn {
            user: prompt.to_string(),
            assistant: payload
                .get("prompt_response")
                .and_then(|v| v.as_str())
                .map(String::from),
        });
    }
    let transcript = payload
        .get("transcript_path")
        .and_then(|v| v.as_str())
        .and_then(|p| std::fs::read_to_string(p).ok());
    let (user, assistant) = transcript
        .map(|raw| extract_last_turn(&raw))
        .unwrap_or((None, None));
    let assistant = payload
        .get("last_assistant_message")
        .and_then(|v| v.as_str())
        .map(String::from)
        .or(assistant);
    user.map(|user| Turn { user, assistant })
}

/// Conservatively capture a finished turn. Agent-agnostic: reads whatever
/// end-of-turn payload the calling agent sends (see [`turn_from_payload`]).
/// Only a short, human-written message with an explicit durable-intent
/// phrase ("remember…", "we decided…") is captured.
pub async fn capture(agent: Option<String>) -> Result<()> {
    let payload = read_hook_payload();
    let Some(turn) = turn_from_payload(&payload) else {
        return Ok(());
    };
    if !is_capturable(&turn.user) {
        return Ok(()); // high-signal gate
    }
    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from);

    let cfg = Config::load_or_init()?;
    let svc = MemoryService::from_config(&cfg);
    if !svc.client().is_healthy().await {
        return Ok(());
    }

    let mut content = format!("User: {}", truncate(&turn.user, 600));
    if let Some(a) = &turn.assistant {
        content.push_str(&format!("\n\nOutcome: {}", truncate(a, 1200)));
    }
    let client = agent.unwrap_or_else(|| "unknown-agent".to_string());
    let req = SaveRequest {
        content,
        title: Some(truncate(&turn.user, 70)),
        r#type: None,
        tags: vec!["auto-capture".to_string(), client.clone()],
        scope: cwd,
        source: Some(Source::Cli),
        source_path: None,
        source_client: Some(client),
    };
    let id = svc.save(req).await?;
    eprintln!("memd: captured turn -> {id}");
    Ok(())
}

/// The auto-capture gate. Besides the intent phrase, the message must look
/// like something a person typed: short, and not a system/skill/agent
/// injection that merely happens to contain the word "remember".
fn is_capturable(user: &str) -> bool {
    let t = user.trim();
    if t.is_empty() || t.chars().count() > 1500 || t.starts_with('<') {
        return false;
    }
    const INJECTED: &[&str] = &[
        "<system-reminder",
        "<agent-message",
        "<command-name",
        "Base directory for this skill",
        "[Subagent hand-back]",
        "<task-notification",
    ];
    if INJECTED.iter().any(|m| t.contains(m)) {
        return false;
    }
    has_save_intent(t)
}

pub fn directives_install() -> Result<()> {
    crate::agents::directives_install_all()
}

/// Remove the managed memd directive block from agent instruction files.
pub fn directives_uninstall() -> Result<()> {
    crate::agents::directives_remove_all()
}

/// Write memd's Claude Code skills into `~/.claude/skills/`.
pub fn skills_install() -> Result<()> {
    if crate::agents::skills_install_all()? {
        println!("Installed memd skills into ~/.claude/skills/ (memd-doctor, memd-memory).");
        println!("Start a new Claude Code session to pick them up.");
    } else {
        println!("memd skills are already up to date.");
    }
    Ok(())
}

/// Remove memd's Claude Code skills from `~/.claude/skills/`.
pub fn skills_uninstall() -> Result<()> {
    if crate::agents::skills_remove_all()? {
        println!("Removed memd skills from ~/.claude/skills/.");
    } else {
        println!("No memd skills to remove.");
    }
    Ok(())
}

// --- helpers ---------------------------------------------------------------

/// Build a service, erroring with a hint if the daemon/Meilisearch is down.
async fn require_daemon(cfg: &Config) -> Result<MemoryService> {
    let svc = MemoryService::from_config(cfg);
    if !svc.client().is_healthy().await {
        bail!("Meilisearch is not running. Start the daemon with `memd up`.");
    }
    Ok(svc)
}

async fn daemon_healthy(cfg: &Config) -> bool {
    let url = format!("http://{}:{}/health", cfg.mcp.host, cfg.mcp.port);
    reqwest::Client::new()
        .get(&url)
        .timeout(Duration::from_secs(2))
        .send()
        .await
        .map(|r| r.status().is_success())
        .unwrap_or(false)
}

fn health_label(up: bool) -> &'static str {
    if up { "up" } else { "down" }
}

/// Format unix seconds as RFC3339 for human output.
fn format_ts(secs: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| secs.to_string())
}

/// Spawn `memd serve` as a detached background process (non-macOS path).
fn spawn_detached() -> Result<()> {
    let exe = std::env::current_exe()?;
    std::process::Command::new(exe)
        .arg("serve")
        .stdin(std::process::Stdio::null())
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
        .context("spawning detached `memd serve`")?;
    Ok(())
}

/// Read a string field from a JSON row (empty string when absent).
fn row_str(row: &Value, key: &str) -> String {
    row.get(key)
        .and_then(|v| v.as_str())
        .unwrap_or("")
        .to_string()
}

/// Extract the last user and last assistant text from a Claude Code transcript
/// (JSONL of `{type, message:{role, content}}` events).
fn extract_last_turn(jsonl: &str) -> (Option<String>, Option<String>) {
    let mut last_user = None;
    let mut last_assistant = None;
    for line in jsonl.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let Ok(v) = serde_json::from_str::<Value>(line) else {
            continue;
        };
        // Claude Code wraps the message in `message`; Codex rollouts in
        // `payload`; others put role/content at the top level.
        let msg = v.get("message").or_else(|| v.get("payload")).unwrap_or(&v);
        // Injected context (skill loads, hook output) and subagent threads are
        // not the human's words.
        let flagged = |k: &str| v.get(k).and_then(|b| b.as_bool()).unwrap_or(false);
        if flagged("isMeta") || flagged("isSidechain") {
            continue;
        }
        let role = msg
            .get("role")
            .and_then(|r| r.as_str())
            .or_else(|| v.get("type").and_then(|t| t.as_str()));
        let text = extract_text(msg.get("content"));
        if text.trim().is_empty() {
            continue;
        }
        match role {
            Some("user") => last_user = Some(text),
            Some("assistant") => last_assistant = Some(text),
            _ => {}
        }
    }
    (last_user, last_assistant)
}

/// Text of a message: a plain string, or the text blocks of a content array.
/// Tool results are skipped — they are the tool's output, not the user's.
fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter(|b| {
                !matches!(
                    b.get("type").and_then(|t| t.as_str()),
                    Some("tool_result") | Some("tool_use") | Some("function_call")
                )
            })
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

fn has_save_intent(s: &str) -> bool {
    let l = s.to_lowercase();
    const SIGNALS: &[&str] = &[
        "remember",
        "note that",
        "for the record",
        "we decided",
        "let's go with",
        "lets go with",
        "going with",
        "i prefer",
        "keep in mind",
        "don't forget",
        "dont forget",
        "make a note",
        "save this",
        "memorize",
        "from now on",
    ];
    SIGNALS.iter().any(|k| l.contains(k))
}

/// Truncate to at most `n` chars on a char boundary, adding an ellipsis.
fn truncate(s: &str, n: usize) -> String {
    let s = s.trim();
    if s.chars().count() <= n {
        return s.to_string();
    }
    let mut out: String = s.chars().take(n).collect();
    out.push('…');
    out
}

/// Prefer the stable installed binary; fall back to the running executable.
pub fn resolve_memd_exe() -> PathBuf {
    if let Ok(p) = paths::installed_bin()
        && p.exists()
    {
        return p;
    }
    std::env::current_exe().unwrap_or_else(|_| PathBuf::from("memd"))
}

/// Copy the running binary to the stable install path. Returns that path.
fn install_self() -> Result<PathBuf> {
    let target = paths::installed_bin()?;
    let current = std::env::current_exe().context("resolving current executable")?;
    let current = std::fs::canonicalize(&current).unwrap_or(current);
    if std::fs::canonicalize(&target).ok().as_deref() == Some(current.as_path()) {
        println!("✓ binary already at {}", target.display());
        return Ok(target);
    }
    if let Some(parent) = target.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::copy(&current, &target)
        .with_context(|| format!("copying memd to {}", target.display()))?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&target)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&target, perms)?;
    }
    println!("✓ installed binary -> {}", target.display());
    if let (Ok(path), Some(dir)) = (std::env::var("PATH"), target.parent()) {
        let dir = dir.to_string_lossy();
        if !path.split(':').any(|p| p == dir) {
            println!("  (add {dir} to your PATH to run `memd` directly)");
        }
    }
    Ok(target)
}

/// Reset a Meilisearch database created by a different engine version: stop the
/// service, back up the db directory, and restart so it recreates cleanly.
async fn fix_db_mismatch() -> Result<()> {
    println!("  fixing database version mismatch…");
    let installed = launchd::is_installed();
    if installed {
        let _ = launchd::unload();
        tokio::time::sleep(Duration::from_millis(500)).await;
    }
    let db = paths::meili_db_dir()?;
    if db.exists() {
        // Distinct prefix: `meili-data.bak.*` is reserved for migration backups,
        // which the updater auto-restores when the db dir is missing — this one
        // must stay stranded (it is version-incompatible by definition).
        let backup = db.with_file_name(format!("meili-data.mismatch.{}", now_secs()));
        std::fs::rename(&db, &backup).with_context(|| format!("backing up {}", db.display()))?;
        println!("  backed up old database -> {}", backup.display());
    }
    if installed {
        let _ = launchd::load();
        println!("  restarted the service; it will recreate the database.");
    } else {
        println!("  done — run `memd up` to recreate the database.");
    }
    Ok(())
}

/// Parse a `--since` value: unix seconds, or a relative span like `30d`, `12h`,
/// `45m`, `90s`.
fn parse_since(s: &str) -> Result<i64> {
    let s = s.trim();
    if let Ok(secs) = s.parse::<i64>() {
        return Ok(secs);
    }
    let (num, unit) = s.split_at(s.len().saturating_sub(1));
    let n: i64 = num
        .parse()
        .with_context(|| format!("invalid --since: {s}"))?;
    let mult = match unit {
        "s" => 1,
        "m" => 60,
        "h" => 3600,
        "d" => 86400,
        "w" => 604800,
        _ => bail!("invalid --since unit in '{s}' (use s/m/h/d/w or unix seconds)"),
    };
    Ok(now_secs() - n * mult)
}

#[cfg(unix)]
unsafe fn libc_kill(pid: i32, sig: i32) {
    unsafe extern "C" {
        fn kill(pid: i32, sig: i32) -> i32;
    }
    unsafe {
        kill(pid, sig);
    }
}

/// Check for updates; apply them unless `check_only`. Manual counterpart of
/// the daemon's daily check — takes the same lock so the two never race.
pub async fn update(check_only: bool) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let state_path = paths::update_state_file()?;
    let mut state = crate::update::UpdateState::load_from(&state_path);

    println!(
        "memd {} (engine pinned {}) — checking GitHub…",
        env!("CARGO_PKG_VERSION"),
        cfg.meilisearch.version
    );
    let memd_res = crate::update::check::latest_release(crate::update::check::MEMD_REPO).await;
    let engine_res = crate::update::check::latest_release(crate::update::check::ENGINE_REPO).await;
    if let Err(e) = &memd_res {
        eprintln!("warning: could not check memd releases: {e:#}");
    }
    if let Err(e) = &engine_res {
        eprintln!("warning: could not check Meilisearch releases: {e:#}");
    }
    if memd_res.is_err() && engine_res.is_err() {
        bail!("GitHub is unreachable — try again later");
    }
    let memd_rel = memd_res.ok();
    let engine_rel = engine_res.ok();
    let asset = crate::update::check::memd_asset_name()?;
    let plan = crate::update::check::decide(
        memd_rel.as_ref(),
        engine_rel.as_ref(),
        env!("CARGO_PKG_VERSION"),
        &cfg.meilisearch.version,
        &state,
        asset,
    );

    match plan {
        crate::update::check::Plan::None => {
            println!("Everything is up to date.");
            return Ok(());
        }
        crate::update::check::Plan::Memd { version, asset_url } => {
            if check_only {
                println!("memd {version} is available (run `memd update` to apply).");
                return Ok(());
            }
            let _lock = crate::update::UpdateLock::acquire()?;
            let installed = paths::installed_bin()?;
            crate::update::self_update::apply(&asset_url, &version, &installed).await?;
            println!("memd updated to {version} at {}", installed.display());
            state.last_result = format!("memd updated to {version}");
            state.last_check_secs = now_secs();
            state.save_to(&state_path)?;
            restart_service(&cfg).await?;
            println!("Run `memd update` again to check for engine updates.");
        }
        crate::update::check::Plan::Engine { version } => {
            if check_only {
                println!(
                    "Meilisearch engine {version} is available (pinned {}).",
                    cfg.meilisearch.version
                );
                return Ok(());
            }
            let svc = MemoryService::from_config(&cfg);
            if !svc.client().is_healthy().await {
                bail!("the Meilisearch engine must be running to migrate — run `memd up` first");
            }
            let _lock = crate::update::UpdateLock::acquire()?;
            crate::update::engine::prepare(&cfg, svc.client(), &version).await?;
            state.last_result = format!("engine migration to {version} prepared");
            state.last_check_secs = now_secs();
            state.save_to(&state_path)?;
            println!("Engine migration to {version} prepared; restarting the daemon to apply…");
            restart_service(&cfg).await?;
            println!("Check `memd status` — the import may take a moment on large stores.");
        }
    }
    Ok(())
}

/// Restart the daemon so a swapped binary / prepared migration takes effect,
/// then poll until it reports healthy again (engine imports can take longer —
/// we report, not fail, on timeout).
async fn restart_service(cfg: &Config) -> Result<()> {
    if launchd::is_installed() {
        launchd::unload()?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        launchd::load()?;
        for _ in 0..40 {
            if daemon_healthy(cfg).await {
                println!("Service restarted.");
                return Ok(());
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        println!(
            "Service reloaded but not healthy yet (an engine import can take a while) — check `memd status` / `memd logs`."
        );
    } else {
        println!(
            "No launchd service installed — restart the daemon manually (`memd down && memd up`). A prepared engine migration expires after 1 hour."
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capture_gate_rejects_injected_context() {
        assert!(is_capturable("remember that we deploy on Fridays"));
        assert!(is_capturable("We decided to use Postgres for billing."));
        assert!(!is_capturable("what time is it"));
        assert!(!is_capturable(
            "Base directory for this skill: /x/skills/tdd\n# TDD\nRemember to write tests first"
        ));
        assert!(!is_capturable(
            "<agent-message from=\"abc\">[Subagent hand-back] remember this</agent-message>"
        ));
        let long = format!("remember {}", "x".repeat(2000));
        assert!(!is_capturable(&long));
    }

    #[test]
    fn turn_from_gemini_and_codex_payloads() {
        let gemini = serde_json::json!({ "prompt": "remember X", "prompt_response": "ok" });
        let t = turn_from_payload(&gemini).unwrap();
        assert_eq!(t.user, "remember X");
        assert_eq!(t.assistant.as_deref(), Some("ok"));

        let codex = serde_json::json!({ "last_assistant_message": "done", "transcript_path": "/nonexistent" });
        assert!(
            turn_from_payload(&codex).is_none(),
            "no user turn without a transcript"
        );
    }

    #[test]
    fn extract_last_turn_handles_claude_and_codex_formats() {
        let claude = r#"{"type":"user","message":{"role":"user","content":[{"type":"text","text":"remember A"}]}}
{"type":"user","isMeta":true,"message":{"role":"user","content":"Base directory for this skill"}}
{"type":"assistant","message":{"role":"assistant","content":[{"type":"text","text":"sure"}]}}
{"type":"user","message":{"role":"user","content":[{"type":"tool_result","content":"remember B"}]}}"#;
        let (u, a) = extract_last_turn(claude);
        assert_eq!(u.as_deref(), Some("remember A"));
        assert_eq!(a.as_deref(), Some("sure"));

        let codex = r#"{"type":"response_item","payload":{"type":"message","role":"user","content":[{"type":"input_text","text":"we decided C"}]}}
{"type":"response_item","payload":{"type":"message","role":"assistant","content":[{"type":"output_text","text":"noted"}]}}"#;
        let (u, a) = extract_last_turn(codex);
        assert_eq!(u.as_deref(), Some("we decided C"));
        assert_eq!(a.as_deref(), Some("noted"));
    }

    #[test]
    fn up_reports_already_running_only_when_fully_healthy() {
        assert_eq!(up_action(true, true), UpAction::AlreadyRunning);
    }

    #[test]
    fn up_restarts_when_mcp_is_up_but_meilisearch_is_down() {
        // The wedged state: daemon process alive, engine dead. "already
        // running" here would leave memory broken.
        assert_eq!(up_action(true, false), UpAction::RestartWedged);
    }

    #[test]
    fn up_starts_when_daemon_is_down() {
        assert_eq!(up_action(false, false), UpAction::Start);
        assert_eq!(up_action(false, true), UpAction::Start);
    }
}
