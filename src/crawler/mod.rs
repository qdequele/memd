//! Passive ingestion: scan configured roots for meaningful files, map them to
//! memory items, keep them in sync via a `notify` watcher + periodic reconcile,
//! and handle deletions (PRD §6.1.4, §9).

use crate::config::Config;
use crate::memory::classify;
use crate::memory::service::path_id;
use crate::memory::{MemoryItem, MemoryService};
use crate::paths;
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::time::Duration;
use walkdir::WalkDir;

/// Outcome of a crawl pass, persisted to the state file for `memd crawl status`.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct CrawlSummary {
    pub scanned: usize,
    pub indexed: usize,
    pub skipped: usize,
    pub deleted: usize,
    pub errors: usize,
    pub finished_at: i64,
}

/// Run a full scan of all configured roots: index/update matching files and
/// remove documents for files that have disappeared.
pub async fn scan(cfg: &Config, svc: &MemoryService) -> Result<CrawlSummary> {
    let deny = build_globset(&cfg.crawler.deny_globs)?;
    let exclude: HashSet<&str> = cfg
        .crawler
        .exclude_dirs
        .iter()
        .map(|s| s.as_str())
        .collect();

    let mut summary = CrawlSummary::default();
    let mut seen: HashSet<String> = HashSet::new();
    let mut batch: Vec<MemoryItem> = Vec::new();
    const BATCH: usize = 200;

    for root in cfg.expand_roots() {
        if !root.exists() {
            continue;
        }
        let walker = WalkDir::new(&root).follow_links(false).into_iter();
        for entry in walker.filter_entry(|e| !is_excluded(e.path(), &exclude)) {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    summary.errors += 1;
                    continue;
                }
            };
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            summary.scanned += 1;

            let Some(ty) = classify::classify_path(&path.to_string_lossy()) else {
                continue;
            };
            if deny.is_match(path) {
                summary.skipped += 1;
                continue;
            }
            if file_too_big(path, cfg.crawler.max_file_bytes) {
                summary.skipped += 1;
                continue;
            }

            let path_str = path.to_string_lossy().to_string();
            seen.insert(path_id(&path_str));

            match std::fs::read_to_string(path) {
                Ok(content) if !content.trim().is_empty() => {
                    let scope = project_scope(&root, path);
                    match svc
                        .prepare_crawled(&path_str, content, ty, scope, None)
                        .await
                    {
                        Ok(Some(item)) => {
                            batch.push(item);
                            summary.indexed += 1;
                            if batch.len() >= BATCH {
                                flush(svc, &mut batch, &mut summary).await;
                            }
                        }
                        Ok(None) => summary.skipped += 1,
                        Err(e) => {
                            tracing::warn!("preparing {path_str} failed: {e}");
                            summary.errors += 1;
                        }
                    }
                }
                Ok(_) => summary.skipped += 1,
                Err(_) => summary.skipped += 1, // binary / unreadable
            }
        }
    }
    flush(svc, &mut batch, &mut summary).await;

    // Deletions: drop crawler docs whose source file no longer matches.
    summary.deleted = reconcile_deletions(svc, &seen).await.unwrap_or(0);
    summary.finished_at = crate::memory::model::now_secs();
    save_summary(&summary)?;

    // Record one rolled-up history event per crawl — but only when something
    // actually changed, so periodic reconciles that find nothing don't flood
    // the audit log.
    if summary.indexed + summary.deleted + summary.errors > 0 {
        let detail = format!(
            "{} indexed, {} skipped, {} deleted, {} errors",
            summary.indexed, summary.skipped, summary.deleted, summary.errors
        );
        svc.events()
            .record(crate::history::MemoryEvent::crawl(detail))
            .await;
    }
    Ok(summary)
}

/// Index (or refresh) a single file, if it qualifies. Used by the watcher.
pub async fn index_one(cfg: &Config, svc: &MemoryService, path: &Path) -> Result<bool> {
    let path_str = path.to_string_lossy().to_string();
    let Some(ty) = classify::classify_path(&path_str) else {
        return Ok(false);
    };
    let deny = build_globset(&cfg.crawler.deny_globs)?;
    if deny.is_match(path) || file_too_big(path, cfg.crawler.max_file_bytes) {
        return Ok(false);
    }
    let content = match std::fs::read_to_string(path) {
        Ok(c) if !c.trim().is_empty() => c,
        _ => return Ok(false),
    };
    let root = cfg
        .expand_roots()
        .into_iter()
        .find(|r| path.starts_with(r))
        .unwrap_or_else(|| path.parent().map(PathBuf::from).unwrap_or_default());
    let scope = project_scope(&root, path);
    svc.upsert_crawled(&path_str, content, ty, scope, None)
        .await
}

/// Remove the document for a deleted/removed file path.
pub async fn remove_one(svc: &MemoryService, path: &Path) -> Result<bool> {
    let id = path_id(&path.to_string_lossy());
    svc.forget(&id, crate::memory::Source::Crawler).await
}

/// Watch configured roots and keep the index in sync. Runs until cancelled.
/// Performs an initial scan, then reacts to filesystem events and reconciles
/// periodically to catch missed events.
pub async fn watch(cfg: Config, svc: MemoryService) -> Result<()> {
    use notify::{RecursiveMode, Watcher};

    // Initial scan.
    if let Err(e) = scan(&cfg, &svc).await {
        tracing::warn!("initial crawl failed: {e}");
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })?;
    for root in cfg.expand_roots() {
        if root.exists()
            && let Err(e) = watcher.watch(&root, RecursiveMode::Recursive)
        {
            tracing::warn!("watching {} failed: {e}", root.display());
        }
    }

    let mut reconcile =
        tokio::time::interval(Duration::from_secs(cfg.crawler.reconcile_secs.max(60)));
    reconcile.tick().await; // consume the immediate first tick

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                handle_event(&cfg, &svc, event).await;
            }
            _ = reconcile.tick() => {
                tracing::info!("periodic crawl reconcile");
                if let Err(e) = scan(&cfg, &svc).await {
                    tracing::warn!("reconcile crawl failed: {e}");
                }
            }
            else => break,
        }
    }
    Ok(())
}

async fn handle_event(cfg: &Config, svc: &MemoryService, event: notify::Event) {
    use notify::EventKind;
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) => {
            for path in event.paths {
                if path.is_file() {
                    let _ = index_one(cfg, svc, &path).await;
                }
            }
        }
        EventKind::Remove(_) => {
            for path in event.paths {
                let _ = remove_one(svc, &path).await;
            }
        }
        _ => {}
    }
}

/// Delete crawler documents whose source path is gone or was not seen this scan.
async fn reconcile_deletions(svc: &MemoryService, seen: &HashSet<String>) -> Result<usize> {
    // First pass: collect ids whose source file is gone or was not seen this
    // scan. Collecting before deleting keeps pagination stable.
    let mut to_delete: Vec<String> = Vec::new();
    let mut offset = 0;
    loop {
        let body = json!({
            "q": "",
            "limit": 1000,
            "offset": offset,
            "filter": "source = 'crawler'",
            "attributesToRetrieve": ["id", "source_path"],
        });
        let resp = svc.client().search(&body).await?;
        let hits = resp
            .get("hits")
            .and_then(|h| h.as_array())
            .cloned()
            .unwrap_or_default();
        if hits.is_empty() {
            break;
        }
        for hit in &hits {
            let id = hit.get("id").and_then(Value::as_str).unwrap_or("");
            let path = hit.get("source_path").and_then(Value::as_str).unwrap_or("");
            let gone = !Path::new(path).exists();
            let unseen = !seen.contains(id);
            if gone || unseen {
                to_delete.push(id.to_string());
            }
        }
        if hits.len() < 1000 {
            break;
        }
        offset += 1000;
    }

    // Second pass: one batched delete per chunk.
    let mut deleted = 0;
    for chunk in to_delete.chunks(1000) {
        deleted += svc.client().delete_many(chunk).await.unwrap_or(0);
    }
    Ok(deleted)
}

/// Load the last crawl summary, if any.
pub fn last_summary() -> Option<CrawlSummary> {
    let path = paths::state_file().ok()?;
    let raw = std::fs::read_to_string(path).ok()?;
    let state: Value = serde_json::from_str(&raw).ok()?;
    serde_json::from_value(state.get("last_crawl")?.clone()).ok()
}

fn save_summary(summary: &CrawlSummary) -> Result<()> {
    let path = paths::state_file()?;
    let mut state: Value = std::fs::read_to_string(&path)
        .ok()
        .and_then(|s| serde_json::from_str(&s).ok())
        .unwrap_or_else(|| json!({}));
    state["last_crawl"] = serde_json::to_value(summary)?;
    std::fs::write(&path, serde_json::to_string_pretty(&state)?)?;
    Ok(())
}

/// Flush a batch of prepared documents to the store, accounting for failures.
async fn flush(svc: &MemoryService, batch: &mut Vec<MemoryItem>, summary: &mut CrawlSummary) {
    if batch.is_empty() {
        return;
    }
    if let Err(e) = svc.upsert_batch(batch).await {
        tracing::warn!("batch upsert of {} docs failed: {e}", batch.len());
        summary.errors += batch.len();
        summary.indexed = summary.indexed.saturating_sub(batch.len());
    }
    batch.clear();
}

fn build_globset(patterns: &[String]) -> Result<GlobSet> {
    let mut builder = GlobSetBuilder::new();
    for p in patterns {
        builder.add(Glob::new(p)?);
    }
    Ok(builder.build()?)
}

fn is_excluded(path: &Path, exclude: &HashSet<&str>) -> bool {
    path.file_name()
        .and_then(|n| n.to_str())
        .map(|n| exclude.contains(n))
        .unwrap_or(false)
}

fn file_too_big(path: &Path, max: u64) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > max)
        .unwrap_or(true)
}

/// Project-level scope: the configured root plus the first path component below
/// it (lean path-prefix scoping per PRD open question #6).
fn project_scope(root: &Path, file: &Path) -> String {
    if let Ok(rel) = file.strip_prefix(root)
        && let Some(first) = rel.components().next()
    {
        let candidate = root.join(first.as_os_str());
        if candidate.is_dir() {
            return candidate.to_string_lossy().to_string();
        }
    }
    root.to_string_lossy().to_string()
}
