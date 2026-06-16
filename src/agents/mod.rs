//! Per-agent integration: detect installed LLM agents, register memd's MCP
//! server with them, write usage directives, and report wiring state. The
//! registry is mostly data — `mcp.rs` carries the three write mechanisms.

mod directives;
mod hooks;
mod mcp;

use crate::config::Config;
use anyhow::Result;
use std::path::{Path, PathBuf};

// Re-exported for `cli::directives_install`/`uninstall`, wired up in Task 6.
#[allow(unused_imports)]
pub use directives::{install_all as directives_install_all, remove_all as directives_remove_all};

/// The HTTP MCP endpoint every agent registers against.
pub fn mcp_url(cfg: &Config) -> String {
    format!("http://{}:{}/mcp", cfg.mcp.host, cfg.mcp.port)
}

/// What `setup` should do for one agent, given its current state, whether the
/// user wants it (desired), and whether we're interactive (only interactive
/// runs are allowed to remove).
#[derive(Debug, PartialEq, Eq)]
pub enum Action {
    Configure,
    Remove,
    Skip,
}

/// Decide the action for one agent given desired-vs-current state. Configuring
/// is idempotent; removal only happens interactively (no destructive change
/// without a human present).
pub fn plan_action(current: &AgentStatus, desired: bool, interactive: bool) -> Action {
    match (desired, current) {
        (true, _) => Action::Configure, // idempotent; converges on current endpoint
        (false, AgentStatus::Configured) if interactive => Action::Remove,
        _ => Action::Skip,
    }
}

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
    /// True if the agent is present on this machine per the rule.
    pub fn detected(&self) -> bool {
        match self {
            DetectRule::CliOnPath(cmd) => which(cmd),
            DetectRule::DirExists(p) => p.is_dir(),
            DetectRule::FileExists(p) => p.is_file(),
        }
    }
}

/// The wiring state of an agent on this machine.
#[derive(Debug, PartialEq, Eq)]
pub enum AgentStatus {
    NotDetected,
    Detected,
    Configured,
}

/// A supported LLM agent: how to detect it, how to write its MCP entry, and
/// which optional integrations (directives, hooks) it supports.
pub struct Agent {
    // stable identifier; referenced in tests/diagnostics
    #[allow(dead_code)]
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
        // Every supported agent registers over HTTP today; the per-variant
        // arms are kept so transports can diverge later without restructuring.
        match self.mcp {
            McpKind::ClaudeCli => "HTTP",
            McpKind::Json { .. } => "HTTP",
            McpKind::Toml { .. } => "HTTP",
        }
    }

    /// Current wiring state: not detected, detected (but unconfigured), or configured.
    pub fn status(&self) -> AgentStatus {
        if !self.detect.detected() {
            return AgentStatus::NotDetected;
        }
        let configured = match &self.mcp {
            McpKind::ClaudeCli => mcp::claude_is_configured(),
            McpKind::Json {
                path, container, ..
            } => mcp::json_has_mcp(path, container),
            McpKind::Toml { path } => mcp::toml_has_mcp(path),
        };
        if configured {
            AgentStatus::Configured
        } else {
            AgentStatus::Detected
        }
    }

    /// Write memd's MCP entry, directives, and (when `install_hooks`) hooks.
    pub fn configure(&self, bin: &Path, url: &str, install_hooks: bool) -> Result<()> {
        match &self.mcp {
            McpKind::ClaudeCli => mcp::claude_register(url)?,
            McpKind::Json {
                path,
                container,
                url_key,
                extra,
            } => mcp::json_merge_mcp(path, container, url, url_key, extra, true)?,
            McpKind::Toml { path } => mcp::toml_merge_mcp(path, url, true)?,
        }
        if let Some(path) = &self.directives {
            directives::upsert_directive(path)?;
        }
        if self.hooks && install_hooks {
            let _ = hooks::install_claude_hooks(bin);
        }
        Ok(())
    }

    /// Reverse `configure`: remove the MCP entry, directives, and hooks.
    pub fn remove(&self) -> Result<()> {
        match &self.mcp {
            McpKind::ClaudeCli => mcp::claude_remove()?,
            McpKind::Json {
                path,
                container,
                url_key,
                extra,
            } => mcp::json_merge_mcp(path, container, "", url_key, extra, false)?,
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
        .map(|path| std::env::split_paths(&path).any(|d| d.join(cmd).is_file()))
        .unwrap_or(false)
}

/// Resolve $HOME, falling back to `/` if it can't be determined.
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
    let cline = cline_settings();
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
            mcp: McpKind::Toml {
                path: h.join(".codex/config.toml"),
            },
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
            detect: DetectRule::FileExists(cline.clone()),
            mcp: McpKind::Json {
                path: cline,
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

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn plan_configures_when_desired() {
        assert_eq!(
            plan_action(&AgentStatus::Detected, true, true),
            Action::Configure
        );
        assert_eq!(
            plan_action(&AgentStatus::NotDetected, true, false),
            Action::Configure
        );
    }

    #[test]
    fn plan_removes_only_when_interactive_and_configured() {
        assert_eq!(
            plan_action(&AgentStatus::Configured, false, true),
            Action::Remove
        );
        // Non-interactive never removes (no destructive change without a human).
        assert_eq!(
            plan_action(&AgentStatus::Configured, false, false),
            Action::Skip
        );
    }

    #[test]
    fn plan_skips_undesired_unconfigured() {
        assert_eq!(
            plan_action(&AgentStatus::Detected, false, true),
            Action::Skip
        );
    }

    #[test]
    fn registry_has_seven_agents_with_resolvable_ids() {
        let ids: Vec<_> = registry().iter().map(|a| a.id).collect();
        assert_eq!(ids.len(), 7);
        for id in [
            "claude-code",
            "codex",
            "gemini-cli",
            "cursor",
            "windsurf",
            "cline",
            "zed",
        ] {
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
