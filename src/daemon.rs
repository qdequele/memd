//! The always-on daemon (`memd serve`): owns Meilisearch, the crawler/watcher,
//! and the HTTP MCP endpoint. Runs in the foreground; under launchd this is the
//! managed process (PRD §6.2).

use crate::config::Config;
use crate::memory::MemoryService;
use crate::{crawler, logging, mcp, meili, paths, update};
use anyhow::{Context, Result};
use std::time::Duration;

/// Run the daemon until terminated. Blocks for the lifetime of the service.
pub async fn serve() -> Result<()> {
    let _guard = init_logging()?;
    let mut cfg = Config::load_or_init()?;

    write_pid()?;
    tracing::info!("memd daemon starting (pid {})", std::process::id());

    // 0. Apply a prepared engine migration before the engine starts. Failure
    //    rolls back and the daemon continues on the previous version.
    match update::engine::apply_pending(&mut cfg).await? {
        update::engine::Applied::Migrated { to } => {
            tracing::info!("engine updated to {to}");
        }
        update::engine::Applied::RolledBack { to, error } => {
            tracing::warn!("engine update to {to} rolled back: {error}");
        }
        update::engine::Applied::Expired { to } => {
            tracing::info!("stale engine migration to {to} discarded; will re-prepare");
        }
        update::engine::Applied::None => {}
    }

    // 1. Start the managed Meilisearch child process. Its output flows through
    //    our size-capped log writer, not an unbounded supervisor redirect.
    let mut child = meili::spawn(&cfg).await?;
    meili::forward_output(&mut child);
    let svc = MemoryService::from_config(&cfg);

    // 2. Wait for health, then ensure the index + local embedder.
    meili::wait_healthy(svc.client(), Duration::from_secs(60)).await?;
    svc.client()
        .ensure_index(&cfg.embedder.source, &cfg.embedder.model)
        .await
        .context("ensuring memories index")?;
    svc.events()
        .ensure()
        .await
        .context("ensuring memory_events index")?;
    tracing::info!("Meilisearch ready; indexes configured");

    // Restart channel: the updater and the health watchdog both use it.
    let (restart_tx, mut restart_rx) = tokio::sync::mpsc::channel::<&'static str>(1);

    // 3. Watchdog: the child staying alive is not enough — Meilisearch can
    //    wedge (process up, every request failing, e.g. after disk pressure).
    //    Restart the daemon when health checks fail long enough.
    let watchdog_client = svc.client().clone();
    let watchdog_tx = restart_tx.clone();
    let watchdog = tokio::spawn(async move {
        let mut detector = meili::WedgeDetector::new(4);
        let mut ticks = tokio::time::interval(Duration::from_secs(30));
        ticks.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
        ticks.tick().await; // skip the immediate first tick
        loop {
            ticks.tick().await;
            if detector.observe(watchdog_client.is_healthy().await) {
                tracing::error!("Meilisearch is wedged (alive but unhealthy) — restarting");
                let _ = watchdog_tx.send("meilisearch wedged").await;
                return;
            }
        }
    });

    // 4. Crawler + watcher in the background.
    let crawl_cfg = cfg.clone();
    let crawl_svc = svc.clone();
    let crawler_task = tokio::spawn(async move {
        if let Err(e) = crawler::watch(crawl_cfg, crawl_svc).await {
            tracing::error!("crawler stopped: {e}");
        }
    });

    // 5. HTTP MCP endpoint.
    let addr = format!("{}:{}", cfg.mcp.host, cfg.mcp.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding MCP endpoint on {addr}"))?;
    tracing::info!("MCP HTTP endpoint listening on http://{addr}/mcp");
    let app = mcp::router(svc.clone());

    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("MCP server error: {e}");
        }
    });

    // 6. Daily auto-update check; a prepared update requests a restart.
    let upd_cfg = cfg.clone();
    let updater = tokio::spawn(async move {
        update::run_loop(upd_cfg, restart_tx).await;
    });

    // 7. Wait for a shutdown signal, child exit, or a restart request.
    let restart = shutdown_signal(&mut child, &mut restart_rx).await;

    tracing::info!("shutting down");
    crawler_task.abort();
    server.abort();
    updater.abort();
    watchdog.abort();
    let _ = child.kill().await;
    let _ = std::fs::remove_file(paths::pid_file()?);

    if restart {
        // Flush buffered log lines before exec replaces the process image —
        // the appender guard's Drop never runs past this point.
        drop(_guard);
        restart_daemon();
    }
    Ok(())
}

/// Exit so the supervisor relaunches us (launchd `KeepAlive`), or re-exec on
/// platforms without one. Never returns.
fn restart_daemon() -> ! {
    if crate::launchd::is_installed() {
        eprintln!("memd: exiting for restart; launchd will relaunch");
        std::process::exit(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let exe = crate::cli::resolve_memd_exe();
        eprintln!("memd: re-exec {} serve", exe.display());
        let err = std::process::Command::new(exe).arg("serve").exec();
        eprintln!("memd: re-exec failed: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    std::process::exit(0);
}

/// Block until SIGINT/SIGTERM, the Meilisearch child exiting, or a restart
/// request from the updater. Returns true when the daemon should restart
/// itself after cleanup.
async fn shutdown_signal(
    child: &mut tokio::process::Child,
    restart_rx: &mut tokio::sync::mpsc::Receiver<&'static str>,
) -> bool {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => { tracing::info!("received SIGTERM"); false }
            _ = int.recv() => { tracing::info!("received SIGINT"); false }
            status = child.wait() => { tracing::error!("Meilisearch exited: {status:?}"); false }
            reason = restart_rx.recv() => {
                tracing::info!("restart requested: {}", reason.unwrap_or("unknown"));
                true
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { tracing::info!("received ctrl-c"); false }
            status = child.wait() => { tracing::error!("Meilisearch exited: {status:?}"); false }
            reason = restart_rx.recv() => {
                tracing::info!("restart requested: {}", reason.unwrap_or("unknown"));
                true
            }
        }
    }
}

/// Configure tracing to append to the daemon log file, size-capped: the file
/// is rotated to `memd.log.1` when it outgrows [`logging::MAX_LOG_BYTES`], so
/// the log can never fill the disk again. The returned guard must be kept
/// alive for logs to flush.
fn init_logging() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let log_path = paths::log_file()?;
    // Bound a log left oversized by a previous run before opening it.
    logging::rotate_if_oversized(&log_path, logging::MAX_LOG_BYTES)?;
    let appender = logging::SizeRotatingWriter::new(log_path, logging::MAX_LOG_BYTES)?;
    let (writer, guard) = tracing_appender::non_blocking(appender);

    use tracing_subscriber::EnvFilter;
    let filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));
    tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_writer(writer)
        .with_ansi(false)
        .init();
    Ok(guard)
}

fn write_pid() -> Result<()> {
    std::fs::write(paths::pid_file()?, std::process::id().to_string())?;
    Ok(())
}
