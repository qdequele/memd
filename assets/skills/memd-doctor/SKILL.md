---
name: memd-doctor
description: Diagnose and repair a broken memd memory setup. Use when memd memory isn't working — recall returns nothing, the daemon won't start, an agent shows no memd MCP tools, or the user asks to check, fix, debug, or restart memd (the local memory daemon backed by Meilisearch).
---

# memd doctor

memd is an always-on local daemon that turns a local Meilisearch into shared,
searchable long-term memory for every LLM tool. When "memory isn't working,"
the cause is almost always one of: the daemon is down, Meilisearch is down, the
database is version-mismatched, or an agent was never wired to the MCP server.

Work the steps **in order** and stop as soon as memory is healthy again.

## 1. Gather state

Run both and read the output before changing anything:

```sh
memd doctor      # config, binary, Meilisearch, db version, MCP endpoint, service
memd status      # health, memory count, per-agent wiring, last crawl
```

## 2. Diagnose, then repair the first failing layer

| Symptom in output | Fix |
|---|---|
| `MCP endpoint: down` / `Meilisearch: down` | `memd up` — starts the daemon (installs the service if needed). Re-check `memd status`. |
| `Database: version … != pinned … — MISMATCH` | `memd doctor --fix` — backs up the old db and recreates it on the pinned engine. |
| `Installed bin: not installed` | `memd setup` — relocates the binary and wires everything. |
| `Index: ERROR …` | `memd up` to (re)configure the index; if it persists, check `memd logs`. |
| An agent shows `detected — not configured` | `memd setup` and select that agent in the picker. |
| Daemon flaps / won't stay up | `memd logs` (or `memd logs -f`) to read the daemon log, then act on the error. |

## 3. Verify the MCP tools are reachable

Memory only works in a session if the agent can see memd's MCP tools
(`get_memory`, `save_memory`, `read_memory`, `list_memories`, ...). If they're
missing in the current agent:

- Confirm wiring: `memd status` should show that agent as `configured`.
- Re-wire if needed: `memd setup`.
- The agent must reload — start a **new** session (in Claude Code, `/mcp` lists
  connected servers; `memd` should appear).

## 4. Confirm

```sh
memd status                       # Meilisearch: up, MCP endpoint: up
memd search "test"                # round-trips through the daemon
```

Report what was wrong and what fixed it. Don't claim it's fixed until
`memd status` shows both Meilisearch and the MCP endpoint `up`.

## Notes

- `memd doctor --fix` currently only repairs a version-mismatched database, and
  it **backs the old db up first** (`meili-data.mismatch.<ts>`). It is safe.
- On macOS the daemon runs under launchd (`KeepAlive`), so a crash self-restarts.
  On Linux there's no supervisor by default — `memd up` re-spawns it.
- Logs live next to the data dir; `memd doctor` prints the exact paths.
