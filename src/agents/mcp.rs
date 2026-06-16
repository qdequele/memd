//! The three ways memd registers/unregisters its MCP server with an agent:
//! the Claude Code CLI, a merged JSON config, or a merged TOML config. All
//! operations are idempotent and preserve unrelated keys.

use anyhow::{Context, Result};
use serde_json::{Value, json};
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

/// True if a `memd` entry exists under `container` in the JSON file.
pub fn json_has_mcp(path: &Path, container: &str) -> bool {
    std::fs::read_to_string(path)
        .ok()
        .and_then(|s| serde_json::from_str::<Value>(&s).ok())
        .and_then(|v| v.get(container).and_then(|c| c.get("memd")).map(|_| true))
        .unwrap_or(false)
}

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

/// Register memd as a user-scoped HTTP MCP server via the `claude` CLI.
/// Re-adds (remove + add) so it converges on the current endpoint.
pub fn claude_register(url: &str) -> Result<()> {
    let _ = Command::new("claude")
        .args(["mcp", "remove", "memd", "-s", "user"])
        .output();
    let status = Command::new("claude")
        .args([
            "mcp",
            "add",
            "--transport",
            "http",
            "memd",
            url,
            "--scope",
            "user",
        ])
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

    #[test]
    fn toml_insert_and_preserve() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        std::fs::write(
            &p,
            "model = \"gpt-5\"\n\n[mcp_servers.other]\nurl = \"http://o\"\n",
        )
        .unwrap();
        toml_merge_mcp(&p, "http://x/mcp", true).unwrap();
        let doc: toml::Value = toml::from_str(&std::fs::read_to_string(&p).unwrap()).unwrap();
        assert_eq!(doc["model"].as_str(), Some("gpt-5"));
        assert_eq!(
            doc["mcp_servers"]["other"]["url"].as_str(),
            Some("http://o")
        );
        assert_eq!(
            doc["mcp_servers"]["memd"]["url"].as_str(),
            Some("http://x/mcp")
        );
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
        assert_eq!(
            doc["mcp_servers"]["other"]["url"].as_str(),
            Some("http://o")
        );
    }

    #[test]
    fn toml_has_mcp_detects_presence() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.toml");
        assert!(!toml_has_mcp(&p));
        toml_merge_mcp(&p, "http://x/mcp", true).unwrap();
        assert!(toml_has_mcp(&p));
    }
}
