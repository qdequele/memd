//! The `MemoryItem` — the atomic unit stored as one Meilisearch document.

use serde::{Deserialize, Serialize};
use std::fmt;
use std::time::{SystemTime, UNIX_EPOCH};

/// Classification of a memory. Inferred heuristically when not supplied.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum MemoryType {
    Fact,
    Preference,
    Decision,
    Task,
    ProjectOverview,
    AgentInstruction,
    FileAnnotation,
    CodeNote,
    Reference,
    Entity,
}

impl MemoryType {
    pub fn as_str(&self) -> &'static str {
        match self {
            MemoryType::Fact => "fact",
            MemoryType::Preference => "preference",
            MemoryType::Decision => "decision",
            MemoryType::Task => "task",
            MemoryType::ProjectOverview => "project_overview",
            MemoryType::AgentInstruction => "agent_instruction",
            MemoryType::FileAnnotation => "file_annotation",
            MemoryType::CodeNote => "code_note",
            MemoryType::Reference => "reference",
            MemoryType::Entity => "entity",
        }
    }

    /// Parse a user-supplied type string (lenient).
    pub fn parse(s: &str) -> Option<MemoryType> {
        match s.trim().to_lowercase().as_str() {
            "fact" => Some(MemoryType::Fact),
            "preference" | "pref" => Some(MemoryType::Preference),
            "decision" => Some(MemoryType::Decision),
            "task" | "todo" => Some(MemoryType::Task),
            "project_overview" | "project" | "overview" => Some(MemoryType::ProjectOverview),
            "agent_instruction" | "agent" | "instruction" => Some(MemoryType::AgentInstruction),
            "file_annotation" | "annotation" => Some(MemoryType::FileAnnotation),
            "code_note" | "code" | "note" => Some(MemoryType::CodeNote),
            "reference" | "ref" => Some(MemoryType::Reference),
            "entity" => Some(MemoryType::Entity),
            _ => None,
        }
    }
}

impl fmt::Display for MemoryType {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.write_str(self.as_str())
    }
}

/// Where a memory originated.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum Source {
    /// An agent wrote it through the MCP server.
    Mcp,
    /// Derived from a file by the crawler.
    Crawler,
    /// Entered manually via the CLI.
    Cli,
}

impl Source {
    pub fn as_str(&self) -> &'static str {
        match self {
            Source::Mcp => "mcp",
            Source::Crawler => "crawler",
            Source::Cli => "cli",
        }
    }
}

/// Structured knowledge carried by a memory. Flattened into the document, so
/// each field is a top-level, filterable attribute. Every field is optional
/// and omitted when empty, so plain memories are unchanged on the wire.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct Knowledge {
    /// Entity display name.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
    /// Entity identity key (the id without its `entity_` prefix).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name_key: Option<String>,
    /// Alias keys (slugs), for resolution.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases: Vec<String>,
    /// Aliases as typed, for display and keyword search.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub aliases_display: Vec<String>,
    /// Entity kind (see `knowledge::ident::KINDS`).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub kind_of: Option<String>,
    /// Lifecycle status (entity, decision, task).
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub status: Option<String>,
    /// Owning entity id.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub owner: Option<String>,
    /// Memory id this decision supersedes.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub supersedes: Option<String>,
    /// Directory of a project entity.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub path: Option<String>,
    /// Home page, repository or dashboard.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub url: Option<String>,
    /// Entity ids this record mentions or belongs to.
    #[serde(default, skip_serializing_if = "Vec::is_empty")]
    pub entities: Vec<String>,
}

/// One memory document, mirroring the Meilisearch `memories` index schema.
///
/// `_vectors` is intentionally absent: embeddings are computed and owned by
/// Meilisearch's local embedder; memd only sends the textual fields.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct MemoryItem {
    pub id: String,
    pub content: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub title: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub summary: Option<String>,
    pub r#type: String,
    pub tags: Vec<String>,
    pub scope: String,
    pub source: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_path: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub source_client: Option<String>,
    pub created_at: i64,
    pub updated_at: i64,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub last_accessed_at: Option<i64>,
    pub content_hash: String,
    /// Structured knowledge fields (flattened into the document).
    #[serde(flatten)]
    pub knowledge: Knowledge,
}

/// Current unix time in seconds.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn sample() -> MemoryItem {
        MemoryItem {
            id: "m1".into(),
            content: "c".into(),
            title: Some("t".into()),
            summary: None,
            r#type: "fact".into(),
            tags: vec![],
            scope: "global".into(),
            source: "mcp".into(),
            source_path: None,
            source_client: None,
            created_at: 1,
            updated_at: 1,
            last_accessed_at: None,
            content_hash: "h".into(),
            knowledge: Knowledge::default(),
        }
    }

    #[test]
    fn knowledge_fields_are_omitted_when_empty() {
        let v = serde_json::to_value(sample()).unwrap();
        for k in [
            "name",
            "name_key",
            "aliases",
            "aliases_display",
            "kind_of",
            "status",
            "owner",
            "supersedes",
            "path",
            "url",
            "entities",
            "knowledge",
        ] {
            assert!(v.get(k).is_none(), "{k} should be absent");
        }
    }

    #[test]
    fn knowledge_fields_round_trip_flattened() {
        let mut item = sample();
        item.knowledge.kind_of = Some("project".into());
        item.knowledge.entities = vec!["entity_memd".into()];
        let v = serde_json::to_value(&item).unwrap();
        assert_eq!(v["kind_of"], "project");
        assert_eq!(v["entities"][0], "entity_memd");
        let back: MemoryItem = serde_json::from_value(v).unwrap();
        assert_eq!(back.knowledge, item.knowledge);
        // Documents written before this change still deserialize.
        let old = serde_json::json!({
            "id": "m", "content": "c", "type": "fact", "tags": [], "scope": "global",
            "source": "mcp", "created_at": 1, "updated_at": 1, "content_hash": "h"
        });
        let parsed: MemoryItem = serde_json::from_value(old).unwrap();
        assert_eq!(parsed.knowledge, Knowledge::default());
    }

    #[test]
    fn entity_type_parses() {
        assert_eq!(MemoryType::parse("entity"), Some(MemoryType::Entity));
        assert_eq!(MemoryType::Entity.as_str(), "entity");
    }
}
