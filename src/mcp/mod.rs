//! MCP server transports: an always-on HTTP endpoint (mounted by the daemon)
//! and a stdio bridge for stdio-only clients. Both share [`protocol`].

mod protocol;

use crate::config::Config;
use crate::memory::MemoryService;
use anyhow::Result;
use axum::{Json, Router, extract::State, http::StatusCode, routing::post};
use serde_json::{Value, json};
use std::sync::Arc;
use tokio::io::{AsyncBufReadExt, AsyncWriteExt, BufReader};

/// Build the axum router exposing the HTTP MCP endpoint at `POST /mcp`.
///
/// Uses the streamable-HTTP transport's JSON response mode: each POST carries
/// one JSON-RPC message and gets one JSON-RPC reply (or 202 for notifications).
pub fn router(svc: MemoryService) -> Router {
    Router::new()
        .route("/mcp", post(mcp_handler))
        .route("/health", axum::routing::get(|| async { "ok" }))
        .with_state(Arc::new(svc))
}

async fn mcp_handler(
    State(svc): State<Arc<MemoryService>>,
    Json(msg): Json<Value>,
) -> (StatusCode, Json<Value>) {
    match protocol::handle_message(&svc, msg).await {
        Some(response) => (StatusCode::OK, Json(response)),
        // Notifications get an accepted-with-no-body acknowledgement.
        None => (StatusCode::OK, Json(json!({}))),
    }
}

/// Run the stdio MCP bridge: read newline-delimited JSON-RPC from stdin, write
/// replies to stdout. A thin bridge straight to the local Meilisearch.
pub async fn run_stdio() -> Result<()> {
    let cfg = Config::load_or_init()?;
    let svc = MemoryService::from_config(&cfg);

    let stdin = tokio::io::stdin();
    let mut reader = BufReader::new(stdin).lines();
    let mut stdout = tokio::io::stdout();

    while let Some(line) = reader.next_line().await? {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        let msg: Value = match serde_json::from_str(line) {
            Ok(v) => v,
            Err(_) => continue,
        };
        if let Some(response) = protocol::handle_message(&svc, msg).await {
            let mut out = serde_json::to_string(&response)?;
            out.push('\n');
            stdout.write_all(out.as_bytes()).await?;
            stdout.flush().await?;
        }
    }
    Ok(())
}
