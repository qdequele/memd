# Agent Auto-Setup Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** Make `memd setup` present an interactive checklist of the LLM agents installed on the machine, configure/remove memd's MCP wiring per the user's selection (sync semantics), and surface each agent's wiring state in `memd status`.

**Architecture:** A new `src/agents/` module owns a static registry of seven agents. Each agent is mostly *data*: how it's detected, how its MCP server entry is written (Claude CLI / JSON-merge / TOML-merge), an optional instruction-file for directives, and a hooks flag. Two shared, well-tested helpers (`json_merge_mcp`, `toml_merge_mcp`) do idempotent insert/remove. `cli::setup` builds the registry, shows a `dialoguer::MultiSelect` on a TTY (auto-uses detected agents otherwise), diffs desired-vs-current, and applies `configure`/`remove`. `cli::status` prints an Agents block from the same registry.

**Tech Stack:** Rust 2024, clap, serde_json, toml, dialoguer (new), anyhow, tempfile (tests). All MCP entries register the daemon's HTTP endpoint `http://127.0.0.1:7702/mcp`.

**Verified agent config reference (June 2026):**

| Agent | id | Detect | MCP mechanism | Container key | URL field | Extra | Directives file |
|---|---|---|---|---|---|---|---|
| Claude Code | `claude-code` | `claude` on PATH | `claude mcp add --transport http memd <url> --scope user` | — | — | — | `~/.claude/CLAUDE.md` |
| Codex | `codex` | `~/.codex/` dir | `~/.codex/config.toml` TOML | `[mcp_servers.memd]` | `url` | — | `~/.codex/AGENTS.md` |
| Gemini CLI | `gemini-cli` | `~/.gemini/` dir | `~/.gemini/settings.json` JSON | `mcpServers` | `httpUrl` | — | `~/.gemini/GEMINI.md` |
| Cursor | `cursor` | `~/.cursor/` dir | `~/.cursor/mcp.json` JSON | `mcpServers` | `url` | — | — |
| Windsurf | `windsurf` | `~/.codeium/windsurf/` dir | `~/.codeium/windsurf/mcp_config.json` JSON | `mcpServers` | `serverUrl` | — | — |
| Cline | `cline` | globalStorage dir (below) | `cline_mcp_settings.json` JSON | `mcpServers` | `url` | `("type","streamableHttp")` | — |
| Zed | `zed` | `~/.config/zed/` dir | `~/.config/zed/settings.json` JSON | `context_servers` | `url` | — | — |

Cline settings path (macOS): `~/Library/Application Support/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`; (Linux): `~/.config/Code/User/globalStorage/saoudrizwan.claude-dev/settings/cline_mcp_settings.json`. Windows is out of scope.

**File structure:**
- Create `src/agents/mod.rs` — `Agent`, `DetectRule`, `AgentStatus`, `Action`, registry builder `registry()`, `configure`, `remove`, `status`, pure `plan_action`.
- Create `src/agents/mcp.rs` — `McpMechanism`, `json_merge_mcp`, `toml_merge_mcp`, Claude-CLI register/remove, per-mechanism `is_configured`.
- Create `src/agents/directives.rs` — directive block + `upsert_directive`/`remove_directive`/`managed_region` (moved from `cli.rs`).
- Create `src/agents/hooks.rs` — `install_claude_hooks`/`ensure_hook` (moved from `cli.rs`).
- Modify `src/main.rs` — add `mod agents;`.
- Modify `src/cli.rs` — rewrite `setup`; add Agents section to `status`; replace `directives_install`/`directives_uninstall` bodies with calls into `agents::directives`; delete the moved helpers and `register_claude_code`/`target_instruction_files`.
- Modify `Cargo.toml` — add `dialoguer`.
- Modify `docs/mcp.mdx`, `README.md` — document the picker + status.

---

### Task 1: Add dependency and scaffold the agents module

**Files:**
- Modify: `Cargo.toml`
- Create: `src/agents/mod.rs`
- Modify: `src/main.rs:7-17` (module list)

- [ ] **Step 1: Add the `dialoguer` dependency**

In `Cargo.toml`, under `[dependencies]` (after the `time = …` line), add:

```toml
dialoguer = { version = "0.11", default-features = false }
```

- [ ] **Step 2: Create the module file with shared imports and URL helper**

Create `src/agents/mod.rs`:

```rust
//! Per-agent integration: detect installed LLM agents, register memd's MCP
//! server with them, write usage directives, and report wiring state. The
//! registry is mostly data — `mcp.rs` carries the three write mechanisms.

mod directives;
mod hooks;
mod mcp;

use crate::config::Config;
use anyhow::Result;
use std::path::{Path, PathBuf};

pub use directives::{install_all as directives_install_all, remove_all as directives_remove_all};

/// The HTTP MCP endpoint every agent registers against.
pub fn mcp_url(cfg: &Config) -> String {
    format!("http://{}:{}/mcp", cfg.mcp.host, cfg.mcp.port)
}
```

- [ ] **Step 3: Register the module**

In `src/main.rs`, add `mod agents;` to the module list (keep alphabetical, after `mod update;` or wherever fits — place it right after `mod cli;`):

```rust
mod agents;
mod cli;
```

- [ ] **Step 4: Verify it compiles (expected: errors about missing submodules)**

Run: `cargo build`
Expected: FAILS — `directives`, `hooks`, `mcp` submodules don't exist yet. This confirms the module is wired in. Proceed to create them in later tasks; do not commit yet.

---

### Task 2: MCP merge helpers (JSON + TOML), test-first

**Files:**
- Create: `src/agents/mcp.rs`
- Test: inline `#[cfg(test)]` in `src/agents/mcp.rs`

- [ ] **Step 1: Write failing tests for `json_merge_mcp`**

Create `src/agents/mcp.rs` with just the tests and stubs:

```rust
//! The three ways memd registers/unregisters its MCP server with an agent:
//! the Claude Code CLI, a merged JSON config, or a merged TOML config. All
//! operations are idempotent and preserve unrelated keys.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;
use std::process::Command;

/// Insert (or remove) a `memd` entry under `container` in a JSON config file,
/// preserving every other key. `url_key` is the field the agent expects the
/// endpoint under ("url" | "httpUrl" | "serverUrl"); `extra` are additional
/// constant fields (e.g. Cline's `("type","streamableHttp")`). Creates the
/// file and parent dirs when adding; a no-op when removing from an absent file.
pub fn json_merge_mcp(
    path: &Path,
    container: &str,
    url: &str,
    url_key: &str,
    extra: &[(&str, &str)],
    present: bool,
) -> Result<()> {
    todo!()
}

/// True if a `memd` entry exists under `container` in the JSON file.
pub fn json_has_mcp(path: &Path, container: &str) -> bool {
    todo!()
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::Value;

    fn read(path: &std::path::Path) -> Value {
        serde_json::from_str(&std::fs::read_to_string(path).unwrap()).unwrap()
    }

    #[test]
    fn json_insert_into_empty() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.json");
        json_merge_mcp(&p, "mcpServers", "http://x/mcp", "url", &[], true).unwrap();
        let v = read(&p);
        assert_eq!(v["mcpServers"]["memd"]["url"], "http://x/mcp");
    }

    #[test]
    fn json_preserves_existing_servers_and_extra_field() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cline.json");
        std::fs::write(
            &p,
            r#"{"mcpServers":{"other":{"url":"http://o"}},"otherTopKey":1}"#,
        )
        .unwrap();
        json_merge_mcp(
            &p,
            "mcpServers",
            "http://x/mcp",
            "url",
            &[("type", "streamableHttp")],
            true,
        )
        .unwrap();
        let v = read(&p);
        assert_eq!(v["mcpServers"]["other"]["url"], "http://o");
        assert_eq!(v["otherTopKey"], 1);
        assert_eq!(v["mcpServers"]["memd"]["url"], "http://x/mcp");
        assert_eq!(v["mcpServers"]["memd"]["type"], "streamableHttp");
    }

    #[test]
    fn json_reinsert_is_idempotent() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.json");
        json_merge_mcp(&p, "mcpServers", "http://x/mcp", "url", &[], true).unwrap();
        json_merge_mcp(&p, "mcpServers", "http://y/mcp", "url", &[], true).unwrap();
        let v = read(&p);
        // Converges on the latest url; single entry.
        assert_eq!(v["mcpServers"]["memd"]["url"], "http://y/mcp");
        assert_eq!(v["mcpServers"].as_object().unwrap().len(), 1);
    }

    #[test]
    fn json_remove_only_touches_memd() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.json");
        std::fs::write(
            &p,
            r#"{"mcpServers":{"memd":{"url":"http://x"},"other":{"url":"http://o"}}}"#,
        )
        .unwrap();
        json_merge_mcp(&p, "mcpServers", "", "url", &[], false).unwrap();
        let v = read(&p);
        assert!(v["mcpServers"]["memd"].is_null());
        assert_eq!(v["mcpServers"]["other"]["url"], "http://o");
    }

    #[test]
    fn json_remove_from_absent_file_is_noop() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("missing.json");
        json_merge_mcp(&p, "mcpServers", "", "url", &[], false).unwrap();
        assert!(!p.exists());
    }

    #[test]
    fn json_has_mcp_detects_presence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("mcp.json");
        assert!(!json_has_mcp(&p, "mcpServers"));
        json_merge_mcp(&p, "mcpServers", "http://x/mcp", "url", &[], true).unwrap();
        assert!(json_has_mcp(&p, "mcpServers"));
    }
}
```

- [ ] **Step 2: Run the tests to confirm they fail**

Run: `cargo test --lib agents::mcp::tests::json`
Expected: FAIL — `not yet implemented` (the `todo!()` panics).

- [ ] **Step 3: Implement `json_merge_mcp` and `json_has_mcp`**

Replace the two `todo!()` bodies:

```rust
pub fn json_merge_mcp(
    path: &Path,
    container: &str,
    url: &str,
    url_key: &str,
    extra: &[(&str, &str)],
    present: bool,
) -> Result<()> {
    let mut root: Value = std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        if !present {
            return Ok(());
        }
        root = json!({});
    }
    let obj = root.as_object_mut().unwrap();

    if !present {
        if !path.exists() {
            return Ok(());
        }
        if let Some(map) = obj.get_mut(container).and_then(|c| c.as_object_mut()) {
            map.remove("memd");
        }
    } else {
        let mut entry = serde_json::Map::new();
        entry.insert(url_key.to_string(), Value::String(url.to_string()));
        for (k, v) in extra {
            entry.insert((*k).to_string(), Value::String((*v).to_string()));
        }
        let map = obj
            .entry(container.to_string())
            .or_insert_with(|| json!({}));
        if !map.is_object() {
            *map = json!({});
        }
        map.as_object_mut()
            .unwrap()
            .insert("memd".to_string(), Value::Object(entry));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, serde_json::to_string_pretty(&root)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

pub fn json_has_mcp(path: &Path, container: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| {
            v.get(container)
                .and_then(|c| c.get("memd"))
                .map(|_| true)
        })
        .unwrap_or(false)
}
```

- [ ] **Step 4: Run the JSON tests to confirm they pass**

Run: `cargo test --lib agents::mcp::tests::json`
Expected: PASS (6 tests).

- [ ] **Step 5: Add failing tests for `toml_merge_mcp`**

Append to the `tests` module in `src/agents/mcp.rs`:

```rust
    #[test]
    fn toml_insert_and_preserve() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(&p, "model = \"gpt-5\"\n\n[mcp_servers.other]\nurl = \"http://o\"\n").unwrap();
        toml_merge_mcp(&p, "http://x/mcp", true).unwrap();
        let doc: toml::Value = toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        assert_eq!(doc["mcp_servers"]["other"]["url"].as_str(), Some("http://o"));
        assert_eq!(doc["mcp_servers"]["memd"]["url"].as_str(), Some("http://x/mcp"));
    }

    #[test]
    fn toml_remove_only_memd() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "[mcp_servers.memd]\nurl = \"http://x\"\n\n[mcp_servers.other]\nurl = \"http://o\"\n",
        )
        .unwrap();
        toml_merge_mcp(&p, "", false).unwrap();
        let doc: toml::Value = toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert!(doc["mcp_servers"].get("memd").is_none());
        assert_eq!(doc["mcp_servers"]["other"]["url"].as_str(), Some("http://o"));
    }

    #[test]
    fn toml_has_mcp() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert!(!toml_has_mcp(&p));
        toml_merge_mcp(&p, "http://x/mcp", true).unwrap();
        assert!(toml_has_mcp(&p));
    }
```

- [ ] **Step 6: Run to confirm they fail**

Run: `cargo test --lib agents::mcp::tests::toml`
Expected: FAIL — `toml_merge_mcp` / `toml_has_mcp` not found (won't compile).

- [ ] **Step 7: Implement `toml_merge_mcp` and `toml_has_mcp`**

Add to `src/agents/mcp.rs` (before the `tests` module). Uses the `toml` crate already in deps:

```rust
/// Insert/remove `[mcp_servers.memd]` in an agent's `config.toml`, preserving
/// other tables. No-op when removing from an absent file.
pub fn toml_merge_mcp(path: &Path, url: &str, present: bool) -> Result<()> {
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let mut doc: toml::Table = if existing.trim().is_empty() {
        toml::Table::new()
    } else {
        existing.parse().context("parsing existing config.toml")?
    };

    if !present {
        if !path.exists() {
            return Ok(());
        }
        if let Some(servers) = doc.get_mut("mcp_servers").and_then(|v| v.as_table_mut()) {
            servers.remove("memd");
        }
    } else {
        let servers = doc
            .entry("mcp_servers".to_string())
            .or_insert_with(|| toml::Value::Table(toml::Table::new()));
        let servers = servers
            .as_table_mut()
            .context("mcp_servers is not a table")?;
        let mut entry = toml::Table::new();
        entry.insert("url".to_string(), toml::Value::String(url.to_string()));
        servers.insert("memd".to_string(), toml::Value::Table(entry));
    }

    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(path, toml::to_string_pretty(&doc)?)
        .with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// True if `[mcp_servers.memd]` exists in the TOML file.
pub fn toml_has_mcp(path: &Path) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| s.parse::<toml::Table>().ok())
        .and_then(|t| {
            t.get("mcp_servers")
                .and_then(|s| s.as_table())
                .map(|s| s.contains_key("memd"))
        })
        .unwrap_or(false)
}
```

- [ ] **Step 8: Run the full mcp test module**

Run: `cargo test --lib agents::mcp`
Expected: PASS (9 tests).

- [ ] **Step 9: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/agents/mod.rs src/agents/mcp.rs
git commit -m "feat(agents): add MCP JSON/TOML merge helpers"
```

Note: `src/agents/mod.rs` still won't fully compile until `directives`/`hooks` exist (Tasks 3–4); commit anyway is fine only if `cargo build` passes. If it doesn't, defer this commit to the end of Task 4 and just commit the test work mentally — **prefer:** complete Tasks 3 and 4 before this commit. (See Task 4 Step for the combined commit.)

---

### Task 3: Move directive logic into `src/agents/directives.rs`

**Files:**
- Create: `src/agents/directives.rs`
- Modify: `src/cli.rs` (remove moved fns later in Task 7; for now add nothing)

- [ ] **Step 1: Create `src/agents/directives.rs` with the relocated code**

This is the existing `cli.rs` directive logic, plus `install_all`/`remove_all` that iterate the registry's directive files. Create `src/agents/directives.rs`:

```rust
//! The managed memd directive block injected into agent instruction files
//! (Claude Code's CLAUDE.md, Codex's AGENTS.md, Gemini's GEMINI.md). Idempotent
//! via the `<!-- memd:start --> … <!-- memd:end -->` markers.

use anyhow::{Context, Result};
use std::path::Path;

const DIRECTIVE_START: &str = "<!-- memd:start (managed by `memd directives`) -->";
const DIRECTIVE_END: &str = "<!-- memd:end -->";

/// The managed directive block injected into agent instruction files.
pub fn directive_block() -> String {
    format!(
        "{DIRECTIVE_START}\n\
## Memory (memd)\n\
`memd` is your persistent, cross-tool long-term memory — an always-on local MCP \
server shared with all of your LLM tools.\n\
- At the start of a task, call `get_memory` (set `scope` to the project path) to \
load prior decisions, preferences, and facts before acting — don't ask the user \
to repeat context you can recall.\n\
- When you learn something durable (a decision, preference, fact, or reusable \
solution), call `save_memory`.\n\
- Use `read_memory(id)` for the full text and `list_memories` to browse.\n\
{DIRECTIVE_END}"
    )
}

/// Insert or replace the managed directive block in `path` (creating the file).
pub fn upsert_directive(path: &Path) -> Result<()> {
    let block = directive_block();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let existing = std::fs::read_to_string(path).unwrap_or_default();
    let next = if let Some(region) = managed_region(&existing) {
        existing.replacen(&region, &block, 1)
    } else if existing.trim().is_empty() {
        format!("{block}\n")
    } else {
        format!("{}\n\n{block}\n", existing.trim_end())
    };
    std::fs::write(path, next).with_context(|| format!("writing {}", path.display()))?;
    Ok(())
}

/// Remove the managed directive block from `path`. Returns whether it changed.
pub fn remove_directive(path: &Path) -> Result<bool> {
    let Ok(existing) = std::fs::read_to_string(path) else {
        return Ok(false);
    };
    let Some(region) = managed_region(&existing) else {
        return Ok(false);
    };
    let cleaned = existing.replacen(&region, "", 1);
    let cleaned = cleaned.replace("\n\n\n", "\n\n");
    std::fs::write(path, cleaned.trim_start())?;
    Ok(true)
}

/// Extract the current `<!-- memd:start --> … <!-- memd:end -->` region, if any.
fn managed_region(text: &str) -> Option<String> {
    let start = text.find(DIRECTIVE_START)?;
    let end = text.find(DIRECTIVE_END)? + DIRECTIVE_END.len();
    if end > start {
        Some(text[start..end].to_string())
    } else {
        None
    }
}

/// Write directives into every registry agent that declares an instruction file.
pub fn install_all() -> Result<()> {
    for agent in super::registry() {
        if let Some(path) = &agent.directives {
            upsert_directive(path)?;
            println!("Wrote memd directives to {}", path.display());
        }
    }
    println!("\nReload your tools (or start a new session) for the directives to take effect.");
    Ok(())
}

/// Remove directives from every registry agent that declares an instruction file.
pub fn remove_all() -> Result<()> {
    for agent in super::registry() {
        if let Some(path) = &agent.directives {
            if remove_directive(path)? {
                println!("Removed memd directives from {}", path.display());
            }
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn upsert_then_remove_roundtrip() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("CLAUDE.md");
        std::fs::write(&p, "# My notes\n\nkeep me\n").unwrap();
        upsert_directive(&p).unwrap();
        let after = std::fs::read_to_string(&p).unwrap();
        assert!(after.contains("keep me"));
        assert!(after.contains("## Memory (memd)"));
        // Idempotent: second upsert doesn't duplicate the block.
        upsert_directive(&p).unwrap();
        let twice = std::fs::read_to_string(&p).unwrap();
        assert_eq!(twice.matches("## Memory (memd)").count(), 1);
        // Remove leaves the user's content.
        assert!(remove_directive(&p).unwrap());
        let cleaned = std::fs::read_to_string(&p).unwrap();
        assert!(cleaned.contains("keep me"));
        assert!(!cleaned.contains("## Memory (memd)"));
    }
}
```

- [ ] **Step 2: Confirm it compiles against the (still incomplete) module**

Run: `cargo build 2>&1 | head -30`
Expected: still FAILS — `super::registry` and `Agent.directives` not defined yet, and `hooks` module missing. That's expected; Task 5 adds `registry()`/`Agent`. Move on.

---

### Task 4: Move Claude hooks into `src/agents/hooks.rs`

**Files:**
- Create: `src/agents/hooks.rs`

- [ ] **Step 1: Create `src/agents/hooks.rs` with the relocated code**

Move the hook installer from `cli.rs` verbatim (function bodies unchanged from `cli.rs:951-1034`), adjusting only visibility/imports:

```rust
//! Claude Code lifecycle hooks: a SessionStart hook that injects relevant
//! memories, and a Stop hook that conservatively captures durable turns. Merged
//! idempotently into `~/.claude/settings.json`.

use anyhow::{Context, Result};
use serde_json::{json, Value};
use std::path::Path;

/// Merge memd's SessionStart/Stop hooks into `~/.claude/settings.json`.
/// Idempotent; returns whether the file changed.
pub fn install_claude_hooks(installed: &Path) -> Result<bool> {
    let path = directories::BaseDirs::new()
        .context("home directory")?
        .home_dir()
        .join(".claude")
        .join("settings.json");
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        root = json!({});
    }

    let exe = installed.to_string_lossy();
    let session_cmd = format!("{exe} context --scope \"$CLAUDE_PROJECT_DIR\"");
    let stop_cmd = format!("{exe} capture");

    let mut changed = false;
    changed |= ensure_hook(&mut root, "SessionStart", "context", &session_cmd, 10);
    changed |= ensure_hook(&mut root, "Stop", "capture", &stop_cmd, 15);

    if changed {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&root)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(changed)
}

/// Ensure a `memd <marker>` command hook exists under `hooks.<event>`.
fn ensure_hook(root: &mut Value, event: &str, marker: &str, command: &str, timeout: u64) -> bool {
    let needle = format!("memd {marker}");
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    let arr = hooks
        .as_object_mut()
        .unwrap()
        .entry(event)
        .or_insert_with(|| json!([]));
    let Some(arr) = arr.as_array_mut() else {
        return false;
    };
    if let Some(group) = arr.iter_mut().find(|group| {
        group
            .get("hooks")
            .and_then(|h| h.as_array())
            .map(|hs| {
                hs.iter().any(|h| {
                    h.get("command")
                        .and_then(|c| c.as_str())
                        .map(|c| c.contains(&needle))
                        .unwrap_or(false)
                })
            })
            .unwrap_or(false)
    }) {
        let current = group
            .get("hooks")
            .and_then(|h| h.as_array())
            .and_then(|hs| hs.first())
            .and_then(|h| h.get("command"))
            .and_then(|c| c.as_str())
            .unwrap_or("");
        if current == command {
            return false;
        }
        *group = json!({
            "hooks": [ { "type": "command", "command": command, "timeout": timeout } ]
        });
        return true;
    }
    arr.push(json!({
        "hooks": [ { "type": "command", "command": command, "timeout": timeout } ]
    }));
    true
}

/// Remove memd's hook groups from `~/.claude/settings.json`. Returns whether it changed.
pub fn remove_claude_hooks() -> Result<bool> {
    let path = directories::BaseDirs::new()
        .context("home directory")?
        .home_dir()
        .join(".claude")
        .join("settings.json");
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    let mut changed = false;
    for event in ["SessionStart", "Stop"] {
        if let Some(arr) = root
            .get_mut("hooks")
            .and_then(|h| h.get_mut(event))
            .and_then(|e| e.as_array_mut())
        {
            let before = arr.len();
            arr.retain(|group| {
                !group
                    .get("hooks")
                    .and_then(|h| h.as_array())
                    .map(|hs| {
                        hs.iter().any(|h| {
                            h.get("command")
                                .and_then(|c| c.as_str())
                                .map(|c| c.contains("memd context") || c.contains("memd capture"))
                                .unwrap_or(false)
                        })
                    })
                    .unwrap_or(false)
            });
            changed |= arr.len() != before;
        }
    }
    if changed {
        std::fs::write(&path, serde_json::to_string_pretty(&root)?)?;
    }
    Ok(changed)
}
```

- [ ] **Step 2: Verify (still fails on registry only)**

Run: `cargo build 2>&1 | head -20`
Expected: FAILS only on `super::registry`/`Agent` references in `directives.rs` and `mod.rs` — `hooks.rs` itself compiles. Next task adds the registry.

---

### Task 5: Registry, detection, and status (test-first)

**Files:**
- Modify: `src/agents/mod.rs`
- Modify: `src/agents/mcp.rs` (add Claude CLI register/remove + `claude_is_configured`)

- [ ] **Step 1: Add the Claude CLI mechanism to `mcp.rs`**

Append to `src/agents/mcp.rs` (before `tests`):

```rust
/// Register memd as a user-scoped HTTP MCP server via the `claude` CLI.
/// Re-adds (remove + add) so it converges on the current endpoint.
pub fn claude_register(url: &str) -> Result<()> {
    let _ = Command::new("claude")
        .args(["mcp", "remove", "memd", "-s", "user"])
        .output();
    let status = Command::new("claude")
        .args(["mcp", "add", "--transport", "http", "memd", url, "--scope", "user"])
        .status()
        .context("running `claude mcp add`")?;
    if !status.success() {
        anyhow::bail!("`claude mcp add` exited with {status}");
    }
    Ok(())
}

/// Remove memd from Claude Code (user scope).
pub fn claude_remove() -> Result<()> {
    let _ = Command::new("claude")
        .args(["mcp", "remove", "memd", "-s", "user"])
        .output();
    Ok(())
}

/// True if `claude mcp get memd` reports a registration.
pub fn claude_is_configured() -> bool {
    Command::new("claude")
        .args(["mcp", "get", "memd"])
        .output()
        .map(|o| o.status.success())
        .unwrap_or(false)
}
```

- [ ] **Step 2: Write failing tests for `plan_action` and detection in `mod.rs`**

Add to `src/agents/mod.rs`:

```rust
/// What `setup` should do for one agent, given its current state, whether the
/// user wants it (desired), and whether we're interactive (only interactive
/// runs are allowed to remove).
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Configure,
    Remove,
    Skip,
}

pub fn plan_action(current: &AgentStatus, desired: bool, interactive: bool) -> Action {
    match (desired, current) {
        (true, _) => Action::Configure, // idempotent; converges on current endpoint
        (false, AgentStatus::Configured) if interactive => Action::Remove,
        _ => Action::Skip,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_configures_when_desired() {
        assert_eq!(plan_action(&AgentStatus::Detected, true, true), Action::Configure);
        assert_eq!(plan_action(&AgentStatus::NotDetected, true, false), Action::Configure);
    }

    #[test]
    fn plan_removes_only_when_interactive_and_configured() {
        assert_eq!(plan_action(&AgentStatus::Configured, false, true), Action::Remove);
        // Non-interactive never removes (no destructive change without a human).
        assert_eq!(plan_action(&AgentStatus::Configured, false, false), Action::Skip);
    }

    #[test]
    fn plan_skips_undesired_unconfigured() {
        assert_eq!(plan_action(&AgentStatus::Detected, false, true), Action::Skip);
    }

    #[test]
    fn registry_has_seven_agents_with_resolvable_ids() {
        let ids: Vec<_> = registry().iter().map(|a| a.id).collect();
        assert_eq!(ids.len(), 7);
        for id in ["claude-code", "codex", "gemini-cli", "cursor", "windsurf", "cline", "zed"] {
            assert!(ids.contains(&id), "missing agent {id}");
        }
    }

    #[test]
    fn dir_detect_reflects_filesystem() {
        let dir = tempfile::tempdir().unwrap();
        let present = dir.path().join("exists");
        std::fs::create_dir_all(&present).unwrap();
        assert!(DetectRule::DirExists(present).detected());
        assert!(!DetectRule::DirExists(dir.path().join("nope")).detected());
    }
}
```

- [ ] **Step 3: Run to confirm failure**

Run: `cargo test --lib agents 2>&1 | head -20`
Expected: FAIL — `AgentStatus`, `DetectRule`, `registry`, `Agent` undefined (won't compile).

- [ ] **Step 4: Implement the registry types and builder in `mod.rs`**

Add to `src/agents/mod.rs` (after the `mcp_url` fn). This defines `Agent`, `DetectRule`, `AgentStatus`, `McpKind`, `registry()`, `status`, `configure`, `remove`:

```rust
/// How an agent's MCP entry is written.
enum McpKind {
    /// Claude Code CLI (`claude mcp add/remove`).
    ClaudeCli,
    /// Merge into a JSON config: (container key, url field, extra const fields).
    Json {
        path: PathBuf,
        container: &'static str,
        url_key: &'static str,
        extra: &'static [(&'static str, &'static str)],
    },
    /// Merge `[mcp_servers.memd]` into a TOML config.
    Toml { path: PathBuf },
}

/// How an agent is detected as installed.
pub enum DetectRule {
    CliOnPath(&'static str),
    DirExists(PathBuf),
    FileExists(PathBuf),
}

impl DetectRule {
    pub fn detected(&self) -> bool {
        match self {
            DetectRule::CliOnPath(cmd) => which(cmd),
            DetectRule::DirExists(p) => p.is_dir(),
            DetectRule::FileExists(p) => p.is_file(),
        }
    }
}

#[derive(Debug, PartialEq, Eq)]
pub enum AgentStatus {
    NotDetected,
    Detected,
    Configured,
}

pub struct Agent {
    pub id: &'static str,
    pub name: &'static str,
    pub detect: DetectRule,
    mcp: McpKind,
    pub directives: Option<PathBuf>,
    pub hooks: bool,
}

impl Agent {
    /// Human label for the MCP transport this agent uses.
    pub fn transport(&self) -> &'static str {
        match self.mcp {
            McpKind::ClaudeCli => "HTTP",
            McpKind::Json { .. } => "HTTP",
            McpKind::Toml { .. } => "HTTP",
        }
    }

    pub fn status(&self) -> AgentStatus {
        if !self.detect.detected() {
            return AgentStatus::NotDetected;
        }
        let configured = match &self.mcp {
            McpKind::ClaudeCli => mcp::claude_is_configured(),
            McpKind::Json { path, container, .. } => mcp::json_has_mcp(path, container),
            McpKind::Toml { path } => mcp::toml_has_mcp(path),
        };
        if configured {
            AgentStatus::Configured
        } else {
            AgentStatus::Detected
        }
    }

    /// Write memd's MCP entry, directives, and (if applicable) hooks.
    pub fn configure(&self, bin: &Path, url: &str) -> Result<()> {
        match &self.mcp {
            McpKind::ClaudeCli => mcp::claude_register(url)?,
            McpKind::Json { path, container, url_key, extra } => {
                mcp::json_merge_mcp(path, container, url, url_key, extra, true)?
            }
            McpKind::Toml { path } => mcp::toml_merge_mcp(path, url, true)?,
        }
        if let Some(path) = &self.directives {
            directives::upsert_directive(path)?;
        }
        if self.hooks {
            let _ = hooks::install_claude_hooks(bin);
        }
        Ok(())
    }

    /// Reverse `configure`: remove the MCP entry, directives, and hooks.
    pub fn remove(&self) -> Result<()> {
        match &self.mcp {
            McpKind::ClaudeCli => mcp::claude_remove()?,
            McpKind::Json { path, container, url_key, extra } => {
                mcp::json_merge_mcp(path, container, "", url_key, extra, false)?
            }
            McpKind::Toml { path } => mcp::toml_merge_mcp(path, "", false)?,
        }
        if let Some(path) = &self.directives {
            directives::remove_directive(path)?;
        }
        if self.hooks {
            let _ = hooks::remove_claude_hooks();
        }
        Ok(())
    }
}

/// Look up a command on PATH.
fn which(cmd: &str) -> bool {
    std::env::var_os("PATH")
        .map(|path| {
            std::env::split_paths(&path).any(|d| d.join(cmd).is_file())
        })
        .unwrap_or(false)
}

fn home() -> PathBuf {
    directories::BaseDirs::new()
        .map(|b| b.home_dir().to_path_buf())
        .unwrap_or_else(|| PathBuf::from("/"))
}

/// macOS/Linux path to Cline's MCP settings file under the VS Code extension's
/// globalStorage. Windows is out of scope.
fn cline_settings() -> PathBuf {
    let h = home();
    let base = if cfg!(target_os = "macos") {
        h.join("Library/Application Support/Code/User/globalStorage")
    } else {
        h.join(".config/Code/User/globalStorage")
    };
    base.join("saoudrizwan.claude-dev/settings/cline_mcp_settings.json")
}

/// The static registry of supported agents (paths resolved against $HOME).
pub fn registry() -> Vec<Agent> {
    let h = home();
    vec![
        Agent {
            id: "claude-code",
            name: "Claude Code",
            detect: DetectRule::CliOnPath("claude"),
            mcp: McpKind::ClaudeCli,
            directives: Some(h.join(".claude/CLAUDE.md")),
            hooks: true,
        },
        Agent {
            id: "codex",
            name: "Codex",
            detect: DetectRule::DirExists(h.join(".codex")),
            mcp: McpKind::Toml { path: h.join(".codex/config.toml") },
            directives: Some(h.join(".codex/AGENTS.md")),
            hooks: false,
        },
        Agent {
            id: "gemini-cli",
            name: "Gemini CLI",
            detect: DetectRule::DirExists(h.join(".gemini")),
            mcp: McpKind::Json {
                path: h.join(".gemini/settings.json"),
                container: "mcpServers",
                url_key: "httpUrl",
                extra: &[],
            },
            directives: Some(h.join(".gemini/GEMINI.md")),
            hooks: false,
        },
        Agent {
            id: "cursor",
            name: "Cursor",
            detect: DetectRule::DirExists(h.join(".cursor")),
            mcp: McpKind::Json {
                path: h.join(".cursor/mcp.json"),
                container: "mcpServers",
                url_key: "url",
                extra: &[],
            },
            directives: None,
            hooks: false,
        },
        Agent {
            id: "windsurf",
            name: "Windsurf",
            detect: DetectRule::DirExists(h.join(".codeium/windsurf")),
            mcp: McpKind::Json {
                path: h.join(".codeium/windsurf/mcp_config.json"),
                container: "mcpServers",
                url_key: "serverUrl",
                extra: &[],
            },
            directives: None,
            hooks: false,
        },
        Agent {
            id: "cline",
            name: "Cline",
            detect: DetectRule::FileExists(cline_settings()),
            mcp: McpKind::Json {
                path: cline_settings(),
                container: "mcpServers",
                url_key: "url",
                extra: &[("type", "streamableHttp")],
            },
            directives: None,
            hooks: false,
        },
        Agent {
            id: "zed",
            name: "Zed",
            detect: DetectRule::DirExists(h.join(".config/zed")),
            mcp: McpKind::Json {
                path: h.join(".config/zed/settings.json"),
                container: "context_servers",
                url_key: "url",
                extra: &[],
            },
            directives: None,
            hooks: false,
        },
    ]
}
```

- [ ] **Step 5: Run the agents tests**

Run: `cargo test --lib agents 2>&1 | tail -20`
Expected: PASS — all `mcp`, `directives`, and `mod` tests green.

- [ ] **Step 6: Full build**

Run: `cargo build 2>&1 | tail -20`
Expected: SUCCEEDS now (the `directives.rs`/`mod.rs` references to `registry()`/`Agent` resolve). There may be `dead_code` warnings for `configure`/`remove`/`mcp_url` until Task 7 wires them — acceptable for now.

- [ ] **Step 7: Commit**

```bash
git add Cargo.toml Cargo.lock src/main.rs src/agents/
git commit -m "feat(agents): registry, detection, status, and MCP merge helpers"
```

---

### Task 6: Rewrite `cli::setup` to use the interactive picker

**Files:**
- Modify: `src/cli.rs:456-525` (`setup`)
- Modify: `src/cli.rs:646-664` (`directives_install`/`directives_uninstall`)
- Modify: `src/cli.rs` — delete moved helpers: `register_claude_code` (924-947), `install_claude_hooks` (951-981), `ensure_hook` (984-1034), `directive_block` (806-820), `target_instruction_files` (823-831), `upsert_directive` (834-848), `remove_directive` (851-862), `managed_region` (865-873), and the `which` helper (917-922) if now unused, plus the `DIRECTIVE_*` consts (802-803).

- [ ] **Step 1: Replace the body of `directives_install`/`directives_uninstall`**

In `src/cli.rs`, change these two `pub fn`s to delegate to the agents module:

```rust
/// Write the managed memd directive block into known agent instruction files.
pub fn directives_install() -> Result<()> {
    crate::agents::directives_install_all()
}

/// Remove the managed memd directive block from agent instruction files.
pub fn directives_uninstall() -> Result<()> {
    crate::agents::directives_remove_all()
}
```

- [ ] **Step 2: Delete the now-moved helpers from `cli.rs`**

Remove these items (their logic now lives in `src/agents/`): `DIRECTIVE_START`/`DIRECTIVE_END` consts, `directive_block`, `target_instruction_files`, `upsert_directive`, `remove_directive`, `managed_region`, `register_claude_code`, `install_claude_hooks`, `ensure_hook`. Keep `which` only if still referenced elsewhere in `cli.rs`; if `cargo build` later flags it unused, delete it too.

- [ ] **Step 3: Replace step 5–7 of `setup` with the picker**

In `src/cli.rs`, replace the block from `// 5. Register with detected MCP clients.` through the end of the hooks block (lines ~504–519) with:

```rust
    // 5–7. Pick agents and converge their memd wiring (MCP + directives + hooks).
    configure_agents(&installed, &cfg, no_hooks)?;
```

- [ ] **Step 4: Add the `configure_agents` helper to `cli.rs`**

Add this function to `src/cli.rs` (near `setup`). It builds the registry, prompts on a TTY, and applies the diff:

```rust
/// Interactive agent selection + convergence. On a TTY, show a multi-select
/// pre-checked with already-configured agents (sync semantics: unchecking a
/// configured agent removes memd from it). Off a TTY, configure every detected
/// agent and remove nothing.
fn configure_agents(installed: &Path, cfg: &Config, no_hooks: bool) -> Result<()> {
    use std::io::IsTerminal;

    let agents = crate::agents::registry();
    let statuses: Vec<_> = agents.iter().map(|a| a.status()).collect();
    let url = crate::agents::mcp_url(cfg);
    let interactive = std::io::stdin().is_terminal();

    // Determine the desired selection (one bool per agent).
    let desired: Vec<bool> = if interactive {
        let items: Vec<String> = agents
            .iter()
            .zip(&statuses)
            .map(|(a, s)| {
                let tag = match s {
                    crate::agents::AgentStatus::Configured => "configured",
                    crate::agents::AgentStatus::Detected => "detected",
                    crate::agents::AgentStatus::NotDetected => "not detected",
                };
                format!("{} ({tag})", a.name)
            })
            .collect();
        let checked: Vec<bool> = statuses
            .iter()
            .map(|s| matches!(s, crate::agents::AgentStatus::Configured))
            .collect();
        match dialoguer::MultiSelect::new()
            .with_prompt("Select agents to connect to memd (space toggles, enter confirms)")
            .items(&items)
            .defaults(&checked)
            .interact_opt()?
        {
            Some(picked) => (0..agents.len()).map(|i| picked.contains(&i)).collect(),
            None => {
                println!("• agent setup skipped");
                return Ok(());
            }
        }
    } else {
        // Non-TTY: desired = detected (or already configured).
        statuses
            .iter()
            .map(|s| !matches!(s, crate::agents::AgentStatus::NotDetected))
            .collect()
    };

    for ((agent, status), want) in agents.iter().zip(&statuses).zip(&desired) {
        match crate::agents::plan_action(status, *want, interactive) {
            crate::agents::Action::Configure => {
                if agent.hooks && no_hooks {
                    // Configure MCP + directives but skip hooks: temporarily
                    // honor --no-hooks by configuring then noting the skip.
                    match agent.configure(installed, &url) {
                        Ok(_) => println!("✓ {}: configured (hooks skipped via --no-hooks)", agent.name),
                        Err(e) => println!("⚠ {}: {e}", agent.name),
                    }
                } else {
                    match agent.configure(installed, &url) {
                        Ok(_) => println!("✓ {}: configured ({})", agent.name, agent.transport()),
                        Err(e) => println!("⚠ {}: {e}", agent.name),
                    }
                }
            }
            crate::agents::Action::Remove => match agent.remove() {
                Ok(_) => println!("• {}: removed", agent.name),
                Err(e) => println!("⚠ {}: {e}", agent.name),
            },
            crate::agents::Action::Skip => {}
        }
    }
    println!("\nOpen a new agent session for the changes to take effect.");
    Ok(())
}
```

Note on `--no-hooks`: the simplest correct behavior is to pass the flag down. To keep `Agent::configure` flag-free, change its signature to `configure(&self, bin, url, install_hooks: bool)` and gate the `if self.hooks` block on `install_hooks`. **Apply that:** update `configure` in `mod.rs` to take `install_hooks: bool` and use `if self.hooks && install_hooks`, then call `agent.configure(installed, &url, !no_hooks)` here and drop the duplicated branch above (call once: `agent.configure(installed, &url, !no_hooks)` and print `configured ({transport})`).

- [ ] **Step 5: Simplify per the note — final `Configure` arm**

Update `Agent::configure` signature in `src/agents/mod.rs`:

```rust
    pub fn configure(&self, bin: &Path, url: &str, install_hooks: bool) -> Result<()> {
        // ... unchanged MCP + directives ...
        if self.hooks && install_hooks {
            let _ = hooks::install_claude_hooks(bin);
        }
        Ok(())
    }
```

And the `Configure` arm in `configure_agents` becomes:

```rust
            crate::agents::Action::Configure => match agent.configure(installed, &url, !no_hooks) {
                Ok(_) => println!("✓ {}: configured ({})", agent.name, agent.transport()),
                Err(e) => println!("⚠ {}: {e}", agent.name),
            },
```

- [ ] **Step 6: Update the `setup` doc-comment line about MCP clients**

The closing `println!` in `setup` ("memd is set up...") stays. Ensure no leftover references to `register_claude_code`/`install_claude_hooks` remain.

- [ ] **Step 7: Build and run unit tests**

Run: `cargo build && cargo test --lib`
Expected: PASS. Fix any unused-import/dead-code errors (e.g. remove `use ...Path` if now unused, delete `which` from `cli.rs` if unused).

- [ ] **Step 8: Smoke-test the picker manually**

Run: `cargo run -- setup --no-hooks`
Expected: after the daemon health steps, a checklist of the 7 agents appears with detected/configured tags; confirming applies and prints per-agent ✓/•/⚠ lines. (Pressing Esc prints "agent setup skipped".)

- [ ] **Step 9: Commit**

```bash
git add src/cli.rs src/agents/mod.rs
git commit -m "feat(setup): interactive agent picker with sync semantics"
```

---

### Task 7: Add the Agents section to `memd status`

**Files:**
- Modify: `src/cli.rs:80-138` (`status`)

- [ ] **Step 1: Append an Agents block to `status`**

In `src/cli.rs`, just before the `match crawler::last_summary()` block at the end of `status`, add:

```rust
    println!("Agents:");
    for agent in crate::agents::registry() {
        let line = match agent.status() {
            crate::agents::AgentStatus::Configured => {
                format!("configured ({})", agent.transport())
            }
            crate::agents::AgentStatus::Detected => "detected — not configured".to_string(),
            crate::agents::AgentStatus::NotDetected => "not detected".to_string(),
        };
        println!("  {:<13} {}", agent.name, line);
    }
```

- [ ] **Step 2: Build and run status**

Run: `cargo build && cargo run -- status`
Expected: output now includes an `Agents:` block listing all 7 agents with their state; agents you connected in Task 6 show `configured (HTTP)`.

- [ ] **Step 3: Commit**

```bash
git add src/cli.rs
git commit -m "feat(status): report per-agent memd wiring state"
```

---

### Task 8: Docs, formatting, lint, and full verification

**Files:**
- Modify: `docs/mcp.mdx`
- Modify: `README.md`

- [ ] **Step 1: Document the picker and status in `docs/mcp.mdx`**

After the `## Transports` section in `docs/mcp.mdx`, add:

```mdx
## Connecting agents

`memd setup` detects the LLM agents installed on your machine — Claude Code,
Codex, Gemini CLI, Cursor, Windsurf, Cline, and Zed — and shows a checklist of
which to connect. Each selected agent gets memd's MCP server registered (HTTP
endpoint), plus usage directives and lifecycle hooks where supported. The
checklist is the source of truth: un-checking a previously connected agent
removes memd from it. In a non-interactive shell (CI, piped), `setup` connects
every detected agent and removes nothing.

Run `memd status` to see each agent's wiring state (`configured`, `detected —
not configured`, or `not detected`).
```

- [ ] **Step 2: Update `README.md` setup section**

Find the README section describing `memd setup` and add a sentence: "On setup, memd shows an interactive checklist of detected agents (Claude Code, Codex, Gemini CLI, Cursor, Windsurf, Cline, Zed) and wires the selected ones to its MCP server; `memd status` reports each agent's state." (Match the README's existing wording/heading style.)

- [ ] **Step 3: Format and lint**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings`
Expected: no errors. Fix any clippy findings (e.g. needless borrows, `format!` in `println!`).

- [ ] **Step 4: Full test + release build**

Run: `cargo test && cargo build --release`
Expected: all tests PASS; release build SUCCEEDS.

- [ ] **Step 5: Commit**

```bash
git add docs/mcp.mdx README.md
git commit -m "docs: document agent picker and status wiring"
```

---

## Self-review notes

- **Spec coverage:** interactive multi-select (Task 6), sync/uncheck-removes (Task 6 `plan_action` + `Action::Remove`), non-TTY auto-detect (Task 6), HTTP-preferred transport — all seven verified HTTP-capable (registry in Task 5), `src/agents/` adapter abstraction (Tasks 2–5), `memd status` Agents section (Task 7), directives limited to Claude/Codex/Gemini (registry `directives` field), hooks Claude-only (`hooks` flag). All covered.
- **Type consistency:** `AgentStatus` {NotDetected, Detected, Configured}, `Action` {Configure, Remove, Skip}, `Agent::configure(bin, url, install_hooks)`, `Agent::remove()`, `plan_action(&AgentStatus, bool, bool)`, `json_merge_mcp(path, container, url, url_key, extra, present)`, `toml_merge_mcp(path, url, present)` used consistently across tasks.
- **No Windows** Cline path (documented out of scope). Exact agent paths verified against June-2026 docs; re-verify if an agent ships a breaking config change.
```
