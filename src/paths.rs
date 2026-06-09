//! Filesystem locations used by memd.
//!
//! Config lives under `~/.config/memd/`; data (Meilisearch db, downloaded
//! binary, logs, runtime state) lives under the platform data dir, e.g.
//! `~/Library/Application Support/memd/` on macOS.

use anyhow::{Context, Result};
use directories::ProjectDirs;
use std::path::PathBuf;

fn project_dirs() -> Result<ProjectDirs> {
    ProjectDirs::from("com", "meilisearch", "memd")
        .context("could not determine platform application directories")
}

/// `~/.config/memd/config.toml`.
pub fn config_file() -> Result<PathBuf> {
    // The PRD pins config to ~/.config/memd/config.toml on every platform.
    let home = directories::BaseDirs::new()
        .context("could not determine home directory")?
        .home_dir()
        .to_path_buf();
    Ok(home.join(".config").join("memd").join("config.toml"))
}

/// Root data directory (created on demand).
pub fn data_dir() -> Result<PathBuf> {
    let dir = project_dirs()?.data_dir().to_path_buf();
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating data dir {}", dir.display()))?;
    Ok(dir)
}

/// Stable install location for the `memd` binary itself (`~/.local/bin/memd`).
/// `memd setup` copies the running binary here so the launchd service and hooks
/// reference a path that survives `cargo clean` or moving the source repo.
pub fn installed_bin() -> Result<PathBuf> {
    let home = directories::BaseDirs::new()
        .context("could not determine home directory")?
        .home_dir()
        .to_path_buf();
    Ok(home.join(".local").join("bin").join("memd"))
}

/// Directory holding downloaded Meilisearch binaries.
pub fn bin_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("bin");
    std::fs::create_dir_all(&dir)?;
    Ok(dir)
}

/// Meilisearch database directory.
pub fn meili_db_dir() -> Result<PathBuf> {
    Ok(data_dir()?.join("meili-data"))
}

/// The daemon log file.
pub fn log_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("memd.log"))
}

/// The daemon pid file.
pub fn pid_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("memd.pid"))
}

/// Persisted runtime state (e.g. last crawl summary).
pub fn state_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("state.json"))
}
