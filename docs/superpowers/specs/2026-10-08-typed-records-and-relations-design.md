# Typed records and relations — design

- **Date:** 2026-10-08
- **Status:** draft, awaiting review
- **Sub-project:** 1 of 3 (knowledge base). Builds on the memory-quality work
  in PR #9 (hierarchical scopes, agent knowledge roots).

## 1. Intent

memd today stores flat text memories. The user wants it to hold *structured*
knowledge about their world — companies, teams, people, projects, products,
services, customers, concepts — with consistent names, typed relations between
them, decisions with a lifecycle, and (later) live work state such as branches.
An agent in any tool should be able to ask "what is Lumen, what depends on it,
who owns it?" and get a graph-shaped answer, not ten snippets.

**Constraints agreed with the user**

- memd stays model-free. Agents are the extractors: tools accept structured
  fields, and directives make agents fill them when they save. memd only adds
  deterministic structure from disk (projects now; branches in sub-project 2).
- Meilisearch remains the only store. Relations live in a second index, the
  same pattern as the existing `memory_events` audit log.
- Token-safe by default, like every other memd query.

**Success looks like**

- `explore("Lumen")` from Claude Code, Codex or Gemini CLI returns the entity,
  its relations in both directions, related entities, and the memories that
  mention it, in one call.
- Every memory saved with a project scope is reachable from that project's
  entity without anyone tagging it.
- Saving the same entity twice, under a name or an alias, updates one record.

**Out of scope (later sub-projects):** branches, tasks, PRs and session
"where things stand" (2); decision supersession rules, alias-driven vocabulary
cleanup, the gap-filling skill (3); any LLM-driven extraction inside memd.

## 2. Data model

### 2.1 Memories index (`memories`)

The existing `type` field is the record kind. It gains one value: `entity`.
`decision` and `task` stay as they are and gain fields. New optional fields,
all absent unless set:

| Field | Type | Filterable | Used by | Meaning |
|---|---|---|---|---|
| `name` | string | no (searchable) | entity | Canonical display name, e.g. `Meilisearch Lab`. |
| `name_key` | string | yes | entity | `normalize(name)`; the identity key. |
| `aliases` | string[] | yes | entity | Alternative names; stored normalised **and** displayed from `aliases_display`. |
| `aliases_display` | string[] | no | entity | Aliases as typed, for output. |
| `kind_of` | enum | yes | entity | `company`, `team`, `person`, `project`, `product`, `service`, `customer`, `concept`. |
| `status` | string | yes | entity, decision, task | Free text with suggested values: entity `active`/`archived`/`stub`; decision `proposed`/`accepted`/`superseded`; task `open`/`done`. |
| `owner` | string | yes | entity | An entity id (`entity:…`), resolved from a name on save. |
| `supersedes` | string | yes | decision | A memory id. Stored only; rules come in sub-project 3. |
| `path` | string | no | entity (project) | Absolute directory for project entities. |
| `url` | string | no | entity | Home page, repo, dashboard. |
| `entities` | string[] | yes | any | Entity ids this record mentions or belongs to. |

`content` remains the free-text body (for an entity: its description).
`title` for an entity is its `name`. Searchable attributes gain `name` and
`aliases_display`. The embedder document template becomes
`{{doc.title}} {{doc.content}} {{doc.tags}} {{doc.aliases_display}}` so an
alias search hits semantically too.

**Entity identity.** `id = "entity:" + slug(name)` where
`slug` = lowercase, Unicode-normalised (NFKD, marks stripped), every run of
non-alphanumeric characters replaced by a single `-`, trimmed. `name_key` is
the same slug. Saving `Meilisearch Lab`, `meilisearch-lab` or `MEILISEARCH LAB`
therefore produces one record. Aliases are stored as slugs in `aliases` and as
typed in `aliases_display`.

### 2.2 Relations index (`memory_relations`)

One document per directed edge.

| Field | Type | Filterable | Meaning |
|---|---|---|---|
| `id` | string | — | `sha256(subject + "\0" + predicate + "\0" + object)` (hex). |
| `subject` | string | yes | Entity id. |
| `predicate` | string | yes | Normalised verb phrase: lowercase, spaces → `_` (`part_of`, `depends_on`). |
| `object` | string | yes | Entity id. |
| `note` | string | no (searchable) | Optional free text ("via the /admin API, ADR 010"). |
| `source` | string | yes | `mcp`, `cli`, `crawler`. |
| `source_client` | string | yes | e.g. `claude-code`. |
| `scope` | string | yes | Scope of the memory that declared it, or `global`. |
| `created_at`, `updated_at` | int | yes, sortable | Unix seconds. |

No embedder. Suggested predicates, listed in the tool description and not
enforced: `part_of`, `depends_on`, `owns`, `uses`, `replaces`, `works_on`,
`member_of`, `customer_of`, `related_to`.

### 2.3 Index settings

`ensure_index` adds the new filterable/searchable attributes to `memories` and
creates `memory_relations` with the settings above. Changing filterable
attributes and the document template re-indexes and re-embeds the ~700
existing documents once at daemon start (minutes with the local embedder).
This is acceptable and already how settings changes are applied.

## 3. Tools and CLI

### 3.1 MCP tools (8 → 11)

**`save_entity`** — `name` (required), `kind_of` (required), `description?`,
`aliases?`, `owner?` (name), `status?`, `scope?`, `tags?`, `url?`,
`relations?: [{predicate, object, note?}]` (subject is this entity). Upserts
by identity (§2.1). On an existing entity: fields that were passed overwrite,
fields absent are preserved, `aliases` and `tags` are merged, relations are
added. Returns `{ id, created: bool }`.

**`explore`** — `name` (required), `depth?` (1, max 2), `limit?` (memories,
default 10, max 50). Resolves the name (§4), then returns:

```json
{
  "entity":   { …entity row with content… },
  "outgoing": [ { "predicate": "part_of", "object": "entity:meilisearch-lab", "name": "Meilisearch Lab", "note": "…" } ],
  "incoming": [ { "predicate": "depends_on", "subject": "entity:glutony", "name": "glutony" } ],
  "related":  [ { …lightweight entity rows for every neighbour… } ],
  "memories": [ …lightweight rows (snippet) whose `entities` contain the id, newest first… ],
  "neighbours": { "<entity id>": { "outgoing": […], "incoming": […] } }   // only when depth = 2
}
```

An unresolved name returns `{ "entity": null, "suggestions": [ …top 5 entity rows by hybrid search… ] }`.

**`forget_relation`** — `subject`, `predicate`, `object` (names or ids).
Returns `{ deleted: bool }`.

**Changed tools.** `save_memory` and `update_memory` accept `entities?:
[names]` and `relations?: [{subject, predicate, object, note?}]` (subject
required here, since a plain memory is not an entity). `get_memory` and
`list_memories` accept `entity?` (a name; resolved then filtered as
`entities = <id>`), `status?` and `kind_of?`. `stats` facets include
`kind_of` and `status`. `history` records entity and relation mutations like
any other memory mutation (`type = entity`; relations as action `relate` /
`unrelate`).

Tool descriptions carry the kind and predicate vocabularies so agents pick
consistent values without a lookup.

### 3.2 CLI

```
memd entity <name> [--depth 2]                      # explore, printed as markdown
memd entity add "<name>" --kind project [--alias a,b] [--owner X] [--desc "…"] [--url …]
memd relate "<subject>" <predicate> "<object>" [--note "…"]
memd unrelate "<subject>" <predicate> "<object>"
memd search "<q>" --entity "<name>" [--status …] [--kind-of …]
```

### 3.3 Directives and server instructions

Both gain two sentences: *When you save, name the entities involved
(`entities`) and declare any relation you learned (`relations`). Before asking
the user what something is, call `explore` with its name.*

## 4. Resolution and identity

`resolve(name, scope) -> Resolution`:

1. Compute `key = slug(name)`.
2. Filter `type = entity AND (name_key = key OR aliases = key)`, restricted to
   the scope chain (project → ancestors → global → sub-scopes) when a scope is
   given, otherwise unrestricted.
3. Zero hits → `NotFound`. One hit → `Found(id)`. Several hits → `Ambiguous(ids)`;
   the caller returns an error listing them (names, kinds, scopes) so the agent
   can pick by id. Ids (`entity:…`) are accepted anywhere a name is and skip
   resolution.

**Stubs.** On `save_memory`/`save_entity`, every unresolved name in
`entities`, `owner`, or a relation endpoint creates a stub: `type = entity`,
`kind_of = concept`, `status = stub`, empty content, scope = the saving
record's scope. Stubs are ordinary entities; `save_entity` later fills them and
clears `status = stub` (unless a status is passed). Stubs make every edge land
somewhere and give sub-project 3's gap-filling skill its worklist.

**Merging.** `save_entity` on an existing id: passed scalar fields overwrite;
`aliases`, `tags` union (by slug / exact); relations upsert by their id; the
incoming name, if it differs from the stored `name` only in case or
punctuation, does not change the display name. A different `kind_of` on a
non-stub entity overwrites with a warning in the response (`"warnings": [...]`).

## 5. Automatic links

**Project entities from disk.** During a scan, every git repository found
becomes a project entity:

- `id = "entity:" + slug(basename)`. Basenames are not unique across roots
  (`_demos/console` vs `meilisearch-cloud/console`), so on collision with an
  existing project entity whose `path` differs, the newcomer uses
  `slug(parent basename + "-" + basename)`. The first repo to claim a slug
  keeps it.
- `name` = basename, `kind_of = project`, `path` = repo path, `scope` = repo
  path, `source = crawler`.
- `content` = the README's first heading and first paragraph, at most 400
  chars, or empty when there is no README.

Re-runs are no-ops when nothing changed (content hash, as for every crawled
doc). When a repo's path disappears, its project entity is deleted with the
other stale crawler docs, unless an agent has written to it. The first agent
write switches `source` to `mcp`/`cli`; such an entity is kept and its
`status` set to `archived`.

**Crawled files → project.** Every crawled document under a repository carries
that project's entity id in `entities`.

**Scoped memories → project.** On every save, if the memory's scope resolves
to a project entity (exact path match, else nearest ancestor path), that id is
added to `entities`. `doctor --fix` back-fills existing non-crawler memories
once.

## 6. Session start

When `memd context`'s scope resolves to a project entity, the output starts
with one block before the memory list:

```
**memd** — project · owner: Quentin · status: active
  part_of → Meilisearch side projects · uses → Meilisearch (local engine, pinned)
```

Only direct relations, at most 8, with notes truncated to 60 chars. Nothing
is printed when the project entity is a crawler-only stub with no relations,
to keep the hook silent for repos nobody has described yet.

## 7. Errors and limits

- Relation writes are best-effort: a failed write to `memory_relations` is
  logged and the memory save still returns its id (same policy as the audit
  log). The response carries `"warnings"` listing relations that failed.
- Invalid `kind_of` → error with the allowed list. `status` is free text.
- `explore`: `depth` > 2 is clamped to 2; `limit` > 50 clamped to 50;
  `related` capped at 50 entities; `neighbours` fetched in one filtered search
  per direction (`subject IN [...]`, `object IN [...]`), not per node.
- `Ambiguous` resolution is always an error, never a guess.
- Names longer than 200 chars are rejected.

## 8. Implementation shape

New module `src/knowledge/`:

- `ident.rs` — `slug`, `normalize_predicate`, `relation_id`. Pure.
- `relations.rs` — `RelationStore` over `MeiliClient::for_index("memory_relations")`:
  `ensure`, `upsert_many`, `delete`, `by_subject(ids)`, `by_object(ids)`.
- `resolve.rs` — `resolve(name, scope)` and stub creation, over `MemoryService`.
- `service.rs` — `KnowledgeService { memories, relations }`: `save_entity`,
  `explore`, `forget_relation`, `link_scope_to_project`, `project_entity_for_repo`.

Touch points: `memory/model.rs` (new fields, `MemoryType::Entity`),
`meili/client.rs` (index settings, `ensure_relations_index`),
`mcp/protocol.rs` (3 tools, extended params), `crawler/mod.rs` (project
entities + `entities` on crawled docs), `cli.rs` (`entity`, `relate`,
`unrelate`, `--entity`, `context` header, `doctor --fix` back-fill),
`agents/directives.rs` + `mcp/protocol.rs` instructions, docs (`mcp.mdx`,
new `knowledge.mdx`), README.

## 9. Testing

Unit (pure, no engine): `slug` (case, accents, punctuation, idempotence),
`relation_id` stability and direction, `normalize_predicate`, resolution
precedence (exact name over alias, scope chain order, ambiguity), merge
semantics (overwrite vs preserve vs union), project-entity id collision rule,
`explore` response shaping from fixture rows, filter builders for the new
params, README headline extraction.

Against the live engine (manual, as for PR #9): build, run through the stdio
bridge — `save_entity` twice with an alias, `explore` depth 1 and 2,
`save_memory` with a relation to an unknown name (stub appears), `memd
context` header for a described project, `doctor --fix` back-fill count.

## 10. Migration

- First daemon start applies the new index settings and creates
  `memory_relations`.
- `memd crawl run --reset` is not required; the next scan creates project
  entities and adds `entities` to crawled docs (content hash unchanged → the
  `entities` field is set through a PUT merge, not a re-embed).
- `memd doctor --fix` back-fills `entities` on existing agent/user memories
  by scope and reports the count.
- Existing `decision` memories have no `status`; readers treat absence as
  `accepted`.
