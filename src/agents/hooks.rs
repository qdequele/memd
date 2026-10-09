//! Agent lifecycle hooks: a session-start hook that injects relevant memories
//! and an end-of-turn hook that conservatively captures durable turns.
//!
//! Three agents expose a compatible hook model (an event name → array of
//! matcher groups → array of command hooks), so one merge routine serves all:
//!
//! | Agent       | File                      | Context output            |
//! |-------------|---------------------------|---------------------------|
//! | Claude Code | `~/.claude/settings.json` | plain stdout              |
//! | Codex       | `~/.codex/hooks.json`     | plain stdout              |
//! | Gemini CLI  | `~/.gemini/settings.json` | JSON `hookSpecificOutput` |
//!
//! Every merge is idempotent and leaves unrelated hooks untouched.

use anyhow::{Context, Result};
use serde_json::{Value, json};
use std::path::{Path, PathBuf};

/// Which agent's hook dialect to write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum HookKind {
    ClaudeCode,
    Codex,
    GeminiCli,
}

/// One hook to install: the agent event name, a marker that identifies memd's
/// entry (`memd <marker>`), the command, and a timeout in seconds.
struct HookSpec {
    event: &'static str,
    marker: &'static str,
    command: String,
    timeout_secs: u64,
    matcher: Option<&'static str>,
}

impl HookKind {
    fn file(self, home: &Path) -> PathBuf {
        match self {
            HookKind::ClaudeCode => home.join(".claude/settings.json"),
            HookKind::Codex => home.join(".codex/hooks.json"),
            HookKind::GeminiCli => home.join(".gemini/settings.json"),
        }
    }

    /// The hooks memd installs for this agent. `ensure` revives a dead daemon
    /// so recall keeps working; `;` runs `context` regardless (it degrades
    /// gracefully when the daemon is down).
    fn specs(self, exe: &str) -> Vec<HookSpec> {
        match self {
            HookKind::ClaudeCode => vec![
                HookSpec {
                    event: "SessionStart",
                    marker: "context",
                    command: format!(
                        "{exe} ensure; {exe} context --agent claude-code --scope \"$CLAUDE_PROJECT_DIR\""
                    ),
                    timeout_secs: 10,
                    matcher: None,
                },
                HookSpec {
                    event: "Stop",
                    marker: "capture",
                    command: format!("{exe} capture --agent claude-code"),
                    timeout_secs: 15,
                    matcher: None,
                },
            ],
            HookKind::Codex => vec![
                HookSpec {
                    event: "SessionStart",
                    marker: "context",
                    command: format!("{exe} ensure; {exe} context --agent codex"),
                    timeout_secs: 10,
                    matcher: Some("startup|resume|clear"),
                },
                HookSpec {
                    event: "Stop",
                    marker: "capture",
                    command: format!("{exe} capture --agent codex"),
                    timeout_secs: 15,
                    matcher: None,
                },
            ],
            HookKind::GeminiCli => vec![
                HookSpec {
                    event: "SessionStart",
                    marker: "context",
                    command: format!(
                        "{exe} ensure; {exe} context --agent gemini-cli --format json"
                    ),
                    timeout_secs: 10,
                    matcher: None,
                },
                HookSpec {
                    event: "AfterAgent",
                    marker: "capture",
                    command: format!("{exe} capture --agent gemini-cli"),
                    timeout_secs: 15,
                    matcher: None,
                },
            ],
        }
    }
}

fn home() -> Result<PathBuf> {
    Ok(directories::BaseDirs::new()
        .context("home directory")?
        .home_dir()
        .to_path_buf())
}

/// Merge memd's hooks for `kind` into the agent's hook file. Idempotent;
/// returns whether the file changed.
pub fn install_hooks(kind: HookKind, installed: &Path) -> Result<bool> {
    let path = kind.file(&home()?);
    let mut root: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    if !root.is_object() {
        root = json!({});
    }

    let exe = installed.to_string_lossy();
    let mut changed = false;
    for spec in kind.specs(&exe) {
        changed |= ensure_hook(&mut root, &spec);
    }
    if kind == HookKind::GeminiCli {
        changed |= enable_gemini_hooks(&mut root);
    }

    if changed {
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)?;
        }
        std::fs::write(&path, serde_json::to_string_pretty(&root)?)
            .with_context(|| format!("writing {}", path.display()))?;
    }
    Ok(changed)
}

/// Remove memd's hook groups for `kind`. Returns whether the file changed.
pub fn remove_hooks(kind: HookKind) -> Result<bool> {
    let path = kind.file(&home()?);
    let Ok(text) = std::fs::read_to_string(&path) else {
        return Ok(false);
    };
    let Ok(mut root) = serde_json::from_str::<Value>(&text) else {
        return Ok(false);
    };
    let mut changed = false;
    if let Some(events) = root.get_mut("hooks").and_then(|h| h.as_object_mut()) {
        for (_, arr) in events.iter_mut() {
            let Some(arr) = arr.as_array_mut() else {
                continue;
            };
            let before = arr.len();
            arr.retain(|group| !group_is_memd(group));
            changed |= arr.len() != before;
        }
    }
    if changed {
        std::fs::write(&path, serde_json::to_string_pretty(&root)?)?;
    }
    Ok(changed)
}

/// Gemini CLI gates hooks behind two flags; set both so the installed hooks
/// actually run. Returns whether anything changed.
fn enable_gemini_hooks(root: &mut Value) -> bool {
    let mut changed = false;
    let tools = root
        .as_object_mut()
        .unwrap()
        .entry("tools")
        .or_insert_with(|| json!({}));
    if tools.is_object() && tools.get("enableHooks") != Some(&Value::Bool(true)) {
        tools["enableHooks"] = Value::Bool(true);
        changed = true;
    }
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if hooks.is_object() && hooks.get("enabled") != Some(&Value::Bool(true)) {
        hooks["enabled"] = Value::Bool(true);
        changed = true;
    }
    changed
}

/// True if a matcher group holds a memd command hook.
fn group_is_memd(group: &Value) -> bool {
    group
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
}

/// Ensure a `memd <marker>` command hook exists under `hooks.<event>`.
fn ensure_hook(root: &mut Value, spec: &HookSpec) -> bool {
    let needle = format!("memd {}", spec.marker);
    let hooks = root
        .as_object_mut()
        .unwrap()
        .entry("hooks")
        .or_insert_with(|| json!({}));
    if !hooks.is_object() {
        *hooks = json!({});
    }
    let arr = hooks
        .as_object_mut()
        .unwrap()
        .entry(spec.event)
        .or_insert_with(|| json!([]));
    let Some(arr) = arr.as_array_mut() else {
        return false;
    };
    let mut desired = json!({
        "hooks": [ { "type": "command", "command": spec.command, "timeout": spec.timeout_secs } ]
    });
    if let Some(m) = spec.matcher {
        desired["matcher"] = Value::String(m.to_string());
    }
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
        if *group == desired {
            return false;
        }
        *group = desired;
        return true;
    }
    arr.push(desired);
    true
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ensure_hook_is_idempotent_and_updates_stale_commands() {
        let mut root = json!({ "hooks": { "SessionStart": [
            { "hooks": [ { "type": "command", "command": "/old/memd context", "timeout": 10 } ] },
            { "hooks": [ { "type": "command", "command": "echo other", "timeout": 1 } ] }
        ] } });
        let spec = HookSpec {
            event: "SessionStart",
            marker: "context",
            command: "/new/memd ensure; /new/memd context".into(),
            timeout_secs: 10,
            matcher: None,
        };
        assert!(ensure_hook(&mut root, &spec));
        assert!(!ensure_hook(&mut root, &spec));
        let arr = root["hooks"]["SessionStart"].as_array().unwrap();
        assert_eq!(arr.len(), 2, "unrelated hook preserved, memd hook replaced");
        assert_eq!(
            arr[0]["hooks"][0]["command"],
            "/new/memd ensure; /new/memd context"
        );
    }

    #[test]
    fn codex_specs_carry_a_matcher_and_gemini_emits_json() {
        let codex = HookKind::Codex.specs("/bin/memd");
        assert_eq!(codex[0].matcher, Some("startup|resume|clear"));
        let gemini = HookKind::GeminiCli.specs("/bin/memd");
        assert!(gemini[0].command.contains("--format json"));
        assert_eq!(gemini[1].event, "AfterAgent");
    }

    #[test]
    fn gemini_flags_are_set_once() {
        let mut root = json!({});
        assert!(enable_gemini_hooks(&mut root));
        assert!(!enable_gemini_hooks(&mut root));
        assert_eq!(root["tools"]["enableHooks"], true);
        assert_eq!(root["hooks"]["enabled"], true);
    }
}
