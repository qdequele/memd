//! MCP over JSON-RPC 2.0 — protocol handling, transport-agnostic.
//!
//! Hand-rolled rather than depending on a fast-moving SDK. Implements the
//! subset every MCP client needs: `initialize`, `tools/list`, `tools/call`,
//! and the `notifications/initialized` no-op. Tool calls are translated into
//! [`MemoryService`] operations.

use crate::history::{EventAction, EventQuery};
use crate::knowledge::KnowledgeService;
use crate::knowledge::ident::entity_id;
use crate::knowledge::resolve::{Resolution, ambiguity_message, resolve};
use crate::knowledge::service::{EntityInput, RelationInput};
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
memd is your persistent, cross-tool long-term memory: a single local store \
shared with every other LLM tool on this machine (Claude Code, Codex, Gemini \
CLI, Cursor, Windsurf, Cline, Zed). What one tool saves, all the others recall.

- RECALL FIRST: before starting a task or answering from assumed context, call \
`get_memory` with the user's goal and `scope` set to the project root path. \
Recall includes the project's parent scopes and `global`. Prefer recalling over \
asking the user to repeat themselves.
- SAVE WHAT OUTLIVES THE SESSION: decisions (with the why), user preferences, \
stable facts about a project, reusable solutions. One fact per memory, \
self-contained, dated when it matters. Set `type` (decision, preference, fact, \
task) and `scope` (project root path, or `global` for user-wide truths).
- DO NOT SAVE what is already on disk (code, README, instruction files, docs) \
or what only matters to the current conversation. Search before saving and \
`update_memory` a near-duplicate instead of adding another.
- NAME THINGS: when you save, list the entities involved in `entities` and \
any relation you learned in `relations`; use `save_entity` for a company, \
team, person, project, product, service, customer or concept. Before asking \
the user what something is, call `explore` with its name.
- Results are lightweight rows with a snippet; call `read_memory(id)` for the \
full text, `list_memories` to browse, `stats` for an overview.

memd is the shared memory layer, not optional scratch space.";

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
    let kinds = crate::knowledge::ident::KINDS.join(", ");
    let predicates = crate::knowledge::ident::PREDICATES.join(", ");
    json!([
        {
            "name": "save_memory",
            "description": "Persist a memory so any LLM tool can recall it later. Stamps timestamps, classifies the type if omitted, dedups identical content, and embeds it locally.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "entities": { "type": "array", "items": { "type": "string" }, "description": "Names (or ids) of the entities this memory is about. Unknown names become stub entities. The project the scope belongs to is linked automatically." },
                    "relations": { "type": "array", "description": format!("Relations you learned, as {{subject, predicate, object, note?}}. Predicates: {predicates} (free text allowed)."), "items": { "type": "object", "properties": { "subject": { "type": "string" }, "predicate": { "type": "string" }, "object": { "type": "string" }, "note": { "type": "string" } }, "required": ["subject", "predicate", "object"] } },
                    "content": { "type": "string", "description": "The memory text." },
                    "type": { "type": "string", "description": "fact, preference, decision, task, project_overview, agent_instruction, file_annotation, code_note, reference." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "scope": { "type": "string", "description": "'global' (user-wide) or the absolute path of the project root the memory belongs to (e.g. ~/Projects/foo)." },
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
                    "entity": { "type": "string", "description": "Only memories that mention this entity (name or id)." },
                    "status": { "type": "string", "description": "Only records with this status (e.g. accepted, superseded, open, active)." },
                    "kind_of": { "type": "string", "description": format!("Only entities of this kind: {kinds}.") },
                    "query": { "type": "string" },
                    "limit": { "type": "integer", "description": "Max rows (default 10)." },
                    "offset": { "type": "integer", "description": "Paging offset." },
                    "type": { "type": "string" },
                    "scope": { "type": "string", "description": "Project root path. Matches memories in this scope, its parent scopes, its sub-scopes, and 'global'." },
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
                    "entities": { "type": "array", "items": { "type": "string" }, "description": "Names (or ids) of the entities this memory is about. Unknown names become stub entities. Added to the memory's existing links." },
                    "relations": { "type": "array", "description": format!("Relations you learned, as {{subject, predicate, object, note?}}. Predicates: {predicates} (free text allowed)."), "items": { "type": "object", "properties": { "subject": { "type": "string" }, "predicate": { "type": "string" }, "object": { "type": "string" }, "note": { "type": "string" } }, "required": ["subject", "predicate", "object"] } },
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
                    "entity": { "type": "string", "description": "Only memories that mention this entity (name or id)." },
                    "status": { "type": "string", "description": "Only records with this status (e.g. accepted, superseded, open, active)." },
                    "kind_of": { "type": "string", "description": format!("Only entities of this kind: {kinds}.") },
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
        },
        {
            "name": "history",
            "description": "Audit timeline of memory changes (most recent first): which memories were created, updated, deleted, or crawled, when, and by which source. Metadata only — no old content. Optionally filter by action/type/scope/time.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "action": { "type": "string", "description": "create, update, delete, or crawl." },
                    "type": { "type": "string", "description": "Filter by memory type." },
                    "scope": { "type": "string", "description": "Filter by scope." },
                    "since": { "type": "integer", "description": "Unix seconds lower bound on the event timestamp." },
                    "limit": { "type": "integer", "description": "Max events (default 20)." }
                }
            }
        },
        {
            "name": "save_entity",
            "description": format!("Create or update a thing in the user's world ({kinds}) with its aliases, owner, status and relations. Saving the same name (or an alias) again updates the same record: passed fields overwrite, absent fields are kept, aliases and tags are merged. Relations start from this entity; predicates: {predicates}."),
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Canonical display name." },
                    "kind_of": { "type": "string", "description": format!("One of: {kinds}.") },
                    "description": { "type": "string", "description": "What it is, in a sentence or two." },
                    "aliases": { "type": "array", "items": { "type": "string" }, "description": "Other names agents or people use for it." },
                    "owner": { "type": "string", "description": "Name or id of the owning person, team or company." },
                    "status": { "type": "string", "description": "e.g. active, archived." },
                    "scope": { "type": "string", "description": "'global' or the project root path it belongs to." },
                    "tags": { "type": "array", "items": { "type": "string" } },
                    "url": { "type": "string" },
                    "relations": { "type": "array", "items": { "type": "object", "properties": { "predicate": { "type": "string" }, "object": { "type": "string" }, "note": { "type": "string" } }, "required": ["predicate", "object"] } }
                },
                "required": ["name", "kind_of"]
            }
        },
        {
            "name": "explore",
            "description": "Everything memd knows about one entity: the entity, its relations in both directions, the related entities, and the memories that mention it. Call this before asking the user what something is. Unknown names return suggestions.",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "name": { "type": "string", "description": "Name, alias or id." },
                    "depth": { "type": "integer", "description": "1 (default) or 2 to include the neighbours' relations." },
                    "limit": { "type": "integer", "description": "Max memories (default 10, max 50)." },
                    "scope": { "type": "string", "description": "Current project root, to break ties between same-named entities." }
                },
                "required": ["name"]
            }
        },
        {
            "name": "forget_relation",
            "description": "Remove one relation between two entities (names or ids).",
            "inputSchema": {
                "type": "object",
                "properties": {
                    "subject": { "type": "string" },
                    "predicate": { "type": "string" },
                    "object": { "type": "string" }
                },
                "required": ["subject", "predicate", "object"]
            }
        }
    ])
}

async fn handle_tool_call(svc: &MemoryService, id: Value, params: Value) -> Value {
    let name = params.get("name").and_then(|n| n.as_str()).unwrap_or("");
    let args = params.get("arguments").cloned().unwrap_or(json!({}));

    let kn = KnowledgeService::new(svc.clone());
    let result: anyhow::Result<Value> = match name {
        "save_memory" => save_memory(&kn, args).await,
        "get_memory" => get_memory(svc, args).await,
        "read_memory" => read_memory(svc, args).await,
        "update_memory" => update_memory(&kn, args).await,
        "forget_memory" => forget_memory(svc, args).await,
        "list_memories" => list_memories(svc, args).await,
        "stats" => stats(svc, args).await,
        "history" => history(svc, args).await,
        "save_entity" => save_entity(&kn, args).await,
        "explore" => explore(&kn, args).await,
        "forget_relation" => forget_relation(&kn, args).await,
        other => Err(anyhow::anyhow!("unknown tool: {other}")),
    };

    match result {
        Ok(value) => ok(id, tool_text(&value)),
        Err(e) => ok(id, tool_error(&e.to_string())),
    }
}

async fn save_memory(kn: &KnowledgeService, args: Value) -> anyhow::Result<Value> {
    let content = args
        .get("content")
        .and_then(|c| c.as_str())
        .ok_or_else(|| anyhow::anyhow!("`content` is required"))?
        .to_string();
    let relations = parse_relations(&args, true)?;
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
        entities: Vec::new(),
    };
    let (saved_id, warnings) = kn
        .save_memory(req, &str_array(&args, "entities"), &relations)
        .await?;
    let mut out = json!({ "id": saved_id });
    if !warnings.is_empty() {
        out["warnings"] = json!(warnings);
    }
    Ok(out)
}

async fn get_memory(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let query = args
        .get("query")
        .and_then(|q| q.as_str())
        .unwrap_or("")
        .to_string();
    let entity = entity_arg(svc, &args).await?;
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
        extra_filters: Vec::new(),
        entity,
        status: str_field(&args, "status"),
        kind_of: str_field(&args, "kind_of"),
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

async fn update_memory(kn: &KnowledgeService, args: Value) -> anyhow::Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow::anyhow!("`id` is required"))?;
    let updated = kn
        .memories()
        .update(
            id,
            str_field(&args, "content"),
            str_field(&args, "title"),
            args.get("tags").map(|_| str_array(&args, "tags")),
            str_field(&args, "scope"),
            args.get("type")
                .and_then(|t| t.as_str())
                .and_then(MemoryType::parse),
            Source::Mcp,
        )
        .await?;
    let entities = str_array(&args, "entities");
    let relations = parse_relations(&args, true)?;
    let mut out = json!({ "updated": updated, "id": id });
    if updated && (!entities.is_empty() || !relations.is_empty()) {
        let warnings = kn
            .link_existing(
                id,
                &entities,
                &relations,
                Source::Mcp,
                str_field(&args, "source_client"),
            )
            .await?;
        if !warnings.is_empty() {
            out["warnings"] = json!(warnings);
        }
    }
    Ok(out)
}

async fn forget_memory(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let id = args
        .get("id")
        .and_then(|i| i.as_str())
        .ok_or_else(|| anyhow::anyhow!("`id` is required"))?;
    let deleted = svc.forget(id, Source::Mcp).await?;
    Ok(json!({ "deleted": deleted, "id": id }))
}

async fn list_memories(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let opts = projection_opts(&args, ProjectionOptions::list_default());
    let req = GetRequest {
        r#type: args
            .get("type")
            .and_then(|t| t.as_str())
            .and_then(MemoryType::parse),
        scope: str_field(&args, "scope"),
        offset: args
            .get("offset")
            .and_then(|o| o.as_u64())
            .map(|n| n as usize),
        entity: entity_arg(svc, &args).await?,
        status: str_field(&args, "status"),
        kind_of: str_field(&args, "kind_of"),
        ..Default::default()
    };
    let limit = args
        .get("limit")
        .and_then(|l| l.as_u64())
        .map(|n| n as usize)
        .unwrap_or(20);
    let result = svc.list_with(&req, limit, &opts).await?;
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

async fn history(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let query = EventQuery {
        action: args
            .get("action")
            .and_then(|a| a.as_str())
            .and_then(EventAction::parse),
        r#type: str_field(&args, "type"),
        scope: str_field(&args, "scope"),
        since: args.get("since").and_then(|s| s.as_i64()),
        limit: args
            .get("limit")
            .and_then(|l| l.as_u64())
            .map(|n| n as usize)
            .unwrap_or(20),
    };
    let events = svc.history(&query).await?;
    Ok(json!({ "count": events.len(), "events": events }))
}

async fn save_entity(kn: &KnowledgeService, args: Value) -> anyhow::Result<Value> {
    let out = kn.save_entity(parse_entity_input(&args)?).await?;
    let mut v = json!({ "id": out.id, "created": out.created });
    if !out.warnings.is_empty() {
        v["warnings"] = json!(out.warnings);
    }
    Ok(v)
}

async fn explore(kn: &KnowledgeService, args: Value) -> anyhow::Result<Value> {
    let name = str_field(&args, "name").ok_or_else(|| anyhow::anyhow!("`name` is required"))?;
    let depth = args
        .get("depth")
        .and_then(|d| d.as_u64())
        .unwrap_or(1)
        .min(2) as u8;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
    kn.explore(&name, depth, limit, str_field(&args, "scope").as_deref())
        .await
}

async fn forget_relation(kn: &KnowledgeService, args: Value) -> anyhow::Result<Value> {
    let get = |k: &str| str_field(&args, k).ok_or_else(|| anyhow::anyhow!("`{k}` is required"));
    let deleted = kn
        .forget_relation(&get("subject")?, &get("predicate")?, &get("object")?)
        .await?;
    Ok(json!({ "deleted": deleted }))
}

// --- helpers ---------------------------------------------------------------

/// Parse a `relations` array. `subject_required` is true for plain memories,
/// where no entity is implied.
fn parse_relations(args: &Value, subject_required: bool) -> anyhow::Result<Vec<RelationInput>> {
    let Some(arr) = args.get("relations").and_then(|r| r.as_array()) else {
        return Ok(Vec::new());
    };
    arr.iter()
        .map(|r| {
            let field = |k: &str| r.get(k).and_then(|v| v.as_str()).map(String::from);
            let predicate = field("predicate")
                .ok_or_else(|| anyhow::anyhow!("each relation needs a `predicate`"))?;
            let object = field("object")
                .ok_or_else(|| anyhow::anyhow!("each relation needs an `object`"))?;
            let subject = field("subject");
            if subject_required && subject.is_none() {
                anyhow::bail!("each relation needs a `subject` (the entity it starts from)");
            }
            Ok(RelationInput {
                subject,
                predicate,
                object,
                note: field("note"),
            })
        })
        .collect()
}

/// Parse `save_entity` arguments.
fn parse_entity_input(args: &Value) -> anyhow::Result<EntityInput> {
    let name = str_field(args, "name").ok_or_else(|| anyhow::anyhow!("`name` is required"))?;
    let kind =
        str_field(args, "kind_of").ok_or_else(|| anyhow::anyhow!("`kind_of` is required"))?;
    let mut e = EntityInput::new(&name, &kind, Source::Mcp);
    e.description = str_field(args, "description");
    e.aliases = str_array(args, "aliases");
    e.owner = str_field(args, "owner");
    e.status = str_field(args, "status");
    e.scope = str_field(args, "scope");
    e.tags = str_array(args, "tags");
    e.url = str_field(args, "url");
    e.source_client = str_field(args, "source_client");
    e.relations = parse_relations(args, false)?;
    Ok(e)
}

/// Resolve an `entity` argument to an id. An unknown name maps to the id it
/// would have, which matches nothing, so the query returns no rows.
async fn entity_arg(svc: &MemoryService, args: &Value) -> anyhow::Result<Option<String>> {
    let Some(name) = str_field(args, "entity") else {
        return Ok(None);
    };
    match resolve(svc, &name, str_field(args, "scope").as_deref()).await? {
        Resolution::Found(id) => Ok(Some(id)),
        Resolution::NotFound => Ok(Some(entity_id(&name)?)),
        Resolution::Ambiguous(c) => anyhow::bail!(ambiguity_message(&name, &c)),
    }
}

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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn exposes_eleven_tools() {
        let defs = tool_defs();
        let mut names: Vec<&str> = defs
            .as_array()
            .unwrap()
            .iter()
            .map(|t| t["name"].as_str().unwrap())
            .collect();
        names.sort();
        assert_eq!(
            names,
            [
                "explore",
                "forget_memory",
                "forget_relation",
                "get_memory",
                "history",
                "list_memories",
                "read_memory",
                "save_entity",
                "save_memory",
                "stats",
                "update_memory"
            ]
        );
    }

    #[test]
    fn tool_descriptions_carry_the_vocabularies() {
        let text = tool_defs().to_string();
        for k in crate::knowledge::ident::KINDS {
            assert!(text.contains(k), "{k}");
        }
        for p in crate::knowledge::ident::PREDICATES {
            assert!(text.contains(p), "{p}");
        }
    }

    #[test]
    fn parses_relations() {
        let args = json!({ "relations": [
            { "subject": "Lumen", "predicate": "part_of", "object": "Meilisearch Lab", "note": "n" },
            { "predicate": "uses", "object": "Meilisearch" }
        ]});
        let rels = parse_relations(&args, false).unwrap();
        assert_eq!(rels.len(), 2);
        assert_eq!(rels[0].subject.as_deref(), Some("Lumen"));
        assert_eq!(rels[0].note.as_deref(), Some("n"));
        assert_eq!(rels[1].subject, None);
        let err = parse_relations(&args, true).unwrap_err().to_string();
        assert!(err.contains("subject"), "{err}");
        let bad = json!({ "relations": [{ "predicate": "uses" }] });
        assert!(parse_relations(&bad, false).is_err());
        assert!(parse_relations(&json!({}), true).unwrap().is_empty());
    }

    #[test]
    fn parses_entity_input() {
        let args = json!({
            "name": "Lumen", "kind_of": "product", "description": "gateway",
            "aliases": ["lumen-gw"], "owner": "Quentin", "status": "active",
            "relations": [{ "predicate": "part_of", "object": "Meilisearch Lab" }]
        });
        let e = parse_entity_input(&args).unwrap();
        assert_eq!(e.name, "Lumen");
        assert_eq!(e.kind_of, "product");
        assert_eq!(e.aliases, vec!["lumen-gw"]);
        assert_eq!(e.relations.len(), 1);
        assert_eq!(e.source, Source::Mcp);
        assert!(parse_entity_input(&json!({ "kind_of": "product" })).is_err());
        assert!(parse_entity_input(&json!({ "name": "X" })).is_err());
    }

    #[test]
    fn update_memory_does_not_promise_an_automatic_project_link() {
        // Review Important 6.
        let defs = tool_defs();
        let update = defs
            .as_array()
            .unwrap()
            .iter()
            .find(|t| t["name"] == "update_memory")
            .unwrap();
        let text = update["inputSchema"]["properties"]["entities"]["description"]
            .as_str()
            .unwrap();
        assert!(!text.contains("linked automatically"), "{text}");
    }
}
