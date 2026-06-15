//! Thin async Meilisearch REST client (reqwest).
//!
//! Covers exactly what the memory service needs: index + settings management,
//! document upsert/delete, search (incl. hybrid), task polling, and health.

use anyhow::{Context, Result, bail};
use serde::Serialize;
use serde_json::{Value, json};
use std::time::Duration;

pub const INDEX: &str = "memories";

#[derive(Clone)]
pub struct MeiliClient {
    http: reqwest::Client,
    base: String,
    key: String,
    /// The index this client targets. Defaults to [`INDEX`]; use
    /// [`MeiliClient::for_index`] to bind a clone to another index.
    index: String,
}

impl MeiliClient {
    pub fn new(base: impl Into<String>, key: impl Into<String>) -> Self {
        let http = reqwest::Client::builder()
            .timeout(Duration::from_secs(30))
            .build()
            .expect("reqwest client");
        Self {
            http,
            base: base.into(),
            key: key.into(),
            index: INDEX.to_string(),
        }
    }

    /// A clone of this client bound to a different index (shares the same HTTP
    /// connection pool, base URL, and key).
    pub fn for_index(&self, index: &str) -> Self {
        Self {
            index: index.to_string(),
            ..self.clone()
        }
    }

    fn url(&self, path: &str) -> String {
        format!("{}{}", self.base, path)
    }

    /// True if the instance answers `GET /health` with status available.
    pub async fn is_healthy(&self) -> bool {
        match self
            .http
            .get(self.url("/health"))
            .timeout(Duration::from_secs(2))
            .send()
            .await
        {
            Ok(resp) => resp.status().is_success(),
            Err(_) => false,
        }
    }

    /// Returns the reported Meilisearch version string (`pkgVersion`).
    pub async fn version(&self) -> Result<String> {
        let v: Value = self.get("/version").await.context("querying /version")?;
        Ok(v.get("pkgVersion")
            .and_then(|s| s.as_str())
            .unwrap_or("unknown")
            .to_string())
    }

    async fn get(&self, path: &str) -> Result<Value> {
        let resp = self
            .http
            .get(self.url(path))
            .bearer_auth(&self.key)
            .send()
            .await?;
        Self::json_or_err(resp).await
    }

    async fn post<T: Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let resp = self
            .http
            .post(self.url(path))
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await?;
        Self::json_or_err(resp).await
    }

    async fn patch<T: Serialize>(&self, path: &str, body: &T) -> Result<Value> {
        let resp = self
            .http
            .patch(self.url(path))
            .bearer_auth(&self.key)
            .json(body)
            .send()
            .await?;
        Self::json_or_err(resp).await
    }

    async fn delete(&self, path: &str) -> Result<Value> {
        let resp = self
            .http
            .delete(self.url(path))
            .bearer_auth(&self.key)
            .send()
            .await?;
        Self::json_or_err(resp).await
    }

    async fn json_or_err(resp: reqwest::Response) -> Result<Value> {
        let status = resp.status();
        let text = resp.text().await.unwrap_or_default();
        if status.is_success() {
            if text.is_empty() {
                return Ok(Value::Null);
            }
            serde_json::from_str(&text).context("parsing Meilisearch response")
        } else {
            bail!("Meilisearch error {}: {}", status, text)
        }
    }

    /// Ensure the `memories` index exists with the configured settings and
    /// local embedder. Idempotent; safe to call on every daemon start.
    pub async fn ensure_index(&self, embedder_source: &str, embedder_model: &str) -> Result<()> {
        // Best-effort: older Meilisearch (≤1.12) gated embedders behind the
        // `vectorStore` experimental feature. On modern releases vector search
        // is stable and this field is gone, so the call is rejected harmlessly —
        // we deliberately ignore the result either way.
        let _ = self
            .patch("/experimental-features", &json!({ "vectorStore": true }))
            .await;

        self.create_index().await;

        let settings = json!({
            "searchableAttributes": ["title", "content", "summary", "tags"],
            "filterableAttributes": [
                "type", "tags", "scope", "source", "source_path",
                "content_hash", "created_at", "updated_at"
            ],
            "sortableAttributes": ["created_at", "updated_at", "last_accessed_at"],
            "embedders": {
                "default": {
                    "source": embedder_source,
                    "model": embedder_model,
                    "documentTemplate": "{{doc.title}} {{doc.content}} {{doc.tags}}"
                }
            }
        });
        self.apply_settings(&settings).await
    }

    /// Ensure the `memory_events` audit-log index exists with its settings. It
    /// is a plain log — no embedder. Idempotent; safe on every daemon start.
    pub async fn ensure_log_index(&self) -> Result<()> {
        self.create_index().await;
        let settings = json!({
            "searchableAttributes": ["title", "detail", "memory_id"],
            "filterableAttributes": ["action", "type", "scope", "source", "memory_id", "ts"],
            "sortableAttributes": ["ts"],
        });
        self.apply_settings(&settings).await
    }

    /// Create `self.index` (ignoring "already exists").
    async fn create_index(&self) {
        let create = self
            .post(
                "/indexes",
                &json!({ "uid": self.index, "primaryKey": "id" }),
            )
            .await;
        if let Ok(v) = create
            && let Some(uid) = v.get("taskUid").and_then(|t| t.as_u64())
        {
            let _ = self.wait_task(uid).await; // tolerate "index already exists"
        }
    }

    /// PATCH settings onto `self.index` and wait for the task.
    async fn apply_settings(&self, settings: &Value) -> Result<()> {
        let v = self
            .patch(&format!("/indexes/{}/settings", self.index), settings)
            .await
            .context("applying index settings")?;
        if let Some(uid) = v.get("taskUid").and_then(|t| t.as_u64()) {
            self.wait_task(uid).await?;
        }
        Ok(())
    }

    /// Insert documents without waiting for the indexing task to finish. Used
    /// for the best-effort event log: eventual consistency is acceptable and
    /// we must not add task-polling latency to every memory mutation.
    pub async fn insert_no_wait<T: Serialize>(&self, docs: &[T]) -> Result<()> {
        self.post(&format!("/indexes/{}/documents", self.index), &docs)
            .await
            .map(|_| ())
    }

    /// Upsert one document and wait for the task to finish (so embeddings are
    /// computed before we return).
    pub async fn upsert<T: Serialize>(&self, doc: &T) -> Result<()> {
        let v = self
            .post(&format!("/indexes/{}/documents", self.index), &[doc])
            .await
            .context("adding document")?;
        if let Some(uid) = v.get("taskUid").and_then(|t| t.as_u64()) {
            self.wait_task(uid).await?;
        }
        Ok(())
    }

    /// Upsert many documents in a single task and wait for completion.
    pub async fn upsert_many<T: Serialize>(&self, docs: &[T]) -> Result<()> {
        let v = self
            .post(&format!("/indexes/{}/documents", self.index), &docs)
            .await
            .context("adding documents")?;
        if let Some(uid) = v.get("taskUid").and_then(|t| t.as_u64()) {
            self.wait_task(uid).await?;
        }
        Ok(())
    }

    /// Delete one document by id and wait for completion.
    pub async fn delete_doc(&self, id: &str) -> Result<bool> {
        let v = self
            .delete(&format!("/indexes/{}/documents/{id}", self.index))
            .await?;
        if let Some(uid) = v.get("taskUid").and_then(|t| t.as_u64()) {
            let task = self.wait_task(uid).await?;
            let deleted = task
                .get("details")
                .and_then(|d| d.get("deletedDocuments"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            return Ok(deleted > 0);
        }
        Ok(false)
    }

    /// Delete many documents by id in a single task. Returns how many were
    /// actually removed.
    pub async fn delete_many(&self, ids: &[String]) -> Result<usize> {
        if ids.is_empty() {
            return Ok(0);
        }
        let v = self
            .post(
                &format!("/indexes/{}/documents/delete-batch", self.index),
                &ids,
            )
            .await
            .context("batch deleting documents")?;
        if let Some(uid) = v.get("taskUid").and_then(|t| t.as_u64()) {
            let task = self.wait_task(uid).await?;
            let deleted = task
                .get("details")
                .and_then(|d| d.get("deletedDocuments"))
                .and_then(|n| n.as_u64())
                .unwrap_or(0);
            return Ok(deleted as usize);
        }
        Ok(0)
    }

    /// Fetch a single document by id, if present.
    pub async fn get_doc(&self, id: &str) -> Result<Option<Value>> {
        let resp = self
            .http
            .get(self.url(&format!("/indexes/{}/documents/{id}", self.index)))
            .bearer_auth(&self.key)
            .send()
            .await?;
        if resp.status() == reqwest::StatusCode::NOT_FOUND {
            return Ok(None);
        }
        Ok(Some(Self::json_or_err(resp).await?))
    }

    /// Run a search. `hybrid` (embedder + semantic ratio) is included when
    /// `semantic_ratio` is `Some`.
    pub async fn search(&self, body: &Value) -> Result<Value> {
        self.post(&format!("/indexes/{}/search", self.index), body)
            .await
            .context("searching index")
    }

    /// Index document/storage statistics.
    pub async fn stats(&self) -> Result<Value> {
        self.get(&format!("/indexes/{}/stats", self.index)).await
    }

    /// Trigger a dump of the whole instance and wait for it to finish.
    /// Returns the dump uid; the file is `<uid>.dump` in the dump dir.
    pub async fn create_dump(&self) -> Result<String> {
        let resp = self
            .http
            .post(self.url("/dumps"))
            .bearer_auth(&self.key)
            .send()
            .await?;
        let v = Self::json_or_err(resp).await.context("creating dump")?;
        let uid = v
            .get("taskUid")
            .and_then(|t| t.as_u64())
            .context("dump response had no taskUid")?;
        let task = self.wait_task(uid).await?;
        task.get("details")
            .and_then(|d| d.get("dumpUid"))
            .and_then(|s| s.as_str())
            .map(String::from)
            .context("dump task did not report a dumpUid")
    }

    /// Poll a task until it reaches a terminal state.
    pub async fn wait_task(&self, uid: u64) -> Result<Value> {
        for _ in 0..600 {
            let task = self.get(&format!("/tasks/{uid}")).await?;
            match task.get("status").and_then(|s| s.as_str()) {
                Some("succeeded") => return Ok(task),
                Some("failed") | Some("canceled") => {
                    bail!("Meilisearch task {} failed: {}", uid, task);
                }
                _ => tokio::time::sleep(Duration::from_millis(200)).await,
            }
        }
        bail!("Meilisearch task {} did not complete in time", uid)
    }
}
