---
name: memd-memory
description: Use memd (shared cross-tool long-term memory) well — recall relevant context before acting, and save durable facts as you learn them. Use at the start of a task to load prior decisions and preferences, and whenever the user shares something worth remembering across sessions and tools.
---

# Using memd memory

memd is your persistent, cross-tool long-term memory: one local MCP server,
shared with every LLM tool on this machine. Memories saved in one tool are
recalled in all the others, and in every future session. Use it proactively —
don't make the user re-explain things they've already told another tool.

The MCP tools (and their CLI equivalents) are:

| Do | MCP tool | CLI |
|---|---|---|
| Recall before acting | `get_memory` | `memd search "<query>"` |
| Save a durable fact | `save_memory` | `memd add "<text>" --type <t>` |
| Read full text of one | `read_memory(id)` | — |
| Browse / list | `list_memories` | `memd search` |
| Correct an existing one | `update_memory(id, …)` | — |
| Delete a wrong/stale one | `forget_memory(id)` | `memd forget <id>` |

## Recall first

At the **start of a task**, call `get_memory` with the user's goal and set
`scope` to the project path (the current working directory) when relevant. Load
prior decisions, preferences, and facts before acting. Prefer recalling over
asking the user to repeat themselves.

## Save what's durable

Save when you learn something that should outlive this session: a decision, a
stated preference, a stable fact about the user or project, or a reusable
solution. Before saving, **search first** to avoid duplicates — if a close
memory already exists, `update_memory` it instead of adding a near-copy.

Write each memory to be useful cold, months later and in another tool:

- **One fact per memory.** Atomic memories recall and update cleanly.
- **Set the `type`** (`fact`, `preference`, `decision`, `task`,
  `project_overview`, ...) and a `scope` — `global` for machine/user-wide
  truths, the project path for project-specific ones.
- **Be self-contained.** Spell out the *why* for decisions and preferences;
  resolve relative dates to absolute ones.
- **Don't save** what's already in the repo (code, git history, README/CLAUDE.md)
  or what only matters to the current conversation.

## Keep it high-signal

- Fix memories that turn out wrong with `update_memory`; `forget_memory` ones
  that are obsolete or were saved by mistake.
- If recall surfaces stale or contradictory memories, reconcile them rather than
  letting both linger.
