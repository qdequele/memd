# memd memory history — design

**Date:** 2026-06-15
**Status:** Approved
**Author:** Quentin de Quelen (with Claude)

## Goal

Give users an auditable timeline of what changed in their memory store: which
memories were created, updated, or deleted, when, and by which source. Today,
once a memory is updated or forgotten its prior state is gone and there is no
record the event happened.

Scope is an **audit timeline** — metadata only. We do not retain old content,
diffs, or an undo/restore capability (explicitly out of scope).

## Storage — a `memory_events` index

A second Meilisearch index alongside `memories`, created idempotently at daemon
startup. It is a plain log: **no embedder**. One document per event.

| field | type | notes |
|---|---|---|
| `id` | string | UUIDv7 primary key (time-ordered) |
| `ts` | i64 | unix seconds — sortable |
| `action` | string | `create` \| `update` \| `delete` \| `crawl` — filterable |
| `memory_id` | string | affected memory id (empty for `crawl`) — filterable |
| `title` | string | snapshot of the memory title (readable after delete) — searchable |
| `type` | string | memory type snapshot — filterable |
| `scope` | string | scope snapshot — filterable |
| `source` | string | `cli` \| `mcp` \| `crawler` — filterable |
| `source_client` | string? | optional client id |
| `detail` | string? | for `crawl`: `"12 indexed, 3 updated, 2 deleted, 0 errors"` — searchable |

Index settings:

- `filterableAttributes = [action, type, scope, source, memory_id, ts]`
- `sortableAttributes = [ts]`
- `searchableAttributes = [title, detail, memory_id]`

No backfill: the log starts empty and accrues going forward — past mutations
cannot be reconstructed. The events index is included automatically in dumps and
engine migrations because those operate instance-wide.

## Recording — single chokepoint in `MemoryService`

`MeiliClient` becomes index-aware: add an `index: String` field (defaulting to
`"memories"`, preserving every existing call site) and a `for_index(uid)` helper
returning a clone pointed at another index. Events reuse the same upsert/search
plumbing.

`MemoryService` gains an `EventLog` (a client bound to `memory_events`):

- `save` logs a `create` **only on a real write** — the content-hash dedup
  early-return logs nothing.
- `update` and `forget` take a new `Source` argument so the event records who
  performed it. `forget` reads the doc's metadata **before** deleting, to
  snapshot title/type/scope into the event.
- Recording is **best-effort and off the critical path**: fire-and-forget, no
  Meilisearch task wait. A logging failure never blocks or fails a memory
  mutation — mirroring the existing `bump_accessed` pattern. (Consequence: the
  log is eventually consistent; an event may not be queryable for a few hundred
  ms after the mutation.)
- The **crawler** stays event-free per file. `crawler::scan` emits a single
  `crawl` summary event at the end, built from its existing `CrawlSummary`.

Access bumps (`bump_accessed`, the `read` last-accessed write) go straight to
the client, not through `save/update`, so they correctly produce no events.

Rejected alternative: logging at each call site (CLI/MCP/crawler). Three places
to keep in sync and easy to miss a path; the service is the single funnel every
mutation already passes through.

## Surface — CLI + MCP

### CLI

```
memd history [--action <create|update|delete|crawl>] [--type <t>] \
             [--scope <s>] [--since <span>] [--limit <n>]
```

Newest first (`ts:desc`). Reuses `parse_since` (`30d`, `12h`, `45m`, unix secs).
One line per event:

```
2026-06-15T10:30:00Z  update  [fact] My title  (mcp · scope: global)
2026-06-15T10:28:11Z  crawl   12 indexed, 3 updated, 2 deleted, 0 errors  (crawler)
```

### MCP

A `history` tool with the same filters (`action`, `type`, `scope`, `since`,
`limit`), returning lightweight JSON rows so an agent can ask "what changed
recently."

### Status (optional)

A `history: N events` line in `memd status`.

## Testing

Unit tests for the pure pieces (no live Meilisearch), matching the existing
unit-test style:

- event construction from each mutation kind (create/update/delete/crawl)
- the history filter-expression builder
- the CLI line renderer
- `Source` round-trips into the event record

## Out of scope

- Before/after content diffs
- Undo / restore / revert
- Per-file crawler events (summary only)
