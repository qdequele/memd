//! Claude Code lifecycle hooks: a SessionStart hook that injects relevant
//! memories, and a Stop hook that conservatively captures durable turns. Merged
//! idempotently into `~/.claude/settings.json`.

use anyhow::{Context, Result};
use serde_json::{Value, json};
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
