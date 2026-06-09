//! The always-on daemon (`memd serve`): owns Meilisearch, the crawler/watcher,
//! and the HTTP MCP endpoint. Runs in the foreground; under launchd this is the
//! managed process (PRD §6.2).

use crate::config::Config;
use crate::memory::MemoryService;
use crate::{crawler, mcp, meili, paths};
use anyhow::{Context, Result};
use std::time::Duration;

/// Run the daemon until terminated. Blocks for the lifetime of the service.
pub async fn serve() -> Result<()> {
    let _guard = init_logging()?;
    let cfg = Config::load_or_init()?;

    write_pid()?;
    tracing::info!("memd daemon starting (pid {})", std::process::id());

    // 1. Start the managed Meilisearch child process.
    let mut child = meili::spawn(&cfg).await?;
    let svc = MemoryService::from_config(&cfg);

    // 2. Wait for health, then ensure the index + local embedder.
    meili::wait_healthy(svc.client(), Duration::from_secs(60)).await?;
    svc.client()
        .ensure_index(&cfg.embedder.source, &cfg.embedder.model)
        .await
        .context("ensuring memories index")?;
    tracing::info!("Meilisearch ready; index configured");

    // 3. Crawler + watcher in the background.
    let crawl_cfg = cfg.clone();
    let crawl_svc = svc.clone();
    let crawler_task = tokio::spawn(async move {
        if let Err(e) = crawler::watch(crawl_cfg, crawl_svc).await {
            tracing::error!("crawler stopped: {e}");
        }
    });

    // 4. HTTP MCP endpoint.
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

    // 5. Wait for shutdown signal or child exit.
    shutdown_signal(&mut child).await;

    tracing::info!("shutting down");
    crawler_task.abort();
    server.abort();
    let _ = child.kill().await;
    let _ = std::fs::remove_file(paths::pid_file()?);
    Ok(())
}

/// Block until SIGINT/SIGTERM, or until the Meilisearch child exits.
async fn shutdown_signal(child: &mut tokio::process::Child) {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => tracing::info!("received SIGTERM"),
            _ = int.recv() => tracing::info!("received SIGINT"),
            status = child.wait() => tracing::error!("Meilisearch exited: {status:?}"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => tracing::info!("received ctrl-c"),
            status = child.wait() => tracing::error!("Meilisearch exited: {status:?}"),
        }
    }
}

/// Configure tracing to append to the daemon log file. The returned guard must
/// be kept alive for logs to flush.
fn init_logging() -> Result<tracing_appender::non_blocking::WorkerGuard> {
    let log_path = paths::log_file()?;
    let dir = log_path.parent().unwrap().to_path_buf();
    let file_name = log_path.file_name().unwrap().to_string_lossy().to_string();
    let appender = tracing_appender::rolling::never(dir, file_name);
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
