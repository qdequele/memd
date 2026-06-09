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
}

/// Current unix time in seconds.
pub fn now_secs() -> i64 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0)
}
