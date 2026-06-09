# PRD — `memd`

> **Universal local memory daemon for LLMs, backed by Meilisearch.**
> A single always-on service that turns a local Meilisearch instance into a persistent, time-aware, semantically-searchable memory layer shared across every LLM tool you use (Claude Code, Codex, Perplexity, Cursor, ChatGPT desktop, …), exposed through one stable MCP server.

- **Status:** Draft v0.1
- **Author:** Quentin de Quelen
- **Date:** 2026-05-30
- **Binary:** `memd` (daemon + CLI) · **Package:** `memd`
- **Stack:** Rust · Meilisearch (local) · MCP (Model Context Protocol)

---

## 1. Problem

Every LLM tool keeps its own siloed, ephemeral notion of "memory":

- Claude Code has per-project `CLAUDE.md` + a private memory dir.
- Codex / Cursor have `AGENTS.md`, `.cursorrules`, scattered instruction files.
- Perplexity / ChatGPT / cloud assistants keep memory server-side, opaque and non-portable.
- "What projects do I have locally and what do they do?" lives only in READMEs nobody re-reads.

There is **no single, local, queryable source of truth** that all of these tools can read from and write to. Knowledge written in one place (a decision, a preference, a project overview) is invisible to every other tool. Context is rebuilt from scratch in every session.

## 2. Vision

A local-first **memory substrate** that:

1. **Runs always-on** as a background daemon, managing a dedicated local Meilisearch.
2. Is **fed two ways**:
   - **Actively** — any LLM/agent calls `save_memory` / `get_memory` through one MCP server.
   - **Passively** — a crawler indexes meaningful files already on disk (project READMEs, `AGENTS.md`/`CLAUDE.md`/`agent.md`, scattered memory files, annotated "important" files) and keeps them in sync.
3. Makes every memory **time-aware** (created / updated / last-accessed) and **classified** (type, tags, scope, source).
4. Is **searchable semantically** (hybrid keyword + vector via Meilisearch embedders), local by default.
5. Exposes **one stable MCP** that any model can mount — the long-term ambition being a generic *model harness*: a single integration point that enriches any LLM with persistent memory (and, later, more capabilities).

**North star:** *Open a new chat in any tool → it already knows the relevant context about your machine, projects, and preferences — because they all share `memd`.*

## 3. Goals & Non-Goals

### Goals
- One persistent, local memory store usable by **all** MCP-capable clients.
- Stable, minimal MCP surface: `save_memory`, `get_memory` first.
- Automatic ingestion of existing on-disk knowledge (READMEs, agent instruction files, memory files).
- Time-stamped, classified, semantically-searchable memories.
- Zero-config "it just works" default: `memd up` starts Meilisearch + crawler + MCP.
- Local-first & private by default (no content leaves the machine unless explicitly opted in).

### Non-Goals (for now)
- Cloud sync / multi-machine replication (later, explicitly out of MVP).
- A GUI (CLI + MCP only at first).
- Being a general document store / RAG-over-arbitrary-corpora product — scope is *personal memory about the user's machine and work*.
- Replacing each tool's native config files — `memd` *indexes* them, it does not own them.
- Fine-grained multi-user / auth (single-user, localhost).

## 4. Users & Use Cases

**Primary user:** the developer (Quentin) running many LLM tools on one machine.

Representative use cases:
- *"Remember that we decided X for project Y."* → an agent calls `save_memory`; tomorrow another tool retrieves it.
- *"What does the `scrapix` project do and where is it?"* → `get_memory` returns the README-derived overview + path.
- *"What instructions apply when I work on Meilisearch repos?"* → returns indexed `CLAUDE.md` / `AGENTS.md` content scoped to that path.
- *"What did I note about Postgres tuning last month?"* → time-filtered semantic search.
- A new Codex session auto-pulls relevant memories at startup via MCP.

## 5. Core Concepts & Data Model

### 5.1 Memory item
The atomic unit. Stored as one Meilisearch document in the `memories` index.

| Field | Type | Notes |
|------|------|------|
| `id` | string | Stable. Manual: UUIDv7 (time-ordered). Crawled: hash of `source_path`. |
| `content` | string | The memory text (or extracted file content). |
| `title` | string? | Short label; auto-derived if absent. |
| `summary` | string? | Optional model-generated summary (enrichment phase). |
| `type` | enum | `fact`, `preference`, `decision`, `task`, `project_overview`, `agent_instruction`, `file_annotation`, `code_note`, `reference`. |
| `tags` | string[] | Freeform, filterable. |
| `scope` | string | `global` or a path prefix (e.g. `~/Projects/Meilisearch`). Enables project-scoped recall. |
| `source` | enum | `mcp` (an agent wrote it), `crawler` (from a file), `cli` (manual). |
| `source_path` | string? | Absolute file path when applicable. |
| `source_client` | string? | Which tool wrote it (e.g. `claude-code`, `codex`). |
| `created_at` | int (unix s) | Sortable + filterable. |
| `updated_at` | int (unix s) | Sortable + filterable. |
| `last_accessed_at` | int? | Bumped on retrieval; powers recency/decay. |
| `content_hash` | string | Skip re-embedding unchanged content. |
| `_vectors` | object | Meilisearch embedder output (managed by MS). |

**Index config:**
- Searchable: `title`, `content`, `summary`, `tags`.
- Filterable: `type`, `tags`, `scope`, `source`, `source_path`, `created_at`, `updated_at`.
- Sortable: `created_at`, `updated_at`, `last_accessed_at`.
- Embedder configured for **hybrid search**.

### 5.2 Single index, filtered by type
One `memories` index. Avoids cross-index query complexity; `type`/`scope` filters carve out subsets. (Revisit only if scale demands it.)

## 6. Architecture

```
                       ┌─────────────────────────────────────────────┐
                       │                 memd daemon                  │
   MCP clients         │  (launchd service, always-on)                │
 ┌───────────┐         │                                              │
 │ Claude Code│◄──MCP──┤  ┌────────────┐   ┌───────────────────────┐  │
 │  Codex     │  HTTP  │  │ MCP server │   │  Crawler + file watch │  │
 │  Cursor …  │  /stdio│  │ save/get   │   │  (notify + periodic)  │  │
 └───────────┘         │  └─────┬──────┘   └───────────┬───────────┘  │
                       │        │                      │              │
                       │        ▼                      ▼              │
                       │  ┌──────────────────────────────────────┐   │
                       │  │     Memory service (core lib)         │   │
                       │  │  classify · dedup · embed · timestamp │   │
                       │  └──────────────────┬───────────────────┘   │
                       │                     ▼                        │
                       │            ┌──────────────────┐              │
                       │            │ Meilisearch (mgr) │  child proc │
                       │            │  index: memories  │  on :7701   │
                       │            └──────────────────┘              │
                       └─────────────────────────────────────────────┘
                          embeddings done *inside* Meilisearch (local huggingFace embedder)
```

### 6.1 Components (each independently testable)
1. **Meilisearch manager** — download/pin a Meilisearch **binary** (no Docker); start/stop/health-check it as a child process; dedicated instance on `127.0.0.1:7701` with a local-only key, data dir under memd's app dir. Owns lifecycle (incl. restart on crash). On first run, configures the index embedder.
2. **Memory service (core lib)** — the heart. CRUD over memories: classify (heuristics), dedup (content hash), stamp timestamps, ensure index settings, run hybrid search. Pure-ish, unit-testable against a test MS. **Embedding is delegated to Meilisearch** — memd never computes vectors itself.
3. **MCP server** — thin adapter exposing tools over the wire; translates MCP tool calls → memory service. Transports: **HTTP (streamable)** primary (one always-on server, many clients) + **stdio** shim for stdio-only clients.
4. **Crawler + watcher** — config-driven roots/globs; initial scan + `notify`-based incremental updates + periodic reconcile; handles deletes; maps files → memory items of type `project_overview` / `agent_instruction` / `file_annotation`.
5. **CLI** — `memd up/down/status/logs`, `memd add/search/forget`, `memd crawl ...`, `memd mcp ...`, `memd doctor`.

### 6.2 Why a daemon *and* an MCP server
MCP stdio servers are spawned per client and die with it — incompatible with "memory maintained all the time." So:
- **Daemon** (`memd serve`, under launchd) owns the always-on parts: Meilisearch, crawler, sync, and an **HTTP MCP endpoint**.
- Clients connect to that HTTP endpoint (shared, persistent). For stdio-only clients, `memd mcp --stdio` is a thin bridge to the daemon/local MS.

## 7. MCP Surface

Start minimal and stable; grow deliberately.

**MVP tools:**
- `save_memory(content, type?, tags?, scope?, title?)` → `{ id }`. Stamps timestamps, classifies if `type` omitted, dedups, embeds.
- `get_memory(query, limit?, type?, scope?, since?, until?, semantic_ratio?)` → ranked memories with metadata. Bumps `last_accessed_at`.

**Phase 2 tools:**
- `update_memory(id, …)`, `forget_memory(id)`, `list_memories(filter)`, `stats()`.

Design rule: the tool surface is a *contract*. Additive changes only; never break `save_memory`/`get_memory`.

## 8. Semantic Search & Models

- **Search:** delegate to Meilisearch **hybrid search** (keyword + vector), `semantic_ratio` tunable per query (default ~0.5). Time filters via `created_at`/`updated_at`; recency boost via sort or score.
- **Embeddings — local, inside Meilisearch:** the `memories` index is configured with a Meilisearch embedder of **source `huggingFace`**, which downloads and runs the embedding model **in-process within Meilisearch** on the local machine. memd never computes or ships vectors itself — it just sends documents; MS embeds them locally on add/update. Nothing leaves the machine. Model is pinned in config (e.g. a small sentence-transformer). Remote embedder sources (OpenAI/Cohere/`rest`) remain a config-level opt-in for users who want them, but are **off by default**. *(Verify the exact `huggingFace` embedder settings against the pinned MS version at build time.)*
- **Classification — heuristics first:** type is inferred deterministically from `source`, path, and filename (e.g. `CLAUDE.md`/`AGENTS.md` → `agent_instruction`, `README*` → `project_overview`, MCP writes default to `fact` unless `type` is given). No model in the loop for MVP. LLM enrichment (summary + refined type/tags) is deferred to M3 as an async, opt-in pass.

## 9. Crawler Targets (default config)

Roots: `~/Projects` (configurable list). 

Include (globs):
- `README*`, `readme*` → `project_overview`
- `CLAUDE.md`, `AGENTS.md`, `agent.md`, `.cursorrules`, `.github/copilot-instructions.md` → `agent_instruction`
- Known memory dirs (e.g. `~/.claude/**/memory/*.md`, `MEMORY.md`) → `fact`/`preference`
- User-annotated "important files" → `file_annotation` (path + description; annotation created via `memd add --file` or an MCP call)

Exclude: `node_modules`, `target`, `.git`, `dist`, `build`, `.next`, vendored dirs, binaries/large files (size cap).

Behavior: stable id from path; `content_hash` skips unchanged; deletions remove the doc; periodic reconcile catches missed FS events.

## 10. Privacy, Security, Footprint

- **Local-first:** all data in a local Meilisearch; bound to `127.0.0.1`. Content leaves the machine only if a remote embedder/enrichment model is explicitly enabled.
- **Secrets hygiene:** crawler skip-list + optional regex redaction for obvious secrets (`.env`, keys). Never index files matched by a denylist.
- **Single-user / localhost:** no external auth in MVP; MS instance uses a local-only key.
- **Resource budget:** small idle footprint; embedding work batched/throttled; configurable size caps.

## 11. Tech Stack

- **Language:** Rust (edition 2024).
- **Async:** `tokio`.
- **Meilisearch client:** `meilisearch-sdk` or `reqwest` wrapper.
- **MCP:** `rmcp` (official Rust MCP SDK) — HTTP + stdio transports. *(verify current API at build time.)*
- **File watching:** `notify`.
- **Embeddings:** none in memd — delegated to Meilisearch's local `huggingFace` embedder (see §8). memd only manages the binary & index config.
- **CLI:** `clap`.
- **Config/serde:** `serde`, `toml`, `directories`/`dirs`.
- **Errors:** `anyhow` (bins) / `thiserror` (lib).
- **Daemonization:** generate a `launchd` plist (macOS first); `memd service install/uninstall`.
- **Config path:** `~/.config/memd/config.toml`; data via Meilisearch's own dir.

## 12. CLI Sketch

```
memd up                 # start daemon (MS + crawler + MCP), install service if needed
memd down               # stop daemon
memd status             # daemon + MS health, index stats, last crawl
memd logs [-f]          # tail daemon logs
memd add "<text>" [--type --tags --scope]   # manual memory
memd add --file <path> --note "<desc>"      # annotate an important file
memd search "<query>" [--type --since --semantic-ratio]
memd forget <id>
memd crawl run|status|config                # manage passive ingestion
memd mcp --stdio        # stdio bridge for stdio-only clients
memd service install|uninstall              # launchd integration
memd doctor             # diagnose MS / model / config issues
```

## 13. Milestones / Roadmap

**M1 — Core memory + MCP (MVP, ship first).**
Meilisearch manager (dedicated instance) · `memories` index + embedder · memory service (CRUD, timestamps, dedup, hybrid search) · MCP server with `save_memory` + `get_memory` (HTTP + stdio) · `memd up/down/status` · launchd install · local embedder default.
*Delivers the core loop: any agent can persist and recall memory across tools.*

**M2 — Passive ingestion.**
Crawler + `notify` watcher + periodic reconcile · default globs (READMEs, agent files, memory files) · deletions · `memd crawl ...` · file annotations.

**M3 — Intelligence.**
LLM enrichment (summary/type/tags) · recency/decay ranking · scope-aware recall · dedup/merge of near-duplicates · `update/forget/list/stats` MCP tools.

**M4 — Harness extras.**
Multi-client polish · richer MCP capabilities (resources/prompts) · optional cloud sync · cross-tool context injection helpers.

## 14. Success Metrics

- A memory saved in tool A is retrievable in tool B in the same session, < 200 ms typical `get_memory`.
- Crawl of `~/Projects` completes and stays in sync without manual intervention.
- Daemon idle footprint is negligible; survives reboot (launchd).
- Subjective: starting a fresh session in any tool surfaces correct, relevant context unprompted.

## 15. Decisions

**Confirmed:**
1. **Embeddings** — local, **inside Meilisearch** via the `huggingFace` embedder source. memd computes no vectors; remote embedders off by default. ✅
2. **Meilisearch provisioning** — **managed binary** run as a child process of the daemon (no Docker), dedicated instance on `:7701`. ✅
3. **Classification** — **heuristics first** (path/filename/source); LLM enrichment deferred to M3. ✅
4. **MCP transport** — HTTP-always-on primary + stdio bridge. ✅

**Still open (lower stakes, can be settled during planning):**
5. **Annotation UX** — how the user marks "important files" (CLI `--file`, sidecar `.memd.toml`, or MCP call). → *Lean CLI + MCP.*
6. **Scope semantics** — path-prefix scoping vs explicit named scopes/namespaces. → *Lean path-prefix.*
7. **Pinned MS version + embedding model** — pick a known-good Meilisearch release and a small local sentence-transformer at planning time.

## 16. Risks

- **MCP "always-on" model** — confirm clients can target a persistent HTTP MCP endpoint; otherwise stdio bridge per client is the fallback.
- **Local embedding quality/perf** — ONNX model size & latency vs recall quality; mitigate with batching + caching by `content_hash`.
- **Crawler noise / secrets** — must aggressively exclude and redact to avoid indexing junk or secrets.
- **Meilisearch version drift** — pin a known-good MS version and embedder API.
```
