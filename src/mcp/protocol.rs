//! MCP over JSON-RPC 2.0 — protocol handling, transport-agnostic.
//!
//! Hand-rolled rather than depending on a fast-moving SDK. Implements the
//! subset every MCP client needs: `initialize`, `tools/list`, `tools/call`,
//! and the `notifications/initialized` no-op. Tool calls are translated into
//! [`MemoryService`] operations.

use crate::memory::{
    GetRequest, MemoryService, MemoryType, ProjectionOptions, QueryResult, SaveRequest, Source,
};
use serde_json::{Value, json};

const PROTOCOL_VERSION: &str = "2024-11-05";
const SERVER_NAME: &str = "memd";

/// Handle one JSON-RPC message. Returns `Some(response)` for requests and
/// `None` for notifications (which get no reply).
pub async fn handle_message(svc: &MemoryService, msg: Value) -> Option<Value> {
    let method = msg.get("method").and_then(|m| m.as_str()).unwrap_or("");
    let id = msg.get("id").cloned();
    let params = msg.get("params").cloned().unwrap_or(Value::Null);

    // Notifications carry no id and expect no response.
    id.as_ref()?;
    let id = id.unwrap();

    match method {
        "initialize" => Some(ok(id, initialize_result())),
        "ping" => Some(ok(id, json!({}))),
        "tools/list" => Some(ok(id, json!({ "tools": tool_defs() }))),
        "tools/call" => Some(handle_tool_call(svc, id, params).await),
        other => Some(err(id, -32601, &format!("method not found: {other}"))),
    }
}

/// System-context instructions surfaced to the model by MCP-compliant clients
/// (Claude Code, etc.) via the `initialize` response. This is memd's strongest
/// in-protocol lever for getting agents to treat it as primary memory.
const SERVER_INSTRUCTIONS: &str = "\
memd is your persistent, cross-tool long-term memory — a single local store \
shared with every other LLM tool on this machine. Treat it as the source of \
truth for what you remember across sessions, and use it proactively:

- BEFORE starting a task or answering from assumed context, call `get_memory` \
with the user's goal (set `scope` to the current project path when relevant) \
to load prior decisions, preferences, and facts. Prefer recalling from memd \
over asking the user to repeat themselves.
- WHEN you learn something durable — a decision, a preference, a fact about the \
user or project, or a reusable solution — call `save_memory` right away so \
other tools and future sessions inherit it.
- Results are lightweight rows with a snippet; call `read_memory(id)` for the \
full text, `list_memories` to browse, and `stats` for an overview.

Do not treat memd as optional scratch space: it is the shared memory layer.";

fn initialize_result() -> Value {
    json!({
        "protocolVersion": PROTOCOL_VERSION,
        "capabilities": { "tools": { "listChanged": false } },
        "serverInfo": { "name": SERVER_NAME, "version": env!("CARGO_PKG_VERSION") },
        "instructions": SERVER_INSTRUCTIONS
    })
}

/// MCP tool definitions (the stable contract, additive-only).
fn tool_defs() -> Value {
    json!([
        {
            "name": "save_memory",
            "description": "Persist a memory so any LLM tool can recall it later. Stamps timestamps, classifies the type if omitted, dedups identical content, and embeds it locally.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "content": { "type": "string", "description": "The memory text." },
                    "type": { "type": "string", "description": "fact, preference, decision, task, project_overview, agent_instruction, file_annotation, code_note, reference." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "scope": { "type": "string", "description": "'global' or a path prefix (e.g. ~/Projects/Foo)." },
                    "title": { "type": "string" }
                },
                "required": ["content"]
            }
        },
        {
            "name": "get_memory",
            "description": "Recall memories by hybrid (keyword + semantic) search. Returns lightweight rows: metadata plus a query-aware ~50-word highlighted snippet of `content` (NOT the full blob). Call read_memory(id) for the full text. Supports type/scope/time filters, paging, and facet counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "description": "Max rows (default 10)." },
                    "offset": { "type": "integer", "description": "Paging offset." },
                    "type": { "type": "string" },
                    "scope": { "type": "string" },
                    "since": { "type": "integer", "description": "Unix seconds lower bound on created_at." },
                    "until": { "type": "integer", "description": "Unix seconds upper bound on created_at." },
                    "semantic_ratio": { "type": "number", "description": "0.0 keyword .. 1.0 vector." },
                    "crop_length": { "type": "integer", "description": "Words to crop the content snippet to (default 50)." },
                    "include_content": { "type": "boolean", "description": "Return full content instead of a snippet (default false). Heavy — prefer read_memory(id)." },
                    "highlight": { "type": "boolean", "description": "Wrap matched terms in markdown **bold** (default true)." },
                    "facets": { "type": "array", "items": { "type": "string" }, "description": "Fields to return server-side counts for (e.g. type, scope, source)." }
                },
                "required": ["query"]
            }
        },
        {
            "name": "read_memory",
            "description": "Fetch the full document (including complete content) for a single memory by id. Use after get_memory/list_memories surface a relevant row.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        },
        {
            "name": "update_memory",
            "description": "Update fields of an existing memory by id.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "id": { "type": "string" },
                    "content": { "type": "string" },
                    "title": { "type": "string" },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "scope": { "type": "string" },
                    "type": { "type": "string" }
                },
                "required": ["id"]
            }
        },
        {
            "name": "forget_memory",
            "description": "Delete a memory by id.",
            "inputSchema": {
                "type": "object",
                "properties": { "id": { "type": "string" } },
                "required": ["id"]
            }
        },
        {
            "name": "list_memories",
            "description": "List memories (most recent first), optionally filtered by type/scope. Returns metadata-only rows by default (no content) so large limits stay token-safe; opt into content with include_content/crop_length. Supports paging and facet counts.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "type": { "type": "string" },
                    "scope": { "type": "string" },
                    "limit": { "type": "integer", "description": "Max rows (default 20)." },
                    "offset": { "type": "integer", "description": "Paging offset." },
                    "include_content": { "type": "boolean", "description": "Include full content (default false)." },
                    "crop_length": { "type": "integer", "description": "If set, include a content snippet cropped to this many words." },
                    "facets": { "type": "array", "items": { "type": "string" }, "description": "Fields to return server-side counts for." }
                }
            }
        },
        {
            "name": "stats",
            "description": "Store statistics: total document count, indexing state, and server-side counts grouped by field (defaults to type/scope/source).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "group_by": { "type": "array", "items": { "type": "string" }, "description": "Fields to group counts by (default: type, scope, source)." }
                }
            }
        }
    ])
}

async fn handle_tool_call(svc: &MemoryService, id: Value, params: Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let result: anyhow::Result<Value> = match name {
        "save_memory" => save_memory(svc, args).await,
        "get_memory" => get_memory(svc, args).await,
        "read_memory" => read_memory(svc, args).await,
        "update_memory" => update_memory(svc, args).await,
        "forget_memory" => forget_memory(svc, args).await,
        "list_memories" => list_memories(svc, args).await,
        "stats" => stats(svc, args).await,
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    match result {
        Ok(value) => ok(id, tool_text(&value)),
        Err(e) => ok(id, tool_error(&e.to_string())),
    }
}

async fn save_memory(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let content = args
        .get("content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("`content` is required"))?
        .to_string();
    let req = SaveRequest {
        content,
        title: str_field(&args, "title"),
        r#type: args
            .get("type")
            .and_then(|t| t.as_str())
            .and_then(MemoryType::parse),
        tags: str_array(&args, "tags"),
        scope: str_field(&args, "scope"),
        source: Some(Source::Mcp),
        source_path: None,
        source_client: str_field(&args, "source_client"),
    };
    let saved_id = svc.save(req).await?;
    Ok(json!({ "id": saved_id }))
}

async fn get_memory(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .unwrap_or("")
        .to_string();
    let req = GetRequest {
        query,
        limit: args
            .get("limit")
            .and_then(|l| l.as_u64())
            .map(|n| n as usize),
        offset: args
            .get("offset")
            .and_then(|o| o.as_u64())
            .map(|n| n as usize),
        r#type: args
            .get("type")
            .and_then(|t| t.as_str())
            .and_then(MemoryType::parse),
        scope: str_field(&args, "scope"),
        since: args.get("since").and_then(|s| s.as_i64()),
        until: args.get("until").and_then(|s| s.as_i64()),
        semantic_ratio: args
            .get("semantic_ratio")
            .and_then(|r| r.as_f64())
            .map(|f| f as f32),
    };
    let opts = projection_opts(&args, ProjectionOptions::search_default());
    let result = svc.get(req, &opts).await?;
    Ok(query_result_json(result))
}

async fn read_memory(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow::anyhow!("`id` is required"))?;
    match svc.read(id).await? {
        Some(item) => Ok(json!({ "memory": item })),
        None => Ok(json!({ "memory": Value::Null, "error": format!("no memory with id {id}") })),
    }
}

async fn update_memory(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow::anyhow!("`id` is required"))?;
    let updated = svc
        .update(
            id,
            str_field(&args, "content"),
            str_field(&args, "title"),
            args.get("tags").map(|_| str_array(&args, "tags")),
            str_field(&args, "scope"),
            args.get("type")
                .and_then(|t| t.as_str())
                .and_then(MemoryType::parse),
        )
        .await?;
    Ok(json!({ "updated": updated, "id": id }))
}

async fn forget_memory(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow::anyhow!("`id` is required"))?;
    let deleted = svc.forget(id).await?;
    Ok(json!({ "deleted": deleted, "id": id }))
}

async fn list_memories(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let opts = projection_opts(&args, ProjectionOptions::list_default());
    let result = svc
        .list(
            args.get("type")
                .and_then(|t| t.as_str())
                .and_then(MemoryType::parse),
            str_field(&args, "scope"),
            args.get("limit")
                .and_then(|l| l.as_u64())
                .map(|n| n as usize)
                .unwrap_or(20),
            args.get("offset")
                .and_then(|o| o.as_u64())
                .map(|n| n as usize)
                .unwrap_or(0),
            &opts,
        )
        .await?;
    Ok(query_result_json(result))
}

/// Parse shared projection knobs (`include_content`, `crop_length`,
/// `highlight`, `facets`) from tool arguments, starting from `base` defaults.
fn projection_opts(args: &Value, mut base: ProjectionOptions) -> ProjectionOptions {
    if let Some(b) = args.get("include_content").and_then(|v| v.as_bool()) {
        base.include_content = b;
    }
    if let Some(n) = args.get("crop_length").and_then(|v| v.as_u64()) {
        base.crop_length = Some(n as usize);
    }
    if let Some(b) = args.get("highlight").and_then(|v| v.as_bool()) {
        base.highlight = b;
    }
    base.facets = str_array(args, "facets");
    base
}

/// Serialize a [`QueryResult`] into the tool response envelope.
fn query_result_json(result: QueryResult) -> Value {
    let mut out = json!({
        "count": result.hits.len(),
        "estimatedTotalHits": result.estimated_total,
        "memories": result.hits,
    });
    if let Some(facets) = result.facet_distribution {
        out["facets"] = facets;
    }
    out
}

async fn stats(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let group_by = str_array(&args, "group_by");
    svc.stats(&group_by).await
}

// --- helpers ---------------------------------------------------------------

fn str_field(args: &Value, key: &str) -> Option<String> {
    args.get(key)
        .and_then(|v| v.as_str())
        .map(|s| s.to_string())
}

fn str_array(args: &Value, key: &str) -> Vec<String> {
    args.get(key)
        .and_then(|v| v.as_array())
        .map(|a| {
            a.iter()
                .filter_map(|x| x.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default()
}

/// Wrap a JSON value as MCP tool result content (pretty-printed text).
fn tool_text(value: &Value) -> Value {
    let text = serde_json::to_string_pretty(value).unwrap_or_else(|_| value.to_string());
    json!({ "content": [{ "type": "text", "text": text }], "isError": false })
}

fn tool_error(message: &str) -> Value {
    json!({ "content": [{ "type": "text", "text": message }], "isError": true })
}

fn ok(id: Value, result: Value) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "result": result })
}

fn err(id: Value, code: i64, message: &str) -> Value {
    json!({ "jsonrpc": "2.0", "id": id, "error": { "code": code, "message": message } })
}
