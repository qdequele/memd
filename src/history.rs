//! Memory mutation history: an audit timeline of create/update/delete events
//! (plus one rolled-up event per crawl), stored in a dedicated `memory_events`
//! Meilisearch index.
//!
//! The log is metadata-only — it records *that* a memory changed, by which
//! source and when, but never retains old content or diffs. Writes are
//! best-effort and off the critical path: a logging failure must never block
//! or fail a memory mutation.

use crate::meili::MeiliClient;
use crate::memory::Source;
use crate::memory::model::now_secs;
use anyhow::Result;
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};

/// The dedicated audit-log index.
pub const EVENTS_INDEX: &str = "memory_events";

/// What happened to a memory.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EventAction {
    Create,
    Update,
    Delete,
    /// A crawl pass — one summary event, not one per file.
    Crawl,
}

impl EventAction {
    pub fn as_str(&self) -> &'static str {
        match self {
            EventAction::Create => "create",
            EventAction::Update => "update",
            EventAction::Delete => "delete",
            EventAction::Crawl => "crawl",
        }
    }

    /// Parse a user-supplied action string (lenient).
    pub fn parse(s: &str) -> Option<EventAction> {
        match s.trim().to_lowercase().as_str() {
            "create" | "created" | "add" => Some(EventAction::Create),
            "update" | "updated" | "edit" => Some(EventAction::Update),
            "delete" | "deleted" | "forget" | "remove" => Some(EventAction::Delete),
            "crawl" | "crawled" => Some(EventAction::Crawl),
            _ => None,
        }
    }
}

/// One audit-log document, mirroring the `memory_events` index schema.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryEvent {
    pub id: String,
    pub ts: i64,
    pub action: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub memory_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub r#type: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub scope: Option<String>,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_client: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub detail: Option<String>,
}

impl MemoryEvent {
    /// A create/update/delete event affecting a single memory.
    #[allow(clippy::too_many_arguments)]
    pub fn mutation(
        action: EventAction,
        memory_id: &str,
        title: Option<String>,
        r#type: Option<String>,
        scope: Option<String>,
        source: Source,
        source_client: Option<String>,
    ) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            ts: now_secs(),
            action: action.as_str().to_string(),
            memory_id: Some(memory_id.to_string()),
            title,
            r#type,
            scope,
            source: source.as_str().to_string(),
            source_client,
            detail: None,
        }
    }

    /// A rolled-up summary event for one crawl pass.
    pub fn crawl(detail: String) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            ts: now_secs(),
            action: EventAction::Crawl.as_str().to_string(),
            memory_id: None,
            title: None,
            r#type: None,
            scope: None,
            source: Source::Crawler.as_str().to_string(),
            source_client: None,
            detail: Some(detail),
        }
    }
}

/// Filters for querying the history (most recent first).
#[derive(Debug, Clone)]
pub struct EventQuery {
    pub action: Option<EventAction>,
    pub r#type: Option<String>,
    pub scope: Option<String>,
    pub since: Option<i64>,
    pub limit: usize,
}

impl Default for EventQuery {
    fn default() -> Self {
        Self {
            action: None,
            r#type: None,
            scope: None,
            since: None,
            limit: 20,
        }
    }
}

/// The audit log: a thin wrapper over a [`MeiliClient`] bound to the
/// `memory_events` index.
#[derive(Clone)]
pub struct EventLog {
    client: MeiliClient,
}

impl EventLog {
    /// Build a log from any client (rebinds it to the events index).
    pub fn from_client(client: &MeiliClient) -> Self {
        Self {
            client: client.for_index(EVENTS_INDEX),
        }
    }

    /// Ensure the events index exists with its settings.
    pub async fn ensure(&self) -> Result<()> {
        self.client.ensure_log_index().await
    }

    /// Append one event. Best-effort: logs and swallows any error so a logging
    /// failure never affects the caller's mutation.
    pub async fn record(&self, event: MemoryEvent) {
        if let Err(e) = self.client.insert_no_wait(&[event]).await {
            tracing::debug!("recording memory event failed: {e}");
        }
    }

    /// Query the timeline, newest first. Returns raw event rows.
    pub async fn query(&self, q: &EventQuery) -> Result<Vec<Value>> {
        let mut body = json!({
            "q": "",
            "limit": q.limit.max(1),
            "sort": ["ts:desc"],
        });
        let filters = build_event_filters(q);
        if !filters.is_empty() {
            body["filter"] = Value::String(filters.join(" AND "));
        }
        let resp = self.client.search(&body).await?;
        Ok(resp
            .get("hits")
            .and_then(|h| h.as_array())
            .cloned()
            .unwrap_or_default())
    }

    /// Total number of recorded events, if the index is reachable.
    pub async fn count(&self) -> Option<u64> {
        self.client
            .stats()
            .await
            .ok()?
            .get("numberOfDocuments")
            .and_then(|n| n.as_u64())
    }
}

/// Build a Meilisearch filter expression list from an event query.
fn build_event_filters(q: &EventQuery) -> Vec<String> {
    let mut f = Vec::new();
    if let Some(action) = q.action {
        f.push(format!("action = '{}'", action.as_str()));
    }
    if let Some(ty) = &q.r#type {
        f.push(format!("type = '{}'", escape(ty)));
    }
    if let Some(scope) = &q.scope {
        f.push(format!("scope = '{}'", escape(scope)));
    }
    if let Some(since) = q.since {
        f.push(format!("ts >= {since}"));
    }
    f
}

/// Escape single quotes in a filter literal.
fn escape(s: &str) -> String {
    s.replace('\'', "\\'")
}

/// Render one event row as a single human-readable line (for `memd history`).
pub fn render_event(ev: &Value) -> String {
    let s = |k: &str| ev.get(k).and_then(|v| v.as_str()).unwrap_or("");
    let when = format_ts(ev.get("ts").and_then(|v| v.as_i64()).unwrap_or(0));
    let action = s("action");

    if action == "crawl" {
        let detail = s("detail");
        return format!("{when}  crawl   {detail}  —  crawler");
    }

    let ty = s("type");
    let ty_tag = if ty.is_empty() {
        String::new()
    } else {
        format!("[{ty}] ")
    };
    let title = {
        let t = s("title");
        if t.is_empty() { "(untitled)" } else { t }
    };
    let scope = {
        let sc = s("scope");
        if sc.is_empty() { "—" } else { sc }
    };
    format!(
        "{when}  {action:<6}  {ty_tag}{title}  —  {source} · {scope} · id {id}",
        source = s("source"),
        id = s("memory_id"),
    )
}

/// Format unix seconds as RFC3339 (falls back to the raw number).
fn format_ts(secs: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| secs.to_string())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn action_parse_is_lenient() {
        assert_eq!(EventAction::parse("created"), Some(EventAction::Create));
        assert_eq!(EventAction::parse("FORGET"), Some(EventAction::Delete));
        assert_eq!(EventAction::parse("crawl"), Some(EventAction::Crawl));
        assert_eq!(EventAction::parse("bogus"), None);
    }

    #[test]
    fn mutation_event_carries_metadata() {
        let ev = MemoryEvent::mutation(
            EventAction::Update,
            "abc",
            Some("My title".into()),
            Some("fact".into()),
            Some("global".into()),
            Source::Mcp,
            None,
        );
        assert_eq!(ev.action, "update");
        assert_eq!(ev.memory_id.as_deref(), Some("abc"));
        assert_eq!(ev.source, "mcp");
        assert!(ev.detail.is_none());
    }

    #[test]
    fn crawl_event_has_detail_and_no_memory() {
        let ev = MemoryEvent::crawl("3 indexed, 1 deleted".into());
        assert_eq!(ev.action, "crawl");
        assert_eq!(ev.source, "crawler");
        assert!(ev.memory_id.is_none());
        assert_eq!(ev.detail.as_deref(), Some("3 indexed, 1 deleted"));
    }

    #[test]
    fn builds_event_filters() {
        let q = EventQuery {
            action: Some(EventAction::Delete),
            scope: Some("global".into()),
            since: Some(100),
            ..Default::default()
        };
        let f = build_event_filters(&q);
        assert!(f.contains(&"action = 'delete'".to_string()));
        assert!(f.contains(&"scope = 'global'".to_string()));
        assert!(f.contains(&"ts >= 100".to_string()));
    }

    #[test]
    fn renders_mutation_and_crawl_lines() {
        let mutation = json!({
            "ts": 0, "action": "update", "type": "fact",
            "title": "My title", "source": "mcp", "scope": "global", "memory_id": "abc"
        });
        let line = render_event(&mutation);
        assert!(line.contains("update"));
        assert!(line.contains("[fact] My title"));
        assert!(line.contains("mcp · global · id abc"));

        let crawl = json!({ "ts": 0, "action": "crawl", "detail": "3 indexed" });
        let line = render_event(&crawl);
        assert!(line.contains("crawl"));
        assert!(line.contains("3 indexed"));
        assert!(line.contains("crawler"));
    }
}
