//! Typed relations: one document per directed edge in `memory_relations`.

use super::ident::{in_filter, is_entity_id, normalize_predicate, relation_id};
use crate::meili::MeiliClient;
use crate::memory::Source;
use anyhow::{Result, bail};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// The relations index.
pub const RELATIONS_INDEX: &str = "memory_relations";

/// Every stored field, for document fetches.
const FIELDS: &[&str] = &[
    "id",
    "subject",
    "predicate",
    "object",
    "note",
    "source",
    "source_client",
    "scope",
    "created_at",
    "updated_at",
];

/// One directed edge `subject —predicate→ object` between two entities.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct Relation {
    pub id: String,
    pub subject: String,
    pub predicate: String,
    pub object: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub note: Option<String>,
    pub source: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub source_client: Option<String>,
    pub scope: String,
    pub created_at: i64,
    pub updated_at: i64,
}

impl Relation {
    /// Build an edge between two entity ids. Normalises the predicate and
    /// rejects self-relations and endpoints that are not entity ids.
    #[allow(clippy::too_many_arguments)]
    pub fn new(
        subject: &str,
        predicate: &str,
        object: &str,
        note: Option<String>,
        source: Source,
        source_client: Option<String>,
        scope: &str,
        now: i64,
    ) -> Result<Relation> {
        if !is_entity_id(subject) || !is_entity_id(object) {
            bail!("relation endpoints must be entity ids, got `{subject}` and `{object}`");
        }
        if subject == object {
            bail!("an entity cannot relate to itself (`{subject}`)");
        }
        let predicate = normalize_predicate(predicate)?;
        Ok(Relation {
            id: relation_id(subject, &predicate, object),
            subject: subject.to_string(),
            predicate,
            object: object.to_string(),
            note: note.map(|n| n.trim().to_string()).filter(|n| !n.is_empty()),
            source: source.as_str().to_string(),
            source_client,
            scope: scope.to_string(),
            created_at: now,
            updated_at: now,
        })
    }
}

/// Storage for relations, over a client bound to [`RELATIONS_INDEX`].
#[derive(Clone)]
pub struct RelationStore {
    client: MeiliClient,
}

impl RelationStore {
    pub fn from_client(client: &MeiliClient) -> Self {
        Self {
            client: client.for_index(RELATIONS_INDEX),
        }
    }

    /// Create the index with its settings. Idempotent.
    pub async fn ensure(&self) -> Result<()> {
        self.client.ensure_relations_index().await
    }

    /// Insert or update edges, keeping the original `created_at` of edges
    /// that already exist.
    pub async fn upsert_many(&self, rels: &[Relation]) -> Result<()> {
        if rels.is_empty() {
            return Ok(());
        }
        let ids: Vec<String> = rels.iter().map(|r| r.id.clone()).collect();
        let created: HashMap<String, i64> = self
            .client
            .fetch_docs(&in_filter("id", &ids), &["id", "created_at"])
            .await
            .unwrap_or_default()
            .into_iter()
            .filter_map(|d| {
                Some((
                    d.get("id")?.as_str()?.to_string(),
                    d.get("created_at")?.as_i64()?,
                ))
            })
            .collect();
        let docs: Vec<Relation> = rels
            .iter()
            .cloned()
            .map(|mut r| {
                if let Some(c) = created.get(&r.id) {
                    r.created_at = *c;
                }
                r
            })
            .collect();
        self.client.upsert_many(&docs).await
    }

    /// Delete one edge by id. Returns whether it existed.
    pub async fn delete(&self, id: &str) -> Result<bool> {
        self.client.delete_doc(id).await
    }

    /// Every edge whose subject is one of `ids`.
    pub async fn by_subjects(&self, ids: &[String]) -> Result<Vec<Relation>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch(&in_filter("subject", ids)).await
    }

    /// Every edge whose object is one of `ids`.
    pub async fn by_objects(&self, ids: &[String]) -> Result<Vec<Relation>> {
        if ids.is_empty() {
            return Ok(Vec::new());
        }
        self.fetch(&in_filter("object", ids)).await
    }

    async fn fetch(&self, filter: &str) -> Result<Vec<Relation>> {
        Ok(self
            .client
            .fetch_docs(filter, FIELDS)
            .await?
            .into_iter()
            .filter_map(|d| serde_json::from_value(d).ok())
            .collect())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_relation_normalises_and_identifies() {
        let r = Relation::new(
            "entity_lumen",
            "Part Of",
            "entity_meilisearch-lab",
            Some("via /admin".into()),
            Source::Mcp,
            Some("claude-code".into()),
            "global",
            7,
        )
        .unwrap();
        assert_eq!(r.predicate, "part_of");
        assert_eq!(
            r.id,
            relation_id("entity_lumen", "part_of", "entity_meilisearch-lab")
        );
        assert_eq!((r.created_at, r.updated_at), (7, 7));
        assert_eq!(r.source, "mcp");
    }

    #[test]
    fn rejects_self_relations_and_non_entity_endpoints() {
        let self_loop = Relation::new(
            "entity_a",
            "uses",
            "entity_a",
            None,
            Source::Cli,
            None,
            "global",
            1,
        );
        assert!(self_loop.unwrap_err().to_string().contains("itself"));
        assert!(
            Relation::new(
                "lumen",
                "uses",
                "entity_a",
                None,
                Source::Cli,
                None,
                "global",
                1
            )
            .is_err()
        );
        assert!(
            Relation::new(
                "entity_a",
                " ",
                "entity_b",
                None,
                Source::Cli,
                None,
                "global",
                1
            )
            .is_err()
        );
    }

    #[test]
    fn note_is_omitted_when_absent() {
        let r = Relation::new(
            "entity_a",
            "uses",
            "entity_b",
            None,
            Source::Cli,
            None,
            "global",
            1,
        )
        .unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("note").is_none());
        assert!(v.get("source_client").is_none());
    }

    #[test]
    fn relation_event_describes_the_edge() {
        let r = Relation::new(
            "entity_a",
            "uses",
            "entity_b",
            Some("n".into()),
            Source::Mcp,
            None,
            "/p",
            1,
        )
        .unwrap();
        let ev = crate::history::MemoryEvent::relation(crate::history::EventAction::Relate, &r);
        assert_eq!(ev.action, "relate");
        assert_eq!(ev.memory_id.as_deref(), Some("entity_a"));
        assert_eq!(ev.title.as_deref(), Some("entity_a uses entity_b"));
        assert_eq!(ev.scope.as_deref(), Some("/p"));
        assert_eq!(
            crate::history::EventAction::parse("unrelate"),
            Some(crate::history::EventAction::Unrelate)
        );
    }
}
