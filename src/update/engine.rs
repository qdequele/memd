//! Engine (Meilisearch) migration: prepared while the daemon runs (dump +
//! binary download + marker file), applied at the next daemon startup with
//! full rollback on failure. Memories are never lost: the old data dir is
//! moved aside, not deleted, and restored if anything goes wrong.

use super::UpdateState;
use crate::config::Config;
use crate::meili::{self, MeiliClient};
use crate::memory::model::now_secs;
use crate::paths;
use anyhow::{Context, Result, bail};
use serde::{Deserialize, Serialize};
use std::path::{Path, PathBuf};
use std::time::Duration;

/// A prepared migration older than this is discarded: writes made after the
/// dump are not in it, so applying a stale dump would silently drop them.
const MAX_MARKER_AGE_SECS: i64 = 3600;

/// Marker file describing a prepared migration (`engine-migration.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Migration {
    pub from: String,
    pub to: String,
    pub dump_path: PathBuf,
    /// Documents in the memories index when the dump was taken.
    pub expected_docs: u64,
    /// Unix seconds when the dump was taken (`0` in markers from older builds).
    #[serde(default)]
    pub prepared_at: i64,
}

impl Migration {
    pub fn load_from(path: &Path) -> Option<Migration> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        let tmp = path.with_extension("json.tmp");
        std::fs::write(&tmp, raw).with_context(|| format!("writing {}", tmp.display()))?;
        std::fs::rename(&tmp, path).with_context(|| format!("committing {}", path.display()))
    }
}

/// While the daemon is up: dump the live instance, fetch the new binary, and
/// leave a marker for the next startup. Does not touch the running engine.
pub async fn prepare(cfg: &Config, client: &MeiliClient, to_version: &str) -> Result<()> {
    let dump_uid = client
        .create_dump()
        .await
        .context("dumping live instance")?;
    let dump_path = paths::dumps_dir()?.join(format!("{dump_uid}.dump"));
    if !dump_path.exists() {
        bail!(
            "dump file {} not found after dump task",
            dump_path.display()
        );
    }
    // A missing index (fresh store) legitimately has zero documents; any other
    // stats failure must abort — otherwise verification would be vacuous.
    let expected_docs = match client.stats().await {
        Ok(s) => s
            .get("numberOfDocuments")
            .and_then(|n| n.as_u64())
            .unwrap_or(0),
        Err(e) if format!("{e:#}").contains("index_not_found") => 0,
        Err(e) => return Err(e).context("reading index stats before migration"),
    };
    meili::download_binary(to_version)
        .await
        .context("downloading new engine binary")?;
    let m = Migration {
        from: cfg.meilisearch.version.clone(),
        to: to_version.to_string(),
        dump_path,
        expected_docs,
        prepared_at: now_secs(),
    };
    m.save_to(&paths::engine_migration_file()?)?;
    tracing::info!("engine migration {} -> {} prepared", m.from, m.to);
    Ok(())
}

/// Outcome of [`apply_pending`].
pub enum Applied {
    /// No marker; nothing done.
    None,
    /// Migration succeeded; `cfg` now pins `to`.
    Migrated { to: String },
    /// Migration failed; rolled back to the previous version, `to` blacklisted.
    RolledBack { to: String, error: String },
    /// A prepared migration was too old to apply safely and was discarded.
    Expired { to: String },
}

/// Apply a pending migration, if any. Called at daemon startup BEFORE the
/// normal engine spawn. Migration failure is not an `Err`: the daemon must
/// always come up, on the old version if need be.
pub async fn apply_pending(cfg: &mut Config) -> Result<Applied> {
    let db = paths::meili_db_dir()?;
    if recover_stranded_backup(&db)? {
        tracing::info!("stranded backup recovered before migration check");
    }

    let marker_path = paths::engine_migration_file()?;
    let Some(m) = Migration::load_from(&marker_path) else {
        return Ok(Applied::None);
    };
    // Single-shot: remove the marker first so a crash mid-migration cannot
    // loop the daemon into repeated migration attempts.
    let _ = std::fs::remove_file(&marker_path);

    if m.prepared_at == 0 || now_secs() - m.prepared_at > MAX_MARKER_AGE_SECS {
        tracing::warn!(
            "discarding stale engine migration to {} (prepared {}s ago); the next daily check will re-prepare it",
            m.to,
            if m.prepared_at == 0 {
                -1
            } else {
                now_secs() - m.prepared_at
            }
        );
        let _ = std::fs::remove_file(&m.dump_path);
        return Ok(Applied::Expired { to: m.to });
    }

    tracing::info!("applying engine migration {} -> {}", m.from, m.to);
    let backup = db.with_file_name(format!("meili-data.bak.{}", now_secs()));
    if db.exists() {
        std::fs::rename(&db, &backup).with_context(|| format!("backing up {}", db.display()))?;
    }

    match import_and_verify(cfg, &m).await {
        Ok(()) => {
            cfg.meilisearch.version = m.to.clone();
            cfg.save()?;
            prune_backups(&db)?;
            let _ = std::fs::remove_file(&m.dump_path);
            tracing::info!("engine migrated to {}", m.to);
            Ok(Applied::Migrated { to: m.to })
        }
        Err(e) => {
            let error = format!("{e:#}");
            tracing::error!("engine migration to {} failed: {error}", m.to);
            if let Err(restore_err) = restore_backup(&db, &backup) {
                tracing::error!(
                    "CRITICAL: rollback restore failed — your memories are intact at {}: {restore_err:#}",
                    backup.display()
                );
            }
            let state_path = paths::update_state_file()?;
            let mut state = UpdateState::load_from(&state_path);
            state.record_engine_failure(&m.to);
            state.last_result = format!("engine {} migration failed: {error}", m.to);
            let _ = state.save_to(&state_path);
            Ok(Applied::RolledBack { to: m.to, error })
        }
    }
}

/// Self-heal after a crash mid-migration: if the db dir is gone but a backup
/// exists (the rename happened, the import never finished), put the newest
/// backup back. Returns true when a recovery happened.
pub fn recover_stranded_backup(db: &Path) -> Result<bool> {
    if db.exists() {
        return Ok(false);
    }
    let Some(parent) = db.parent() else {
        return Ok(false);
    };
    let prefix = "meili-data.bak.";
    let mut backups: Vec<PathBuf> = std::fs::read_dir(parent)
        .into_iter()
        .flatten()
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    backups.sort();
    let Some(newest) = backups.pop() else {
        return Ok(false);
    };
    std::fs::rename(&newest, db)
        .with_context(|| format!("restoring stranded backup {}", newest.display()))?;
    tracing::warn!(
        "recovered database from stranded backup {} (crash during a previous migration?)",
        newest.display()
    );
    Ok(true)
}

/// Boot the new engine with `--import-dump` on the (empty) db dir, wait for
/// it, verify version and document count, then stop it. The regular daemon
/// spawn takes over afterwards on the migrated database.
async fn import_and_verify(cfg: &Config, m: &Migration) -> Result<()> {
    if !m.dump_path.exists() {
        bail!("dump file {} is missing", m.dump_path.display());
    }
    let mut new_cfg = cfg.clone();
    new_cfg.meilisearch.version = m.to.clone();
    let mut child = meili::spawn_with_import(&new_cfg, Some(&m.dump_path)).await?;
    let client = MeiliClient::new(new_cfg.meili_url(), new_cfg.meilisearch.master_key.clone());

    let result = async {
        // Meilisearch imports the dump before serving HTTP; allow plenty of
        // time, and fail fast if the process dies.
        let deadline = tokio::time::Instant::now() + Duration::from_secs(600);
        loop {
            if let Some(status) = child.try_wait()? {
                bail!("engine exited during import: {status}");
            }
            if client.is_healthy().await {
                break;
            }
            if tokio::time::Instant::now() > deadline {
                bail!("engine did not become healthy within 10 minutes");
            }
            tokio::time::sleep(Duration::from_millis(500)).await;
        }
        let v = client.version().await?;
        let want = m.to.trim_start_matches('v');
        if v != want {
            bail!("engine reports version {v}, expected {want}");
        }
        if m.expected_docs == 0 {
            tracing::warn!("no baseline document count — verification limited to version + health");
        }
        let docs = client
            .stats()
            .await
            .ok()
            .and_then(|s| s.get("numberOfDocuments").and_then(|n| n.as_u64()))
            .unwrap_or(0);
        if docs < m.expected_docs {
            bail!(
                "imported {docs} documents, expected at least {}",
                m.expected_docs
            );
        }
        Ok(())
    }
    .await;

    let _ = child.kill().await;
    result
}

/// Discard a half-imported db dir and put the backup back in place.
fn restore_backup(db: &Path, backup: &Path) -> Result<()> {
    if db.exists() {
        std::fs::remove_dir_all(db)
            .with_context(|| format!("removing half-imported {}", db.display()))?;
    }
    if backup.exists() {
        std::fs::rename(backup, db).with_context(|| format!("restoring {}", backup.display()))?;
    }
    Ok(())
}

/// Keep only the most recent `meili-data.bak.*` next to the db dir.
pub fn prune_backups(db_dir: &Path) -> Result<()> {
    let Some(parent) = db_dir.parent() else {
        return Ok(());
    };
    let prefix = "meili-data.bak.";
    let mut backups: Vec<PathBuf> = std::fs::read_dir(parent)?
        .filter_map(|e| e.ok())
        .map(|e| e.path())
        .filter(|p| {
            p.file_name()
                .and_then(|n| n.to_str())
                .is_some_and(|n| n.starts_with(prefix))
        })
        .collect();
    // Suffixes are unix seconds (fixed width until 2286) — lexicographic sort
    // is chronological.
    backups.sort();
    for old in backups.iter().rev().skip(1) {
        let result = if old.is_dir() {
            std::fs::remove_dir_all(old)
        } else {
            std::fs::remove_file(old)
        };
        if let Err(e) = result {
            tracing::warn!("could not prune old backup {}: {e}", old.display());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn migration_marker_round_trips() {
        let dir = tempfile::tempdir().unwrap();
        let path = dir.path().join("engine-migration.json");
        let m = Migration {
            from: "v1.45.1".into(),
            to: "v1.46.0".into(),
            dump_path: dir.path().join("dumps/abc.dump"),
            expected_docs: 123,
            prepared_at: 1234,
        };
        m.save_to(&path).unwrap();
        let loaded = Migration::load_from(&path).unwrap();
        assert_eq!(loaded.to, "v1.46.0");
        assert_eq!(loaded.expected_docs, 123);
        assert_eq!(loaded.prepared_at, 1234);
        assert!(Migration::load_from(&dir.path().join("missing.json")).is_none());
        // A marker missing `prepared_at` (older build) must still deserialise.
        assert_eq!(
            serde_json::from_str::<Migration>(
                r#"{"from":"a","to":"b","dump_path":"/x","expected_docs":1}"#
            )
            .unwrap()
            .prepared_at,
            0
        );
    }

    #[test]
    fn restore_backup_replaces_half_imported_db() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meili-data");
        let backup = dir.path().join("meili-data.bak.100");
        // Half-imported junk in db; sentinel in backup.
        std::fs::create_dir_all(&db).unwrap();
        std::fs::write(db.join("junk"), "partial import").unwrap();
        std::fs::create_dir_all(&backup).unwrap();
        std::fs::write(backup.join("VERSION"), "1.45.1").unwrap();

        restore_backup(&db, &backup).unwrap();

        assert!(db.join("VERSION").exists(), "backup contents restored");
        assert!(!db.join("junk").exists(), "junk removed");
        assert!(!backup.exists(), "backup moved back");
    }

    #[test]
    fn restore_backup_tolerates_missing_pieces() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meili-data");
        let backup = dir.path().join("meili-data.bak.100");
        // Neither exists: no-op, no error.
        restore_backup(&db, &backup).unwrap();
    }

    #[test]
    fn prune_keeps_only_newest_backup() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meili-data");
        for ts in [100, 200, 300] {
            std::fs::create_dir_all(dir.path().join(format!("meili-data.bak.{ts}"))).unwrap();
        }
        prune_backups(&db).unwrap();
        assert!(!dir.path().join("meili-data.bak.100").exists());
        assert!(!dir.path().join("meili-data.bak.200").exists());
        assert!(dir.path().join("meili-data.bak.300").exists());
    }

    #[test]
    fn recover_stranded_backup_restores_newest() {
        let dir = tempfile::tempdir().unwrap();
        let db = dir.path().join("meili-data");
        for ts in [100, 200] {
            let b = dir.path().join(format!("meili-data.bak.{ts}"));
            std::fs::create_dir_all(&b).unwrap();
            std::fs::write(b.join("VERSION"), ts.to_string()).unwrap();
        }
        assert!(recover_stranded_backup(&db).unwrap());
        assert_eq!(std::fs::read_to_string(db.join("VERSION")).unwrap(), "200");
        assert!(
            dir.path().join("meili-data.bak.100").exists(),
            "older backup untouched"
        );
        // With the db present, recovery is a no-op.
        assert!(!recover_stranded_backup(&db).unwrap());
    }
}
