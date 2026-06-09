//! The memory service — the heart of memd (PRD §6.1.2).
//!
//! CRUD over memories: classify (heuristics), dedup (content hash), stamp
//! timestamps, run hybrid search. Embedding is delegated to Meilisearch.

use super::classify;
use super::model::{MemoryItem, MemoryType, Source, now_secs};
use crate::config::Config;
use crate::meili::MeiliClient;
use anyhow::{Context, Result};
use serde_json::{Value, json};
use sha2::{Digest, Sha256};

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
    default_semantic_ratio: f32,
}

impl MemoryService {
    pub fn new(client: MeiliClient, default_semantic_ratio: f32) -> Self {
        Self {
            client,
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
            scope: req.scope.unwrap_or_else(|| "global".to_string()),
            source: source.as_str().to_string(),
            source_path: req.source_path,
            source_client: req.source_client,
            created_at: now,
            updated_at: now,
            last_accessed_at: None,
            content_hash: hash,
        };
        self.client.upsert(&item).await?;
        Ok(id)
    }

    /// Build the document for a crawled file, or `None` if it is unchanged
    /// (same content hash) and therefore needs no re-embedding. Preserves the
    /// original `created_at` when the document already exists.
    pub async fn prepare_crawled(
        &self,
        source_path: &str,
        content: String,
        ty: MemoryType,
        scope: String,
        title: Option<String>,
    ) -> Result<Option<MemoryItem>> {
        let id = path_id(source_path);
        let hash = content_hash(&content);
        let now = now_secs();

        let mut created_at = now;
        if let Some(existing) = self.client.get_doc(&id).await? {
            if existing.get("content_hash").and_then(|h| h.as_str()) == Some(hash.as_str()) {
                return Ok(None);
            }
            if let Some(c) = existing.get("created_at").and_then(|c| c.as_i64()) {
                created_at = c;
            }
        }

        Ok(Some(MemoryItem {
            id,
            content,
            title: title.or_else(|| Some(file_title(source_path))),
            summary: None,
            r#type: ty.to_string(),
            tags: vec![],
            scope,
            source: Source::Crawler.as_str().to_string(),
            source_path: Some(source_path.to_string()),
            source_client: None,
            created_at,
            updated_at: now,
            last_accessed_at: None,
            content_hash: hash,
        }))
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
        match self
            .prepare_crawled(source_path, content, ty, scope, title)
            .await?
        {
            Some(item) => {
                self.client.upsert(&item).await?;
                Ok(true)
            }
            None => Ok(false),
        }
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

        let filters = build_filters(&req);
        if !filters.is_empty() {
            body["filter"] = Value::String(filters.join(" AND "));
        }

        let result = self.run_query(&body, opts).await?;
        self.bump_accessed(&result.hits).await;
        Ok(result)
    }

    /// Fetch the full document for a memory by id (the end of the funnel).
    pub async fn read(&self, id: &str) -> Result<Option<MemoryItem>> {
        match self.client.get_doc(id).await? {
            Some(doc) => {
                // Bump last-accessed; tolerate failure.
                let _ = self
                    .client
                    .upsert(&json!({ "id": id, "last_accessed_at": now_secs() }))
                    .await;
                Ok(serde_json::from_value(doc).ok())
            }
            None => Ok(None),
        }
    }

    /// Delete a memory by id. Returns whether something was deleted.
    pub async fn forget(&self, id: &str) -> Result<bool> {
        self.client.delete_doc(id).await
    }

    /// Update fields on an existing memory (partial). Returns false if absent.
    pub async fn update(
        &self,
        id: &str,
        content: Option<String>,
        title: Option<String>,
        tags: Option<Vec<String>>,
        scope: Option<String>,
        ty: Option<MemoryType>,
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
        let filters = build_filters(&req);
        if !filters.is_empty() {
            body["filter"] = Value::String(filters.join(" AND "));
        }
        self.run_query(&body, opts).await
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
            ["type", "scope", "source"]
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
    async fn bump_accessed(&self, rows: &[Value]) {
        let now = now_secs();
        let patches: Vec<Value> = rows
            .iter()
            .filter_map(|r| r.get("id").and_then(|i| i.as_str()))
            .map(|id| json!({ "id": id, "last_accessed_at": now }))
            .collect();
        if !patches.is_empty() {
            let _ = self.client.upsert_many(&patches).await;
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

/// Title for a crawled file: its file name.
fn file_title(path: &str) -> String {
    std::path::Path::new(path)
        .file_name()
        .and_then(|n| n.to_str())
        .unwrap_or(path)
        .to_string()
}

/// Build a Meilisearch filter expression list from a get/list request.
fn build_filters(req: &GetRequest) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(ty) = req.r#type {
        f.push(format!("type = '{}'", ty));
    }
    if let Some(scope) = &req.scope {
        // Path-prefix scoping: exact match (lean path-prefix per PRD open #6).
        f.push(format!("scope = '{}'", escape(scope)));
    }
    if let Some(since) = req.since {
        f.push(format!("created_at >= {since}"));
    }
    if let Some(until) = req.until {
        f.push(format!("created_at <= {until}"));
    }
    f
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
        let f = build_filters(&req);
        assert!(f.contains(&"type = 'fact'".to_string()));
        assert!(f.contains(&"created_at >= 100".to_string()));
    }
}
