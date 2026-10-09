//! The memory service — the heart of memd (PRD §6.1.2).
//!
//! CRUD over memories: classify (heuristics), dedup (content hash), stamp
//! timestamps, run hybrid search. Embedding is delegated to Meilisearch.

use super::classify;
use super::model::{Knowledge, MemoryItem, MemoryType, Source, now_secs};
use crate::config::Config;
use crate::history::{EventAction, EventLog, EventQuery, MemoryEvent};
use crate::meili::MeiliClient;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};
use std::collections::HashMap;

/// A request to persist a memory.
#[derive(Debug, Clone, Default)]
pub struct SaveRequest {
    pub content: String,
    pub title: Option<String>,
    pub r#type: Option<MemoryType>,
    pub tags: Vec<String>,
    pub scope: Option<String>,
    pub source: Option<Source>,
    pub source_path: Option<String>,
    pub source_client: Option<String>,
    /// Entity ids this memory mentions (already resolved by the caller).
    pub entities: Vec<String>,
}

/// A request to recall memories.
#[derive(Debug, Clone, Default)]
pub struct GetRequest {
    pub query: String,
    pub limit: Option<usize>,
    pub offset: Option<usize>,
    pub r#type: Option<MemoryType>,
    pub scope: Option<String>,
    pub since: Option<i64>,
    pub until: Option<i64>,
    pub semantic_ratio: Option<f32>,
    /// Additional raw Meilisearch filter clauses, ANDed with the rest. Used by
    /// `memd context` to shape what gets injected into a session.
    pub extra_filters: Vec<String>,
    /// Only memories that mention this entity id.
    pub entity: Option<String>,
    /// Only records with this `status`.
    pub status: Option<String>,
    /// Only entities of this kind.
    pub kind_of: Option<String>,
}

/// Controls how much of each memory a query returns. The funnel is:
/// list/search → lightweight rows (cropped snippet) → `read(id)` → full blob.
#[derive(Debug, Clone)]
pub struct ProjectionOptions {
    /// Return the full `content` instead of a cropped snippet.
    pub include_content: bool,
    /// Crop `content` to this many words (query-aware). Ignored when
    /// `include_content` is set; `None` here means omit content entirely.
    pub crop_length: Option<usize>,
    /// Wrap matched terms in markdown `**bold**` in the returned snippet.
    pub highlight: bool,
    /// Facet fields to compute server-side distributions for.
    pub facets: Vec<String>,
}

impl ProjectionOptions {
    /// Defaults for `search`: a query-aware ~50-word highlighted snippet.
    pub fn search_default() -> Self {
        Self {
            include_content: false,
            crop_length: Some(50),
            highlight: true,
            facets: Vec::new(),
        }
    }

    /// Defaults for `list`: metadata only, no content at all (token-safe).
    pub fn list_default() -> Self {
        Self {
            include_content: false,
            crop_length: None,
            highlight: false,
            facets: Vec::new(),
        }
    }
}

/// A page of query results: lightweight rows plus optional facet counts.
#[derive(Debug, Clone)]
pub struct QueryResult {
    pub hits: Vec<Value>,
    pub estimated_total: u64,
    pub facet_distribution: Option<Value>,
}

/// Lean metadata fields returned for every row (never the full `content`).
const META_FIELDS: &[&str] = &[
    "id",
    "title",
    "type",
    "scope",
    "source",
    "source_path",
    "tags",
    "created_at",
    "updated_at",
    "last_accessed_at",
];

#[derive(Clone)]
pub struct MemoryService {
    client: MeiliClient,
    events: EventLog,
    default_semantic_ratio: f32,
}

impl MemoryService {
    pub fn new(client: MeiliClient, default_semantic_ratio: f32) -> Self {
        let events = EventLog::from_client(&client);
        Self {
            client,
            events,
            default_semantic_ratio,
        }
    }

    /// Build a service from config, pointing at the dedicated MS instance.
    pub fn from_config(cfg: &Config) -> Self {
        let client = MeiliClient::new(cfg.meili_url(), cfg.meilisearch.master_key.clone());
        Self::new(client, cfg.embedder.default_semantic_ratio)
    }

    pub fn client(&self) -> &MeiliClient {
        &self.client
    }

    /// The audit log of memory mutations.
    pub fn events(&self) -> &EventLog {
        &self.events
    }

    /// Query the mutation history (newest first).
    pub async fn history(&self, q: &EventQuery) -> Result<Vec<Value>> {
        self.events.query(q).await
    }

    /// Record a create/update/delete event, best-effort. Crawler-sourced
    /// mutations are skipped here — they are summarized once per crawl pass.
    #[allow(clippy::too_many_arguments)]
    async fn record_mutation(
        &self,
        action: EventAction,
        memory_id: &str,
        title: Option<String>,
        ty: Option<String>,
        scope: Option<String>,
        source: Source,
        source_client: Option<String>,
    ) {
        if source == Source::Crawler {
            return;
        }
        self.events
            .record(MemoryEvent::mutation(
                action,
                memory_id,
                title,
                ty,
                scope,
                source,
                source_client,
            ))
            .await;
    }

    /// Persist a memory. Classifies the type if absent, dedups on content hash,
    /// stamps timestamps, and upserts (Meilisearch embeds it locally).
    /// Returns the document id.
    pub async fn save(&self, req: SaveRequest) -> Result<String> {
        let source = req.source.unwrap_or(Source::Cli);
        let hash = content_hash(&req.content);

        // Dedup: identical content already stored → return it untouched.
        if let Some(existing) = self.find_by_hash(&hash).await? {
            return Ok(existing);
        }

        let ty = req
            .r#type
            .unwrap_or_else(|| classify::classify(source, req.source_path.as_deref()));
        let now = now_secs();
        let id = uuid::Uuid::now_v7().to_string();
        let title = req.title.or_else(|| derive_title(&req.content));

        let item = MemoryItem {
            id: id.clone(),
            content: req.content,
            title,
            summary: None,
            r#type: ty.to_string(),
            tags: req.tags,
            scope: normalize_scope(req.scope.as_deref()),
            source: source.as_str().to_string(),
            source_path: req.source_path,
            source_client: req.source_client,
            created_at: now,
            updated_at: now,
            last_accessed_at: None,
            content_hash: hash,
            knowledge: Knowledge {
                entities: req.entities,
                ..Default::default()
            },
        };
        self.client.upsert(&item).await?;
        self.record_mutation(
            EventAction::Create,
            &id,
            item.title.clone(),
            Some(item.r#type.clone()),
            Some(item.scope.clone()),
            source,
            item.source_client.clone(),
        )
        .await;
        Ok(id)
    }

    /// Build the document for a crawled file, or `None` if it is unchanged
    /// (same content hash) and therefore needs no re-embedding. `existing` is
    /// the stored `(content_hash, created_at)` for this path, if any; the
    /// original `created_at` is preserved on re-index.
    pub fn prepare_crawled(
        &self,
        source_path: &str,
        content: String,
        ty: MemoryType,
        scope: String,
        title: Option<String>,
        existing: Option<(&str, i64)>,
    ) -> Option<MemoryItem> {
        let id = path_id(source_path);
        let hash = content_hash(&content);
        let now = now_secs();

        let mut created_at = now;
        if let Some((old_hash, old_created)) = existing {
            if old_hash == hash {
                return None;
            }
            created_at = old_created;
        }

        let title = title
            .or_else(|| frontmatter_title(&content))
            .or_else(|| Some(file_title(source_path)));
        Some(MemoryItem {
            id,
            content,
            title,
            summary: None,
            r#type: ty.to_string(),
            tags: vec![],
            scope: normalize_scope(Some(&scope)),
            source: Source::Crawler.as_str().to_string(),
            source_path: Some(source_path.to_string()),
            source_client: None,
            created_at,
            updated_at: now,
            last_accessed_at: None,
            content_hash: hash,
            knowledge: Knowledge::default(),
        })
    }

    /// Load the crawler's stored state: `path_id → (content_hash, created_at)`
    /// for every crawler document, in a few paged requests.
    pub async fn crawled_state(&self) -> Result<HashMap<String, (String, i64)>> {
        let docs = self
            .client
            .fetch_docs("source = 'crawler'", &["id", "content_hash", "created_at"])
            .await?;
        Ok(docs
            .into_iter()
            .filter_map(|d| {
                let id = d.get("id")?.as_str()?.to_string();
                let hash = d.get("content_hash")?.as_str()?.to_string();
                let created = d.get("created_at").and_then(|c| c.as_i64()).unwrap_or(0);
                Some((id, (hash, created)))
            })
            .collect())
    }

    /// Delete every crawler-sourced document (the next scan rebuilds them).
    pub async fn forget_all_crawled(&self) -> Result<usize> {
        self.client.delete_by_filter("source = 'crawler'").await
    }

    /// Upsert a single crawled file (used by the watcher). Skips unchanged
    /// content. Returns `true` if a write happened.
    pub async fn upsert_crawled(
        &self,
        source_path: &str,
        content: String,
        ty: MemoryType,
        scope: String,
        title: Option<String>,
    ) -> Result<bool> {
        let existing = self.client.get_doc(&path_id(source_path)).await?;
        let existing = existing.as_ref().and_then(|d| {
            Some((
                d.get("content_hash")?.as_str()?,
                d.get("created_at").and_then(|c| c.as_i64()).unwrap_or(0),
            ))
        });
        match self.prepare_crawled(source_path, content, ty, scope, title, existing) {
            Some(item) => {
                self.client.upsert(&item).await?;
                Ok(true)
            }
            None => Ok(false),
        }
    }

    /// Fetch a memory without bumping `last_accessed_at`.
    pub async fn get_item(&self, id: &str) -> Result<Option<MemoryItem>> {
        Ok(self
            .client
            .get_doc(id)
            .await?
            .and_then(|d| serde_json::from_value(d).ok()))
    }

    /// Write an entity document as-is and record the mutation. Unlike
    /// [`save`](Self::save) this never dedups on content: two entities can
    /// legitimately share (empty) content.
    pub async fn put_entity(&self, item: &MemoryItem, created: bool) -> Result<()> {
        self.client.upsert(item).await?;
        self.record_mutation(
            if created {
                EventAction::Create
            } else {
                EventAction::Update
            },
            &item.id,
            item.title.clone(),
            Some(item.r#type.clone()),
            Some(item.scope.clone()),
            Source::parse(&item.source).unwrap_or(Source::Cli),
            item.source_client.clone(),
        )
        .await;
        Ok(())
    }

    /// Merge partial documents into existing ones (`PUT`).
    #[allow(dead_code)] // first caller: Task 7; removed there
    pub async fn patch(&self, patches: &[Value]) -> Result<()> {
        if patches.is_empty() {
            return Ok(());
        }
        self.client.update_many(patches).await
    }

    /// Upsert a batch of documents in one Meilisearch task (one embedding pass
    /// for the whole batch — far faster than per-document during a full crawl).
    pub async fn upsert_batch(&self, items: &[MemoryItem]) -> Result<()> {
        if items.is_empty() {
            return Ok(());
        }
        self.client.upsert_many(items).await
    }

    /// Recall memories via hybrid search, returning lightweight rows (a
    /// query-aware cropped snippet by default). Bumps `last_accessed_at` on hits.
    pub async fn get(&self, req: GetRequest, opts: &ProjectionOptions) -> Result<QueryResult> {
        let ratio = req.semantic_ratio.unwrap_or(self.default_semantic_ratio);
        let mut body = json!({
            "q": req.query,
            "limit": req.limit.unwrap_or(10),
            "offset": req.offset.unwrap_or(0),
            "hybrid": { "embedder": "default", "semanticRatio": ratio },
        });
        apply_projection(&mut body, opts);

        let result = self.run_filtered(body, &req, opts).await?;
        self.bump_accessed(&result.hits).await;
        Ok(result)
    }

    /// Run `body` with the request's filters. Sub-scope matching uses
    /// `STARTS WITH`, which older engines reject; on that error the query is
    /// retried with exact/ancestor scopes only.
    async fn run_filtered(
        &self,
        mut body: Value,
        req: &GetRequest,
        opts: &ProjectionOptions,
    ) -> Result<QueryResult> {
        let filters = build_filters(req, true);
        if !filters.is_empty() {
            body["filter"] = Value::String(filters.join(" AND "));
        }
        match self.run_query(&body, opts).await {
            Ok(r) => Ok(r),
            Err(e) if e.to_string().contains("STARTS WITH") => {
                let filters = build_filters(req, false);
                body["filter"] = Value::String(filters.join(" AND "));
                self.run_query(&body, opts).await
            }
            Err(e) => Err(e),
        }
    }

    /// Fetch the full document for a memory by id (the end of the funnel).
    pub async fn read(&self, id: &str) -> Result<Option<MemoryItem>> {
        match self.client.get_doc(id).await? {
            Some(doc) => {
                // Bump last-accessed; tolerate failure.
                let _ = self
                    .client
                    .update_many(&[json!({ "id": id, "last_accessed_at": now_secs() })])
                    .await;
                Ok(serde_json::from_value(doc).ok())
            }
            None => Ok(None),
        }
    }

    /// Delete a memory by id. Returns whether something was deleted. Records a
    /// `delete` event (with a metadata snapshot taken before deletion), unless
    /// the deletion came from the crawler.
    pub async fn forget(&self, id: &str, source: Source) -> Result<bool> {
        // Snapshot metadata before deletion so the audit row stays readable.
        let meta = self.client.get_doc(id).await.ok().flatten();
        let deleted = self.client.delete_doc(id).await?;
        if deleted {
            let field = |k: &str| {
                meta.as_ref()
                    .and_then(|d| d.get(k))
                    .and_then(|v| v.as_str())
                    .map(String::from)
            };
            self.record_mutation(
                EventAction::Delete,
                id,
                field("title"),
                field("type"),
                field("scope"),
                source,
                None,
            )
            .await;
        }
        Ok(deleted)
    }

    /// Update fields on an existing memory (partial). Returns false if absent.
    #[allow(clippy::too_many_arguments)]
    pub async fn update(
        &self,
        id: &str,
        content: Option<String>,
        title: Option<String>,
        tags: Option<Vec<String>>,
        scope: Option<String>,
        ty: Option<MemoryType>,
        source: Source,
    ) -> Result<bool> {
        let Some(mut doc) = self.client.get_doc(id).await? else {
            return Ok(false);
        };
        if let Some(c) = content {
            doc["content_hash"] = Value::String(content_hash(&c));
            doc["content"] = Value::String(c);
        }
        if let Some(t) = title {
            doc["title"] = Value::String(t);
        }
        if let Some(t) = tags {
            doc["tags"] = json!(t);
        }
        if let Some(s) = scope {
            doc["scope"] = Value::String(s);
        }
        if let Some(t) = ty {
            doc["type"] = Value::String(t.to_string());
        }
        doc["updated_at"] = json!(now_secs());
        self.client.upsert(&doc).await?;
        let field = |k: &str| doc.get(k).and_then(|v| v.as_str()).map(String::from);
        self.record_mutation(
            EventAction::Update,
            id,
            field("title"),
            field("type"),
            field("scope"),
            source,
            None,
        )
        .await;
        Ok(true)
    }

    /// List memories matching optional filters (most recent first). Returns
    /// metadata-only rows by default — token-safe regardless of `limit`.
    pub async fn list(
        &self,
        ty: Option<MemoryType>,
        scope: Option<String>,
        limit: usize,
        offset: usize,
        opts: &ProjectionOptions,
    ) -> Result<QueryResult> {
        let mut body = json!({
            "q": "",
            "limit": limit,
            "offset": offset,
            "sort": ["updated_at:desc"],
        });
        apply_projection(&mut body, opts);
        let req = GetRequest {
            r#type: ty,
            scope,
            ..Default::default()
        };
        self.run_filtered(body, &req, opts).await
    }

    /// Like [`list`](Self::list) but with the full request (extra filters).
    pub async fn list_with(
        &self,
        req: &GetRequest,
        limit: usize,
        opts: &ProjectionOptions,
    ) -> Result<QueryResult> {
        let mut body = json!({
            "q": "",
            "limit": limit,
            "offset": req.offset.unwrap_or(0),
            "sort": ["updated_at:desc"],
        });
        apply_projection(&mut body, opts);
        self.run_filtered(body, req, opts).await
    }

    /// Store statistics: total document count plus server-side facet
    /// distributions (counts grouped by the requested fields).
    pub async fn stats(&self, group_by: &[String]) -> Result<Value> {
        let raw = self.client.stats().await?;
        let count = raw
            .get("numberOfDocuments")
            .and_then(|n| n.as_u64())
            .unwrap_or(0);
        let is_indexing = raw
            .get("isIndexing")
            .and_then(|b| b.as_bool())
            .unwrap_or(false);

        let fields: Vec<String> = if group_by.is_empty() {
            ["type", "scope", "source", "kind_of", "status"]
                .iter()
                .map(|s| s.to_string())
                .collect()
        } else {
            group_by.to_vec()
        };
        let body = json!({ "q": "", "limit": 0, "facets": fields });
        let facets = self
            .client
            .search(&body)
            .await
            .ok()
            .and_then(|r| r.get("facetDistribution").cloned());

        Ok(json!({
            "numberOfDocuments": count,
            "isIndexing": is_indexing,
            "distribution": facets,
        }))
    }

    /// Run a prepared search body and shape hits into lean rows.
    async fn run_query(&self, body: &Value, opts: &ProjectionOptions) -> Result<QueryResult> {
        let resp = self.client.search(body).await?;
        let hits = resp
            .get("hits")
            .and_then(|h| h.as_array())
            .map(|a| a.iter().map(|h| shape_row(h, opts)).collect())
            .unwrap_or_default();
        Ok(QueryResult {
            hits,
            estimated_total: resp
                .get("estimatedTotalHits")
                .and_then(|n| n.as_u64())
                .unwrap_or(0),
            facet_distribution: resp.get("facetDistribution").cloned(),
        })
    }

    /// Find a document id by content hash, if any.
    async fn find_by_hash(&self, hash: &str) -> Result<Option<String>> {
        let body = json!({
            "q": "",
            "limit": 1,
            "filter": format!("content_hash = '{hash}'"),
            "attributesToRetrieve": ["id"],
        });
        let resp = self.client.search(&body).await.context("dedup lookup")?;
        Ok(resp
            .get("hits")
            .and_then(|h| h.as_array())
            .and_then(|a| a.first())
            .and_then(|d| d.get("id"))
            .and_then(|i| i.as_str())
            .map(String::from))
    }

    /// Best-effort bump of `last_accessed_at` for retrieved rows (batched).
    /// Must be a merge (`PUT`): a replace would wipe the memory down to its id.
    async fn bump_accessed(&self, rows: &[Value]) {
        let now = now_secs();
        let patches: Vec<Value> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(|i| i.as_str()))
            .map(|id| json!({ "id": id, "last_accessed_at": now }))
            .collect();
        if !patches.is_empty() {
            let _ = self.client.update_many(&patches).await;
        }
    }
}

/// Apply projection/crop/highlight/facet settings to a search body so the
/// server returns lean rows. Mirrors the Meilisearch search params 1:1.
fn apply_projection(body: &mut Value, opts: &ProjectionOptions) {
    let mut attrs: Vec<String> = META_FIELDS.iter().map(|s| s.to_string()).collect();
    if opts.include_content {
        // Full blob requested → retrieve content at top level, no crop.
        attrs.push("content".to_string());
    } else if let Some(len) = opts.crop_length {
        // Snippet: keep content out of attributesToRetrieve (so the full blob
        // is never returned) but crop it — the cropped value lands in
        // `_formatted.content`.
        body["attributesToCrop"] = json!(["content"]);
        body["cropLength"] = json!(len);
        body["cropMarker"] = json!("…");
        if opts.highlight {
            // Markdown bold reads better than HTML tags for LLM consumers.
            body["attributesToHighlight"] = json!(["content"]);
            body["highlightPreTag"] = json!("**");
            body["highlightPostTag"] = json!("**");
        }
    }
    // else: metadata only — no content at all.
    body["attributesToRetrieve"] = json!(attrs);
    if !opts.facets.is_empty() {
        body["facets"] = json!(opts.facets);
    }
}

/// Shape one raw Meilisearch hit into a lean row: top-level metadata (correct
/// types) plus the cropped/highlighted snippet from `_formatted.content` when
/// the full blob was not requested.
fn shape_row(hit: &Value, opts: &ProjectionOptions) -> Value {
    let mut row = hit.clone();
    let formatted = row.as_object_mut().and_then(|o| o.remove("_formatted"));
    if !opts.include_content
        && let Some(snippet) = formatted
            .as_ref()
            .and_then(|f| f.get("content"))
            .and_then(|c| c.as_str())
    {
        row["content"] = Value::String(snippet.to_string());
    }
    row
}

/// SHA-256 hex of trimmed content.
pub fn content_hash(content: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(content.trim().as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Stable document id derived from a file path (crawled docs).
pub fn path_id(path: &str) -> String {
    let mut hasher = Sha256::new();
    hasher.update(path.as_bytes());
    format!("{:x}", hasher.finalize())
}

/// Derive a short title from free-text content (first non-empty line).
fn derive_title(content: &str) -> Option<String> {
    let line = content.lines().map(str::trim).find(|l| !l.is_empty())?;
    let trimmed = line.trim_start_matches('#').trim();
    let title: String = trimmed.chars().take(80).collect();
    if title.is_empty() { None } else { Some(title) }
}

/// Title from a YAML front matter block: `description:` first (what agents
/// write their memory summaries into), else `name:`/`title:`.
fn frontmatter_title(content: &str) -> Option<String> {
    let rest = content.strip_prefix("---")?;
    let end = rest.find("\n---")?;
    let block = &rest[..end];
    let field = |key: &str| {
        block.lines().find_map(|l| {
            let v = l.strip_prefix(key)?.trim();
            let v = v.trim_matches('"').trim_matches('\'').trim();
            (!v.is_empty()).then(|| v.chars().take(120).collect::<String>())
        })
    };
    field("description:")
        .or_else(|| field("name:"))
        .or_else(|| field("title:"))
}

/// Title for a crawled file: its file name.
fn file_title(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Build a Meilisearch filter expression list from a get/list request.
///
/// Scope is hierarchical: a query scoped to `/a/b/c` matches memories scoped
/// to `/a/b/c`, to any ancestor (`/a/b`, `/a`), to `global`, and — when
/// `with_subscopes` — to any sub-path (`/a/b/c/…`, e.g. a worktree or a
/// nested spec directory).
fn build_filters(req: &GetRequest, with_subscopes: bool) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(ty) = req.r#type {
        f.push(format!("type = '{}'", ty));
    }
    if let Some(scope) = &req.scope {
        f.push(scope_filter(scope, with_subscopes));
    }
    if let Some(since) = req.since {
        f.push(format!("created_at >= {since}"));
    }
    if let Some(until) = req.until {
        f.push(format!("created_at <= {until}"));
    }
    if let Some(e) = &req.entity {
        f.push(format!("entities = '{}'", escape(e)));
    }
    if let Some(s) = &req.status {
        if s == "accepted" {
            // Decisions saved before statuses existed count as accepted.
            f.push(
                "(status = 'accepted' OR (type = 'decision' AND status NOT EXISTS))".to_string(),
            );
        } else {
            f.push(format!("status = '{}'", escape(s)));
        }
    }
    if let Some(k) = &req.kind_of {
        f.push(format!("kind_of = '{}'", escape(k)));
    }
    f.extend(req.extra_filters.iter().cloned());
    f
}

/// The scope clause for `scope` (see [`build_filters`]).
pub fn scope_filter(scope: &str, with_subscopes: bool) -> String {
    let scope = normalize_scope(Some(scope));
    if scope == "global" {
        return "scope = 'global'".to_string();
    }
    let chain: Vec<String> = scope_chain(&scope)
        .into_iter()
        .map(|s| format!("'{}'", escape(&s)))
        .collect();
    let exact = format!("scope IN [{}]", chain.join(", "));
    if with_subscopes {
        format!("({exact} OR scope STARTS WITH '{}/')", escape(&scope))
    } else {
        exact
    }
}

/// `global` plus every ancestor of `scope` down to itself, shortest first.
pub fn scope_chain(scope: &str) -> Vec<String> {
    let mut out = vec!["global".to_string()];
    if scope == "global" {
        return out;
    }
    let mut acc = String::new();
    for part in scope.split('/').filter(|p| !p.is_empty()) {
        acc.push('/');
        acc.push_str(part);
        out.push(acc.clone());
    }
    if !scope.starts_with('/') && scope != "global" {
        // Relative/named scope: keep it verbatim too.
        out.push(scope.to_string());
    }
    out
}

/// Canonical scope string: `global`, or an absolute path with `~` expanded,
/// no trailing slash, and no `.claude/worktrees/<wt>` segment (a worktree is
/// the same project as its parent repo).
pub fn normalize_scope(scope: Option<&str>) -> String {
    let Some(raw) = scope.map(str::trim).filter(|s| !s.is_empty()) else {
        return "global".to_string();
    };
    if raw.eq_ignore_ascii_case("global") {
        return "global".to_string();
    }
    let expanded = crate::config::expand_tilde(raw)
        .to_string_lossy()
        .to_string();
    let trimmed = expanded.trim_end_matches('/');
    let s = if trimmed.is_empty() { "/" } else { trimmed };
    match s.find("/.claude/worktrees/") {
        Some(i) => s[..i].to_string(),
        None => s.to_string(),
    }
}

/// Escape single quotes in a filter literal.
fn escape(s: &str) -> String {
    s.replace('\'', "\\'")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn hash_is_stable_and_trim_insensitive() {
        assert_eq!(content_hash("hello"), content_hash("  hello  "));
        assert_ne!(content_hash("a"), content_hash("b"));
    }

    #[test]
    fn derives_title_from_heading() {
        assert_eq!(derive_title("# My Title\nbody"), Some("My Title".into()));
        assert_eq!(derive_title("\n\nplain line"), Some("plain line".into()));
        assert_eq!(derive_title("   "), None);
    }

    #[test]
    fn builds_filters() {
        let req = GetRequest {
            r#type: Some(MemoryType::Fact),
            since: Some(100),
            ..Default::default()
        };
        let f = build_filters(&req, true);
        assert!(f.contains(&"type = 'fact'".to_string()));
        assert!(f.contains(&"created_at >= 100".to_string()));
    }

    #[test]
    fn scope_chain_includes_global_and_ancestors() {
        assert_eq!(
            scope_chain("/a/b/c"),
            vec!["global", "/a", "/a/b", "/a/b/c"]
        );
        assert_eq!(scope_chain("global"), vec!["global"]);
    }

    #[test]
    fn scope_filter_matches_ancestors_and_subscopes() {
        let f = scope_filter("/a/b", true);
        assert_eq!(
            f,
            "(scope IN ['global', '/a', '/a/b'] OR scope STARTS WITH '/a/b/')"
        );
        assert_eq!(
            scope_filter("/a/b", false),
            "scope IN ['global', '/a', '/a/b']"
        );
        assert_eq!(scope_filter("global", true), "scope = 'global'");
    }

    #[test]
    fn normalizes_scopes() {
        assert_eq!(normalize_scope(None), "global");
        assert_eq!(normalize_scope(Some("  ")), "global");
        assert_eq!(normalize_scope(Some("Global")), "global");
        assert_eq!(normalize_scope(Some("/a/b/")), "/a/b");
        assert_eq!(
            normalize_scope(Some("/p/memd/.claude/worktrees/wt-1/sub")),
            "/p/memd"
        );
        let home = crate::config::expand_tilde("~/x")
            .to_string_lossy()
            .to_string();
        assert_eq!(normalize_scope(Some("~/x/")), home);
    }

    #[test]
    fn frontmatter_titles() {
        let md = "---\nname: foo\ndescription: \"Why we chose X\"\n---\n\nbody";
        assert_eq!(frontmatter_title(md).as_deref(), Some("Why we chose X"));
        assert_eq!(
            frontmatter_title("---\nname: bar\n---\n").as_deref(),
            Some("bar")
        );
        assert_eq!(frontmatter_title("# plain"), None);
    }

    #[test]
    fn prepare_crawled_skips_unchanged_and_keeps_created_at() {
        let client = MeiliClient::new("http://127.0.0.1:1", "k");
        let svc = MemoryService::new(client, 0.5);
        let hash = content_hash("hello");
        assert!(
            svc.prepare_crawled(
                "/r/README.md",
                "hello".into(),
                MemoryType::ProjectOverview,
                "/r".into(),
                None,
                Some((&hash, 42))
            )
            .is_none()
        );
        let item = svc
            .prepare_crawled(
                "/r/README.md",
                "changed".into(),
                MemoryType::ProjectOverview,
                "/r/".into(),
                None,
                Some((&hash, 42)),
            )
            .unwrap();
        assert_eq!(item.created_at, 42);
        assert_eq!(item.scope, "/r");
        assert_eq!(item.title.as_deref(), Some("README.md"));
    }

    #[test]
    fn builds_knowledge_filters() {
        let req = GetRequest {
            entity: Some("entity_lumen".into()),
            status: Some("superseded".into()),
            kind_of: Some("project".into()),
            ..Default::default()
        };
        let f = build_filters(&req, true);
        assert!(
            f.contains(&"entities = 'entity_lumen'".to_string()),
            "{f:?}"
        );
        assert!(f.contains(&"status = 'superseded'".to_string()), "{f:?}");
        assert!(f.contains(&"kind_of = 'project'".to_string()), "{f:?}");

        // Decisions saved before statuses existed count as accepted (spec §10).
        let accepted = GetRequest {
            status: Some("accepted".into()),
            ..Default::default()
        };
        assert_eq!(
            build_filters(&accepted, true),
            vec!["(status = 'accepted' OR (type = 'decision' AND status NOT EXISTS))".to_string()]
        );
    }
}
