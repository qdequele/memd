//! The knowledge service: entities, relations, and their links to memories.

use super::explore::{MAX_RELATED, shape};
use super::ident::in_filter;
use super::ident::{
    entity_id, is_entity_id, key_of, normalize_predicate, relation_id, slug, validate_kind,
};
use super::relations::{Relation, RelationStore};
use super::resolve::ENTITY_ROW_FIELDS;
use super::resolve::{Resolution, ambiguity_message, resolve};
use crate::history::{EventAction, MemoryEvent};
use crate::memory::model::now_secs;
use crate::memory::service::{content_hash, normalize_scope};
use crate::memory::{GetRequest, MemoryType, ProjectionOptions};
use crate::memory::{Knowledge, MemoryItem, MemoryService, Source};
use anyhow::{Result, anyhow, bail};
use serde_json::{Value, json};

/// A relation as an agent states it: names or ids for both ends.
#[derive(Debug, Clone, Default)]
pub struct RelationInput {
    /// Subject name or id. Implied (the entity itself) for `save_entity`.
    pub subject: Option<String>,
    pub predicate: String,
    pub object: String,
    pub note: Option<String>,
}

/// What a caller wants an entity to look like after `save_entity`.
#[derive(Debug, Clone)]
pub struct EntityInput {
    pub name: String,
    pub kind_of: String,
    pub description: Option<String>,
    pub aliases: Vec<String>,
    /// Owner name or id.
    pub owner: Option<String>,
    pub status: Option<String>,
    pub scope: Option<String>,
    pub tags: Vec<String>,
    pub url: Option<String>,
    pub path: Option<String>,
    pub relations: Vec<RelationInput>,
    pub source: Source,
    pub source_client: Option<String>,
}

impl EntityInput {
    pub fn new(name: &str, kind_of: &str, source: Source) -> Self {
        Self {
            name: name.trim().to_string(),
            kind_of: kind_of.to_string(),
            description: None,
            aliases: Vec::new(),
            owner: None,
            status: None,
            scope: None,
            tags: Vec::new(),
            url: None,
            path: None,
            relations: Vec::new(),
            source,
            source_client: None,
        }
    }
}

/// Result of `save_entity`.
#[derive(Debug, Clone)]
pub struct SaveEntityOutcome {
    pub id: String,
    pub created: bool,
    pub warnings: Vec<String>,
}

/// Result of [`build_entity`].
#[derive(Debug, Clone)]
pub struct BuiltEntity {
    pub item: MemoryItem,
    pub created: bool,
    pub warnings: Vec<String>,
}

/// An entity's searchable body: its description, then its aliases (which is
/// how aliases reach the embedder — see the plan's deviation 2).
pub fn entity_content(description: Option<&str>, aliases_display: &[String]) -> String {
    let mut out = description.unwrap_or("").trim().to_string();
    if !aliases_display.is_empty() {
        if !out.is_empty() {
            out.push_str("\n\n");
        }
        out.push_str("Also known as: ");
        out.push_str(&aliases_display.join(", "));
    }
    out
}

/// Build the stored document for `id` from an optional existing document and
/// the caller's input. Passed scalar fields overwrite; absent ones are kept;
/// aliases and tags are unioned; the display name never changes once set.
pub fn build_entity(
    id: &str,
    existing: Option<&MemoryItem>,
    input: &EntityInput,
    owner_id: Option<String>,
    now: i64,
) -> Result<BuiltEntity> {
    let kind = validate_kind(&input.kind_of)?;
    let mut warnings = Vec::new();
    let created = existing.is_none();
    let mut item = match existing {
        Some(e) => e.clone(),
        None => MemoryItem {
            id: id.to_string(),
            content: String::new(),
            title: Some(input.name.clone()),
            summary: None,
            r#type: "entity".to_string(),
            tags: Vec::new(),
            scope: normalize_scope(input.scope.as_deref()),
            source: input.source.as_str().to_string(),
            source_path: None,
            source_client: input.source_client.clone(),
            created_at: now,
            updated_at: now,
            last_accessed_at: None,
            content_hash: String::new(),
            knowledge: Knowledge {
                name: Some(input.name.clone()),
                name_key: Some(key_of(id).to_string()),
                ..Default::default()
            },
        },
    };

    let was_stub = item.knowledge.status.as_deref() == Some("stub");
    if let Some(old) = item.knowledge.kind_of.clone()
        && !created
        && !was_stub
        && old != kind
    {
        warnings.push(format!("kind_of changed from `{old}` to `{kind}`"));
    }
    item.knowledge.kind_of = Some(kind);

    match &input.status {
        Some(s) => item.knowledge.status = Some(s.clone()),
        None if was_stub => item.knowledge.status = None,
        None => {}
    }
    if let Some(d) = &input.description {
        item.summary = Some(d.trim().to_string()).filter(|d| !d.is_empty());
    }
    let own_key = item.knowledge.name_key.clone().unwrap_or_default();
    for alias in &input.aliases {
        let key = slug(alias);
        if key.is_empty() || key == own_key || item.knowledge.aliases.contains(&key) {
            continue;
        }
        item.knowledge.aliases.push(key);
        item.knowledge
            .aliases_display
            .push(alias.trim().to_string());
    }
    for tag in &input.tags {
        if !item.tags.contains(tag) {
            item.tags.push(tag.clone());
        }
    }
    if owner_id.is_some() {
        item.knowledge.owner = owner_id;
    }
    if input.url.is_some() {
        item.knowledge.url = input.url.clone();
    }
    if input.path.is_some() {
        item.knowledge.path = input.path.clone();
    }
    if input.scope.is_some() && !created {
        item.scope = normalize_scope(input.scope.as_deref());
    }
    item.source = input.source.as_str().to_string();
    if input.source_client.is_some() {
        item.source_client = input.source_client.clone();
    }

    item.content = entity_content(item.summary.as_deref(), &item.knowledge.aliases_display);
    // The id is part of the hash so entities never collide in content dedup.
    item.content_hash = content_hash(&format!("{}\n{}", item.id, item.content));
    item.updated_at = now;
    Ok(BuiltEntity {
        item,
        created,
        warnings,
    })
}

/// Entities, relations, and their links to memories.
#[derive(Clone)]
pub struct KnowledgeService {
    mem: MemoryService,
    rel: RelationStore,
}

impl KnowledgeService {
    pub fn new(mem: MemoryService) -> Self {
        let rel = RelationStore::from_client(mem.client());
        Self { mem, rel }
    }

    pub fn memories(&self) -> &MemoryService {
        &self.mem
    }

    pub fn relations(&self) -> &RelationStore {
        &self.rel
    }

    /// Resolve a name to an entity id, creating a stub when nothing matches.
    pub async fn resolve_or_stub(
        &self,
        name: &str,
        scope: &str,
        source: Source,
        client: Option<String>,
    ) -> Result<String> {
        match resolve(&self.mem, name, Some(scope)).await? {
            Resolution::Found(id) => Ok(id),
            Resolution::Ambiguous(c) => bail!(ambiguity_message(name, &c)),
            Resolution::NotFound => {
                if is_entity_id(name.trim()) {
                    bail!("no entity with id `{}`", name.trim());
                }
                let id = entity_id(name)?;
                let mut stub = EntityInput::new(name, "concept", source);
                stub.status = Some("stub".into());
                stub.scope = Some(scope.to_string());
                stub.source_client = client;
                let built = build_entity(&id, None, &stub, None, now_secs())?;
                self.mem.put_entity(&built.item, true).await?;
                Ok(id)
            }
        }
    }

    /// Create or update an entity, then add its relations (best-effort).
    pub async fn save_entity(&self, input: EntityInput) -> Result<SaveEntityOutcome> {
        let target = match resolve(&self.mem, &input.name, input.scope.as_deref()).await? {
            Resolution::Found(id) => id,
            Resolution::NotFound => entity_id(&input.name)?,
            Resolution::Ambiguous(c) => bail!(ambiguity_message(&input.name, &c)),
        };
        let existing = self.mem.get_item(&target).await?;
        let scope = match (&input.scope, &existing) {
            (Some(s), _) => normalize_scope(Some(s)),
            (None, Some(e)) => e.scope.clone(),
            (None, None) => "global".to_string(),
        };
        let owner = match &input.owner {
            Some(o) => Some(
                self.resolve_or_stub(o, &scope, input.source, input.source_client.clone())
                    .await?,
            ),
            None => None,
        };
        let built = build_entity(&target, existing.as_ref(), &input, owner, now_secs())?;
        self.mem.put_entity(&built.item, built.created).await?;

        let rels: Vec<RelationInput> = input
            .relations
            .iter()
            .map(|r| RelationInput {
                subject: Some(target.clone()),
                ..r.clone()
            })
            .collect();
        let mut warnings = built.warnings;
        warnings.extend(
            self.relate(&rels, &scope, input.source, input.source_client.clone())
                .await,
        );
        Ok(SaveEntityOutcome {
            id: target,
            created: built.created,
            warnings,
        })
    }

    /// Store relations, resolving (or stubbing) both ends. Never fails: each
    /// problem becomes a warning string.
    pub async fn relate(
        &self,
        rels: &[RelationInput],
        scope: &str,
        source: Source,
        client: Option<String>,
    ) -> Vec<String> {
        let mut warnings = Vec::new();
        let mut ok = Vec::new();
        let now = now_secs();
        for r in rels {
            let built: Result<Relation> = async {
                let subject = r.subject.as_deref().ok_or_else(|| anyhow!("no subject"))?;
                let s = self
                    .resolve_or_stub(subject, scope, source, client.clone())
                    .await?;
                let o = self
                    .resolve_or_stub(&r.object, scope, source, client.clone())
                    .await?;
                Relation::new(
                    &s,
                    &r.predicate,
                    &o,
                    r.note.clone(),
                    source,
                    client.clone(),
                    scope,
                    now,
                )
            }
            .await;
            match built {
                Ok(rel) => ok.push(rel),
                Err(e) => warnings.push(format!(
                    "relation `{} {} {}` skipped: {e}",
                    r.subject.as_deref().unwrap_or("?"),
                    r.predicate,
                    r.object
                )),
            }
        }
        if let Err(e) = self.rel.upsert_many(&ok).await {
            tracing::warn!("storing {} relation(s) failed: {e}", ok.len());
            warnings.push(format!("{} relation(s) not stored: {e}", ok.len()));
            return warnings;
        }
        for rel in &ok {
            self.mem
                .events()
                .record(MemoryEvent::relation(EventAction::Relate, rel))
                .await;
        }
        warnings
    }

    /// Everything memd knows about one entity. Depth is clamped to 1..=2 and
    /// the memory limit to 1..=50.
    pub async fn explore(
        &self,
        name: &str,
        depth: u8,
        limit: usize,
        scope: Option<&str>,
    ) -> Result<Value> {
        let depth = depth.clamp(1, 2);
        let limit = limit.clamp(1, 50);
        let id = match resolve(&self.mem, name, scope).await? {
            Resolution::Found(id) => id,
            Resolution::Ambiguous(c) => bail!(ambiguity_message(name, &c)),
            Resolution::NotFound => {
                let req = GetRequest {
                    query: name.to_string(),
                    limit: Some(5),
                    r#type: Some(MemoryType::Entity),
                    ..Default::default()
                };
                let hits = self
                    .mem
                    .get(req, &ProjectionOptions::search_default())
                    .await
                    .map(|r| r.hits)
                    .unwrap_or_default();
                return Ok(json!({ "entity": Value::Null, "suggestions": hits }));
            }
        };

        let entity = self
            .mem
            .get_item(&id)
            .await?
            .ok_or_else(|| anyhow!("entity `{id}` disappeared"))?;
        let one = std::slice::from_ref(&id);
        let outgoing = self.rel.by_subjects(one).await?;
        let incoming = self.rel.by_objects(one).await?;

        let mut neighbour_ids: Vec<String> = outgoing
            .iter()
            .map(|r| r.object.clone())
            .chain(incoming.iter().map(|r| r.subject.clone()))
            .chain(entity.knowledge.owner.clone())
            .filter(|n| n != &id)
            .collect();
        neighbour_ids.sort();
        neighbour_ids.dedup();
        neighbour_ids.truncate(MAX_RELATED);

        let related = if neighbour_ids.is_empty() {
            Vec::new()
        } else {
            self.mem
                .client()
                .fetch_docs(&in_filter("id", &neighbour_ids), ENTITY_ROW_FIELDS)
                .await?
        };

        let memories = self
            .mem
            .list_with(
                &GetRequest {
                    entity: Some(id.clone()),
                    extra_filters: vec!["type != 'entity'".to_string()],
                    ..Default::default()
                },
                limit,
                &ProjectionOptions {
                    include_content: false,
                    crop_length: Some(40),
                    highlight: false,
                    facets: Vec::new(),
                },
            )
            .await?
            .hits;

        let second: Option<Vec<Relation>> = if depth == 2 && !neighbour_ids.is_empty() {
            let mut edges = self.rel.by_subjects(&neighbour_ids).await?;
            edges.extend(self.rel.by_objects(&neighbour_ids).await?);
            edges.sort_by(|a, b| a.id.cmp(&b.id));
            edges.dedup_by(|a, b| a.id == b.id);
            Some(edges)
        } else {
            None
        };

        Ok(shape(
            serde_json::to_value(&entity)?,
            &outgoing,
            &incoming,
            related,
            memories,
            second.as_deref(),
        ))
    }

    /// Delete one relation, by names or ids. Unknown endpoints mean there is
    /// nothing to delete.
    pub async fn forget_relation(
        &self,
        subject: &str,
        predicate: &str,
        object: &str,
    ) -> Result<bool> {
        let s = match resolve(&self.mem, subject, None).await? {
            Resolution::Found(id) => id,
            Resolution::NotFound => return Ok(false),
            Resolution::Ambiguous(c) => bail!(ambiguity_message(subject, &c)),
        };
        let o = match resolve(&self.mem, object, None).await? {
            Resolution::Found(id) => id,
            Resolution::NotFound => return Ok(false),
            Resolution::Ambiguous(c) => bail!(ambiguity_message(object, &c)),
        };
        let p = normalize_predicate(predicate)?;
        let id = relation_id(&s, &p, &o);
        let existing = self.rel.by_subjects(std::slice::from_ref(&s)).await?;
        let deleted = self.rel.delete(&id).await?;
        if deleted && let Some(rel) = existing.iter().find(|r| r.id == id) {
            self.mem
                .events()
                .record(MemoryEvent::relation(EventAction::Unrelate, rel))
                .await;
        }
        Ok(deleted)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn input(name: &str) -> EntityInput {
        EntityInput::new(name, "product", Source::Mcp)
    }

    #[test]
    fn new_entity_carries_identity_and_derived_content() {
        let mut i = input("Lumen");
        i.description = Some("LLM gateway".into());
        i.aliases = vec!["lumen-gw".into()];
        let b = build_entity("entity_lumen", None, &i, None, 10).unwrap();
        assert!(b.created);
        let it = &b.item;
        assert_eq!(it.r#type, "entity");
        assert_eq!(it.title.as_deref(), Some("Lumen"));
        assert_eq!(it.knowledge.name.as_deref(), Some("Lumen"));
        assert_eq!(it.knowledge.name_key.as_deref(), Some("lumen"));
        assert_eq!(it.knowledge.kind_of.as_deref(), Some("product"));
        assert_eq!(it.knowledge.aliases, vec!["lumen-gw"]);
        assert_eq!(it.summary.as_deref(), Some("LLM gateway"));
        assert_eq!(it.content, "LLM gateway\n\nAlso known as: lumen-gw");
        assert_eq!(it.scope, "global");
    }

    #[test]
    fn empty_entities_never_share_a_content_hash() {
        // Review focus 1: two stubs with empty content must stay distinct.
        let mut a = input("Alpha");
        a.status = Some("stub".into());
        let mut b = input("Beta");
        b.status = Some("stub".into());
        let ha = build_entity("entity_alpha", None, &a, None, 1)
            .unwrap()
            .item
            .content_hash;
        let hb = build_entity("entity_beta", None, &b, None, 1)
            .unwrap()
            .item
            .content_hash;
        assert_ne!(ha, hb);
    }

    #[test]
    fn merge_preserves_unpassed_fields_and_unions_lists() {
        let mut first = input("Lumen");
        first.description = Some("old".into());
        first.aliases = vec!["a".into()];
        first.tags = vec!["x".into()];
        let existing = build_entity("entity_lumen", None, &first, None, 1)
            .unwrap()
            .item;

        let mut second = input("lumen");
        second.aliases = vec!["B".into(), "a".into(), "Lumen".into()];
        second.tags = vec!["x".into(), "y".into()];
        let b = build_entity("entity_lumen", Some(&existing), &second, None, 2).unwrap();
        assert!(!b.created);
        let it = &b.item;
        assert_eq!(
            it.summary.as_deref(),
            Some("old"),
            "description not passed → kept"
        );
        assert_eq!(
            it.knowledge.name.as_deref(),
            Some("Lumen"),
            "display name kept"
        );
        assert_eq!(
            it.knowledge.aliases,
            vec!["a", "b"],
            "own name skipped, duplicates dropped"
        );
        assert_eq!(it.tags, vec!["x", "y"]);
        assert_eq!(it.created_at, 1);
        assert_eq!(it.updated_at, 2);

        let mut third = input("Lumen");
        third.description = Some("new".into());
        let it = build_entity("entity_lumen", Some(&b.item), &third, None, 3)
            .unwrap()
            .item;
        assert!(it.content.starts_with("new"));
    }

    #[test]
    fn stubs_are_cleared_and_kind_changes_warn() {
        let mut stub = EntityInput::new("Quentin", "concept", Source::Mcp);
        stub.status = Some("stub".into());
        let s = build_entity("entity_quentin", None, &stub, None, 1)
            .unwrap()
            .item;

        let filled = build_entity(
            "entity_quentin",
            Some(&s),
            &EntityInput::new("Quentin", "person", Source::Mcp),
            None,
            2,
        )
        .unwrap();
        assert_eq!(filled.item.knowledge.status, None, "stub status cleared");
        assert!(
            filled.warnings.is_empty(),
            "filling a stub's kind is not a change"
        );

        let changed = build_entity(
            "entity_quentin",
            Some(&filled.item),
            &EntityInput::new("Quentin", "team", Source::Mcp),
            None,
            3,
        )
        .unwrap();
        assert_eq!(changed.item.knowledge.kind_of.as_deref(), Some("team"));
        assert_eq!(changed.warnings.len(), 1);

        let mut keep = EntityInput::new("Quentin", "team", Source::Mcp);
        keep.status = Some("active".into());
        let kept = build_entity("entity_quentin", Some(&changed.item), &keep, None, 4).unwrap();
        assert_eq!(kept.item.knowledge.status.as_deref(), Some("active"));
    }

    #[test]
    fn invalid_kind_is_rejected() {
        let err = build_entity(
            "entity_x",
            None,
            &EntityInput::new("X", "planet", Source::Cli),
            None,
            1,
        )
        .unwrap_err()
        .to_string();
        assert!(err.contains("unknown kind_of"), "{err}");
    }

    #[test]
    fn agent_write_takes_ownership_of_a_crawled_entity() {
        let mut crawled = EntityInput::new("memd", "project", Source::Crawler);
        crawled.path = Some("/p/memd".into());
        let c = build_entity("entity_memd", None, &crawled, None, 1)
            .unwrap()
            .item;
        assert_eq!(c.source, "crawler");
        let it = build_entity(
            "entity_memd",
            Some(&c),
            &EntityInput::new("memd", "project", Source::Mcp),
            None,
            2,
        )
        .unwrap()
        .item;
        assert_eq!(it.source, "mcp");
        assert_eq!(it.knowledge.path.as_deref(), Some("/p/memd"), "path kept");
        assert_eq!(Source::parse(&c.source), Some(Source::Crawler));
        assert_eq!(Source::parse("nope"), None);
    }
}
