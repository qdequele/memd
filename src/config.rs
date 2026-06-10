//! Configuration: `~/.config/memd/config.toml`.
//!
//! Loaded on every invocation. Missing files are created with defaults, so the
//! "it just works" zero-config promise holds: the first run writes a sane file
//! the user can later tweak.

use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::PathBuf;

/// Default pinned Meilisearch release. Embedders + hybrid search are stable
/// (no experimental flag needed) on modern releases.
pub const DEFAULT_MEILI_VERSION: &str = "v1.45.1";
/// Default local embedding model run inside Meilisearch's huggingFace embedder.
pub const DEFAULT_EMBED_MODEL: &str = "BAAI/bge-small-en-v1.5";
/// Default port for memd's dedicated Meilisearch instance.
pub const DEFAULT_MEILI_PORT: u16 = 7701;
/// Default port for the always-on HTTP MCP endpoint.
pub const DEFAULT_MCP_PORT: u16 = 7702;

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
#[derive(Default)]
pub struct Config {
    pub meilisearch: MeiliConfig,
    pub mcp: McpConfig,
    pub embedder: EmbedderConfig,
    pub crawler: CrawlerConfig,
    pub update: UpdateConfig,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct MeiliConfig {
    /// Pinned Meilisearch version (git tag, e.g. `v1.45.1`).
    pub version: String,
    /// Bind host. Localhost-only by default.
    pub host: String,
    /// Bind port.
    pub port: u16,
    /// Master key for the dedicated instance. Auto-generated on first run.
    pub master_key: String,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct McpConfig {
    /// Bind host for the HTTP MCP endpoint.
    pub host: String,
    /// Bind port for the HTTP MCP endpoint.
    pub port: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct EmbedderConfig {
    /// Embedder source. `huggingFace` runs locally inside Meilisearch.
    pub source: String,
    /// Model id (HuggingFace repo) for the local embedder.
    pub model: String,
    /// Default semantic ratio for hybrid search (0.0 keyword .. 1.0 vector).
    pub default_semantic_ratio: f32,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct CrawlerConfig {
    /// Roots to scan (supports a leading `~`).
    pub roots: Vec<String>,
    /// Directory names to skip entirely.
    pub exclude_dirs: Vec<String>,
    /// Maximum file size to index, in bytes.
    pub max_file_bytes: u64,
    /// Filename patterns that must never be indexed (secrets hygiene).
    pub deny_globs: Vec<String>,
    /// Periodic reconcile interval, in seconds.
    pub reconcile_secs: u64,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Automatically apply memd + engine updates found by the daily check.
    pub auto: bool,
}

impl Default for MeiliConfig {
    fn default() -> Self {
        Self {
            version: DEFAULT_MEILI_VERSION.to_string(),
            host: "127.0.0.1".to_string(),
            port: DEFAULT_MEILI_PORT,
            master_key: String::new(),
        }
    }
}

impl Default for McpConfig {
    fn default() -> Self {
        Self {
            host: "127.0.0.1".to_string(),
            port: DEFAULT_MCP_PORT,
        }
    }
}

impl Default for EmbedderConfig {
    fn default() -> Self {
        Self {
            source: "huggingFace".to_string(),
            model: DEFAULT_EMBED_MODEL.to_string(),
            default_semantic_ratio: 0.5,
        }
    }
}

impl Default for CrawlerConfig {
    fn default() -> Self {
        Self {
            roots: vec!["~/Projects".to_string()],
            exclude_dirs: vec![
                "node_modules",
                "target",
                ".git",
                "dist",
                "build",
                ".next",
                "vendor",
                ".venv",
                "venv",
                "__pycache__",
                ".cache",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            max_file_bytes: 1_000_000,
            deny_globs: vec![
                "**/.env",
                "**/.env.*",
                "**/*.pem",
                "**/*.key",
                "**/id_rsa*",
                "**/*.p12",
                "**/*.pfx",
                "**/secrets*",
                "**/*.secret",
            ]
            .into_iter()
            .map(String::from)
            .collect(),
            reconcile_secs: 3600,
        }
    }
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { auto: true }
    }
}

impl Config {
    /// Load config from disk, creating it with defaults (and a freshly
    /// generated master key) if it does not yet exist.
    pub fn load_or_init() -> Result<Config> {
        let path = paths::config_file()?;
        if path.exists() {
            let raw = std::fs::read_to_string(&path)
                .with_context(|| format!("reading config {}", path.display()))?;
            let mut cfg: Config = toml::from_str(&raw)
                .with_context(|| format!("parsing config {}", path.display()))?;
            if cfg.meilisearch.master_key.is_empty() {
                cfg.meilisearch.master_key = generate_key();
                cfg.save()?;
            }
            Ok(cfg)
        } else {
            let mut cfg = Config::default();
            cfg.meilisearch.master_key = generate_key();
            cfg.save()?;
            Ok(cfg)
        }
    }

    /// Persist the config back to disk.
    pub fn save(&self) -> Result<()> {
        let path = paths::config_file()?;
        if let Some(parent) = path.parent() {
            std::fs::create_dir_all(parent)
                .with_context(|| format!("creating config dir {}", parent.display()))?;
        }
        let raw = toml::to_string_pretty(self).context("serializing config")?;
        std::fs::write(&path, raw).with_context(|| format!("writing config {}", path.display()))?;
        Ok(())
    }

    /// Base URL of the dedicated Meilisearch instance.
    pub fn meili_url(&self) -> String {
        format!("http://{}:{}", self.meilisearch.host, self.meilisearch.port)
    }

    /// Expand a configured root path, resolving a leading `~`.
    pub fn expand_roots(&self) -> Vec<PathBuf> {
        self.crawler.roots.iter().map(|r| expand_tilde(r)).collect()
    }
}

/// Resolve a leading `~` to the user's home directory.
pub fn expand_tilde(path: &str) -> PathBuf {
    if let Some(rest) = path.strip_prefix("~/")
        && let Some(base) = directories::BaseDirs::new()
    {
        return base.home_dir().join(rest);
    }
    PathBuf::from(path)
}

/// Generate a random master key (two UUIDv4s concatenated, hyphens stripped).
fn generate_key() -> String {
    let a = uuid::Uuid::new_v4().simple().to_string();
    let b = uuid::Uuid::new_v4().simple().to_string();
    format!("{a}{b}")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn update_auto_defaults_to_true() {
        assert!(Config::default().update.auto);
    }

    #[test]
    fn update_section_parses_from_toml() {
        let cfg: Config = toml::from_str("[update]\nauto = false\n").unwrap();
        assert!(!cfg.update.auto);
    }
}
