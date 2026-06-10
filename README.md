<div align="center">

# 🧠 memd

### Universal local memory for every LLM tool you use.

One always-on daemon that turns a local [Meilisearch](https://www.meilisearch.com)
into a persistent, time-aware, semantically-searchable memory layer — shared across
Claude Code, Codex, Cursor, and anything else that speaks **MCP**.

[![CI](https://github.com/qdequele/memd/actions/workflows/ci.yml/badge.svg)](https://github.com/qdequele/memd/actions/workflows/ci.yml)
[![Release](https://github.com/qdequele/memd/actions/workflows/release.yml/badge.svg)](https://github.com/qdequele/memd/releases)
[![License: MIT](https://img.shields.io/badge/license-MIT-blue.svg)](LICENSE)
![Rust](https://img.shields.io/badge/rust-edition_2024-orange.svg)

</div>

---

> **The problem:** every LLM tool keeps its own siloed, forgetful memory. A decision
> you explained to Claude is invisible to Codex tomorrow. Context gets rebuilt from
> scratch in every session.
>
> **memd fixes it:** save a memory in one tool, recall it in every other — and in
> every future session. Local-first, private, and embeddings run on your machine.

```text
   Claude Code  ┐
   Codex        ├──MCP──▶  memd daemon ──▶  local Meilisearch  (your memories,
   Cursor  …    ┘          (always-on)       embedded on-device)
```

> **North star:** open a new chat in any tool → it already knows the relevant
> context about your machine, projects, and preferences, because they all share memd.

---

## 🚀 Install in 30 seconds

```sh
curl -fsSL https://raw.githubusercontent.com/qdequele/memd/main/scripts/install.sh | bash
```

This downloads the prebuilt binary to `~/.local/bin/memd` and runs `memd setup`,
which **does everything**: starts the daemon, registers memd with your installed
agents, and wires up memory injection. That's it — open a new agent session and it
has memory.

<details>
<summary><b>Build from source</b> (requires <a href="https://rustup.rs">Rust</a>)</summary>

```sh
git clone https://github.com/qdequele/memd && cd memd
./install.sh        # builds, then runs `memd setup`
```

</details>

### What `memd setup` does (so you don't have to)

| Step | What happens |
|------|--------------|
| 📦 Binary | Copied to a stable path (`~/.local/bin/memd`) |
| 🔍 Meilisearch | Pinned engine downloaded + started as a managed service (no Docker) |
| 🔌 Agents | Auto-registered (`claude mcp add` for Claude Code, …) |
| 📝 Directives | Usage block written into `CLAUDE.md` / `AGENTS.md` |
| 🪝 Hooks | SessionStart (auto-recall) + Stop (auto-capture) wired into Claude Code |

It's **idempotent** — re-run any time (e.g. after an upgrade) to reconverge.

---

## ✨ What you get

- **🔁 Shared across tools** — one MCP server every MCP-capable client can mount.
- **🧲 Semantic recall** — hybrid keyword + vector search; embeddings computed locally
  *inside* Meilisearch (nothing leaves your machine).
- **⏱️ Time-aware & typed** — every memory has timestamps, a type, tags, and a scope.
- **🐝 Passive ingestion** — a crawler indexes the knowledge already on disk
  (`README*`, `CLAUDE.md`/`AGENTS.md`, memory files) and keeps it in sync.
- **🪶 Token-safe by design** — search returns lightweight snippets, not blobs; fetch
  the full text only when you ask.
- **🔒 Local-first** — bound to `127.0.0.1`, local-only key, no cloud by default.
- **🔄 Self-updating** — a daily check keeps memd and its Meilisearch engine current; engine upgrades migrate via dump/import with automatic rollback.

---

## 🎬 Quickstart

```sh
memd status                                   # health, index stats, last crawl
memd add "We chose Postgres for billing" --type decision --tags billing
memd search "what database for billing"       # hybrid keyword + semantic
memd doctor --fix                             # diagnose & repair
memd down                                     # stop the daemon
```

First run downloads the pinned Meilisearch binary; the embedding model is fetched
once, by Meilisearch, on your first memory.

---

## 🔌 Using memd from your agents

`memd setup` registers **Claude Code** automatically. Other clients connect to the
same daemon two ways:

- **HTTP (always-on, preferred):** point the client at `http://127.0.0.1:7702/mcp`.
- **stdio bridge:** `{ "command": "memd", "args": ["mcp", "--stdio"] }`

### MCP tools

| Tool | Purpose |
|------|---------|
| `save_memory(content, type?, tags?, scope?, title?)` | Persist a memory → `{ id }` |
| `get_memory(query, …, crop_length?, facets?)` | Hybrid recall → lightweight rows with a query-aware snippet |
| `read_memory(id)` | Fetch the full content for one memory |
| `update_memory(id, …)` · `forget_memory(id)` | Edit / delete |
| `list_memories(type?, scope?, …)` | Browse (metadata-only by default) |
| `stats(group_by?)` | Counts + server-side facet distributions |

**The recall funnel keeps results token-safe:** `get_memory`/`list_memories` return
metadata + a cropped, highlighted snippet → `read_memory(id)` returns the full blob
on demand. Projection, cropping, highlighting, and grouped counts all run server-side
in Meilisearch.

---

## 🎯 Making memd the default memory

Models can't be *forced* to call a tool, but memd biases them at three layers,
weakest → strongest (all set up by `memd setup`):

1. **In-protocol (universal):** memd's MCP `initialize` advertises usage instructions
   that compliant clients inject into the model's system prompt.
2. **Instruction files (cross-tool):** `memd directives install` writes a managed
   block into `~/.claude/CLAUDE.md`, `~/.codex/AGENTS.md`, …
3. **Harness hooks (deterministic):** a Claude Code **SessionStart** hook injects
   relevant memories into *every* session; a **Stop** hook auto-captures turns that
   signal durable intent ("remember…", "we decided…").

---

## 🛠️ CLI reference

```
memd setup [--no-hooks]     One-command install / reconverge
memd up | down              Start / stop the daemon
memd status                 Daemon + Meilisearch health, index stats, last crawl
memd logs [-f]              Tail daemon logs
memd doctor [--fix]         Diagnose (and repair) common issues

memd add "<text>" [--type --tags --scope --title]
memd add --file <path> --note "<desc>"        Annotate an important file
memd search "<query>" [--type --since --semantic-ratio --limit]
memd forget <id>

memd crawl run|status|config                  Passive ingestion
memd context [--scope --query --limit]        Print memories as markdown (for hooks)
memd capture                                  Auto-capture a turn (Stop-hook stdin)
memd directives install|uninstall            Inject usage directives into agent files
memd mcp --stdio                              stdio bridge for stdio-only clients
memd service install|uninstall               launchd integration (macOS)
```

`--since` accepts unix seconds or a relative span (`30d`, `12h`, `45m`, `90s`).

---

## ⚙️ Configuration

`~/.config/memd/config.toml` is created with sane defaults on first run.

```toml
[meilisearch]
version = "v1.45.1"      # pinned engine, downloaded + managed by memd
host = "127.0.0.1"
port = 7701
master_key = "…"         # auto-generated, local-only

[mcp]
host = "127.0.0.1"
port = 7702              # always-on HTTP MCP endpoint

[embedder]
source = "huggingFace"   # runs locally inside Meilisearch
model = "BAAI/bge-small-en-v1.5"
default_semantic_ratio = 0.5

[crawler]
roots = ["~/Projects"]
exclude_dirs = ["node_modules", "target", ".git", "dist", "build", ".next", "…"]
max_file_bytes = 1000000
deny_globs = ["**/.env", "**/*.pem", "**/*.key", "…"]   # secrets hygiene
reconcile_secs = 3600
```

---

## 🗂️ Data model

Each memory is one Meilisearch document in the `memories` index:

`id`, `content`, `title?`, `summary?`, `type`, `tags[]`, `scope`, `source`,
`source_path?`, `source_client?`, `created_at`, `updated_at`, `last_accessed_at?`,
`content_hash`.

Types: `fact`, `preference`, `decision`, `task`, `project_overview`,
`agent_instruction`, `file_annotation`, `code_note`, `reference`.

---

## 🔒 Privacy

Local-first by design. Meilisearch is bound to `127.0.0.1` with a local-only key, and
embeddings are computed on-device. Nothing leaves your machine unless you explicitly
switch to a remote embedder. The crawler skips excluded dirs and denylisted files
(`.env`, keys) so secrets are never indexed.

---

## 🧑‍💻 Development

```sh
cargo build          # build
cargo test           # unit tests
cargo clippy         # lints
cargo fmt            # format
```

The codebase is a single binary with clear modules: `meili/` (engine manager +
client), `memory/` (model, classification, service), `mcp/` (JSON-RPC server),
`crawler/`, `daemon`, `launchd`, `cli`.

Releases are built by [`.github/workflows/release.yml`](.github/workflows/release.yml)
for macOS (arm64/x64) and Linux (x64/arm64) on every `v*` tag.

---

## License

[MIT](LICENSE) © Quentin de Quelen
