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
    "id",
    "name",
    "kind_of",
    "status",
    "owner",
    "scope",
    "path",
    "url",
    "summary",
    "aliases_display",
    "source",
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
            decide(
                "entity_lab",
                false,
                &[hit("entity_meilisearch-lab", "global")],
                Some("/x")
            ),
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
        assert!(
            msg.contains("entity_a") && msg.contains("entity_b") && msg.contains("Pass the id"),
            "{msg}"
        );
    }

    #[test]
    fn alias_filter_escapes_the_key() {
        assert_eq!(
            alias_filter("o-reilly"),
            "type = 'entity' AND aliases = 'o-reilly'"
        );
    }
}
