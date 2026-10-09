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
            .map(|r| {
                edge(
                    &r.predicate,
                    "object",
                    &r.object,
                    name_of(&r.object),
                    &r.note,
                )
            })
            .collect()
    };
    let inc = |rels: &[Relation]| -> Vec<Value> {
        rels.iter()
            .map(|r| {
                edge(
                    &r.predicate,
                    "subject",
                    &r.subject,
                    name_of(&r.subject),
                    &r.note,
                )
            })
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
                map.insert(
                    id.clone(),
                    json!({ "outgoing": out(&o), "incoming": inc(&i) }),
                );
            }
        }
        v["neighbours"] = Value::Object(map);
    }
    v
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::memory::Source;
    use serde_json::json;

    fn rel(s: &str, p: &str, o: &str, note: Option<&str>) -> Relation {
        Relation::new(
            s,
            p,
            o,
            note.map(String::from),
            Source::Mcp,
            None,
            "global",
            1,
        )
        .unwrap()
    }

    #[test]
    fn shapes_both_directions_with_names() {
        let out = [rel(
            "entity_lumen",
            "part_of",
            "entity_meilisearch-lab",
            Some("data plane"),
        )];
        let inc = [rel("entity_glutony", "depends_on", "entity_lumen", None)];
        let related = vec![
            json!({ "id": "entity_meilisearch-lab", "name": "Meilisearch Lab" }),
            json!({ "id": "entity_glutony", "name": "glutony" }),
        ];
        let v = shape(
            json!({ "id": "entity_lumen" }),
            &out,
            &inc,
            related,
            vec![json!({"id": "m1"})],
            None,
        );
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
        let second = [
            rel("entity_b", "uses", "entity_c", None),
            rel("entity_d", "owns", "entity_b", None),
        ];
        let v = shape(json!({}), &out, &[], related, vec![], Some(&second));
        assert_eq!(
            v["neighbours"]["entity_b"]["outgoing"][0]["object"],
            "entity_c"
        );
        assert_eq!(
            v["neighbours"]["entity_b"]["incoming"][0]["subject"],
            "entity_d"
        );
    }
}
