# memd agent auto-setup — design

**Date:** 2026-06-16
**Status:** Approved
**Author:** Quentin de Quelen (with Claude)

## Goal

Turn `memd setup` into an agent-aware installer. Today `setup` hardcodes its
integrations: it always registers Claude Code (MCP + hooks) and writes directives
to Claude Code + Codex. Instead, `setup` should present an interactive checklist
of the LLM agents on the machine and let the user pick which ones memd configures.
`memd status` should report which agents are currently configured so the wiring is
observable after the fact.

## Behavior

- **TTY:** `memd setup` shows a multi-select checklist of all known agents,
  pre-checked according to what is currently configured. The checkbox state is the
  **desired state** (sync semantics):
  - checked → install memd's config into that agent
  - unchecked → remove memd's config from that agent
  `setup` becomes the single source of truth for agent wiring.
- **Non-TTY (CI, piped, no terminal):** skip the prompt and auto-configure every
  **detected** agent. Never remove anything in this mode — no destructive change
  without a human in the loop.
- **`--no-hooks`** continues to suppress the Claude Code SessionStart/Stop hooks.
- No new flags. Selection is interactive-only (per decision below); non-TTY uses
  detection as the desired set.

## Supported agents

A static registry of seven agents. Each declares: how it is detected, how its MCP
server is registered, whether it has a global instruction file for directives, and
whether it supports lifecycle hooks.

| Agent | Detect via | MCP transport | MCP config mechanism | Directives file | Hooks |
|---|---|---|---|---|---|
| Claude Code | `claude` on PATH | HTTP | `claude mcp add` CLI | `~/.claude/CLAUDE.md` | ✅ SessionStart+Stop |
| Codex | `~/.codex/` exists | HTTP | `~/.codex/config.toml` `[mcp_servers.memd]` `url` | `~/.codex/AGENTS.md` | — |
| Gemini CLI | `~/.gemini/` exists | HTTP | `~/.gemini/settings.json` `mcpServers` | `~/.gemini/GEMINI.md` | — |
| Cursor | `~/.cursor/` exists | HTTP | `~/.cursor/mcp.json` `mcpServers` | — | — |
| Windsurf | `~/.codeium/windsurf/` exists | HTTP/SSE | `~/.codeium/windsurf/mcp_config.json` | — | — |
| Cline | VS Code globalStorage dir | HTTP/SSE | `cline_mcp_settings.json` | — | — |
| Zed | `~/.config/zed/` exists | HTTP | `settings.json` `context_servers` `url` | — | — |

**Transport rule:** register the always-on daemon's HTTP endpoint
(`http://127.0.0.1:7702/mcp`, from `cfg.mcp.host`/`port`) where the agent supports
HTTP/SSE; fall back to the stdio bridge (`memd mcp --stdio`) for stdio-only clients.
This matches the "one shared server, many clients" recommendation in the MCP docs.
(In practice all seven supported agents were verified HTTP-capable — Codex/Cursor/Zed
via `url`, Gemini via `httpUrl`, Windsurf via `serverUrl`, Cline via `url`+`type`,
Claude Code via `--transport http` — so every agent registers over HTTP and the stdio
fallback is currently unused.)

**Note on accuracy:** the exact config paths and JSON/TOML keys per agent are taken
from current knowledge and MUST be verified against each agent's current docs at
implementation time (project rule: don't trust training-cutoff knowledge). The
design assumes they are confirmable, not final. Where a path cannot be confirmed,
that agent ships behind the same registry but its mechanism is corrected before
release.

## Architecture — agent adapter abstraction

New module `src/agents/` that absorbs the integration helpers currently inlined in
`cli.rs` (`register_claude_code`, `directives_*`, `install_claude_hooks`,
`target_instruction_files`).

```rust
struct Agent {
    id: &'static str,          // "claude-code", "cursor", …
    name: &'static str,        // "Claude Code"
    detect: DetectRule,        // CliOnPath("claude") | DirExists(path) | FileExists(path)
    mcp: McpMechanism,         // ClaudeCli | JsonMerge { path, pointer, transport } | TomlMerge { path }
    directives: Option<PathBuf>,
    hooks: bool,               // claude-code only
}

enum AgentStatus {
    NotDetected,
    Detected,        // installed but memd not wired in
    Configured,      // memd MCP entry present
}

impl Agent {
    fn status(&self) -> AgentStatus;
    fn configure(&self, bin: &Path, mcp_url: &str) -> Result<()>;  // MCP + directives + hooks
    fn remove(&self) -> Result<()>;                                // reverse of configure
}
```

Most agents are **data, not code**. Two shared helpers carry the weight:

- `json_merge_mcp(path, entry, present: bool)` — idempotently insert or remove a
  `memd` entry in an agent's JSON config (`mcpServers` / `context_servers` /
  equivalent), preserving every other key. Used by Gemini, Cursor, Windsurf, Cline,
  Zed.
- `toml_merge_mcp(path, present: bool)` — same for Codex's `config.toml`
  `[mcp_servers.memd]`.

`ClaudeCli` shells out to `claude mcp add/remove` (the existing `register_claude_code`
logic, extended to register HTTP). Directives upsert/removal and the Claude hook
installer move into this module largely unchanged.

`configure()` and `remove()` are each idempotent and composed of the agent's
applicable steps (MCP always; directives and hooks only where declared).

## `memd setup` flow (revised `cli::setup`)

Steps 1–4 (relocate binary, ensure config, start daemon as a service, wait for
health) are unchanged. Step 5 ("register with detected MCP clients") is replaced:

1. Build the registry; compute `status()` for each agent.
2. **TTY:** render `dialoguer::MultiSelect`. Each row is labelled by state, e.g.
   `Claude Code (configured)`, `Cursor (detected)`, `Zed (not detected)`; rows
   currently `Configured` start checked. **Non-TTY:** desired set = all `Detected`
   (and `Configured`) agents; no prompt.
3. Diff desired vs current:
   - newly-checked / detected-and-unconfigured → `configure(bin, mcp_url)`
   - newly-unchecked (was `Configured`) → `remove()`  *(TTY only)*
   - unchanged → leave alone
4. Print a per-agent summary line (`✓ configured`, `removed`, `⚠ <error>`).
5. `--no-hooks` suppresses the Claude hook step as today.

TTY detection uses std's `std::io::IsTerminal` on stdin — no extra crate.

## `memd status` — Agents section

After the existing `Service:` line, `status` prints an **Agents** block driven by
the same registry/`status()` call:

```
Agents:
  Claude Code   configured (HTTP)
  Codex         configured (HTTP)
  Cursor        detected — not configured
  Gemini CLI    not detected
  …
```

For configured agents, show the transport used. This makes the wiring observable
without re-running setup. `memd doctor` is unchanged (status is the right home for
this), though a follow-up could surface stale/broken entries there.

## CLI surface

- `memd setup` — gains the interactive picker; no new flags.
- `memd directives install/uninstall` — unchanged; remains the directive-only path.
- `memd status` — gains the Agents section.

## Dependencies

- Add `dialoguer` for the `MultiSelect` prompt.
- TTY detection via `std::io::IsTerminal` (std, no crate).

## Testing

Unit tests (with `tempfile`):

- `json_merge_mcp`: insert into empty file, insert into file with existing servers
  (other keys preserved), idempotent re-insert (no duplicate), remove (only the
  `memd` entry goes), remove-when-absent (no-op).
- `toml_merge_mcp`: same matrix for Codex's `config.toml`.
- Registry: every agent's declared paths resolve under a fake home; `status()`
  returns the expected variant for present/absent fixtures.
- Diff logic (desired set vs current statuses → list of configure/remove actions)
  tested as a pure function, independent of any prompt.

The interactive `MultiSelect` itself is not unit-tested.

## Out of scope (YAGNI)

- Project-scoped (per-repo) MCP config — global/user scope only.
- Cursor/Windsurf/Cline rule-file directives — those agents get MCP only; directives
  stay limited to agents with a stable global instruction-file convention
  (Claude Code, Codex, Gemini CLI).
- A standalone `memd agents` command — folded into `setup` + `status`.
- Hooks for non-Claude agents — no comparable lifecycle-hook system exists.
