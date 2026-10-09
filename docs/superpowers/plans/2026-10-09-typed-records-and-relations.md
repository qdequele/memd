# Typed Records and Relations Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Let memd store entities (companies, teams, people, projects, products, services, customers, concepts) and typed relations between them, so any agent can `explore("Lumen")` and get a graph-shaped answer.

**Architecture:** Entities are ordinary documents in the `memories` index with `type = entity` and a flattened set of optional knowledge fields. Relations are one document per directed edge in a new `memory_relations` index. A new `src/knowledge/` module owns identity, resolution, relation storage and the `explore` query; the MCP server, CLI and crawler call into it. memd stays model-free: agents supply structure, the crawler adds project entities from disk.

**Tech Stack:** Rust 2024, tokio, serde_json, reqwest against a local Meilisearch 1.54, `unicode-normalization` (new).

**Spec:** `docs/superpowers/specs/2026-10-08-typed-records-and-relations-design.md`

## Deviations from the spec (forced by the engine or by identity rules)

1. **Entity ids use `entity_` not `entity:`.** Meilisearch document ids may only contain `A-Z a-z 0-9 - _`. Ids are `entity_<slug>`, and the slug is ASCII-only. A name with no ASCII letters or digits but other letters (e.g. `東京`) gets `entity_x<12 hex of sha256>`.
2. **The embedder document template is not changed.** On 2026-10-08 the engine failed every task whose document lacked a field named in the template (`liquid: Unknown index … requested index=title`). Adding `{{doc.aliases_display}}` would break every existing document. Instead, an entity's `content` is its description plus an `Also known as: …` line, so aliases are embedded through `content`, and `aliases_display` is added to the keyword-searchable attributes.
3. **Resolution is id-first and global.** Ids are global (§2.1), so restricting name lookup to the scope chain (§4) would create a stub that overwrites an entity defined in another project. A name resolves to its id when that document exists, anywhere. Only alias matches use the scope chain, to break ties.
4. **An entity's description lives in `summary`.** `summary` already exists and is unused. `content` is derived from `summary` plus aliases.
5. **The memories index makes `id` filterable**, so `explore` can fetch up to 50 neighbours in one request.

## Global Constraints

- Rust edition 2024; let-chains are used throughout the codebase.
- Only one new dependency: `unicode-normalization` (latest 0.1.x, added with `cargo add`).
- `cargo fmt --all -- --check`, `cargo clippy --all-targets -- -D warnings` and `cargo test` pass after every task.
- The shared `~/.cargo/registry` is corrupted for the `cc` crate on this machine. Run every cargo command with `export CARGO_HOME="$TMPDIR/memd-cargo-home" PATH="$HOME/.cargo/bin:$PATH"`. The first build re-downloads every crate.
- Commit messages carry **no** `Co-Authored-By` line (user's global CLAUDE.md).
- The MCP tool surface is a contract: changes are additive only. Existing tool names and existing parameters keep their meaning.
- Kinds (`kind_of`), exact list: `company`, `team`, `person`, `project`, `product`, `service`, `customer`, `concept`.
- Suggested predicates, exact list: `part_of`, `depends_on`, `owns`, `uses`, `replaces`, `works_on`, `member_of`, `customer_of`, `related_to`.
- Entity names longer than 200 characters are rejected. `explore` depth is clamped to 1..=2, its memory limit to 1..=50, its related entities to 50.
- Relation writes are best-effort: a failed relation never fails the memory or entity save; it becomes a string in `warnings`.

## Review Focus

1. **A stub or an entity with empty content must never hit content-hash dedup.** `MemoryService::save` returns any existing document whose content hash matches, so two empty stubs would collapse into one unrelated id. Entities are written only through `MemoryService::put_entity`, and their hash covers the id. Pinned in Task 5.
2. **Naming an entity that already exists in another project must reuse it, not stub over it.** Pinned in Task 4 (`decide` returns `Found` when the id exists, whatever the scope).
3. **An agent-edited project entity must survive the next crawl.** The crawler only writes project entities whose stored `source` is `crawler`. Pinned in Task 9.
4. **Names with no letters or digits, or longer than 200 characters, must fail with a clear message**, never produce `entity_`. Pinned in Task 1.
5. **Quotes in names (`O'Reilly`) and self-relations (`X uses X`) must not break filters or create loops.** Filter literals are escaped; a relation whose subject equals its object is rejected with a warning. Pinned in Tasks 1 and 3.

---

## File Structure

| File | Status | Responsibility |
|---|---|---|
| `src/knowledge/mod.rs` | create | Module root; re-exports. |
| `src/knowledge/ident.rs` | create | Slugs, entity ids, predicates, relation ids, kind validation, filter literals. Pure. |
| `src/knowledge/relations.rs` | create | `Relation` document and `RelationStore` over the `memory_relations` index. |
| `src/knowledge/resolve.rs` | create | Name → entity id resolution and ambiguity messages. |
| `src/knowledge/explore.rs` | create | Pure shaping of the `explore` response. |
| `src/knowledge/service.rs` | create | `KnowledgeService`: save entities, relate, forget relations, explore, link memories to projects. |
| `src/memory/model.rs` | modify | `Knowledge` fields flattened into `MemoryItem`; `MemoryType::Entity`; `Source::parse`. |
| `src/memory/service.rs` | modify | `SaveRequest.entities`, `GetRequest.{entity,status,kind_of}`, filters, `put_entity`, `get_item`, `patch`, `CrawledDoc`. |
| `src/meili/client.rs` | modify | New index settings; `ensure_relations_index`. |
| `src/history.rs` | modify | `relate` / `unrelate` actions. |
| `src/daemon.rs` | modify | Ensure the relations index at start. |
| `src/mcp/protocol.rs` | modify | 3 new tools, extended params, instructions. |
| `src/agents/directives.rs` | modify | Directive text. |
| `src/crawler/mod.rs` | modify | Project entities, `entities` on crawled docs, archive. |
| `src/cli.rs`, `src/main.rs` | modify | `entity`, `relate`, `unrelate`, search flags, context header, doctor back-fill. |
| `docs/knowledge.mdx`, `docs/mint.json`, `docs/mcp.mdx`, `README.md` | create/modify | Documentation. |

---

### Task 1: Identity primitives

**Files:**
- Create: `src/knowledge/mod.rs`, `src/knowledge/ident.rs`
- Modify: `Cargo.toml` (via `cargo add`), `src/main.rs:7-19` (module list)

**Interfaces:**
- Consumes: nothing.
- Produces (all in `crate::knowledge::ident`):
  - `pub const ENTITY_PREFIX: &str = "entity_";`
  - `pub const KINDS: &[&str]`, `pub const PREDICATES: &[&str]`, `pub const MAX_NAME_LEN: usize = 200;`
  - `pub fn slug(s: &str) -> String`
  - `pub fn entity_id(name: &str) -> anyhow::Result<String>`
  - `pub fn is_entity_id(s: &str) -> bool`
  - `pub fn key_of(id: &str) -> &str` (the id without the prefix)
  - `pub fn normalize_predicate(p: &str) -> anyhow::Result<String>`
  - `pub fn relation_id(subject: &str, predicate: &str, object: &str) -> String`
  - `pub fn validate_kind(k: &str) -> anyhow::Result<String>`
  - `pub fn filter_literal(s: &str) -> String`
  - `pub fn in_filter(field: &str, values: &[String]) -> String`

- [ ] **Step 1: Add the dependency and the module skeleton**

```bash
export CARGO_HOME="$TMPDIR/memd-cargo-home" PATH="$HOME/.cargo/bin:$PATH"
cargo add unicode-normalization@0.1
```

Create `src/knowledge/mod.rs`:

```rust
//! Structured knowledge: entities (typed records in the `memories` index) and
//! the typed relations between them (the `memory_relations` index).
//!
//! memd stays model-free: agents supply names, kinds and relations; this
//! module normalises identities, resolves names, and answers `explore`.

pub mod ident;
```

In `src/main.rs`, add after `mod history;`:

```rust
// Built up across several tasks; the allow is removed once every item is wired
// into the CLI and MCP server (Task 10).
#[allow(dead_code)]
mod knowledge;
```

- [ ] **Step 2: Write the failing tests**

Create `src/knowledge/ident.rs` with only the test module:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slug_normalises_case_accents_and_punctuation() {
        assert_eq!(slug("Meilisearch Lab"), "meilisearch-lab");
        assert_eq!(slug("meilisearch-lab"), "meilisearch-lab");
        assert_eq!(slug("  MEILISEARCH   LAB!! "), "meilisearch-lab");
        assert_eq!(slug("Crème Brûlée"), "creme-brulee");
        assert_eq!(slug("_side_projects/memd"), "side-projects-memd");
        assert_eq!(slug(&slug("Déjà Vu")), slug("Déjà Vu"));
    }

    #[test]
    fn entity_ids_are_prefixed_slugs() {
        assert_eq!(entity_id("Lumen").unwrap(), "entity_lumen");
        assert_eq!(entity_id("Meilisearch Lab").unwrap(), "entity_meilisearch-lab");
        assert!(is_entity_id("entity_lumen"));
        assert!(!is_entity_id("entity_"));
        assert!(!is_entity_id("01a0e7d4-3925-73e0-941a-db46558fe242"));
        assert_eq!(key_of("entity_lumen"), "lumen");
    }

    #[test]
    fn entity_ids_only_use_characters_meilisearch_accepts() {
        for name in ["Crème Brûlée", "O'Reilly & Co.", "東京", "a/b\\c"] {
            let id = entity_id(name).unwrap();
            assert!(
                id.chars().all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
                "{name} -> {id}"
            );
        }
        // Non-ASCII-only names hash rather than vanish.
        let tokyo = entity_id("東京").unwrap();
        assert!(tokyo.starts_with("entity_x"), "{tokyo}");
        assert_eq!(tokyo, entity_id(" 東京 ").unwrap());
    }

    #[test]
    fn entity_id_rejects_empty_and_long_names() {
        assert!(entity_id("!!!").is_err());
        assert!(entity_id("—").is_err());
        assert!(entity_id("   ").is_err());
        assert!(entity_id(&"a".repeat(201)).is_err());
        assert!(entity_id(&"a".repeat(200)).is_ok());
        let msg = entity_id("!!!").unwrap_err().to_string();
        assert!(msg.contains("no letters or digits"), "{msg}");
    }

    #[test]
    fn predicates_normalise() {
        assert_eq!(normalize_predicate("Part Of").unwrap(), "part_of");
        assert_eq!(normalize_predicate("depends-on").unwrap(), "depends_on");
        assert_eq!(normalize_predicate(" uses ").unwrap(), "uses");
        assert!(normalize_predicate("  ").is_err());
    }

    #[test]
    fn relation_ids_are_stable_and_directional() {
        let a = relation_id("entity_a", "uses", "entity_b");
        assert_eq!(a, relation_id("entity_a", "uses", "entity_b"));
        assert_ne!(a, relation_id("entity_b", "uses", "entity_a"));
        assert_eq!(a.len(), 64);
    }

    #[test]
    fn kinds_validate() {
        assert_eq!(validate_kind(" Project ").unwrap(), "project");
        let err = validate_kind("planet").unwrap_err().to_string();
        assert!(err.contains("company") && err.contains("concept"), "{err}");
    }

    #[test]
    fn filter_literals_escape_quotes_and_backslashes() {
        assert_eq!(filter_literal("O'Reilly"), "'O\\'Reilly'");
        assert_eq!(filter_literal("a\\b"), "'a\\\\b'");
        assert_eq!(
            in_filter("id", &["entity_a".into(), "it's".into()]),
            "id IN ['entity_a', 'it\\'s']"
        );
    }
}
```

In `src/knowledge/mod.rs` the `pub mod ident;` line is already present.

- [ ] **Step 3: Run the tests to verify they fail**

Run: `cargo test knowledge::ident`
Expected: compile errors, `cannot find function slug in this scope` (and the other functions).

- [ ] **Step 4: Implement**

Insert above the test module in `src/knowledge/ident.rs`:

```rust
//! Identity rules for entities and relations. Pure functions, no I/O.
//!
//! Meilisearch document ids may only contain `A-Z a-z 0-9 - _`, so entity ids
//! are `entity_<ascii slug>`.

use anyhow::{Result, bail};
use sha2::{Digest, Sha256};
use unicode_normalization::UnicodeNormalization;
use unicode_normalization::char::is_combining_mark;

/// Prefix of every entity id.
pub const ENTITY_PREFIX: &str = "entity_";

/// Allowed values of `kind_of`.
pub const KINDS: &[&str] = &[
    "company", "team", "person", "project", "product", "service", "customer", "concept",
];

/// Suggested relation predicates (advertised to agents, not enforced).
pub const PREDICATES: &[&str] = &[
    "part_of",
    "depends_on",
    "owns",
    "uses",
    "replaces",
    "works_on",
    "member_of",
    "customer_of",
    "related_to",
];

/// Longest accepted entity name, in characters.
pub const MAX_NAME_LEN: usize = 200;

/// Lowercase ASCII slug: accents stripped (NFKD), every run of other
/// characters collapsed to a single `-`, no leading or trailing `-`.
pub fn slug(s: &str) -> String {
    let mut out = String::new();
    let mut dash = false;
    for c in s
        .nfkd()
        .filter(|c| !is_combining_mark(*c))
        .flat_map(char::to_lowercase)
    {
        if c.is_ascii_alphanumeric() {
            out.push(c);
            dash = false;
        } else if !out.is_empty() && !dash {
            out.push('-');
            dash = true;
        }
    }
    while out.ends_with('-') {
        out.pop();
    }
    out
}

/// The entity id for a display name. Errors on names that are too long or
/// contain no letters or digits at all.
pub fn entity_id(name: &str) -> Result<String> {
    let trimmed = name.trim();
    if trimmed.chars().count() > MAX_NAME_LEN {
        bail!("entity name is longer than {MAX_NAME_LEN} characters");
    }
    let key = slug(trimmed);
    if !key.is_empty() {
        return Ok(format!("{ENTITY_PREFIX}{key}"));
    }
    if trimmed.chars().any(char::is_alphanumeric) {
        // Letters outside ASCII only (e.g. CJK): a stable hash keeps the id
        // valid for Meilisearch without losing the entity.
        let normalised: String = trimmed.nfkc().flat_map(char::to_lowercase).collect();
        let digest = format!("{:x}", Sha256::digest(normalised.as_bytes()));
        return Ok(format!("{ENTITY_PREFIX}x{}", &digest[..12]));
    }
    bail!("entity name `{name}` has no letters or digits")
}

/// True for strings shaped like an entity id.
pub fn is_entity_id(s: &str) -> bool {
    s.len() > ENTITY_PREFIX.len() && s.starts_with(ENTITY_PREFIX)
}

/// The identity key of an entity id (the id without its prefix).
pub fn key_of(id: &str) -> &str {
    id.strip_prefix(ENTITY_PREFIX).unwrap_or(id)
}

/// Normalise a relation predicate: lowercase, runs of other characters → `_`.
pub fn normalize_predicate(p: &str) -> Result<String> {
    let mut out = String::new();
    let mut sep = false;
    for c in p.trim().chars().flat_map(char::to_lowercase) {
        if c.is_alphanumeric() {
            out.push(c);
            sep = false;
        } else if !out.is_empty() && !sep {
            out.push('_');
            sep = true;
        }
    }
    while out.ends_with('_') {
        out.pop();
    }
    if out.is_empty() {
        bail!("relation predicate is empty");
    }
    Ok(out)
}

/// Stable id of a directed edge.
pub fn relation_id(subject: &str, predicate: &str, object: &str) -> String {
    let mut h = Sha256::new();
    h.update(subject.as_bytes());
    h.update([0]);
    h.update(predicate.as_bytes());
    h.update([0]);
    h.update(object.as_bytes());
    format!("{:x}", h.finalize())
}

/// Validate and normalise a `kind_of` value.
pub fn validate_kind(k: &str) -> Result<String> {
    let k = k.trim().to_lowercase();
    if KINDS.contains(&k.as_str()) {
        Ok(k)
    } else {
        bail!("unknown kind_of `{k}` (expected one of: {})", KINDS.join(", "))
    }
}

/// A single-quoted Meilisearch filter literal with quotes and backslashes escaped.
pub fn filter_literal(s: &str) -> String {
    format!("'{}'", s.replace('\\', "\\\\").replace('\'', "\\'"))
}

/// `field IN ['a', 'b']`.
pub fn in_filter(field: &str, values: &[String]) -> String {
    let list: Vec<String> = values.iter().map(|v| filter_literal(v)).collect();
    format!("{field} IN [{}]", list.join(", "))
}
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test knowledge::ident`
Expected: `test result: ok. 8 passed`.

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add Cargo.toml Cargo.lock src/main.rs src/knowledge/
git commit -m "feat(knowledge): identity primitives for entities and relations"
```

---

### Task 2: Knowledge fields on memories, entity type, filters, index settings

**Files:**
- Modify: `src/memory/model.rs`, `src/memory/service.rs`, `src/meili/client.rs` (`ensure_index`), `src/mcp/protocol.rs` (struct literals), `src/cli.rs` (struct literals)

**Interfaces:**
- Consumes: nothing from Task 1.
- Produces:
  - `crate::memory::model::Knowledge` (all fields `pub`): `name: Option<String>`, `name_key: Option<String>`, `aliases: Vec<String>`, `aliases_display: Vec<String>`, `kind_of: Option<String>`, `status: Option<String>`, `owner: Option<String>`, `supersedes: Option<String>`, `path: Option<String>`, `url: Option<String>`, `entities: Vec<String>`. Derives `Debug, Clone, Default, PartialEq, Serialize, Deserialize`.
  - `MemoryItem.knowledge: Knowledge` (`#[serde(flatten)]`).
  - `MemoryType::Entity` (`as_str` → `"entity"`).
  - `SaveRequest.entities: Vec<String>` (already-resolved entity ids).
  - `GetRequest.entity: Option<String>` (an entity id), `GetRequest.status: Option<String>`, `GetRequest.kind_of: Option<String>`.
  - `pub use model::Knowledge` from `crate::memory`.

- [ ] **Step 1: Write the failing tests**

Append to the `tests` module of `src/memory/model.rs` (create `#[cfg(test)] mod tests { use super::*; }` at the end of the file if it does not exist):

```rust
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
            "name", "name_key", "aliases", "aliases_display", "kind_of", "status", "owner",
            "supersedes", "path", "url", "entities", "knowledge",
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
```

Append to the `tests` module of `src/memory/service.rs`:

```rust
    #[test]
    fn builds_knowledge_filters() {
        let req = GetRequest {
            entity: Some("entity_lumen".into()),
            status: Some("accepted".into()),
            kind_of: Some("project".into()),
            ..Default::default()
        };
        let f = build_filters(&req, true);
        assert!(f.contains(&"entities = 'entity_lumen'".to_string()), "{f:?}");
        assert!(f.contains(&"status = 'accepted'".to_string()), "{f:?}");
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
```

Also, in the existing test `prepare_crawled_skips_unchanged_and_keeps_created_at`, nothing changes yet.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test memory::`
Expected: compile errors (`Knowledge` not found, no field `entity` on `GetRequest`).

- [ ] **Step 3: Implement the model**

In `src/memory/model.rs`:

Add the `Entity` variant at the end of `MemoryType`, its `as_str` arm `MemoryType::Entity => "entity",` and its `parse` arm `"entity" => Some(MemoryType::Entity),`.

Add above `MemoryItem`:

```rust
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
```

Add as the last field of `MemoryItem`:

```rust
    /// Structured knowledge fields (flattened into the document).
    #[serde(flatten)]
    pub knowledge: Knowledge,
```

In `src/memory/mod.rs` change the model re-export to `pub use model::{Knowledge, MemoryItem, MemoryType, Source};`.

- [ ] **Step 4: Implement the service changes**

In `src/memory/service.rs`:

1. Add to `SaveRequest`, after `source_client`:

```rust
    /// Entity ids this memory mentions (already resolved by the caller).
    pub entities: Vec<String>,
```

2. Add to `GetRequest`, after `extra_filters`:

```rust
    /// Only memories that mention this entity id.
    pub entity: Option<String>,
    /// Only records with this `status`.
    pub status: Option<String>,
    /// Only entities of this kind.
    pub kind_of: Option<String>,
```

3. In `save`, the `MemoryItem` literal gains `knowledge: Knowledge { entities: req.entities, ..Default::default() },` and the `use super::model::{…}` line gains `Knowledge`.

4. In `prepare_crawled`, the `MemoryItem` literal gains `knowledge: Knowledge::default(),` (Task 9 replaces this).

5. In `build_filters`, before `f.extend(req.extra_filters.iter().cloned());`:

```rust
    if let Some(e) = &req.entity {
        f.push(format!("entities = '{}'", escape(e)));
    }
    if let Some(s) = &req.status {
        if s == "accepted" {
            // Decisions saved before statuses existed count as accepted.
            f.push("(status = 'accepted' OR (type = 'decision' AND status NOT EXISTS))".to_string());
        } else {
            f.push(format!("status = '{}'", escape(s)));
        }
    }
    if let Some(k) = &req.kind_of {
        f.push(format!("kind_of = '{}'", escape(k)));
    }
```

6. In `stats`, the default facet list becomes `["type", "scope", "source", "kind_of", "status"]`.

7. Every `SaveRequest { … }` literal outside this file gets `entities: Vec::new(),` after `source_client`: `src/mcp/protocol.rs` (`save_memory`), `src/cli.rs` (`add`, both branches, and `capture`). Every full `GetRequest { … }` literal gets `entity: None, status: None, kind_of: None,` after `extra_filters`: `src/mcp/protocol.rs` (`get_memory`) and `src/cli.rs` (`search`). Literals that end in `..Default::default()` need no change.

- [ ] **Step 5: Index settings**

In `src/meili/client.rs`, `ensure_index`, replace the `settings` value with:

```rust
        let settings = json!({
            "searchableAttributes": ["title", "name", "aliases_display", "content", "summary", "tags"],
            "filterableAttributes": [
                "id", "type", "tags", "scope", "source", "source_path",
                "content_hash", "created_at", "updated_at",
                "name_key", "aliases", "kind_of", "status", "owner", "supersedes", "entities"
            ],
            "sortableAttributes": ["created_at", "updated_at", "last_accessed_at"],
            "embedders": {
                "default": {
                    "source": embedder_source,
                    "model": embedder_model,
                    // Unchanged on purpose: every field named here must exist
                    // on every document, or the engine fails the task.
                    "documentTemplate": "{{doc.title}} {{doc.content}} {{doc.tags}}"
                }
            }
        });
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test`
Expected: all tests pass, including the 4 new ones.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/
git commit -m "feat(memory): knowledge fields, entity type, entity/status/kind filters"
```

---

### Task 3: Relation store and relation events

**Files:**
- Create: `src/knowledge/relations.rs`
- Modify: `src/knowledge/mod.rs`, `src/meili/client.rs`, `src/history.rs`, `src/daemon.rs`

**Interfaces:**
- Consumes: `ident::{relation_id, normalize_predicate, is_entity_id, in_filter}` (Task 1); `Source` (Task 2).
- Produces:
  - `crate::knowledge::relations::RELATIONS_INDEX: &str = "memory_relations"`
  - `pub struct Relation { pub id: String, pub subject: String, pub predicate: String, pub object: String, pub note: Option<String>, pub source: String, pub source_client: Option<String>, pub scope: String, pub created_at: i64, pub updated_at: i64 }` (Serialize, Deserialize, Clone, Debug, PartialEq)
  - `Relation::new(subject: &str, predicate: &str, object: &str, note: Option<String>, source: Source, source_client: Option<String>, scope: &str, now: i64) -> anyhow::Result<Relation>`
  - `#[derive(Clone)] pub struct RelationStore` with `from_client(&MeiliClient) -> Self`, `async ensure(&self) -> Result<()>`, `async upsert_many(&self, &[Relation]) -> Result<()>`, `async delete(&self, id: &str) -> Result<bool>`, `async by_subjects(&self, &[String]) -> Result<Vec<Relation>>`, `async by_objects(&self, &[String]) -> Result<Vec<Relation>>`
  - `MeiliClient::ensure_relations_index(&self) -> Result<()>`
  - `EventAction::{Relate, Unrelate}`; `MemoryEvent::relation(action: EventAction, rel: &Relation) -> MemoryEvent`

- [ ] **Step 1: Write the failing tests**

Create `src/knowledge/relations.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn new_relation_normalises_and_identifies() {
        let r = Relation::new(
            "entity_lumen", "Part Of", "entity_meilisearch-lab",
            Some("via /admin".into()), Source::Mcp, Some("claude-code".into()), "global", 7,
        )
        .unwrap();
        assert_eq!(r.predicate, "part_of");
        assert_eq!(r.id, relation_id("entity_lumen", "part_of", "entity_meilisearch-lab"));
        assert_eq!((r.created_at, r.updated_at), (7, 7));
        assert_eq!(r.source, "mcp");
    }

    #[test]
    fn rejects_self_relations_and_non_entity_endpoints() {
        let self_loop = Relation::new("entity_a", "uses", "entity_a", None, Source::Cli, None, "global", 1);
        assert!(self_loop.unwrap_err().to_string().contains("itself"));
        assert!(Relation::new("lumen", "uses", "entity_a", None, Source::Cli, None, "global", 1).is_err());
        assert!(Relation::new("entity_a", " ", "entity_b", None, Source::Cli, None, "global", 1).is_err());
    }

    #[test]
    fn note_is_omitted_when_absent() {
        let r = Relation::new("entity_a", "uses", "entity_b", None, Source::Cli, None, "global", 1).unwrap();
        let v = serde_json::to_value(&r).unwrap();
        assert!(v.get("note").is_none());
        assert!(v.get("source_client").is_none());
    }

    #[test]
    fn relation_event_describes_the_edge() {
        let r = Relation::new("entity_a", "uses", "entity_b", Some("n".into()), Source::Mcp, None, "/p", 1).unwrap();
        let ev = crate::history::MemoryEvent::relation(crate::history::EventAction::Relate, &r);
        assert_eq!(ev.action, "relate");
        assert_eq!(ev.memory_id.as_deref(), Some("entity_a"));
        assert_eq!(ev.title.as_deref(), Some("entity_a uses entity_b"));
        assert_eq!(ev.scope.as_deref(), Some("/p"));
        assert_eq!(crate::history::EventAction::parse("unrelate"), Some(crate::history::EventAction::Unrelate));
    }
}
```

Add `pub mod relations;` to `src/knowledge/mod.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test knowledge::relations`
Expected: compile errors (`Relation` not found).

- [ ] **Step 3: Implement the relation document and store**

Insert above the tests in `src/knowledge/relations.rs`:

```rust
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
    "id", "subject", "predicate", "object", "note", "source", "source_client", "scope",
    "created_at", "updated_at",
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
            .filter_map(|d| Some((d.get("id")?.as_str()?.to_string(), d.get("created_at")?.as_i64()?)))
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
        self.fetch(&in_filter("subject", ids)).await
    }

    /// Every edge whose object is one of `ids`.
    pub async fn by_objects(&self, ids: &[String]) -> Result<Vec<Relation>> {
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
```

`by_subjects`/`by_objects` must return an empty vec for an empty `ids` slice instead of sending `IN []`. Add at the top of both: `if ids.is_empty() { return Ok(Vec::new()); }`.

- [ ] **Step 4: Index creation and daemon wiring**

In `src/meili/client.rs`, after `ensure_log_index`:

```rust
    /// Ensure the `memory_relations` index exists with its settings. No
    /// embedder. Idempotent; safe on every daemon start.
    pub async fn ensure_relations_index(&self) -> Result<()> {
        self.create_index().await;
        let settings = json!({
            "searchableAttributes": ["note", "predicate"],
            "filterableAttributes": [
                "id", "subject", "predicate", "object", "source", "source_client",
                "scope", "created_at", "updated_at"
            ],
            "sortableAttributes": ["created_at", "updated_at"],
        });
        self.apply_settings(&settings).await
    }
```

In `src/daemon.rs`, right after the `svc.events().ensure()…?;` statement:

```rust
    crate::knowledge::relations::RelationStore::from_client(svc.client())
        .ensure()
        .await
        .context("ensuring memory_relations index")?;
```

- [ ] **Step 5: Relation events**

In `src/history.rs`:

1. Add variants `Relate` and `Unrelate` to `EventAction` (doc comment: `/// A relation was added.` / `/// A relation was removed.`).
2. `as_str`: `EventAction::Relate => "relate", EventAction::Unrelate => "unrelate",`.
3. `parse`: `"relate" | "related" | "link" => Some(EventAction::Relate), "unrelate" | "unlink" => Some(EventAction::Unrelate),`.
4. Add to `impl MemoryEvent`:

```rust
    /// A relation added or removed. `memory_id` is the subject entity.
    pub fn relation(action: EventAction, rel: &crate::knowledge::relations::Relation) -> Self {
        Self {
            id: uuid::Uuid::now_v7().to_string(),
            ts: now_secs(),
            action: action.as_str().to_string(),
            memory_id: Some(rel.subject.clone()),
            title: Some(format!("{} {} {}", rel.subject, rel.predicate, rel.object)),
            r#type: None,
            scope: Some(rel.scope.clone()),
            source: rel.source.clone(),
            source_client: rel.source_client.clone(),
            detail: rel.note.clone(),
        }
    }
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test`
Expected: all pass, including 4 new relation tests.

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/
git commit -m "feat(knowledge): memory_relations index, relation store and events"
```

---

### Task 4: Name resolution

**Files:**
- Create: `src/knowledge/resolve.rs`
- Modify: `src/knowledge/mod.rs`

**Interfaces:**
- Consumes: `ident::{entity_id, is_entity_id, key_of, filter_literal}`; `MemoryService::client()`, `memory::service::{scope_chain, normalize_scope}`; `MeiliClient::{get_doc, fetch_docs}`.
- Produces (`crate::knowledge::resolve`):
  - `pub enum Resolution { Found(String), NotFound, Ambiguous(Vec<serde_json::Value>) }` (Debug, PartialEq)
  - `pub const ENTITY_ROW_FIELDS: &[&str]`
  - `pub fn decide(id: &str, id_exists: bool, alias_hits: &[Value], scope: Option<&str>) -> Resolution`
  - `pub async fn resolve(mem: &MemoryService, name: &str, scope: Option<&str>) -> anyhow::Result<Resolution>`
  - `pub fn ambiguity_message(name: &str, candidates: &[Value]) -> String`

- [ ] **Step 1: Write the failing tests**

Create `src/knowledge/resolve.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn hit(id: &str, scope: &str) -> Value {
        json!({ "id": id, "name": id, "kind_of": "project", "scope": scope })
    }

    #[test]
    fn existing_id_wins_regardless_of_scope() {
        // The entity lives in another project; it must be reused, never stubbed over.
        let other = [hit("entity_other", "/elsewhere")];
        assert_eq!(
            decide("entity_lumen", true, &other, Some("/a/b")),
            Resolution::Found("entity_lumen".into())
        );
    }

    #[test]
    fn single_alias_hit_resolves() {
        assert_eq!(decide("entity_lab", false, &[], None), Resolution::NotFound);
        assert_eq!(
            decide("entity_lab", false, &[hit("entity_meilisearch-lab", "global")], Some("/x")),
            Resolution::Found("entity_meilisearch-lab".into())
        );
    }

    #[test]
    fn several_alias_hits_prefer_the_scope_chain() {
        let hits = [hit("entity_a", "/p/one"), hit("entity_b", "/q")];
        assert_eq!(
            decide("entity_x", false, &hits, Some("/p/one/sub")),
            Resolution::Found("entity_a".into())
        );
        match decide("entity_x", false, &hits, Some("/r")) {
            Resolution::Ambiguous(c) => assert_eq!(c.len(), 2),
            other => panic!("expected ambiguity, got {other:?}"),
        }
        match decide("entity_x", false, &hits, None) {
            Resolution::Ambiguous(_) => {}
            other => panic!("expected ambiguity, got {other:?}"),
        }
    }

    #[test]
    fn ambiguity_message_lists_every_candidate() {
        let msg = ambiguity_message("Lab", &[hit("entity_a", "/p"), hit("entity_b", "/q")]);
        assert!(msg.contains("entity_a") && msg.contains("entity_b") && msg.contains("Pass the id"), "{msg}");
    }

    #[test]
    fn alias_filter_escapes_the_key() {
        assert_eq!(
            alias_filter("o-reilly"),
            "type = 'entity' AND aliases = 'o-reilly'"
        );
    }
}
```

Add `pub mod resolve;` to `src/knowledge/mod.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test knowledge::resolve`
Expected: compile errors (`decide` not found).

- [ ] **Step 3: Implement**

Insert above the tests:

```rust
//! Name → entity resolution. Ids are global, so resolution is id-first:
//! a name whose id exists resolves there, whatever the scope. Only alias
//! matches use the scope chain, to break ties between several entities.

use super::ident::{entity_id, filter_literal, is_entity_id, key_of};
use crate::memory::MemoryService;
use crate::memory::service::{normalize_scope, scope_chain};
use anyhow::Result;
use serde_json::Value;

/// Lightweight entity row fields, used wherever entities are listed.
pub const ENTITY_ROW_FIELDS: &[&str] = &[
    "id", "name", "kind_of", "status", "owner", "scope", "path", "url", "summary",
    "aliases_display", "source",
];

/// Outcome of resolving a name.
#[derive(Debug, PartialEq)]
pub enum Resolution {
    Found(String),
    NotFound,
    Ambiguous(Vec<Value>),
}

/// Filter for entities that list `key` among their aliases.
pub fn alias_filter(key: &str) -> String {
    format!("type = 'entity' AND aliases = {}", filter_literal(key))
}

fn row_str<'a>(v: &'a Value, k: &str) -> &'a str {
    v.get(k).and_then(|s| s.as_str()).unwrap_or("")
}

/// Pure decision step of [`resolve`].
pub fn decide(id: &str, id_exists: bool, alias_hits: &[Value], scope: Option<&str>) -> Resolution {
    if id_exists {
        return Resolution::Found(id.to_string());
    }
    match alias_hits {
        [] => Resolution::NotFound,
        [one] => Resolution::Found(row_str(one, "id").to_string()),
        many => {
            if let Some(scope) = scope {
                let chain = scope_chain(&normalize_scope(Some(scope)));
                let in_chain: Vec<&Value> = many
                    .iter()
                    .filter(|h| chain.iter().any(|c| c == row_str(h, "scope")))
                    .collect();
                if let [only] = in_chain.as_slice() {
                    return Resolution::Found(row_str(only, "id").to_string());
                }
            }
            Resolution::Ambiguous(many.to_vec())
        }
    }
}

/// Resolve a display name or an entity id to an existing entity.
pub async fn resolve(mem: &MemoryService, name: &str, scope: Option<&str>) -> Result<Resolution> {
    let trimmed = name.trim();
    let id = if is_entity_id(trimmed) {
        trimmed.to_string()
    } else {
        entity_id(trimmed)?
    };
    if mem.client().get_doc(&id).await?.is_some() {
        return Ok(Resolution::Found(id));
    }
    let hits = mem
        .client()
        .fetch_docs(&alias_filter(key_of(&id)), ENTITY_ROW_FIELDS)
        .await?;
    Ok(decide(&id, false, &hits, scope))
}

/// Error text for an ambiguous name.
pub fn ambiguity_message(name: &str, candidates: &[Value]) -> String {
    let list: Vec<String> = candidates
        .iter()
        .map(|c| {
            format!(
                "{} ({}, {}, scope {})",
                row_str(c, "id"),
                row_str(c, "name"),
                row_str(c, "kind_of"),
                row_str(c, "scope")
            )
        })
        .collect();
    format!(
        "`{name}` matches several entities: {}. Pass the id instead of the name.",
        list.join("; ")
    )
}
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test knowledge::resolve`
Expected: `test result: ok. 5 passed`.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/knowledge/
git commit -m "feat(knowledge): id-first name resolution with scoped alias tie-break"
```

---

### Task 5: KnowledgeService — save entities, relate, forget relations

**Files:**
- Create: `src/knowledge/service.rs`
- Modify: `src/knowledge/mod.rs`, `src/memory/service.rs`

**Interfaces:**
- Consumes: Tasks 1–4.
- Produces:
  - `Source::parse(&str) -> Option<Source>` (in `memory::model`)
  - `MemoryService::get_item(&self, id: &str) -> Result<Option<MemoryItem>>` (no access bump)
  - `MemoryService::put_entity(&self, item: &MemoryItem, created: bool) -> Result<()>` (full upsert + create/update event; never dedups)
  - `MemoryService::patch(&self, patches: &[Value]) -> Result<()>` (PUT merge)
  - `crate::knowledge::service::{KnowledgeService, EntityInput, RelationInput, SaveEntityOutcome, BuiltEntity, build_entity, entity_content}`
  - `pub struct RelationInput { pub subject: Option<String>, pub predicate: String, pub object: String, pub note: Option<String> }` (Debug, Clone, Default)
  - `pub struct EntityInput { pub name, pub kind_of: String, pub description: Option<String>, pub aliases: Vec<String>, pub owner: Option<String>, pub status: Option<String>, pub scope: Option<String>, pub tags: Vec<String>, pub url: Option<String>, pub path: Option<String>, pub relations: Vec<RelationInput>, pub source: Source, pub source_client: Option<String> }` with `EntityInput::new(name: &str, kind_of: &str, source: Source) -> Self`
  - `pub struct SaveEntityOutcome { pub id: String, pub created: bool, pub warnings: Vec<String> }`
  - `pub struct BuiltEntity { pub item: MemoryItem, pub created: bool, pub warnings: Vec<String> }`
  - `pub fn build_entity(id: &str, existing: Option<&MemoryItem>, input: &EntityInput, owner_id: Option<String>, now: i64) -> Result<BuiltEntity>`
  - `impl KnowledgeService`: `new(mem: MemoryService) -> Self`, `memories(&self) -> &MemoryService`, `relations(&self) -> &RelationStore`, `async resolve_or_stub(&self, name: &str, scope: &str, source: Source, client: Option<String>) -> Result<String>`, `async save_entity(&self, input: EntityInput) -> Result<SaveEntityOutcome>`, `async relate(&self, rels: &[RelationInput], scope: &str, source: Source, client: Option<String>) -> Vec<String>`, `async forget_relation(&self, subject: &str, predicate: &str, object: &str) -> Result<bool>`

- [ ] **Step 1: Write the failing tests**

Create `src/knowledge/service.rs` with only:

```rust
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
        let ha = build_entity("entity_alpha", None, &a, None, 1).unwrap().item.content_hash;
        let hb = build_entity("entity_beta", None, &b, None, 1).unwrap().item.content_hash;
        assert_ne!(ha, hb);
    }

    #[test]
    fn merge_preserves_unpassed_fields_and_unions_lists() {
        let mut first = input("Lumen");
        first.description = Some("old".into());
        first.aliases = vec!["a".into()];
        first.tags = vec!["x".into()];
        let existing = build_entity("entity_lumen", None, &first, None, 1).unwrap().item;

        let mut second = input("lumen");
        second.aliases = vec!["B".into(), "a".into(), "Lumen".into()];
        second.tags = vec!["x".into(), "y".into()];
        let b = build_entity("entity_lumen", Some(&existing), &second, None, 2).unwrap();
        assert!(!b.created);
        let it = &b.item;
        assert_eq!(it.summary.as_deref(), Some("old"), "description not passed → kept");
        assert_eq!(it.knowledge.name.as_deref(), Some("Lumen"), "display name kept");
        assert_eq!(it.knowledge.aliases, vec!["a", "b"], "own name skipped, duplicates dropped");
        assert_eq!(it.tags, vec!["x", "y"]);
        assert_eq!(it.created_at, 1);
        assert_eq!(it.updated_at, 2);

        let mut third = input("Lumen");
        third.description = Some("new".into());
        let it = build_entity("entity_lumen", Some(&b.item), &third, None, 3).unwrap().item;
        assert!(it.content.starts_with("new"));
    }

    #[test]
    fn stubs_are_cleared_and_kind_changes_warn() {
        let mut stub = EntityInput::new("Quentin", "concept", Source::Mcp);
        stub.status = Some("stub".into());
        let s = build_entity("entity_quentin", None, &stub, None, 1).unwrap().item;

        let filled = build_entity("entity_quentin", Some(&s), &EntityInput::new("Quentin", "person", Source::Mcp), None, 2)
            .unwrap();
        assert_eq!(filled.item.knowledge.status, None, "stub status cleared");
        assert!(filled.warnings.is_empty(), "filling a stub's kind is not a change");

        let changed = build_entity("entity_quentin", Some(&filled.item), &EntityInput::new("Quentin", "team", Source::Mcp), None, 3)
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
        let err = build_entity("entity_x", None, &EntityInput::new("X", "planet", Source::Cli), None, 1)
            .unwrap_err()
            .to_string();
        assert!(err.contains("unknown kind_of"), "{err}");
    }

    #[test]
    fn agent_write_takes_ownership_of_a_crawled_entity() {
        let mut crawled = EntityInput::new("memd", "project", Source::Crawler);
        crawled.path = Some("/p/memd".into());
        let c = build_entity("entity_memd", None, &crawled, None, 1).unwrap().item;
        assert_eq!(c.source, "crawler");
        let it = build_entity("entity_memd", Some(&c), &EntityInput::new("memd", "project", Source::Mcp), None, 2)
            .unwrap()
            .item;
        assert_eq!(it.source, "mcp");
        assert_eq!(it.knowledge.path.as_deref(), Some("/p/memd"), "path kept");
        assert_eq!(Source::parse(&c.source), Some(Source::Crawler));
        assert_eq!(Source::parse("nope"), None);
    }
}
```

Add `pub mod service;` and `pub use service::KnowledgeService;` to `src/knowledge/mod.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test knowledge::service`
Expected: compile errors (`EntityInput` not found).

- [ ] **Step 3: MemoryService helpers**

In `src/memory/model.rs`, add to `impl Source`:

```rust
    /// Parse a stored source string.
    pub fn parse(s: &str) -> Option<Source> {
        match s {
            "mcp" => Some(Source::Mcp),
            "crawler" => Some(Source::Crawler),
            "cli" => Some(Source::Cli),
            _ => None,
        }
    }
```

In `src/memory/service.rs`, inside `impl MemoryService`:

```rust
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
            if created { EventAction::Create } else { EventAction::Update },
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
    pub async fn patch(&self, patches: &[Value]) -> Result<()> {
        if patches.is_empty() {
            return Ok(());
        }
        self.client.update_many(patches).await
    }
```

- [ ] **Step 4: Implement build_entity and the service**

Insert above the tests in `src/knowledge/service.rs`:

```rust
//! The knowledge service: entities, relations, and their links to memories.

use super::ident::{entity_id, is_entity_id, key_of, normalize_predicate, relation_id, slug, validate_kind};
use super::relations::{Relation, RelationStore};
use super::resolve::{Resolution, ambiguity_message, resolve};
use crate::history::{EventAction, MemoryEvent};
use crate::memory::model::now_secs;
use crate::memory::service::{content_hash, normalize_scope};
use crate::memory::{Knowledge, MemoryItem, MemoryService, Source};
use anyhow::{Result, anyhow, bail};

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
        item.knowledge.aliases_display.push(alias.trim().to_string());
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
    Ok(BuiltEntity { item, created, warnings })
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
                let subject = r
                    .subject
                    .as_deref()
                    .ok_or_else(|| anyhow!("no subject"))?;
                let s = self.resolve_or_stub(subject, scope, source, client.clone()).await?;
                let o = self.resolve_or_stub(&r.object, scope, source, client.clone()).await?;
                Relation::new(&s, &r.predicate, &o, r.note.clone(), source, client.clone(), scope, now)
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

    /// Delete one relation, by names or ids. Unknown endpoints mean there is
    /// nothing to delete.
    pub async fn forget_relation(&self, subject: &str, predicate: &str, object: &str) -> Result<bool> {
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test knowledge::service`
Expected: `test result: ok. 6 passed`.

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/
git commit -m "feat(knowledge): save entities with merge semantics, relate, forget relations"
```

---

### Task 6: explore

**Files:**
- Create: `src/knowledge/explore.rs`
- Modify: `src/knowledge/mod.rs`, `src/knowledge/service.rs`

**Interfaces:**
- Consumes: `Relation`, `RelationStore::{by_subjects, by_objects}`, `resolve`, `ENTITY_ROW_FIELDS`, `ident::{in_filter, key_of}`, `MemoryService::{get_item, list_with, get}`, `GetRequest.entity`.
- Produces:
  - `crate::knowledge::explore::shape(entity: Value, outgoing: &[Relation], incoming: &[Relation], related: Vec<Value>, memories: Vec<Value>, neighbours: Option<&[Relation]>) -> Value`
  - `KnowledgeService::explore(&self, name: &str, depth: u8, limit: usize, scope: Option<&str>) -> Result<Value>`
  - `pub const MAX_RELATED: usize = 50;`

- [ ] **Step 1: Write the failing tests**

Create `src/knowledge/explore.rs` with only:

```rust
#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Source;
    use serde_json::json;

    fn rel(s: &str, p: &str, o: &str, note: Option<&str>) -> Relation {
        Relation::new(s, p, o, note.map(String::from), Source::Mcp, None, "global", 1).unwrap()
    }

    #[test]
    fn shapes_both_directions_with_names() {
        let out = [rel("entity_lumen", "part_of", "entity_meilisearch-lab", Some("data plane"))];
        let inc = [rel("entity_glutony", "depends_on", "entity_lumen", None)];
        let related = vec![
            json!({ "id": "entity_meilisearch-lab", "name": "Meilisearch Lab" }),
            json!({ "id": "entity_glutony", "name": "glutony" }),
        ];
        let v = shape(json!({ "id": "entity_lumen" }), &out, &inc, related, vec![json!({"id": "m1"})], None);
        assert_eq!(v["outgoing"][0]["object"], "entity_meilisearch-lab");
        assert_eq!(v["outgoing"][0]["name"], "Meilisearch Lab");
        assert_eq!(v["outgoing"][0]["note"], "data plane");
        assert_eq!(v["incoming"][0]["subject"], "entity_glutony");
        assert_eq!(v["incoming"][0]["name"], "glutony");
        assert!(v["incoming"][0].get("note").is_none());
        assert_eq!(v["memories"][0]["id"], "m1");
        assert!(v.get("neighbours").is_none(), "depth 1 has no neighbours");
    }

    #[test]
    fn unknown_names_fall_back_to_the_key() {
        let out = [rel("entity_a", "uses", "entity_unseen-thing", None)];
        let v = shape(json!({}), &out, &[], vec![], vec![], None);
        assert_eq!(v["outgoing"][0]["name"], "unseen-thing");
    }

    #[test]
    fn depth_two_groups_neighbour_edges() {
        let out = [rel("entity_a", "uses", "entity_b", None)];
        let related = vec![json!({ "id": "entity_b", "name": "B" })];
        let second = [rel("entity_b", "uses", "entity_c", None), rel("entity_d", "owns", "entity_b", None)];
        let v = shape(json!({}), &out, &[], related, vec![], Some(&second));
        assert_eq!(v["neighbours"]["entity_b"]["outgoing"][0]["object"], "entity_c");
        assert_eq!(v["neighbours"]["entity_b"]["incoming"][0]["subject"], "entity_d");
    }
}
```

Add `pub mod explore;` to `src/knowledge/mod.rs`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test knowledge::explore`
Expected: compile errors (`shape` not found).

- [ ] **Step 3: Implement shape**

Insert above the tests in `src/knowledge/explore.rs`:

```rust
//! Shaping of the `explore` response. Pure; the service gathers the inputs.

use super::ident::key_of;
use super::relations::Relation;
use serde_json::{Map, Value, json};
use std::collections::HashMap;

/// Cap on related entities returned by `explore`.
pub const MAX_RELATED: usize = 50;

fn edge(predicate: &str, role: &str, id: &str, name: String, note: &Option<String>) -> Value {
    let mut e = json!({ "predicate": predicate, "name": name });
    e[role] = json!(id);
    if let Some(n) = note {
        e["note"] = json!(n);
    }
    e
}

/// Build the `explore` response from its parts.
pub fn shape(
    entity: Value,
    outgoing: &[Relation],
    incoming: &[Relation],
    related: Vec<Value>,
    memories: Vec<Value>,
    neighbours: Option<&[Relation]>,
) -> Value {
    let names: HashMap<String, String> = related
        .iter()
        .filter_map(|r| {
            Some((
                r.get("id")?.as_str()?.to_string(),
                r.get("name")?.as_str()?.to_string(),
            ))
        })
        .collect();
    let name_of = |id: &str| {
        names
            .get(id)
            .cloned()
            .unwrap_or_else(|| key_of(id).to_string())
    };
    let out = |rels: &[Relation]| -> Vec<Value> {
        rels.iter()
            .map(|r| edge(&r.predicate, "object", &r.object, name_of(&r.object), &r.note))
            .collect()
    };
    let inc = |rels: &[Relation]| -> Vec<Value> {
        rels.iter()
            .map(|r| edge(&r.predicate, "subject", &r.subject, name_of(&r.subject), &r.note))
            .collect()
    };

    let mut v = json!({
        "entity": entity,
        "outgoing": out(outgoing),
        "incoming": inc(incoming),
        "related": related,
        "memories": memories,
    });
    if let Some(edges) = neighbours {
        let mut map = Map::new();
        let mut ids: Vec<&String> = names.keys().collect();
        ids.sort();
        for id in ids {
            let o: Vec<Relation> = edges.iter().filter(|e| &e.subject == id).cloned().collect();
            let i: Vec<Relation> = edges.iter().filter(|e| &e.object == id).cloned().collect();
            if !o.is_empty() || !i.is_empty() {
                map.insert(id.clone(), json!({ "outgoing": out(&o), "incoming": inc(&i) }));
            }
        }
        v["neighbours"] = Value::Object(map);
    }
    v
}
```

- [ ] **Step 4: Implement KnowledgeService::explore**

In `src/knowledge/service.rs`, extend the imports with `use super::explore::{MAX_RELATED, shape};`, `use super::ident::in_filter;`, `use super::resolve::ENTITY_ROW_FIELDS;`, `use crate::memory::{GetRequest, MemoryType, ProjectionOptions};`, `use serde_json::{Value, json};`, then add to `impl KnowledgeService`:

```rust
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
```

- [ ] **Step 5: Run the tests to verify they pass**

Run: `cargo test knowledge::`
Expected: all knowledge tests pass (3 new).

- [ ] **Step 6: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/knowledge/
git commit -m "feat(knowledge): explore an entity's relations, neighbours and memories"
```

---

### Task 7: Link memories to entities and projects

**Files:**
- Modify: `src/knowledge/service.rs`

**Interfaces:**
- Consumes: Tasks 2, 5.
- Produces:
  - `pub fn longest_path_match(scope: &str, projects: &[(String, String)]) -> Option<String>` (pairs are `(entity id, path)`)
  - `pub fn backfill_patches(docs: &[Value], projects: &[(String, String)]) -> Vec<Value>`
  - `KnowledgeService::project_entities(&self) -> Result<Vec<(String, String)>>`
  - `KnowledgeService::project_for_scope(&self, scope: &str) -> Result<Option<String>>`
  - `KnowledgeService::save_memory(&self, req: SaveRequest, entity_names: &[String], relations: &[RelationInput]) -> Result<(String, Vec<String>)>`
  - `KnowledgeService::link_existing(&self, id: &str, entity_names: &[String], relations: &[RelationInput], source: Source, client: Option<String>) -> Result<Vec<String>>`
  - `KnowledgeService::backfill_project_links(&self) -> Result<usize>`

- [ ] **Step 1: Write the failing tests**

Append to the tests in `src/knowledge/service.rs`:

```rust
    #[test]
    fn longest_path_match_picks_the_nearest_project() {
        let projects = vec![
            ("entity_meilisearch".to_string(), "/p/meilisearch".to_string()),
            ("entity_memd".to_string(), "/p/side/memd".to_string()),
            ("entity_side".to_string(), "/p/side".to_string()),
        ];
        assert_eq!(longest_path_match("/p/side/memd", &projects).as_deref(), Some("entity_memd"));
        assert_eq!(longest_path_match("/p/side/memd/crates/x", &projects).as_deref(), Some("entity_memd"));
        assert_eq!(longest_path_match("/p/side/other", &projects).as_deref(), Some("entity_side"));
        // A sibling that merely shares a prefix is not a match.
        assert_eq!(longest_path_match("/p/side/memd2", &projects).as_deref(), Some("entity_side"));
        assert_eq!(longest_path_match("/q", &projects), None);
        assert_eq!(longest_path_match("global", &projects), None);
    }

    #[test]
    fn backfill_only_patches_unlinked_memories_in_a_project() {
        let projects = vec![("entity_memd".to_string(), "/p/memd".to_string())];
        let docs = vec![
            serde_json::json!({ "id": "a", "scope": "/p/memd" }),
            serde_json::json!({ "id": "b", "scope": "/p/memd", "entities": ["entity_memd"] }),
            serde_json::json!({ "id": "c", "scope": "/p/memd", "entities": ["entity_x"] }),
            serde_json::json!({ "id": "d", "scope": "global" }),
        ];
        let patches = backfill_patches(&docs, &projects);
        assert_eq!(patches.len(), 2);
        assert_eq!(patches[0], serde_json::json!({ "id": "a", "entities": ["entity_memd"] }));
        assert_eq!(patches[1], serde_json::json!({ "id": "c", "entities": ["entity_x", "entity_memd"] }));
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test knowledge::service`
Expected: compile errors (`longest_path_match` not found).

- [ ] **Step 3: Implement**

Add `SaveRequest` to the `crate::memory::{…}` import in `src/knowledge/service.rs`. Add the free functions:

```rust
/// The project whose directory contains `scope` most closely.
pub fn longest_path_match(scope: &str, projects: &[(String, String)]) -> Option<String> {
    projects
        .iter()
        .filter(|(_, p)| scope == p || scope.starts_with(&format!("{p}/")))
        .max_by_key(|(_, p)| p.len())
        .map(|(id, _)| id.clone())
}

/// PUT patches adding the matching project entity to memories that lack it.
pub fn backfill_patches(docs: &[Value], projects: &[(String, String)]) -> Vec<Value> {
    docs.iter()
        .filter_map(|d| {
            let id = d.get("id")?.as_str()?;
            let scope = d.get("scope")?.as_str()?;
            let project = longest_path_match(scope, projects)?;
            let mut entities: Vec<String> = d
                .get("entities")
                .and_then(|e| e.as_array())
                .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                .unwrap_or_default();
            if entities.contains(&project) {
                return None;
            }
            entities.push(project);
            Some(json!({ "id": id, "entities": entities }))
        })
        .collect()
}

fn push_unique(v: &mut Vec<String>, id: String) {
    if !v.contains(&id) {
        v.push(id);
    }
}
```

Add to `impl KnowledgeService`:

```rust
    /// Every project entity with a directory: `(id, path)`.
    pub async fn project_entities(&self) -> Result<Vec<(String, String)>> {
        Ok(self
            .mem
            .client()
            .fetch_docs("type = 'entity' AND kind_of = 'project'", &["id", "path"])
            .await?
            .into_iter()
            .filter_map(|d| {
                Some((
                    d.get("id")?.as_str()?.to_string(),
                    d.get("path")?.as_str()?.to_string(),
                ))
            })
            .collect())
    }

    /// The project entity a scope belongs to, if any.
    pub async fn project_for_scope(&self, scope: &str) -> Result<Option<String>> {
        if scope == "global" {
            return Ok(None);
        }
        Ok(longest_path_match(scope, &self.project_entities().await?))
    }

    async fn resolve_names(
        &self,
        names: &[String],
        scope: &str,
        source: Source,
        client: Option<String>,
        into: &mut Vec<String>,
        warnings: &mut Vec<String>,
    ) {
        for n in names {
            match self.resolve_or_stub(n, scope, source, client.clone()).await {
                Ok(id) => push_unique(into, id),
                Err(e) => warnings.push(format!("entity `{n}` skipped: {e}")),
            }
        }
    }

    /// Save a memory with its entity mentions and relations. The project its
    /// scope belongs to is linked automatically.
    pub async fn save_memory(
        &self,
        mut req: SaveRequest,
        entity_names: &[String],
        relations: &[RelationInput],
    ) -> Result<(String, Vec<String>)> {
        let source = req.source.unwrap_or(Source::Cli);
        let client = req.source_client.clone();
        let scope = normalize_scope(req.scope.as_deref());
        let mut warnings = Vec::new();
        let mut ids = std::mem::take(&mut req.entities);
        self.resolve_names(entity_names, &scope, source, client.clone(), &mut ids, &mut warnings)
            .await;
        if let Ok(Some(project)) = self.project_for_scope(&scope).await {
            push_unique(&mut ids, project);
        }
        req.entities = ids;
        let id = self.mem.save(req).await?;
        warnings.extend(self.relate(relations, &scope, source, client).await);
        Ok((id, warnings))
    }

    /// Add entity mentions and relations to an existing memory.
    pub async fn link_existing(
        &self,
        id: &str,
        entity_names: &[String],
        relations: &[RelationInput],
        source: Source,
        client: Option<String>,
    ) -> Result<Vec<String>> {
        let item = self
            .mem
            .get_item(id)
            .await?
            .ok_or_else(|| anyhow!("no memory with id {id}"))?;
        let mut warnings = Vec::new();
        let mut entities = item.knowledge.entities.clone();
        self.resolve_names(entity_names, &item.scope, source, client.clone(), &mut entities, &mut warnings)
            .await;
        if entities != item.knowledge.entities {
            self.mem.patch(&[json!({ "id": id, "entities": entities })]).await?;
        }
        warnings.extend(self.relate(relations, &item.scope, source, client).await);
        Ok(warnings)
    }

    /// One-time link of existing agent/user memories to their project entity.
    pub async fn backfill_project_links(&self) -> Result<usize> {
        let projects = self.project_entities().await?;
        let docs = self
            .mem
            .client()
            .fetch_docs("source != 'crawler' AND type != 'entity'", &["id", "scope", "entities"])
            .await?;
        let patches = backfill_patches(&docs, &projects);
        self.mem.patch(&patches).await?;
        Ok(patches.len())
    }
```

- [ ] **Step 4: Run the tests to verify they pass**

Run: `cargo test knowledge::service`
Expected: 8 passed.

- [ ] **Step 5: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/knowledge/
git commit -m "feat(knowledge): link memories to mentioned entities and their project"
```

---

### Task 8: MCP surface — three tools, extended parameters, instructions

**Files:**
- Modify: `src/mcp/protocol.rs`, `src/agents/directives.rs`

**Interfaces:**
- Consumes: `KnowledgeService::{new, save_entity, explore, forget_relation, save_memory, link_existing}`, `resolve`, `EntityInput`, `RelationInput`, `ident::{entity_id, KINDS, PREDICATES}`.
- Produces: tools `save_entity`, `explore`, `forget_relation`; `save_memory`/`update_memory` accept `entities` and `relations`; `get_memory`/`list_memories` accept `entity`, `status`, `kind_of`. Helper fns `parse_relations(args: &Value, subject_required: bool) -> anyhow::Result<Vec<RelationInput>>` and `parse_entity_input(args: &Value) -> anyhow::Result<EntityInput>`.

- [ ] **Step 1: Write the failing tests**

Append to `src/mcp/protocol.rs` (create the test module if absent):

```rust
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
                "explore", "forget_memory", "forget_relation", "get_memory", "history",
                "list_memories", "read_memory", "save_entity", "save_memory", "stats",
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
}
```

In `src/agents/directives.rs` tests, add to `upsert_then_remove_roundtrip` after the first upsert: `assert!(after.contains("explore"));`.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test mcp:: agents::directives`
Expected: compile errors (`parse_relations` not found) and the directive assertion fails.

- [ ] **Step 3: Parsers and handlers**

In `src/mcp/protocol.rs`, extend the imports:

```rust
use crate::knowledge::KnowledgeService;
use crate::knowledge::ident::entity_id;
use crate::knowledge::resolve::{Resolution, ambiguity_message, resolve};
use crate::knowledge::service::{EntityInput, RelationInput};
```

Add the parsers in the helpers section:

```rust
/// Parse a `relations` array. `subject_required` is true for plain memories,
/// where no entity is implied.
fn parse_relations(args: &Value, subject_required: bool) -> anyhow::Result<Vec<RelationInput>> {
    let Some(arr) = args.get("relations").and_then(|r| r.as_array()) else {
        return Ok(Vec::new());
    };
    arr.iter()
        .map(|r| {
            let field = |k: &str| r.get(k).and_then(|v| v.as_str()).map(String::from);
            let predicate = field("predicate").ok_or_else(|| anyhow::anyhow!("each relation needs a `predicate`"))?;
            let object = field("object").ok_or_else(|| anyhow::anyhow!("each relation needs an `object`"))?;
            let subject = field("subject");
            if subject_required && subject.is_none() {
                anyhow::bail!("each relation needs a `subject` (the entity it starts from)");
            }
            Ok(RelationInput { subject, predicate, object, note: field("note") })
        })
        .collect()
}

/// Parse `save_entity` arguments.
fn parse_entity_input(args: &Value) -> anyhow::Result<EntityInput> {
    let name = str_field(args, "name").ok_or_else(|| anyhow::anyhow!("`name` is required"))?;
    let kind = str_field(args, "kind_of").ok_or_else(|| anyhow::anyhow!("`kind_of` is required"))?;
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
```

Replace the body of `handle_tool_call`'s dispatch with:

```rust
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
```

Rewrite `save_memory`:

```rust
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
        r#type: args.get("type").and_then(|t| t.as_str()).and_then(MemoryType::parse),
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
```

Rewrite `update_memory` to take `kn: &KnowledgeService`, call `kn.memories().update(…)` exactly as before, then:

```rust
    let entities = str_array(&args, "entities");
    let relations = parse_relations(&args, true)?;
    let mut out = json!({ "updated": updated, "id": id });
    if updated && (!entities.is_empty() || !relations.is_empty()) {
        let warnings = kn
            .link_existing(id, &entities, &relations, Source::Mcp, str_field(&args, "source_client"))
            .await?;
        if !warnings.is_empty() {
            out["warnings"] = json!(warnings);
        }
    }
    Ok(out)
```

In `get_memory`, before building the request: `let entity = entity_arg(svc, &args).await?;` and in the `GetRequest` literal replace `entity: None, status: None, kind_of: None,` with `entity, status: str_field(&args, "status"), kind_of: str_field(&args, "kind_of"),`.

Rewrite `list_memories`:

```rust
async fn list_memories(svc: &MemoryService, args: Value) -> anyhow::Result<Value> {
    let opts = projection_opts(&args, ProjectionOptions::list_default());
    let req = GetRequest {
        r#type: args.get("type").and_then(|t| t.as_str()).and_then(MemoryType::parse),
        scope: str_field(&args, "scope"),
        offset: args.get("offset").and_then(|o| o.as_u64()).map(|n| n as usize),
        entity: entity_arg(svc, &args).await?,
        status: str_field(&args, "status"),
        kind_of: str_field(&args, "kind_of"),
        ..Default::default()
    };
    let limit = args.get("limit").and_then(|l| l.as_u64()).map(|n| n as usize).unwrap_or(20);
    let result = svc.list_with(&req, limit, &opts).await?;
    Ok(query_result_json(result))
}
```

Add the three new handlers:

```rust
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
    let depth = args.get("depth").and_then(|d| d.as_u64()).unwrap_or(1).min(2) as u8;
    let limit = args.get("limit").and_then(|l| l.as_u64()).unwrap_or(10) as usize;
    kn.explore(&name, depth, limit, str_field(&args, "scope").as_deref()).await
}

async fn forget_relation(kn: &KnowledgeService, args: Value) -> anyhow::Result<Value> {
    let get = |k: &str| str_field(&args, k).ok_or_else(|| anyhow::anyhow!("`{k}` is required"));
    let deleted = kn
        .forget_relation(&get("subject")?, &get("predicate")?, &get("object")?)
        .await?;
    Ok(json!({ "deleted": deleted }))
}
```

- [ ] **Step 4: Tool definitions**

The kind and predicate lists below must not be typed by hand: at the top of
`tool_defs()` add

```rust
    let kinds = crate::knowledge::ident::KINDS.join(", ");
    let predicates = crate::knowledge::ident::PREDICATES.join(", ");
```

and write every description that lists them with `format!` (e.g.
`"description": format!("Only entities of this kind: {kinds}.")`). The JSON
shown here spells the lists out for readability; the code interpolates them.
This keeps the constants the single source of truth and keeps them in use
outside tests.

In `tool_defs()`:

1. `save_memory` `properties` gains:

```json
"entities": { "type": "array", "items": { "type": "string" }, "description": "Names (or ids) of the entities this memory is about. Unknown names become stub entities. The project the scope belongs to is linked automatically." },
"relations": { "type": "array", "description": "Relations you learned, as {subject, predicate, object, note?}. Predicates: part_of, depends_on, owns, uses, replaces, works_on, member_of, customer_of, related_to (free text allowed).", "items": { "type": "object", "properties": { "subject": { "type": "string" }, "predicate": { "type": "string" }, "object": { "type": "string" }, "note": { "type": "string" } }, "required": ["subject", "predicate", "object"] } }
```

2. `update_memory` `properties` gains the same two entries.

3. `get_memory` and `list_memories` `properties` gain:

```json
"entity": { "type": "string", "description": "Only memories that mention this entity (name or id)." },
"status": { "type": "string", "description": "Only records with this status (e.g. accepted, superseded, open, active)." },
"kind_of": { "type": "string", "description": "Only entities of this kind: company, team, person, project, product, service, customer, concept." }
```

4. Append three tool objects:

```json
{
    "name": "save_entity",
    "description": "Create or update a thing in the user's world — a company, team, person, project, product, service, customer or concept — with its aliases, owner, status and relations. Saving the same name (or an alias) again updates the same record: passed fields overwrite, absent fields are kept, aliases and tags are merged. Relations start from this entity; predicates: part_of, depends_on, owns, uses, replaces, works_on, member_of, customer_of, related_to.",
    "inputSchema": {
        "type": "object",
        "properties": {
            "name": { "type": "string", "description": "Canonical display name." },
            "kind_of": { "type": "string", "description": "company, team, person, project, product, service, customer, concept." },
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
```

5. `SERVER_INSTRUCTIONS`: insert before the `- Results are lightweight rows…` bullet:

```text
- NAME THINGS: when you save, list the entities involved in `entities` and \
any relation you learned in `relations`; use `save_entity` for a company, \
team, person, project, product, service, customer or concept. Before asking \
the user what something is, call `explore` with its name.
```

(Keep the `\` line-continuation style of the surrounding constant.)

- [ ] **Step 5: Directive text**

In `src/agents/directives.rs`, `directive_block()`, insert before the `read_memory(id)` bullet:

```text
- **Name things.** When you save, list the entities involved (`entities`) and any \
relation you learned (`relations`); use `save_entity` for a company, team, person, \
project, product, service, customer or concept. Before asking the user what \
something is, call `explore` with its name.\n\
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test`
Expected: all pass (4 new protocol tests, directive assertion).

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/
git commit -m "feat(mcp): save_entity, explore, forget_relation; entity filters and links"
```

---

### Task 9: Crawler — project entities and links

**Files:**
- Modify: `src/crawler/mod.rs`, `src/memory/service.rs`

**Interfaces:**
- Consumes: `ident::slug`, `KINDS`; `MemoryService::patch`; `Knowledge`.
- Produces:
  - `pub struct CrawledDoc { pub hash: String, pub created_at: i64, pub entities: Vec<String> }` in `memory::service`; `crawled_state` returns `HashMap<String, CrawledDoc>`
  - `prepare_crawled(&self, source_path, content, ty, scope, title, existing: Option<(&str, i64)>, entities: Vec<String>) -> Option<MemoryItem>`
  - `upsert_crawled(&self, source_path, content, ty, scope, title, entities: Vec<String>) -> Result<bool>`
  - In `crawler`: `pub fn readme_headline(md: &str) -> String`, `pub fn assign_project_ids(repos: &[PathBuf], taken: &HashMap<String, PathBuf>) -> Vec<(PathBuf, String)>`, `pub fn project_entity_item(id: &str, repo: &Path, readme: Option<&str>, created_at: Option<i64>, now: i64) -> MemoryItem`, `pub fn archive_patches(existing: &[Value]) -> Vec<Value>`, `pub fn crawler_writable(assigned: &[(PathBuf, String)], existing: &[Value]) -> Vec<(PathBuf, String)>`; `Crawler.projects: RwLock<HashMap<String, String>>` (scope path → project id).

- [ ] **Step 1: Write the failing tests**

Append to the tests in `src/crawler/mod.rs`:

```rust
    #[test]
    fn readme_headline_takes_heading_and_first_paragraph() {
        let md = "# memd\n\n[![CI](x)](y)\n<div>\n\nUniversal local memory\nfor every LLM tool.\n\n## Install\n";
        assert_eq!(readme_headline(md), "memd — Universal local memory for every LLM tool.");
        assert_eq!(readme_headline("Just text."), "Just text.");
        assert_eq!(readme_headline("# Only a title\n"), "Only a title");
        assert_eq!(readme_headline(""), "");
        let long = format!("# T\n\n{}", "word ".repeat(200));
        assert!(readme_headline(&long).chars().count() <= 400);
    }

    #[test]
    fn project_ids_resolve_collisions_and_keep_existing_claims() {
        let taken = HashMap::from([("entity_console".to_string(), PathBuf::from("/p/cloud/console"))]);
        let repos = vec![
            PathBuf::from("/p/cloud/console"),
            PathBuf::from("/p/demos/console"),
            PathBuf::from("/p/memd"),
            PathBuf::from("/q/demos/console"),
        ];
        let got: HashMap<PathBuf, String> = assign_project_ids(&repos, &taken).into_iter().collect();
        assert_eq!(got[&PathBuf::from("/p/cloud/console")], "entity_console", "existing claim kept");
        assert_eq!(got[&PathBuf::from("/p/demos/console")], "entity_demos-console");
        assert_eq!(got[&PathBuf::from("/q/demos/console")], "entity_demos-console-2");
        assert_eq!(got[&PathBuf::from("/p/memd")], "entity_memd");
    }

    #[test]
    fn project_entity_items_are_crawler_owned_projects() {
        let it = project_entity_item("entity_memd", Path::new("/p/memd"), Some("# memd\n\nMemory daemon."), Some(5), 9);
        assert_eq!(it.r#type, "entity");
        assert_eq!(it.source, "crawler");
        assert_eq!(it.scope, "/p/memd");
        assert_eq!(it.created_at, 5);
        assert_eq!(it.knowledge.kind_of.as_deref(), Some("project"));
        assert_eq!(it.knowledge.path.as_deref(), Some("/p/memd"));
        assert_eq!(it.knowledge.name.as_deref(), Some("memd"));
        assert_eq!(it.summary.as_deref(), Some("memd — Memory daemon."));
        let other = project_entity_item("entity_memd", Path::new("/p/memd"), None, None, 9);
        assert_ne!(it.content_hash, other.content_hash);
    }

    #[test]
    fn crawler_never_writes_a_project_an_agent_took_over() {
        let assigned = vec![
            (PathBuf::from("/p/memd"), "entity_memd".to_string()),
            (PathBuf::from("/p/lab"), "entity_lab".to_string()),
            (PathBuf::from("/p/new"), "entity_new".to_string()),
        ];
        let existing = vec![
            serde_json::json!({ "id": "entity_memd", "path": "/p/memd", "source": "mcp" }),
            serde_json::json!({ "id": "entity_lab", "path": "/p/lab", "source": "crawler" }),
        ];
        let ids: Vec<String> = crawler_writable(&assigned, &existing).into_iter().map(|(_, id)| id).collect();
        assert_eq!(ids, vec!["entity_lab", "entity_new"]);
    }

    #[test]
    fn only_agent_owned_projects_with_missing_paths_are_archived() {
        let existing = vec![
            serde_json::json!({ "id": "entity_gone", "path": "/definitely/missing/xyz", "source": "mcp" }),
            serde_json::json!({ "id": "entity_done", "path": "/definitely/missing/xyz", "source": "mcp", "status": "archived" }),
            serde_json::json!({ "id": "entity_crawled", "path": "/definitely/missing/xyz", "source": "crawler" }),
            serde_json::json!({ "id": "entity_here", "path": "/", "source": "cli" }),
        ];
        assert_eq!(
            archive_patches(&existing),
            vec![serde_json::json!({ "id": "entity_gone", "status": "archived" })]
        );
    }
```

In `src/memory/service.rs`, update the test `prepare_crawled_skips_unchanged_and_keeps_created_at`: both `prepare_crawled(…)` calls gain a final argument `vec!["entity_r".into()]`, and add `assert_eq!(item.knowledge.entities, vec!["entity_r"]);` at the end.

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test crawler:: memory::service`
Expected: compile errors (`readme_headline` not found, wrong argument count).

- [ ] **Step 3: Service changes**

In `src/memory/service.rs`:

```rust
/// What the crawler knows about a document it wrote.
#[derive(Debug, Clone)]
pub struct CrawledDoc {
    pub hash: String,
    pub created_at: i64,
    pub entities: Vec<String>,
}
```

`crawled_state` becomes:

```rust
    pub async fn crawled_state(&self) -> Result<HashMap<String, CrawledDoc>> {
        let docs = self
            .client
            .fetch_docs("source = 'crawler'", &["id", "content_hash", "created_at", "entities"])
            .await?;
        Ok(docs
            .into_iter()
            .filter_map(|d| {
                let id = d.get("id")?.as_str()?.to_string();
                Some((
                    id,
                    CrawledDoc {
                        hash: d.get("content_hash")?.as_str()?.to_string(),
                        created_at: d.get("created_at").and_then(|c| c.as_i64()).unwrap_or(0),
                        entities: d
                            .get("entities")
                            .and_then(|e| e.as_array())
                            .map(|a| a.iter().filter_map(|x| x.as_str().map(String::from)).collect())
                            .unwrap_or_default(),
                    },
                ))
            })
            .collect())
    }
```

`prepare_crawled` gains the last parameter `entities: Vec<String>` and sets `knowledge: Knowledge { entities, ..Default::default() },`. `upsert_crawled` gains the last parameter `entities: Vec<String>` and passes it through.

- [ ] **Step 4: Crawler pure helpers**

In `src/crawler/mod.rs` add imports `use crate::knowledge::ident::slug;`, `use crate::memory::{Knowledge, MemoryType};`, `use crate::memory::service::{CrawledDoc, content_hash};`, `use std::sync::RwLock;`, then:

```rust
/// "Heading — first paragraph" of a README, at most 400 characters. Badges,
/// HTML, images, tables, quotes and code fences are skipped.
pub fn readme_headline(md: &str) -> String {
    let mut heading: Option<String> = None;
    let mut para: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in md.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if t.is_empty() {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        if let Some(h) = t.strip_prefix('#') {
            if heading.is_none() && para.is_empty() {
                heading = Some(h.trim_start_matches('#').trim().to_string());
                continue;
            }
            if !para.is_empty() {
                break;
            }
            continue;
        }
        let noise = ["<", "!", "[!", "[![", "|", ">", "---", "==="];
        if noise.iter().any(|n| t.starts_with(n)) {
            continue;
        }
        para.push(t);
    }
    let body = para.join(" ");
    let out = match (heading, body.is_empty()) {
        (Some(h), false) => format!("{h} — {body}"),
        (Some(h), true) => h,
        (None, _) => body,
    };
    out.chars().take(400).collect()
}

/// Give every repository a project entity id. Ids already claimed (by path)
/// are kept; a new repository takes `slug(basename)`, else
/// `slug(parent-basename)`, else that with `-2`, `-3`, …
pub fn assign_project_ids(repos: &[PathBuf], taken: &HashMap<String, PathBuf>) -> Vec<(PathBuf, String)> {
    let mut taken = taken.clone();
    let mut out = Vec::new();
    for repo in repos {
        if let Some((id, _)) = taken.iter().find(|(_, p)| *p == repo) {
            out.push((repo.clone(), id.clone()));
            continue;
        }
        let base = repo.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
        let parent = repo
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let first = format!("entity_{}", slug(&base));
        let second = format!("entity_{}", slug(&format!("{parent}-{base}")));
        let id = if !taken.contains_key(&first) {
            first
        } else if !taken.contains_key(&second) {
            second
        } else {
            (2..)
                .map(|n| format!("{second}-{n}"))
                .find(|c| !taken.contains_key(c))
                .unwrap()
        };
        taken.insert(id.clone(), repo.clone());
        out.push((repo.clone(), id));
    }
    out
}

/// The crawler-owned entity document for a repository.
pub fn project_entity_item(
    id: &str,
    repo: &Path,
    readme: Option<&str>,
    created_at: Option<i64>,
    now: i64,
) -> MemoryItem {
    let name = repo.file_name().map(|n| n.to_string_lossy().to_string()).unwrap_or_default();
    let path = repo.to_string_lossy().to_string();
    let summary = readme.map(readme_headline).filter(|s| !s.is_empty());
    let content = summary.clone().unwrap_or_default();
    MemoryItem {
        id: id.to_string(),
        content_hash: content_hash(&format!("{id}\n{path}\n{content}")),
        content,
        title: Some(name.clone()),
        summary,
        r#type: "entity".to_string(),
        tags: Vec::new(),
        scope: path.clone(),
        source: "crawler".to_string(),
        source_path: None,
        source_client: None,
        created_at: created_at.unwrap_or(now),
        updated_at: now,
        last_accessed_at: None,
        knowledge: Knowledge {
            name: Some(name),
            name_key: Some(id.trim_start_matches("entity_").to_string()),
            kind_of: Some("project".to_string()),
            path: Some(path),
            ..Default::default()
        },
    }
}

/// The `(repo, id)` pairs the crawler may (re)write. A project entity whose
/// stored source is not `crawler` belongs to an agent and is left alone.
pub fn crawler_writable(assigned: &[(PathBuf, String)], existing: &[Value]) -> Vec<(PathBuf, String)> {
    let agent_owned: HashSet<&str> = existing
        .iter()
        .filter(|d| d.get("source").and_then(|s| s.as_str()) != Some("crawler"))
        .filter_map(|d| d.get("id")?.as_str())
        .collect();
    assigned
        .iter()
        .filter(|(_, id)| !agent_owned.contains(id.as_str()))
        .cloned()
        .collect()
}

/// Status patches for agent-owned project entities whose directory is gone.
pub fn archive_patches(existing: &[Value]) -> Vec<Value> {
    existing
        .iter()
        .filter(|d| d.get("source").and_then(|s| s.as_str()) != Some("crawler"))
        .filter(|d| d.get("status").and_then(|s| s.as_str()) != Some("archived"))
        .filter_map(|d| {
            let path = d.get("path")?.as_str()?;
            if Path::new(path).exists() {
                return None;
            }
            Some(json!({ "id": d.get("id")?.as_str()?, "status": "archived" }))
        })
        .collect()
}
```

- [ ] **Step 5: Restructure the scan**

Add the field `projects: RwLock<HashMap<String, String>>,` to `Crawler` (doc: `/// Repository path → project entity id, refreshed by every scan.`), initialised with `projects: RwLock::new(HashMap::new()),` in `Crawler::new`. Add:

```rust
    /// The project entity ids for a scope (empty or one element).
    fn project_ids_for(&self, scope: &str) -> Vec<String> {
        self.projects
            .read()
            .map(|m| m.get(scope).cloned().into_iter().collect())
            .unwrap_or_default()
    }
```

Add a candidate type:

```rust
/// A qualifying file found by the walk, ingested once project ids are known.
struct Candidate {
    path: PathBuf,
    ty: MemoryType,
    scope: String,
}
```

Replace `scan_with` and `ingest` with:

```rust
async fn scan_with(crawler: &Crawler, svc: &MemoryService, reset: bool) -> Result<CrawlSummary> {
    let mut summary = CrawlSummary::default();
    let state: HashMap<String, CrawledDoc> = if reset {
        summary.deleted = svc.forget_all_crawled().await?;
        HashMap::new()
    } else {
        svc.crawled_state().await?
    };

    // 1. Walk every root, collecting qualifying files and git repositories.
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut git_roots: Vec<PathBuf> = Vec::new();
    let mut scope_cache = ScopeCache::new();
    for root in &crawler.roots {
        let walker = WalkDir::new(root).follow_links(false).into_iter();
        for entry in
            walker.filter_entry(|e| !e.file_type().is_dir() || !crawler.dir_excluded(e.path()))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    summary.errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            if entry.file_type().is_dir() {
                if path.join(".git").is_dir() {
                    git_roots.push(path.to_path_buf());
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            summary.scanned += 1;
            let Some(ty) = classify::classify_path(&path.to_string_lossy()) else {
                continue;
            };
            if crawler.deny.is_match(path) || file_too_big(path, crawler.max_file_bytes) {
                summary.skipped += 1;
                continue;
            }
            let scope = crawler.project_scope(root, path, &mut scope_cache);
            candidates.push(Candidate { path: path.to_path_buf(), ty, scope });
        }
    }
    let slugs: HashMap<String, PathBuf> = git_roots
        .iter()
        .map(|d| (claude_slug(&d.to_string_lossy()), d.clone()))
        .collect();
    for root in &crawler.knowledge_roots {
        let walker = WalkDir::new(root).follow_links(false).into_iter();
        for entry in walker
            .filter_entry(|e| !e.file_type().is_dir() || !crawler.dir_excluded(e.path()))
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            summary.scanned += 1;
            let Some(ty) = classify::classify_path(&path.to_string_lossy()) else {
                continue;
            };
            if crawler.deny.is_match(path) || file_too_big(path, crawler.max_file_bytes) {
                summary.skipped += 1;
                continue;
            }
            let scope = crawler.knowledge_scope(path, &slugs);
            candidates.push(Candidate { path: path.to_path_buf(), ty, scope });
        }
    }

    // 2. Give every repository a project entity id.
    let existing_projects = svc
        .client()
        .fetch_docs(
            "type = 'entity' AND kind_of = 'project'",
            &["id", "path", "source", "status"],
        )
        .await
        .unwrap_or_default();
    let taken: HashMap<String, PathBuf> = existing_projects
        .iter()
        .filter_map(|d| Some((d.get("id")?.as_str()?.to_string(), PathBuf::from(d.get("path")?.as_str()?))))
        .collect();
    git_roots.sort();
    let assigned = assign_project_ids(&git_roots, &taken);
    let by_scope: HashMap<String, String> = assigned
        .iter()
        .map(|(p, id)| (p.to_string_lossy().to_string(), id.clone()))
        .collect();
    if let Ok(mut m) = crawler.projects.write() {
        *m = by_scope.clone();
    }

    // 3. Files.
    let mut seen: HashSet<String> = HashSet::new();
    let mut batch: Vec<MemoryItem> = Vec::new();
    let mut patches: Vec<Value> = Vec::new();
    const BATCH: usize = 200;
    for c in &candidates {
        let entities: Vec<String> = by_scope.get(&c.scope).cloned().into_iter().collect();
        ingest(svc, c, entities, &state, &mut seen, &mut batch, &mut patches, &mut summary);
        if batch.len() >= BATCH {
            flush(svc, &mut batch, &mut summary).await;
        }
    }

    // 4. Project entities. Agent-owned ones (source ≠ crawler) are never
    //    overwritten; they are not in `state`, so stale deletion skips them too.
    let now = crate::memory::model::now_secs();
    for (repo, id) in &crawler_writable(&assigned, &existing_projects) {
        let readme = candidates
            .iter()
            .find(|c| c.ty == MemoryType::ProjectOverview && c.path.parent() == Some(repo.as_path()))
            .and_then(|c| std::fs::read_to_string(&c.path).ok());
        let prev = state.get(id);
        let item = project_entity_item(id, repo, readme.as_deref(), prev.map(|p| p.created_at), now);
        seen.insert(id.clone());
        if prev.map(|p| p.hash == item.content_hash).unwrap_or(false) {
            summary.skipped += 1;
            continue;
        }
        batch.push(item);
        summary.indexed += 1;
    }
    flush(svc, &mut batch, &mut summary).await;

    // 5. Link-only updates and archived projects.
    patches.extend(archive_patches(&existing_projects));
    if let Err(e) = svc.patch(&patches).await {
        tracing::warn!("patching {} crawled docs failed: {e}", patches.len());
        summary.errors += patches.len();
    }

    // 6. Deletions: stored crawler docs no qualifying file produced this pass.
    let stale: Vec<String> = state
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    for chunk in stale.chunks(1000) {
        summary.deleted += svc.client().delete_many(chunk).await.unwrap_or(0);
    }

    summary.finished_at = crate::memory::model::now_secs();
    save_summary(&summary)?;
    tracing::info!(
        "crawl: {} scanned, {} indexed, {} skipped, {} deleted, {} errors",
        summary.scanned,
        summary.indexed,
        summary.skipped,
        summary.deleted,
        summary.errors
    );
    if summary.indexed + summary.deleted + summary.errors > 0 {
        let detail = format!(
            "{} indexed, {} skipped, {} deleted, {} errors",
            summary.indexed, summary.skipped, summary.deleted, summary.errors
        );
        svc.events()
            .record(crate::history::MemoryEvent::crawl(detail))
            .await;
    }
    Ok(summary)
}

/// Read one qualifying file and queue it: a full upsert when its content
/// changed, a link-only patch when only its project changed.
#[allow(clippy::too_many_arguments)]
fn ingest(
    svc: &MemoryService,
    c: &Candidate,
    entities: Vec<String>,
    state: &HashMap<String, CrawledDoc>,
    seen: &mut HashSet<String>,
    batch: &mut Vec<MemoryItem>,
    patches: &mut Vec<Value>,
    summary: &mut CrawlSummary,
) {
    let path_str = c.path.to_string_lossy().to_string();
    let id = path_id(&path_str);
    let content = match std::fs::read_to_string(&c.path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => {
            summary.skipped += 1; // empty, binary, or unreadable
            return;
        }
    };
    seen.insert(id.clone());
    let prev = state.get(&id);
    let existing = prev.map(|p| (p.hash.as_str(), p.created_at));
    match svc.prepare_crawled(&path_str, content, c.ty, c.scope.clone(), None, existing, entities.clone()) {
        Some(item) => {
            batch.push(item);
            summary.indexed += 1;
        }
        None => {
            if prev.map(|p| p.entities != entities).unwrap_or(false) {
                patches.push(json!({ "id": id, "entities": entities }));
            }
            summary.skipped += 1;
        }
    }
}
```

`MemoryType` must be `PartialEq` (it already derives `PartialEq, Eq`).

In `index_one`, change the last call to:

```rust
    let scope = crawler.scope_for(path);
    let entities = crawler.project_ids_for(&scope);
    svc.upsert_crawled(&path.to_string_lossy(), content, ty, scope, None, entities)
        .await
```

- [ ] **Step 6: Run the tests to verify they pass**

Run: `cargo test`
Expected: all pass (5 new crawler tests, updated service test).

- [ ] **Step 7: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/
git commit -m "feat(crawler): project entities from git repositories, linked crawled files"
```

---

### Task 10: CLI — entity, relate, unrelate, search filters, context header, back-fill

**Files:**
- Modify: `src/main.rs`, `src/cli.rs`

**Interfaces:**
- Consumes: everything above.
- Produces: commands `memd entity <name> [--depth N]`, `memd entity add …`, `memd relate`, `memd unrelate`, `memd search --entity --status --kind-of`; pure fns in `cli.rs`: `render_explore(v: &Value) -> String`, `context_header(entity: &MemoryItem, outgoing: &[Relation], incoming: &[Relation], names: &HashMap<String, String>) -> Option<String>`.

- [ ] **Step 1: Write the failing tests**

Append to the tests in `src/cli.rs`:

```rust
    use crate::knowledge::relations::Relation;
    use std::collections::HashMap;

    fn rel(s: &str, p: &str, o: &str, note: Option<&str>) -> Relation {
        Relation::new(s, p, o, note.map(String::from), Source::Mcp, None, "global", 1).unwrap()
    }

    fn entity(source: &str) -> crate::memory::MemoryItem {
        let mut e = crate::knowledge::service::build_entity(
            "entity_memd",
            None,
            &crate::knowledge::service::EntityInput::new("memd", "project", Source::Mcp),
            Some("entity_quentin".into()),
            1,
        )
        .unwrap()
        .item;
        e.source = source.into();
        e
    }

    #[test]
    fn context_header_lists_kind_owner_status_and_relations() {
        let names = HashMap::from([
            ("entity_quentin".to_string(), "Quentin".to_string()),
            ("entity_side".to_string(), "Meilisearch side projects".to_string()),
            ("entity_meili".to_string(), "Meilisearch".to_string()),
        ]);
        let out = [
            rel("entity_memd", "part_of", "entity_side", None),
            rel("entity_memd", "uses", "entity_meili", Some("local engine, pinned")),
        ];
        let h = context_header(&entity("mcp"), &out, &[], &names).unwrap();
        assert!(h.starts_with("**memd** — project · owner: Quentin · status: active"), "{h}");
        assert!(h.contains("part_of → Meilisearch side projects"), "{h}");
        assert!(h.contains("uses → Meilisearch (local engine, pinned)"), "{h}");
    }

    #[test]
    fn context_header_is_silent_for_undescribed_crawled_projects() {
        assert!(context_header(&entity("crawler"), &[], &[], &HashMap::new()).is_none());
        assert!(context_header(&entity("mcp"), &[], &[], &HashMap::new()).is_some());
    }

    #[test]
    fn context_header_caps_relations_and_notes() {
        let out: Vec<Relation> = (0..12)
            .map(|i| rel("entity_memd", "uses", &format!("entity_t{i}"), Some(&"n".repeat(100))))
            .collect();
        let h = context_header(&entity("mcp"), &out, &[], &HashMap::new()).unwrap();
        assert_eq!(h.matches("uses →").count(), 8);
        assert!(!h.contains(&"n".repeat(61)));
    }

    #[test]
    fn render_explore_prints_entity_relations_and_memories() {
        let v = serde_json::json!({
            "entity": { "id": "entity_lumen", "name": "Lumen", "kind_of": "product", "summary": "Gateway.", "aliases_display": ["lumen-gw"] },
            "outgoing": [{ "predicate": "part_of", "object": "entity_lab", "name": "Lab", "note": "data plane" }],
            "incoming": [{ "predicate": "depends_on", "subject": "entity_glutony", "name": "glutony" }],
            "related": [],
            "memories": [{ "id": "m1", "type": "decision", "title": "Use leases", "content": "Leases…" }]
        });
        let s = render_explore(&v);
        assert!(s.contains("## Lumen (product)"), "{s}");
        assert!(s.contains("Gateway."));
        assert!(s.contains("Aliases: lumen-gw"));
        assert!(s.contains("- part_of → Lab — data plane"));
        assert!(s.contains("- glutony depends_on → Lumen"));
        assert!(s.contains("[decision] Use leases"));
        let none = render_explore(&serde_json::json!({ "entity": null, "suggestions": [{ "id": "entity_lab", "title": "Lab" }] }));
        assert!(none.contains("No entity") && none.contains("Lab"), "{none}");
    }
```

- [ ] **Step 2: Run the tests to verify they fail**

Run: `cargo test cli::`
Expected: compile errors (`context_header` not found).

- [ ] **Step 3: Pure renderers**

In `src/cli.rs` add imports `use crate::knowledge::KnowledgeService;`, `use crate::knowledge::relations::Relation;`, `use crate::knowledge::ident::key_of;`, `use std::collections::HashMap;`, then:

```rust
/// One block about the current project, printed before the session's
/// memories. Silent for crawled projects nobody has described yet.
fn context_header(
    entity: &crate::memory::MemoryItem,
    outgoing: &[Relation],
    incoming: &[Relation],
    names: &HashMap<String, String>,
) -> Option<String> {
    if outgoing.is_empty() && incoming.is_empty() && entity.source == "crawler" {
        return None;
    }
    let name_of = |id: &str| names.get(id).cloned().unwrap_or_else(|| key_of(id).to_string());
    let note = |n: &Option<String>| {
        n.as_ref()
            .map(|n| format!(" ({})", truncate(n, 60)))
            .unwrap_or_default()
    };
    let k = &entity.knowledge;
    let mut parts = vec![k.kind_of.clone().unwrap_or_else(|| "entity".into())];
    if let Some(o) = &k.owner {
        parts.push(format!("owner: {}", name_of(o)));
    }
    parts.push(format!("status: {}", k.status.as_deref().unwrap_or("active")));
    let title = k.name.clone().unwrap_or_else(|| key_of(&entity.id).to_string());
    let mut out = format!("**{title}** — {}", parts.join(" · "));
    let rels: Vec<String> = outgoing
        .iter()
        .map(|r| format!("{} → {}{}", r.predicate, name_of(&r.object), note(&r.note)))
        .chain(
            incoming
                .iter()
                .map(|r| format!("{} {} → {title}{}", name_of(&r.subject), r.predicate, note(&r.note))),
        )
        .take(8)
        .collect();
    if !rels.is_empty() {
        out.push_str("\n  ");
        out.push_str(&rels.join(" · "));
    }
    Some(out)
}

/// Markdown for an `explore` response.
fn render_explore(v: &Value) -> String {
    let s = |o: &Value, k: &str| o.get(k).and_then(|x| x.as_str()).unwrap_or("").to_string();
    let entity = &v["entity"];
    if entity.is_null() {
        let mut out = "No entity by that name.".to_string();
        if let Some(sugg) = v["suggestions"].as_array().filter(|a| !a.is_empty()) {
            out.push_str(" Did you mean:\n");
            for h in sugg {
                out.push_str(&format!("- {} `{}`\n", s(h, "title"), s(h, "id")));
            }
        }
        return out;
    }
    let mut out = format!("## {} ({}", s(entity, "name"), s(entity, "kind_of"));
    let status = s(entity, "status");
    if !status.is_empty() {
        out.push_str(&format!(", {status}"));
    }
    out.push_str(&format!(")  `{}`\n", s(entity, "id")));
    let summary = s(entity, "summary");
    if !summary.is_empty() {
        out.push_str(&format!("{summary}\n"));
    }
    if let Some(a) = entity["aliases_display"].as_array().filter(|a| !a.is_empty()) {
        let list: Vec<&str> = a.iter().filter_map(|x| x.as_str()).collect();
        out.push_str(&format!("Aliases: {}\n", list.join(", ")));
    }
    let note = |e: &Value| {
        let n = s(e, "note");
        if n.is_empty() { String::new() } else { format!(" — {n}") }
    };
    let out_edges = v["outgoing"].as_array().cloned().unwrap_or_default();
    let in_edges = v["incoming"].as_array().cloned().unwrap_or_default();
    if !out_edges.is_empty() || !in_edges.is_empty() {
        out.push_str("\n### Relations\n");
        for e in &out_edges {
            out.push_str(&format!("- {} → {}{}\n", s(e, "predicate"), s(e, "name"), note(e)));
        }
        for e in &in_edges {
            out.push_str(&format!(
                "- {} {} → {}{}\n",
                s(e, "name"),
                s(e, "predicate"),
                s(entity, "name"),
                note(e)
            ));
        }
    }
    if let Some(mems) = v["memories"].as_array().filter(|a| !a.is_empty()) {
        out.push_str("\n### Memories\n");
        for m in mems {
            let snippet: String = s(m, "content").replace('\n', " ").chars().take(160).collect();
            out.push_str(&format!("- [{}] {} — {} `id:{}`\n", s(m, "type"), s(m, "title"), snippet, s(m, "id")));
        }
    }
    out
}
```

- [ ] **Step 4: Commands**

In `src/main.rs`, add `use clap::Args;` next to the existing clap import, and these variants to `Command`:

```rust
    /// Show what memd knows about an entity, or add one (`memd entity add`).
    Entity(EntityArgs),
    /// Declare a relation: `memd relate "Lumen" part_of "Meilisearch Lab"`.
    Relate {
        subject: String,
        predicate: String,
        object: String,
        /// Optional note on the relation.
        #[arg(long)]
        note: Option<String>,
    },
    /// Remove a relation.
    Unrelate {
        subject: String,
        predicate: String,
        object: String,
    },
```

Add to `Search`: `#[arg(long)] entity: Option<String>,` `#[arg(long)] status: Option<String>,` `#[arg(long = "kind-of")] kind_of: Option<String>,`.

Add the argument types:

```rust
#[derive(Args)]
#[command(args_conflicts_with_subcommands = true)]
struct EntityArgs {
    #[command(subcommand)]
    action: Option<EntityAction>,
    /// Entity name, alias or id.
    name: Option<String>,
    /// 1, or 2 to include the neighbours' relations.
    #[arg(long, default_value_t = 1)]
    depth: u8,
}

#[derive(Subcommand)]
enum EntityAction {
    /// Create or update an entity.
    Add {
        name: String,
        /// company, team, person, project, product, service, customer, concept.
        #[arg(long)]
        kind: String,
        #[arg(long = "desc")]
        description: Option<String>,
        #[arg(long, value_delimiter = ',')]
        alias: Vec<String>,
        #[arg(long)]
        owner: Option<String>,
        #[arg(long)]
        status: Option<String>,
        #[arg(long)]
        url: Option<String>,
        #[arg(long)]
        scope: Option<String>,
    },
}
```

Dispatch:

```rust
        Command::Entity(args) => match args.action {
            Some(EntityAction::Add { name, kind, description, alias, owner, status, url, scope }) => {
                cli::entity_add(name, kind, description, alias, owner, status, url, scope).await
            }
            None => match args.name {
                Some(name) => cli::entity_show(name, args.depth).await,
                None => anyhow::bail!("usage: memd entity <name> | memd entity add <name> --kind <kind>"),
            },
        },
        Command::Relate { subject, predicate, object, note } => cli::relate(subject, predicate, object, note).await,
        Command::Unrelate { subject, predicate, object } => cli::unrelate(subject, predicate, object).await,
```

and `Command::Search { … }` passes `entity, status, kind_of` as three extra arguments.

In `src/cli.rs`:

```rust
pub async fn entity_show(name: String, depth: u8) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let kn = KnowledgeService::new(require_daemon(&cfg).await?);
    let scope = std::env::current_dir().ok().map(|p| p.to_string_lossy().to_string());
    let v = kn.explore(&name, depth, 10, scope.as_deref()).await?;
    print!("{}", render_explore(&v));
    Ok(())
}

#[allow(clippy::too_many_arguments)]
pub async fn entity_add(
    name: String,
    kind: String,
    description: Option<String>,
    aliases: Vec<String>,
    owner: Option<String>,
    status: Option<String>,
    url: Option<String>,
    scope: Option<String>,
) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let kn = KnowledgeService::new(require_daemon(&cfg).await?);
    let mut input = crate::knowledge::service::EntityInput::new(&name, &kind, Source::Cli);
    input.description = description;
    input.aliases = aliases;
    input.owner = owner;
    input.status = status;
    input.url = url;
    input.scope = scope;
    input.source_client = Some("cli".into());
    let out = kn.save_entity(input).await?;
    println!("{} {}", if out.created { "Created" } else { "Updated" }, out.id);
    for w in out.warnings {
        println!("⚠ {w}");
    }
    Ok(())
}

pub async fn relate(subject: String, predicate: String, object: String, note: Option<String>) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let kn = KnowledgeService::new(require_daemon(&cfg).await?);
    let rel = crate::knowledge::service::RelationInput { subject: Some(subject), predicate, object, note };
    let warnings = kn.relate(&[rel], "global", Source::Cli, Some("cli".into())).await;
    if warnings.is_empty() {
        println!("Related.");
    }
    for w in warnings {
        println!("⚠ {w}");
    }
    Ok(())
}

pub async fn unrelate(subject: String, predicate: String, object: String) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let kn = KnowledgeService::new(require_daemon(&cfg).await?);
    let deleted = kn.forget_relation(&subject, &predicate, &object).await?;
    println!("{}", if deleted { "Removed." } else { "No such relation." });
    Ok(())
}
```

`search` gains `entity: Option<String>, status: Option<String>, kind_of: Option<String>` parameters. Before the request:

```rust
    let entity = match entity {
        Some(n) => match crate::knowledge::resolve::resolve(&svc, &n, None).await? {
            crate::knowledge::resolve::Resolution::Found(id) => Some(id),
            crate::knowledge::resolve::Resolution::NotFound => Some(crate::knowledge::ident::entity_id(&n)?),
            crate::knowledge::resolve::Resolution::Ambiguous(c) => {
                bail!(crate::knowledge::resolve::ambiguity_message(&n, &c))
            }
        },
        None => None,
    };
```

and the `GetRequest` literal uses `entity, status, kind_of,`.

- [ ] **Step 5: Context header and doctor back-fill**

In `context`, after `let scope = crate::memory::service::normalize_scope(scope.as_deref());` add:

```rust
    let kn = KnowledgeService::new(svc.clone());
    let header = project_header(&kn, &scope).await.unwrap_or(None);
```

Replace `if hits.is_empty() { return Ok(()); }` with `if hits.is_empty() && header.is_none() { return Ok(()); }`, and right after the intro paragraph is pushed to `out`, insert:

```rust
    if let Some(h) = &header {
        out.push_str(h);
        out.push_str("\n\n");
    }
```

Add the helper:

```rust
/// The current project's header block, if the scope belongs to one.
async fn project_header(kn: &KnowledgeService, scope: &str) -> Result<Option<String>> {
    let Some(id) = kn.project_for_scope(scope).await? else {
        return Ok(None);
    };
    let Some(entity) = kn.memories().get_item(&id).await? else {
        return Ok(None);
    };
    let one = std::slice::from_ref(&id);
    let outgoing = kn.relations().by_subjects(one).await?;
    let incoming = kn.relations().by_objects(one).await?;
    let mut ids: Vec<String> = outgoing
        .iter()
        .map(|r| r.object.clone())
        .chain(incoming.iter().map(|r| r.subject.clone()))
        .chain(entity.knowledge.owner.clone())
        .collect();
    ids.sort();
    ids.dedup();
    let names: HashMap<String, String> = if ids.is_empty() {
        HashMap::new()
    } else {
        kn.memories()
            .client()
            .fetch_docs(&crate::knowledge::ident::in_filter("id", &ids), &["id", "name"])
            .await?
            .into_iter()
            .filter_map(|d| Some((d.get("id")?.as_str()?.to_string(), d.get("name")?.as_str()?.to_string())))
            .collect()
    };
    Ok(context_header(&entity, &outgoing, &incoming, &names))
}
```

In `doctor`, inside the `if fix {` block after the task prune:

```rust
            match KnowledgeService::new(svc.clone()).backfill_project_links().await {
                Ok(n) => println!("Links:         linked {n} memories to their project entity"),
                Err(e) => println!("Links:         back-fill failed — {e}"),
            }
```

- [ ] **Step 6: Remove the temporary allow**

In `src/main.rs`, delete the two comment lines and `#[allow(dead_code)]` above `mod knowledge;`. Every item must now be used; fix any `dead_code` error clippy reports by deleting the unused item, not by re-adding the allow.

- [ ] **Step 7: Run the tests to verify they pass**

Run: `cargo test`
Expected: all pass (4 new CLI tests).

- [ ] **Step 8: Gate and commit**

```bash
cargo fmt --all && cargo clippy --all-targets -- -D warnings && cargo test
git add src/
git commit -m "feat(cli): entity, relate, unrelate, entity filters, project header, back-fill"
```

---

### Task 11: Docs, live verification, pull request

**Files:**
- Create: `docs/knowledge.mdx`
- Modify: `docs/mint.json`, `docs/mcp.mdx`, `README.md`

**Interfaces:**
- Consumes: the finished feature.
- Produces: documentation and a pull request.

- [ ] **Step 1: `docs/knowledge.mdx`**

~~~~mdx
---
title: Knowledge
description: "Entities, relations and a shared vocabulary across every agent."
---

memd stores more than notes. It keeps the **things** in your world —
companies, teams, people, projects, products, services, customers, concepts —
and the **relations** between them, so any agent can ask "what is Lumen and
what depends on it?" and get one structured answer.

memd never runs a model. Agents name things as they save, and the crawler adds
one project entity per git repository it finds.

## Entities

An entity is a memory with `type = entity`:

| Field | Meaning |
|-------|---------|
| `name` | Canonical display name. |
| `kind_of` | `company`, `team`, `person`, `project`, `product`, `service`, `customer`, `concept`. |
| `aliases` | Other names. Saving under an alias updates the same entity. |
| `owner` | Owning person, team or company. |
| `status` | `active`, `archived`, … (`stub` for names nobody described yet). |
| `summary` | The description. |

Ids are `entity_<slug>` — `Meilisearch Lab`, `meilisearch-lab` and
`MEILISEARCH LAB` are the same entity.

## Relations

One edge per fact: `Lumen —part_of→ Meilisearch Lab`. Suggested predicates:
`part_of`, `depends_on`, `owns`, `uses`, `replaces`, `works_on`, `member_of`,
`customer_of`, `related_to`. Any verb phrase works and is normalised
(`Depends On` → `depends_on`).

Naming an entity that does not exist yet creates a **stub**, so a relation
always lands somewhere. Describing it later with `save_entity` fills the stub.

## Automatic links

- Every git repository becomes a `project` entity, described by its README's
  heading and first paragraph. An agent can enrich it; the crawler then leaves
  it alone.
- Every crawled file in a repository, and every memory saved with that
  repository's scope, is linked to the project entity.
- A new session in a described project starts with one line about it:

  ```
  **memd** — project · owner: Quentin · status: active
    part_of → Meilisearch side projects · uses → Meilisearch (local engine, pinned)
  ```

## MCP tools

| Tool | Purpose |
|------|---------|
| `save_entity(name, kind_of, description?, aliases?, owner?, status?, scope?, tags?, url?, relations?)` | Create or update an entity. |
| `explore(name, depth?, limit?, scope?)` | The entity, its relations both ways, related entities, and memories that mention it. |
| `forget_relation(subject, predicate, object)` | Remove one relation. |
| `save_memory(…, entities?, relations?)` | Link a memory to entities and declare relations. |
| `get_memory` / `list_memories` `(…, entity?, status?, kind_of?)` | Filter by entity, status, kind. |

## CLI

```sh
memd entity "Lumen"                    # explore
memd entity "Lumen" --depth 2
memd entity add "Lumen" --kind product --desc "LLM gateway" --alias lumen-gw --owner Quentin
memd relate "Lumen" part_of "Meilisearch Lab" --note "data plane"
memd unrelate "Lumen" part_of "Meilisearch Lab"
memd search "billing" --entity "Meilisearch Lab" --status accepted
memd doctor --fix                      # links existing memories to their project
```
~~~~

In `docs/mint.json`, add `"knowledge"` to the `Usage` group after `"mcp"`.

In `docs/mcp.mdx`, under `## Tools`, add a sentence and link: `Entities and relations: see [Knowledge](/knowledge) — save_entity, explore, forget_relation.`

In `README.md`, the MCP tools table gains three rows:

```markdown
| `save_entity(name, kind_of, …, relations?)` | Create/update a company, team, person, project, product, service, customer or concept |
| `explore(name, depth?)` | An entity, its relations both ways, and the memories that mention it |
| `forget_relation(subject, predicate, object)` | Remove one relation |
```

and the CLI block gains:

```
memd entity <name> [--depth 2]               Explore an entity
memd entity add <name> --kind <kind> [...]   Create/update an entity
memd relate <subject> <predicate> <object>   Declare a relation
```

Commit:

```bash
git add docs/ README.md
git commit -m "docs: knowledge — entities, relations, project entities"
```

- [ ] **Step 2: Deploy locally**

```bash
cargo build --release
memd down
rm -f ~/.local/bin/memd && cp target/release/memd ~/.local/bin/memd   # rm first: overwriting in place gets the binary SIGKILLed on macOS
memd up
memd crawl run
```

Expected: `memd up` reports healthy; the crawl reports `indexed` ≥ the number of git repositories (one project entity each) and `0 errors`.

- [ ] **Step 3: Verify against the live engine**

Use names prefixed `memd-plan-test-` and clean them up at the end.

```bash
M=~/.local/bin/memd
call() { printf '%s\n' "{\"jsonrpc\":\"2.0\",\"id\":1,\"method\":\"tools/call\",\"params\":{\"name\":\"$1\",\"arguments\":$2}}" | $M mcp --stdio; }

call save_entity '{"name":"memd-plan-test Lab","kind_of":"product","aliases":["mpt-lab"],"description":"Test entity."}'
call save_entity '{"name":"mpt-lab","kind_of":"product","tags":["t"]}'        # alias → same id, created:false
call save_entity '{"name":"memd-plan-test Gateway","kind_of":"service","relations":[{"predicate":"part_of","object":"memd-plan-test Lab"},{"predicate":"uses","object":"memd-plan-test Unknown"}]}'
call explore '{"name":"memd-plan-test Lab","depth":2}'                        # incoming part_of from Gateway
call explore '{"name":"memd-plan-test Unknown"}'                              # status: stub
call save_memory '{"content":"memd-plan-test: gateway uses leases","entities":["memd-plan-test Gateway"],"scope":"'"$PWD"'"}'
call get_memory '{"query":"leases","entity":"memd-plan-test Gateway"}'        # finds it; entities include the memd project id
$M entity memd
$M context --agent codex --scope "$PWD" </dev/null | head -5
$M doctor --fix | grep Links
```

Expected, in order: `created: true`; `created: false` with the same id; `created: true` and no warnings; `incoming` lists the gateway; the unknown entity has `"status": "stub"`; an id; one hit whose `entities` contain `entity_memd-plan-test-gateway` and `entity_memd`; the `memd` project entity printed with its README headline; the context header line (only if the memd project has relations or is agent-owned — otherwise none, which is also correct); a `Links:` line.

Clean up:

```bash
call forget_relation '{"subject":"memd-plan-test Gateway","predicate":"part_of","object":"memd-plan-test Lab"}'
call forget_relation '{"subject":"memd-plan-test Gateway","predicate":"uses","object":"memd-plan-test Unknown"}'
for id in entity_memd-plan-test-lab entity_memd-plan-test-gateway entity_memd-plan-test-unknown; do call forget_memory "{\"id\":\"$id\"}"; done
$M search "memd-plan-test" --limit 5      # forget the test memory by the id it prints
```

- [ ] **Step 4: Open the pull request**

```bash
git push -u origin qdequele/knowledge-graph
gh pr create --base main --title "feat: typed records and relations (knowledge base, sub-project 1)" --body "<summary of the spec, the five deviations, and the live verification results>

🤖 Generated with [Claude Code](https://claude.com/claude-code)"
```

The branch sits on top of PR #9 (`qdequele/memory-quality`). If PR #9 is not merged yet, open this one with `--base qdequele/memory-quality` instead of `main`.
