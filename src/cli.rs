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
use serde_json::{Value, json};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// Start the daemon. In foreground mode this *is* the daemon; otherwise it
/// installs the launchd service (macOS) or spawns a detached process.
pub async fn up(foreground: bool) -> Result<()> {
    if foreground {
        return crate::daemon::serve().await;
    }

    let cfg = Config::load_or_init()?;
    if daemon_healthy(&cfg).await {
        println!(
            "memd is already running (MCP on http://{}:{}).",
            cfg.mcp.host, cfg.mcp.port
        );
        return Ok(());
    }

    if cfg!(target_os = "macos") {
        launchd::install(&resolve_memd_exe())?;
    } else {
        spawn_detached()?;
        println!("Started memd daemon in the background.");
    }

    // Wait for the MCP endpoint to come up.
    print!("Waiting for daemon to become healthy");
    for _ in 0..120 {
        if daemon_healthy(&cfg).await {
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
pub async fn crawl_run() -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = require_daemon(&cfg).await?;
    println!("Crawling {} root(s)...", cfg.crawler.roots.len());
    let summary = crawler::scan(&cfg, &svc).await?;
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
pub async fn setup(no_hooks: bool) -> Result<()> {
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

    // 5. Register with detected MCP clients.
    register_claude_code(&installed);

    // 6. Cross-tool instruction directives.
    directives_install()?;

    // 7. Claude Code lifecycle hooks (deterministic recall + capture).
    if no_hooks {
        println!("• skipped hooks (--no-hooks)");
    } else {
        match install_claude_hooks(&installed) {
            Ok(true) => println!("✓ Claude Code hooks added (SessionStart + Stop)"),
            Ok(false) => println!("✓ Claude Code hooks already present"),
            Err(e) => println!("⚠ could not write hooks: {e}"),
        }
    }

    println!(
        "\nmemd is set up. Open a new agent session (or `/hooks` in Claude Code) to activate."
    );
    Ok(())
}

/// Print relevant memories as markdown, for injection by a SessionStart hook.
/// Stays silent (and exits 0) if the daemon is down, so it never disrupts a
/// session start.
pub async fn context(scope: Option<String>, query: Option<String>, limit: usize) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = MemoryService::from_config(&cfg);
    if !svc.client().is_healthy().await {
        return Ok(());
    }
    let opts = ProjectionOptions {
        include_content: false,
        crop_length: Some(40),
        highlight: false,
        facets: Vec::new(),
    };

    let result = if let Some(q) = query {
        let req = GetRequest {
            query: q,
            limit: Some(limit),
            scope: scope.clone(),
            ..Default::default()
        };
        svc.get(req, &opts).await?
    } else {
        let mut r = svc.list(None, scope.clone(), limit, 0, &opts).await?;
        // If nothing is scoped to this project, fall back to recent globals.
        if r.hits.is_empty() && scope.is_some() {
            r = svc.list(None, None, limit, 0, &opts).await?;
        }
        r
    };
    if result.hits.is_empty() {
        return Ok(());
    }

    println!("## Memory (memd)");
    println!(
        "Relevant long-term memory shared across your LLM tools. Save durable \
         facts with `save_memory`; call `read_memory(<id>)` for the full text.\n"
    );
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
        if snippet.is_empty() {
            println!("- **[{ty}]** {title} `id:{id}`");
        } else {
            println!("- **[{ty}]** {title} — {snippet} `id:{id}`");
        }
    }
    Ok(())
}

/// Conservatively capture a finished turn from a Claude Code Stop hook. Reads
/// the hook JSON payload on stdin and only saves when the user's last message
/// signals durable intent — keeping the store high-signal. Never writes to
/// stdout (so it adds nothing to the model's context).
pub async fn capture() -> Result<()> {
    use std::io::Read;
    let mut input = String::new();
    let _ = std::io::stdin().read_to_string(&mut input);
    let payload: Value = serde_json::from_str(input.trim()).unwrap_or(Value::Null);

    let Some(transcript_path) = payload.get("transcript_path").and_then(|v| v.as_str()) else {
        return Ok(());
    };
    let cwd = payload
        .get("cwd")
        .and_then(|v| v.as_str())
        .map(String::from);
    let Ok(raw) = std::fs::read_to_string(transcript_path) else {
        return Ok(());
    };

    let (last_user, last_assistant) = extract_last_turn(&raw);
    let Some(user) = last_user else { return Ok(()) };
    if !has_save_intent(&user) {
        return Ok(()); // high-signal gate
    }

    let cfg = Config::load_or_init()?;
    let svc = MemoryService::from_config(&cfg);
    if !svc.client().is_healthy().await {
        return Ok(());
    }

    let mut content = format!("User: {}", truncate(&user, 600));
    if let Some(a) = last_assistant {
        content.push_str(&format!("\n\nOutcome: {}", truncate(&a, 1200)));
    }
    let req = SaveRequest {
        content,
        title: Some(truncate(&user, 70)),
        r#type: None,
        tags: vec!["auto-capture".to_string(), "claude-code".to_string()],
        scope: cwd,
        source: Some(Source::Cli),
        source_path: None,
        source_client: Some("claude-code".to_string()),
    };
    let id = svc.save(req).await?;
    eprintln!("memd: captured turn -> {id}");
    Ok(())
}

/// Write the managed memd directive block into known agent instruction files.
pub fn directives_install() -> Result<()> {
    let block = directive_block();
    for path in target_instruction_files() {
        upsert_directive(&path, &block)?;
        println!("Wrote memd directives to {}", path.display());
    }
    println!("\nReload your tools (or start a new session) for the directives to take effect.");
    Ok(())
}

/// Remove the managed memd directive block from agent instruction files.
pub fn directives_uninstall() -> Result<()> {
    for path in target_instruction_files() {
        if remove_directive(&path)? {
            println!("Removed memd directives from {}", path.display());
        }
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
        let msg = v.get("message").unwrap_or(&v);
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

/// Flatten MCP/Anthropic message content (string or array of text blocks).
fn extract_text(content: Option<&Value>) -> String {
    match content {
        Some(Value::String(s)) => s.clone(),
        Some(Value::Array(arr)) => arr
            .iter()
            .filter_map(|b| b.get("text").and_then(|t| t.as_str()))
            .collect::<Vec<_>>()
            .join(" "),
        _ => String::new(),
    }
}

/// True if a message signals durable memory intent (the auto-capture gate).
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

const DIRECTIVE_START: &str = "<!-- memd:start (managed by `memd directives`) -->";
const DIRECTIVE_END: &str = "<!-- memd:end -->";

/// The managed directive block injected into agent instruction files.
fn directive_block() -> String {
    format!(
        "{DIRECTIVE_START}\n\
## Memory (memd)\n\
`memd` is your persistent, cross-tool long-term memory — an always-on local MCP \
server shared with all of your LLM tools.\n\
- At the start of a task, call `get_memory` (set `scope` to the project path) to \
load prior decisions, preferences, and facts before acting — don't ask the user \
to repeat context you can recall.\n\
- When you learn something durable (a decision, preference, fact, or reusable \
solution), call `save_memory`.\n\
- Use `read_memory(id)` for the full text and `list_memories` to browse.\n\
{DIRECTIVE_END}"
    )
}

/// Known global agent instruction files to inject directives into.
fn target_instruction_files() -> Vec<PathBuf> {
    let mut files = Vec::new();
    if let Some(base) = directories::BaseDirs::new() {
        let home = base.home_dir();
        files.push(home.join(".claude").join("CLAUDE.md")); // Claude Code (global)
        files.push(home.join(".codex").join("AGENTS.md")); // Codex (global)
    }
    files
}

/// Insert or replace the managed directive block in `path` (creating the file).
fn upsert_directive(path: &PathBuf, block: &str) -> Result<()> {
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let next = if let Some(region) = managed_region(&existing) {
        existing.replacen(&region, block, 1)
    } else if existing.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{block}\n", existing.trim_end())
    };
    std::fs::write(path, next).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Remove the managed directive block from `path`. Returns whether it changed.
fn remove_directive(path: &PathBuf) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let Some(region) = managed_region(&existing) else {
        return Ok(false);
    };
    let cleaned = existing.replacen(&region, "", 1);
    let cleaned = cleaned.replace("\n\n\n", "\n\n");
    std::fs::write(path, cleaned.trim_start())?;
    Ok(true)
}

/// Extract the current `<!-- memd:start --> … <!-- memd:end -->` region, if any.
fn managed_region(text: &str) -> Option<String> {
    let start = text.find(DIRECTIVE_START)?;
    let end = text.find(DIRECTIVE_END)? + DIRECTIVE_END.len();
    if end > start {
        Some(text[start..end].to_string())
    } else {
        None
    }
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

/// Look up a command on PATH.
fn which(cmd: &str) -> Option<PathBuf> {
    let path = std::env::var_os("PATH")?;
    std::env::split_paths(&path)
        .map(|d| d.join(cmd))
        .find(|c| c.is_file())
}

/// Register memd as a user-scoped stdio MCP server in Claude Code, if the
/// `claude` CLI is present. Converges on the stable binary path by re-adding.
fn register_claude_code(installed: &Path) {
    if which("claude").is_none() {
        println!("• Claude Code CLI not found — skipping MCP registration.");
        return;
    }
    let exe = installed.to_string_lossy().to_string();
    // Drop any prior registration so it can't keep pointing at an old path.
    let _ = std::process::Command::new("claude")
        .args(["mcp", "remove", "memd", "-s", "user"])
        .output();
    let status = std::process::Command::new("claude")
        .args([
            "mcp", "add", "memd", "--scope", "user", "--", &exe, "mcp", "--stdio",
        ])
        .status();
    match status {
        Ok(s) if s.success() => println!("✓ Claude Code: registered memd (user scope, stdio)"),
        _ => println!(
            "⚠ Claude Code: registration failed — run `claude mcp add memd -- {exe} mcp --stdio`"
        ),
    }
}

/// Merge memd's SessionStart/Stop hooks into `~/.claude/settings.json`.
/// Idempotent; returns whether the file changed.
fn install_claude_hooks(installed: &Path) -> Result<bool> {
    let path = directories::BaseDirs::new()
        .context("home directory")?
        .home_dir()
        .join(".claude")
        .join("settings.json");
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        root = json!({});
    }

    let exe = installed.to_string_lossy();
    let session_cmd = format!("{exe} context --scope \"$CLAUDE_PROJECT_DIR\"");
    let stop_cmd = format!("{exe} capture");

    let mut changed = false;
    changed |= ensure_hook(&mut root, "SessionStart", "context", &session_cmd, 10);
    changed |= ensure_hook(&mut root, "Stop", "capture", &stop_cmd, 15);

    if changed {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&root)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(changed)
}

/// Ensure a `memd <marker>` command hook exists under `hooks.<event>`.
fn ensure_hook(root: &mut Value, event: &str, marker: &str, command: &str, timeout: u64) -> bool {
    let needle = format!("memd {marker}");
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let arr = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| json!([]));
    let Some(arr) = arr.as_array_mut() else {
        return false;
    };
    // If a memd hook for this event already exists, leave it alone when its
    // command is identical, otherwise replace it (converges on the new path).
    if let Some(group) = arr.iter_mut().find(|group| {
        group
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains(&needle))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }) {
        let current = group
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|hs| hs.first())
            .and_then(|h| h.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if current == command {
            return false;
        }
        *group = json!({
            "hooks": [ { "type": "command", "command": command, "timeout": timeout } ]
        });
        return true;
    }
    arr.push(json!({
        "hooks": [ { "type": "command", "command": command, "timeout": timeout } ]
    }));
    true
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
