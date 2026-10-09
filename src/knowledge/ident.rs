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
        bail!(
            "unknown kind_of `{k}` (expected one of: {})",
            KINDS.join(", ")
        )
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
        assert_eq!(
            entity_id("Meilisearch Lab").unwrap(),
            "entity_meilisearch-lab"
        );
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
                id.chars()
                    .all(|c| c.is_ascii_alphanumeric() || c == '-' || c == '_'),
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
