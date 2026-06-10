//! Auto-update: daily check against GitHub releases for memd itself and the
//! managed Meilisearch engine.
//! Spec: docs/superpowers/specs/2026-06-10-auto-update-design.md

use crate::paths;
use anyhow::{Context, Result};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};

/// Persisted updater state (`update-state.json` in the data dir).
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateState {
    /// Unix seconds of the last completed check (0 = never).
    pub last_check_secs: i64,
    /// Human-readable outcome of the last check.
    pub last_result: String,
    /// Engine versions whose migration failed; never retried.
    pub failed_engine_versions: Vec<String>,
}

impl UpdateState {
    /// Load from `path`; a missing or corrupt file yields the default.
    pub fn load_from(path: &Path) -> UpdateState {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
            .unwrap_or_default()
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))
    }

    pub fn load() -> UpdateState {
        paths::update_state_file()
            .map(|p| Self::load_from(&p))
            .unwrap_or_default()
    }

    pub fn save(&self) -> Result<()> {
        self.save_to(&paths::update_state_file()?)
    }

    pub fn engine_version_failed(&self, version: &str) -> bool {
        self.failed_engine_versions.iter().any(|v| v == version)
    }

    pub fn record_engine_failure(&mut self, version: &str) {
        if !self.engine_version_failed(version) {
            self.failed_engine_versions.push(version.to_string());
        }
    }
}

/// Exclusive lock guarding update application (daemon vs `memd update`).
/// Released on drop. A lock older than an hour is presumed stale (crashed
/// updater) and reclaimed — no single update step takes anywhere near that.
pub struct UpdateLock {
    path: PathBuf,
}

impl UpdateLock {
    pub fn acquire() -> Result<UpdateLock> {
        Self::acquire_at(paths::update_lock_file()?)
    }

    pub fn acquire_at(path: PathBuf) -> Result<UpdateLock> {
        match Self::try_create(&path) {
            Ok(lock) => Ok(lock),
            Err(e) if e.kind() == std::io::ErrorKind::AlreadyExists => {
                let stale = std::fs::metadata(&path)
                    .and_then(|m| m.modified())
                    .map(|t| t.elapsed().map(|d| d.as_secs() > 3600).unwrap_or(true))
                    .unwrap_or(true);
                if stale {
                    // Tiny remove→create race window is acceptable: at most one
                    // daemon and one interactive `memd update` contend here.
                    let _ = std::fs::remove_file(&path);
                    Self::try_create(&path)
                        .with_context(|| format!("reclaiming stale lock {}", path.display()))
                } else {
                    anyhow::bail!("another update is in progress (lock {})", path.display())
                }
            }
            Err(e) => Err(e).with_context(|| format!("creating lock {}", path.display())),
        }
    }

    fn try_create(path: &Path) -> std::io::Result<UpdateLock> {
        std::fs::OpenOptions::new()
            .write(true)
            .create_new(true)
            .open(path)?;
        Ok(UpdateLock {
            path: path.to_path_buf(),
        })
    }
}

impl Drop for UpdateLock {
    fn drop(&mut self) {
        let _ = std::fs::remove_file(&self.path);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn state_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update-state.json");
        let mut s = UpdateState {
            last_check_secs: 42,
            last_result: "ok".into(),
            ..Default::default()
        };
        s.record_engine_failure("v1.46.0");
        s.save_to(&path).unwrap();
        let loaded = UpdateState::load_from(&path);
        assert_eq!(loaded.last_check_secs, 42);
        assert_eq!(loaded.last_result, "ok");
        assert!(loaded.engine_version_failed("v1.46.0"));
        assert!(!loaded.engine_version_failed("v1.47.0"));
    }

    #[test]
    fn missing_or_corrupt_state_yields_default() {
        let dir = tempfile::tempdir().unwrap();
        let missing = dir.path().join("nope.json");
        assert_eq!(UpdateState::load_from(&missing).last_check_secs, 0);
        let corrupt = dir.path().join("bad.json");
        std::fs::write(&corrupt, "{not json").unwrap();
        assert_eq!(UpdateState::load_from(&corrupt).last_check_secs, 0);
    }

    #[test]
    fn record_engine_failure_deduplicates() {
        let mut s = UpdateState::default();
        s.record_engine_failure("v1.46.0");
        s.record_engine_failure("v1.46.0");
        assert_eq!(s.failed_engine_versions.len(), 1);
    }

    #[test]
    fn lock_is_exclusive_and_released_on_drop() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("update.lock");
        let lock = UpdateLock::acquire_at(path.clone()).unwrap();
        assert!(UpdateLock::acquire_at(path.clone()).is_err());
        drop(lock);
        assert!(UpdateLock::acquire_at(path).is_ok());
    }
}
