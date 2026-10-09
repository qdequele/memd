//! Passive ingestion: scan configured roots for meaningful files, map them to
//! memory items, keep them in sync via a `notify` watcher + periodic reconcile,
//! and handle deletions (PRD §6.1.4, §9).
//!
//! Two kinds of roots are scanned:
//!
//! - **Project roots** from the config (`~/Projects` by default): READMEs and
//!   agent instruction files, scoped to the git repository they live in.
//! - **Agent knowledge roots** (see [`crate::agents::knowledge_roots`]): the
//!   directories where each supported agent keeps its own memories and global
//!   rules (`~/.claude/projects/*/memory`, `~/.codex/memories`, …). Indexing
//!   them is what lets one agent recall what another one learned.
//!
//! Linked git worktrees (a directory whose `.git` is a file) are skipped
//! everywhere: they are copies of a repository that is already indexed.

use crate::config::Config;
use crate::memory::classify;
use crate::memory::service::path_id;
use crate::memory::{MemoryItem, MemoryService};
use crate::paths;
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
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

/// Everything a scan or the watcher needs to decide whether a path qualifies
/// and which scope it belongs to. Built once from the config.
pub struct Crawler {
    deny: GlobSet,
    /// Excluded directory names (`node_modules`) or trailing path fragments
    /// (`.claude/worktrees`).
    exclude: Vec<String>,
    max_file_bytes: u64,
    /// Configured project roots.
    roots: Vec<PathBuf>,
    /// Agent knowledge roots (memories, global rules) that exist on this machine.
    knowledge_roots: Vec<PathBuf>,
}

impl Crawler {
    pub fn new(cfg: &Config) -> Result<Self> {
        Ok(Self {
            deny: build_globset(&cfg.crawler.deny_globs)?,
            exclude: cfg
                .crawler
                .exclude_dirs
                .iter()
                .map(|s| s.trim_matches('/').to_string())
                .collect(),
            max_file_bytes: cfg.crawler.max_file_bytes,
            roots: cfg
                .expand_roots()
                .into_iter()
                .filter(|r| r.exists())
                .collect(),
            knowledge_roots: crate::agents::knowledge_roots(),
        })
    }

    /// All directories the watcher should observe.
    pub fn watch_roots(&self) -> Vec<PathBuf> {
        self.roots
            .iter()
            .chain(self.knowledge_roots.iter())
            .cloned()
            .collect()
    }

    /// True if this directory must not be descended into: an excluded name or
    /// path fragment, or a linked git worktree.
    fn dir_excluded(&self, dir: &Path) -> bool {
        let s = dir.to_string_lossy();
        let name = dir.file_name().and_then(|n| n.to_str()).unwrap_or("");
        let by_name = self.exclude.iter().any(|e| {
            if e.contains('/') {
                s.ends_with(&format!("/{e}"))
            } else {
                e == name
            }
        });
        by_name || is_linked_worktree(dir)
    }

    /// True if any ancestor of `path` (up to the nearest root) is excluded.
    fn under_excluded_dir(&self, path: &Path) -> bool {
        let stop = self.watch_roots().into_iter().find(|r| path.starts_with(r));
        let mut cur = path.parent();
        while let Some(dir) = cur {
            if Some(dir) == stop.as_deref() {
                break;
            }
            if self.dir_excluded(dir) {
                return true;
            }
            cur = dir.parent();
        }
        false
    }

    /// Whether a file is something the crawler indexes: classifiable by path,
    /// not denied, not inside an excluded directory, not too big.
    fn qualifies(&self, path: &Path) -> Option<crate::memory::MemoryType> {
        let ty = classify::classify_path(&path.to_string_lossy())?;
        if self.deny.is_match(path) || self.under_excluded_dir(path) {
            return None;
        }
        if file_too_big(path, self.max_file_bytes) {
            return None;
        }
        Some(ty)
    }

    /// Scope for a file under a project root: the nearest enclosing git
    /// repository, else the file's own directory.
    fn project_scope(&self, root: &Path, file: &Path, cache: &mut ScopeCache) -> String {
        let dir = file.parent().unwrap_or(root).to_path_buf();
        if let Some(s) = cache.get(&dir) {
            return s.clone();
        }
        let mut cur: Option<&Path> = Some(&dir);
        let mut found: Option<PathBuf> = None;
        while let Some(d) = cur {
            if d.join(".git").is_dir() {
                found = Some(d.to_path_buf());
                break;
            }
            if d == root {
                break;
            }
            cur = d.parent();
        }
        let scope = found
            .unwrap_or_else(|| dir.clone())
            .to_string_lossy()
            .to_string();
        cache.insert(dir, scope.clone());
        scope
    }

    /// Scope for a file under an agent knowledge root. Claude Code's memory
    /// directories are named after the project path they belong to (its slug),
    /// which `slugs` maps back to a real directory; everything else is global.
    fn knowledge_scope(&self, file: &Path, slugs: &HashMap<String, PathBuf>) -> String {
        let s = file.to_string_lossy();
        if let Some(i) = s.find("/.claude/projects/") {
            let rest = &s[i + "/.claude/projects/".len()..];
            if let Some(slug) = rest.split('/').next() {
                return resolve_claude_slug(slug, slugs);
            }
        }
        "global".to_string()
    }

    /// Scope for any file (used by the watcher, which has no scan context).
    fn scope_for(&self, file: &Path) -> String {
        if let Some(root) = self.roots.iter().find(|r| file.starts_with(r)) {
            return self.project_scope(root, file, &mut HashMap::new());
        }
        let slugs = self.slug_map();
        self.knowledge_scope(file, &slugs)
    }

    /// Map of Claude Code project slugs → directories, built from every git
    /// repository under the configured roots.
    fn slug_map(&self) -> HashMap<String, PathBuf> {
        let mut map = HashMap::new();
        for root in &self.roots {
            for dir in self.git_roots(root) {
                map.insert(claude_slug(&dir.to_string_lossy()), dir);
            }
        }
        map
    }

    /// Every git repository directory under `root` (not descending into
    /// excluded directories or linked worktrees).
    fn git_roots(&self, root: &Path) -> Vec<PathBuf> {
        let mut out = Vec::new();
        let walker = WalkDir::new(root).follow_links(false).into_iter();
        for entry in walker
            .filter_entry(|e| !e.file_type().is_dir() || !self.dir_excluded(e.path()))
            .flatten()
        {
            if entry.file_type().is_dir() && entry.path().join(".git").is_dir() {
                out.push(entry.path().to_path_buf());
            }
        }
        out
    }
}

type ScopeCache = HashMap<PathBuf, String>;

/// Claude Code names a project's memory directory after its path with every
/// non-alphanumeric character replaced by `-`.
pub fn claude_slug(path: &str) -> String {
    path.chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '-' })
        .collect()
}

/// Map a Claude Code project slug back to a scope path. In order: an exact
/// known repository; the repository a worktree slug belongs to
/// (`<repo>--claude-worktrees-<name>`); the longest known repository the slug
/// extends (a sub-directory session); and finally a path reconstructed from
/// the slug itself. A session's memory is never made `global`: that would
/// inject it into every other project.
pub fn resolve_claude_slug(slug: &str, slugs: &HashMap<String, PathBuf>) -> String {
    if let Some(dir) = slugs.get(slug) {
        return dir.to_string_lossy().to_string();
    }
    if let Some(i) = slug.find("--claude-worktrees-")
        && let Some(dir) = slugs.get(&slug[..i])
    {
        return dir.to_string_lossy().to_string();
    }
    let mut best: Option<(&String, &PathBuf)> = None;
    for (k, v) in slugs {
        if slug.starts_with(k.as_str())
            && slug[k.len()..].starts_with('-')
            && best.map(|(b, _)| k.len() > b.len()).unwrap_or(true)
        {
            best = Some((k, v));
        }
    }
    if let Some((_, dir)) = best {
        return dir.to_string_lossy().to_string();
    }
    // `-Users-q-Projects-foo` → `/Users/q/Projects/foo` (lossy: `_` and `.`
    // also became `-`, but the result is still a path only that session uses).
    let guess: String = slug
        .split('-')
        .filter(|p| !p.is_empty())
        .map(|p| format!("/{p}"))
        .collect();
    if guess.is_empty() {
        "global".to_string()
    } else {
        guess
    }
}

/// A linked git worktree has a `.git` *file* (pointing at the main repo's
/// gitdir) instead of a `.git` directory.
fn is_linked_worktree(dir: &Path) -> bool {
    dir.join(".git").is_file()
}

/// Run a full scan of all roots: index/update matching files and remove
/// documents for files that have disappeared. With `reset`, every crawler
/// document is dropped first so the index is rebuilt from scratch.
pub async fn scan(cfg: &Config, svc: &MemoryService, reset: bool) -> Result<CrawlSummary> {
    let crawler = Crawler::new(cfg)?;
    scan_with(&crawler, svc, reset).await
}

async fn scan_with(crawler: &Crawler, svc: &MemoryService, reset: bool) -> Result<CrawlSummary> {
    let mut summary = CrawlSummary::default();

    // The crawler's stored state, loaded in a few paged requests rather than
    // one GET per file.
    let state: HashMap<String, (String, i64)> = if reset {
        summary.deleted = svc.forget_all_crawled().await?;
        HashMap::new()
    } else {
        svc.crawled_state().await?
    };

    let mut seen: HashSet<String> = HashSet::new();
    let mut batch: Vec<MemoryItem> = Vec::new();
    let mut scope_cache = ScopeCache::new();
    let mut git_roots: Vec<PathBuf> = Vec::new();
    const BATCH: usize = 200;

    // 1. Project roots.
    for root in &crawler.roots {
        let walker = WalkDir::new(root).follow_links(false).into_iter();
        for entry in
            walker.filter_entry(|e| !e.file_type().is_dir() || !crawler.dir_excluded(e.path()))
        {
            let entry = match entry {
                Ok(e) => e,
                Err(_) => {
                    summary.errors += 1;
                    continue;
                }
            };
            let path = entry.path();
            if entry.file_type().is_dir() {
                if path.join(".git").is_dir() {
                    git_roots.push(path.to_path_buf());
                }
                continue;
            }
            if !entry.file_type().is_file() {
                continue;
            }
            summary.scanned += 1;
            let Some(ty) = classify::classify_path(&path.to_string_lossy()) else {
                continue;
            };
            if crawler.deny.is_match(path) || file_too_big(path, crawler.max_file_bytes) {
                summary.skipped += 1;
                continue;
            }
            let scope = crawler.project_scope(root, path, &mut scope_cache);
            ingest(
                svc,
                path,
                ty,
                scope,
                &state,
                &mut seen,
                &mut batch,
                &mut summary,
            );
            if batch.len() >= BATCH {
                flush(svc, &mut batch, &mut summary).await;
            }
        }
    }

    // 2. Agent knowledge roots (memories, global rules).
    let slugs: HashMap<String, PathBuf> = git_roots
        .iter()
        .map(|d| (claude_slug(&d.to_string_lossy()), d.clone()))
        .collect();
    for root in &crawler.knowledge_roots {
        let walker = WalkDir::new(root).follow_links(false).into_iter();
        for entry in walker
            .filter_entry(|e| !e.file_type().is_dir() || !crawler.dir_excluded(e.path()))
            .flatten()
        {
            if !entry.file_type().is_file() {
                continue;
            }
            let path = entry.path();
            summary.scanned += 1;
            let Some(ty) = classify::classify_path(&path.to_string_lossy()) else {
                continue;
            };
            if crawler.deny.is_match(path) || file_too_big(path, crawler.max_file_bytes) {
                summary.skipped += 1;
                continue;
            }
            let scope = crawler.knowledge_scope(path, &slugs);
            ingest(
                svc,
                path,
                ty,
                scope,
                &state,
                &mut seen,
                &mut batch,
                &mut summary,
            );
            if batch.len() >= BATCH {
                flush(svc, &mut batch, &mut summary).await;
            }
        }
    }
    flush(svc, &mut batch, &mut summary).await;

    // 3. Deletions: every stored crawler doc that no qualifying file produced
    //    this pass (moved, deleted, or no longer matching the rules).
    let stale: Vec<String> = state
        .keys()
        .filter(|id| !seen.contains(*id))
        .cloned()
        .collect();
    for chunk in stale.chunks(1000) {
        summary.deleted += svc.client().delete_many(chunk).await.unwrap_or(0);
    }

    summary.finished_at = crate::memory::model::now_secs();
    save_summary(&summary)?;
    tracing::info!(
        "crawl: {} scanned, {} indexed, {} skipped, {} deleted, {} errors",
        summary.scanned,
        summary.indexed,
        summary.skipped,
        summary.deleted,
        summary.errors
    );

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

/// Read one qualifying file and queue it for upsert if its content changed.
#[allow(clippy::too_many_arguments)]
fn ingest(
    svc: &MemoryService,
    path: &Path,
    ty: crate::memory::MemoryType,
    scope: String,
    state: &HashMap<String, (String, i64)>,
    seen: &mut HashSet<String>,
    batch: &mut Vec<MemoryItem>,
    summary: &mut CrawlSummary,
) {
    let path_str = path.to_string_lossy().to_string();
    let id = path_id(&path_str);
    let content = match std::fs::read_to_string(path) {
        Ok(c) if !c.trim().is_empty() => c,
        _ => {
            summary.skipped += 1; // empty, binary, or unreadable
            return;
        }
    };
    seen.insert(id.clone());
    let existing = state.get(&id).map(|(h, c)| (h.as_str(), *c));
    match svc.prepare_crawled(&path_str, content, ty, scope, None, existing) {
        Some(item) => {
            batch.push(item);
            summary.indexed += 1;
        }
        None => summary.skipped += 1,
    }
}

/// Index (or refresh) a single file, if it qualifies. Used by the watcher.
pub async fn index_one(crawler: &Crawler, svc: &MemoryService, path: &Path) -> Result<bool> {
    let Some(ty) = crawler.qualifies(path) else {
        return Ok(false);
    };
    let content = match std::fs::read_to_string(path) {
        Ok(c) if !c.trim().is_empty() => c,
        _ => return Ok(false),
    };
    let scope = crawler.scope_for(path);
    svc.upsert_crawled(&path.to_string_lossy(), content, ty, scope, None)
        .await
}

/// Remove the document for a deleted file — only if it is a path the crawler
/// would have indexed. Every other deletion under a root (build artefacts,
/// `.git` internals, …) is ignored without touching the engine.
pub async fn remove_one(crawler: &Crawler, svc: &MemoryService, path: &Path) -> Result<bool> {
    if classify::classify_path(&path.to_string_lossy()).is_none()
        || crawler.under_excluded_dir(path)
    {
        return Ok(false);
    }
    let id = path_id(&path.to_string_lossy());
    svc.forget(&id, crate::memory::Source::Crawler).await
}

/// Watch all roots and keep the index in sync. Runs until cancelled. Performs
/// an initial scan, then reacts to filesystem events and reconciles
/// periodically to catch missed events.
pub async fn watch(cfg: Config, svc: MemoryService) -> Result<()> {
    use notify::{RecursiveMode, Watcher};

    let crawler = Crawler::new(&cfg)?;

    // Initial scan.
    if let Err(e) = scan_with(&crawler, &svc, false).await {
        tracing::warn!("initial crawl failed: {e}");
    }

    let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel::<notify::Event>();
    let mut watcher = notify::recommended_watcher(move |res: notify::Result<notify::Event>| {
        if let Ok(event) = res {
            let _ = tx.send(event);
        }
    })?;
    for root in crawler.watch_roots() {
        if let Err(e) = watcher.watch(&root, RecursiveMode::Recursive) {
            tracing::warn!("watching {} failed: {e}", root.display());
        }
    }

    let mut reconcile =
        tokio::time::interval(Duration::from_secs(cfg.crawler.reconcile_secs.max(60)));
    reconcile.tick().await; // consume the immediate first tick
    // With the engine down every event would log; cap the noise per process.
    let mut warn_limit = crate::logging::LogLimiter::new(10);

    loop {
        tokio::select! {
            Some(event) = rx.recv() => {
                handle_event(&crawler, &svc, event, &mut warn_limit).await;
            }
            _ = reconcile.tick() => {
                if warn_limit.suppressed() > 0 {
                    tracing::warn!(
                        "{} more watcher failures were not logged individually",
                        warn_limit.suppressed()
                    );
                    warn_limit = crate::logging::LogLimiter::new(10);
                }
                tracing::info!("periodic crawl reconcile");
                if let Err(e) = scan_with(&crawler, &svc, false).await {
                    tracing::warn!("reconcile crawl failed: {e}");
                }
            }
            else => break,
        }
    }
    Ok(())
}

async fn handle_event(
    crawler: &Crawler,
    svc: &MemoryService,
    event: notify::Event,
    warn_limit: &mut crate::logging::LogLimiter,
) {
    use notify::EventKind;
    // Cheap pre-filter: only paths the classifier recognises can matter, so
    // the flood of build/git/editor events never reaches the engine.
    let paths: Vec<PathBuf> = event
        .paths
        .into_iter()
        .filter(|p| classify::classify_path(&p.to_string_lossy()).is_some())
        .collect();
    if paths.is_empty() {
        return;
    }
    match event.kind {
        EventKind::Create(_) | EventKind::Modify(_) => {
            for path in paths {
                let res = if path.is_file() {
                    index_one(crawler, svc, &path).await
                } else if !path.exists() {
                    // Editors often report a rename/replace as a modify.
                    remove_one(crawler, svc, &path).await
                } else {
                    Ok(false)
                };
                if let Err(e) = res
                    && warn_limit.should_log()
                {
                    tracing::warn!("indexing {} failed: {e}", path.display());
                }
            }
        }
        EventKind::Remove(_) => {
            for path in paths {
                if let Err(e) = remove_one(crawler, svc, &path).await
                    && warn_limit.should_log()
                {
                    tracing::warn!("removing {} failed: {e}", path.display());
                }
            }
        }
        _ => {}
    }
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

fn file_too_big(path: &Path, max: u64) -> bool {
    std::fs::metadata(path)
        .map(|m| m.len() > max)
        .unwrap_or(true)
}

#[cfg(test)]
mod tests {
    use super::*;

    fn crawler_for(root: &Path) -> Crawler {
        let mut cfg = Config::default();
        cfg.crawler.roots = vec![root.to_string_lossy().to_string()];
        let mut c = Crawler::new(&cfg).unwrap();
        c.knowledge_roots.clear();
        c
    }

    #[test]
    fn claude_slug_matches_claude_code() {
        assert_eq!(
            claude_slug("/Users/q/Projects/Meilisearch/_side_projects/memd"),
            "-Users-q-Projects-Meilisearch--side-projects-memd"
        );
    }

    #[test]
    fn excludes_names_fragments_and_linked_worktrees() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let c = crawler_for(root);
        std::fs::create_dir_all(root.join("app/node_modules/x")).unwrap();
        std::fs::create_dir_all(root.join("app/.claude/worktrees/wt")).unwrap();
        std::fs::create_dir_all(root.join("app/linked")).unwrap();
        std::fs::write(root.join("app/linked/.git"), "gitdir: ../.git/worktrees/l").unwrap();
        std::fs::create_dir_all(root.join("app/.git")).unwrap();

        assert!(c.dir_excluded(&root.join("app/node_modules")));
        assert!(c.dir_excluded(&root.join("app/.claude/worktrees")));
        assert!(c.dir_excluded(&root.join("app/linked")), "linked worktree");
        assert!(
            !c.dir_excluded(&root.join("app")),
            "main repo is not excluded"
        );
        assert!(c.under_excluded_dir(&root.join("app/node_modules/x/README.md")));
        assert!(c.under_excluded_dir(&root.join("app/linked/CLAUDE.md")));
        assert!(!c.under_excluded_dir(&root.join("app/CLAUDE.md")));
    }

    #[test]
    fn scope_is_nearest_git_repo_or_own_dir() {
        let dir = tempfile::tempdir().unwrap();
        let root = dir.path();
        let c = crawler_for(root);
        std::fs::create_dir_all(root.join("org/repo/.git")).unwrap();
        std::fs::create_dir_all(root.join("org/repo/sub/deep")).unwrap();
        std::fs::create_dir_all(root.join("org/plain")).unwrap();
        let mut cache = ScopeCache::new();
        assert_eq!(
            c.project_scope(root, &root.join("org/repo/sub/deep/README.md"), &mut cache),
            root.join("org/repo").to_string_lossy()
        );
        assert_eq!(
            c.project_scope(root, &root.join("org/plain/README.md"), &mut cache),
            root.join("org/plain").to_string_lossy()
        );
    }

    #[test]
    fn knowledge_scope_resolves_claude_slugs() {
        let dir = tempfile::tempdir().unwrap();
        let c = crawler_for(dir.path());
        let project = PathBuf::from("/Users/q/Projects/foo");
        let slugs = HashMap::from([(claude_slug("/Users/q/Projects/foo"), project.clone())]);
        let f = Path::new("/Users/q/.claude/projects/-Users-q-Projects-foo/memory/x.md");
        assert_eq!(c.knowledge_scope(f, &slugs), "/Users/q/Projects/foo");
        // A worktree session belongs to its repository.
        let wt = Path::new(
            "/Users/q/.claude/projects/-Users-q-Projects-foo--claude-worktrees-wt-1/memory/x.md",
        );
        assert_eq!(c.knowledge_scope(wt, &slugs), "/Users/q/Projects/foo");
        // A sub-directory session belongs to the repository it extends.
        let sub =
            Path::new("/Users/q/.claude/projects/-Users-q-Projects-foo-crates-core/memory/x.md");
        assert_eq!(c.knowledge_scope(sub, &slugs), "/Users/q/Projects/foo");
        // Unknown: reconstructed, never global.
        let unknown = Path::new("/Users/q/.claude/projects/-Users-q-elsewhere/memory/x.md");
        assert_eq!(c.knowledge_scope(unknown, &slugs), "/Users/q/elsewhere");
        assert_eq!(
            c.knowledge_scope(Path::new("/Users/q/.codex/memories/MEMORY.md"), &slugs),
            "global"
        );
    }
}
