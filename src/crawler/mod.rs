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
use crate::knowledge::ident::slug;
use crate::memory::classify;
use crate::memory::service::path_id;
use crate::memory::service::{CrawledDoc, content_hash};
use crate::memory::{Knowledge, MemoryItem, MemoryService, MemoryType};
use crate::paths;
use anyhow::Result;
use globset::{Glob, GlobSet, GlobSetBuilder};
use serde::{Deserialize, Serialize};
use serde_json::{Value, json};
use std::collections::{HashMap, HashSet};
use std::path::{Path, PathBuf};
use std::sync::RwLock;
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
    /// Repository path → project entity id, refreshed by every scan.
    projects: RwLock<HashMap<String, String>>,
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
            projects: RwLock::new(HashMap::new()),
        })
    }

    /// The project entity ids for a scope (empty or one element).
    fn project_ids_for(&self, scope: &str) -> Vec<String> {
        self.projects
            .read()
            .map(|m| m.get(scope).cloned().into_iter().collect())
            .unwrap_or_default()
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

/// "Heading — first paragraph" of a README, at most 400 characters. Badges,
/// HTML, images, tables, quotes and code fences are skipped.
pub fn readme_headline(md: &str) -> String {
    let mut heading: Option<String> = None;
    let mut para: Vec<&str> = Vec::new();
    let mut in_fence = false;
    for line in md.lines() {
        let t = line.trim();
        if t.starts_with("```") {
            in_fence = !in_fence;
            continue;
        }
        if in_fence {
            continue;
        }
        if t.is_empty() {
            if !para.is_empty() {
                break;
            }
            continue;
        }
        if let Some(h) = t.strip_prefix('#') {
            if heading.is_none() && para.is_empty() {
                heading = Some(h.trim_start_matches('#').trim().to_string());
                continue;
            }
            if !para.is_empty() {
                break;
            }
            continue;
        }
        let noise = ["<", "!", "[!", "[![", "|", ">", "---", "==="];
        if noise.iter().any(|n| t.starts_with(n)) {
            continue;
        }
        para.push(t);
    }
    let body = para.join(" ");
    let out = match (heading, body.is_empty()) {
        (Some(h), false) => format!("{h} — {body}"),
        (Some(h), true) => h,
        (None, _) => body,
    };
    out.chars().take(400).collect()
}

/// What existing entities mean for project id assignment:
/// - `taken`: project ids already bound to a directory (kept by that repo).
///   A crawler-owned project whose directory is gone is left out, so a moved
///   repository reclaims its id.
/// - `blocked`: ids a repository may never take — any non-stub entity that is
///   not a project (an agent's company, person, product…).
///
/// Stubs and agent projects without a path are neither: a repository with that
/// id adopts them.
pub fn project_claims(existing: &[Value]) -> (HashMap<String, PathBuf>, HashSet<String>) {
    let mut taken = HashMap::new();
    let mut blocked = HashSet::new();
    for d in existing {
        let s = |k: &str| d.get(k).and_then(|v| v.as_str());
        let Some(id) = s("id") else {
            continue;
        };
        if s("status") == Some("stub") {
            continue;
        }
        if s("kind_of") != Some("project") {
            blocked.insert(id.to_string());
            continue;
        }
        if let Some(path) = s("path") {
            if s("source") == Some("crawler") && !Path::new(path).exists() {
                continue;
            }
            taken.insert(id.to_string(), PathBuf::from(path));
        }
    }
    (taken, blocked)
}

/// Give every repository a project entity id. Ids already claimed (by path)
/// are kept; a new repository takes `slug(basename)`, else
/// `slug(parent-basename)`, else that with `-2`, `-3`, … — skipping ids that
/// are taken or blocked (see [`project_claims`]).
pub fn assign_project_ids(
    repos: &[PathBuf],
    taken: &HashMap<String, PathBuf>,
    blocked: &HashSet<String>,
) -> Vec<(PathBuf, String)> {
    let mut taken = taken.clone();
    let mut out = Vec::new();
    for repo in repos {
        if let Some((id, _)) = taken.iter().find(|(_, p)| *p == repo) {
            out.push((repo.clone(), id.clone()));
            continue;
        }
        let base = repo
            .file_name()
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let parent = repo
            .parent()
            .and_then(|p| p.file_name())
            .map(|n| n.to_string_lossy().to_string())
            .unwrap_or_default();
        let free = |c: &String, taken: &HashMap<String, PathBuf>| {
            !taken.contains_key(c) && !blocked.contains(c)
        };
        let first = format!("entity_{}", slug(&base));
        let second = format!("entity_{}", slug(&format!("{parent}-{base}")));
        let id = if free(&first, &taken) {
            first
        } else if free(&second, &taken) {
            second
        } else {
            (2..)
                .map(|n| format!("{second}-{n}"))
                .find(|c| free(c, &taken))
                .unwrap()
        };
        taken.insert(id.clone(), repo.clone());
        out.push((repo.clone(), id));
    }
    out
}

/// The crawler-owned entity document for a repository.
pub fn project_entity_item(
    id: &str,
    repo: &Path,
    readme: Option<&str>,
    created_at: Option<i64>,
    now: i64,
) -> MemoryItem {
    let name = repo
        .file_name()
        .map(|n| n.to_string_lossy().to_string())
        .unwrap_or_default();
    let path = repo.to_string_lossy().to_string();
    let summary = readme.map(readme_headline).filter(|s| !s.is_empty());
    let content = summary.clone().unwrap_or_default();
    MemoryItem {
        id: id.to_string(),
        content_hash: content_hash(&format!("{id}\n{path}\n{content}")),
        content,
        title: Some(name.clone()),
        summary,
        r#type: "entity".to_string(),
        tags: Vec::new(),
        scope: path.clone(),
        source: "crawler".to_string(),
        source_path: None,
        source_client: None,
        created_at: created_at.unwrap_or(now),
        updated_at: now,
        last_accessed_at: None,
        knowledge: Knowledge {
            name: Some(name),
            name_key: Some(id.trim_start_matches("entity_").to_string()),
            kind_of: Some("project".to_string()),
            path: Some(path),
            ..Default::default()
        },
    }
}

/// The `(repo, id)` pairs the crawler may (re)write. A project entity whose
/// stored source is not `crawler` belongs to an agent and is left alone.
pub fn crawler_writable(
    assigned: &[(PathBuf, String)],
    existing: &[Value],
) -> Vec<(PathBuf, String)> {
    // Stubs are placeholders anyone may fill; everything else an agent wrote
    // is theirs.
    let agent_owned: HashSet<&str> = existing
        .iter()
        .filter(|d| d.get("source").and_then(|s| s.as_str()) != Some("crawler"))
        .filter(|d| d.get("status").and_then(|s| s.as_str()) != Some("stub"))
        .filter_map(|d| d.get("id")?.as_str())
        .collect();
    assigned
        .iter()
        .filter(|(_, id)| !agent_owned.contains(id.as_str()))
        .cloned()
        .collect()
}

/// Path patches for agent-owned project entities a repository adopted: an
/// agent described the project but never said where it lives.
pub fn adopt_patches(assigned: &[(PathBuf, String)], existing: &[Value]) -> Vec<Value> {
    assigned
        .iter()
        .filter_map(|(repo, id)| {
            let d = existing
                .iter()
                .find(|d| d.get("id").and_then(|v| v.as_str()) == Some(id.as_str()))?;
            let s = |k: &str| d.get(k).and_then(|v| v.as_str());
            if s("source") == Some("crawler")
                || s("status") == Some("stub")
                || s("kind_of") != Some("project")
                || s("path").is_some()
            {
                return None;
            }
            Some(json!({ "id": id, "path": repo.to_string_lossy() }))
        })
        .collect()
}

/// Status patches for agent-owned project entities whose directory is gone.
pub fn archive_patches(existing: &[Value]) -> Vec<Value> {
    existing
        .iter()
        .filter(|d| d.get("kind_of").and_then(|s| s.as_str()) == Some("project"))
        .filter(|d| d.get("source").and_then(|s| s.as_str()) != Some("crawler"))
        .filter(|d| d.get("status").and_then(|s| s.as_str()) != Some("archived"))
        .filter_map(|d| {
            let path = d.get("path")?.as_str()?;
            if Path::new(path).exists() {
                return None;
            }
            Some(json!({ "id": d.get("id")?.as_str()?, "status": "archived" }))
        })
        .collect()
}

/// A qualifying file found by the walk, ingested once project ids are known.
struct Candidate {
    path: PathBuf,
    ty: MemoryType,
    scope: String,
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
    let state: HashMap<String, CrawledDoc> = if reset {
        summary.deleted = svc.forget_all_crawled().await?;
        HashMap::new()
    } else {
        svc.crawled_state().await?
    };

    // 1. Walk every root, collecting qualifying files and git repositories.
    let mut candidates: Vec<Candidate> = Vec::new();
    let mut git_roots: Vec<PathBuf> = Vec::new();
    let mut scope_cache = ScopeCache::new();
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
            candidates.push(Candidate {
                path: path.to_path_buf(),
                ty,
                scope,
            });
        }
    }
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
            candidates.push(Candidate {
                path: path.to_path_buf(),
                ty,
                scope,
            });
        }
    }

    // 2. Give every repository a project entity id. Every entity is
    //    consulted, so a repository never takes an id an agent uses for
    //    something else. A failed fetch aborts the scan: guessing here would
    //    overwrite agent-owned entities.
    let existing_projects = svc
        .client()
        .fetch_docs(
            "type = 'entity'",
            &["id", "path", "source", "status", "kind_of"],
        )
        .await?;
    let (taken, blocked) = project_claims(&existing_projects);
    git_roots.sort();
    let assigned = assign_project_ids(&git_roots, &taken, &blocked);
    let by_scope: HashMap<String, String> = assigned
        .iter()
        .map(|(p, id)| (p.to_string_lossy().to_string(), id.clone()))
        .collect();
    if let Ok(mut m) = crawler.projects.write() {
        *m = by_scope.clone();
    }

    // 3. Files.
    let mut seen: HashSet<String> = HashSet::new();
    let mut batch: Vec<MemoryItem> = Vec::new();
    let mut patches: Vec<Value> = Vec::new();
    const BATCH: usize = 200;
    for c in &candidates {
        let entities: Vec<String> = by_scope.get(&c.scope).cloned().into_iter().collect();
        ingest(
            svc,
            c,
            entities,
            &state,
            &mut seen,
            &mut batch,
            &mut patches,
            &mut summary,
        );
        if batch.len() >= BATCH {
            flush(svc, &mut batch, &mut summary).await;
        }
    }

    // 4. Project entities. Agent-owned ones (source ≠ crawler) are never
    //    overwritten; they are not in `state`, so stale deletion skips them too.
    let now = crate::memory::model::now_secs();
    for (repo, id) in &crawler_writable(&assigned, &existing_projects) {
        let readme = candidates
            .iter()
            .find(|c| {
                c.ty == MemoryType::ProjectOverview && c.path.parent() == Some(repo.as_path())
            })
            .and_then(|c| std::fs::read_to_string(&c.path).ok());
        let prev = state.get(id);
        let item =
            project_entity_item(id, repo, readme.as_deref(), prev.map(|p| p.created_at), now);
        seen.insert(id.clone());
        if prev.map(|p| p.hash == item.content_hash).unwrap_or(false) {
            summary.skipped += 1;
            continue;
        }
        batch.push(item);
        summary.indexed += 1;
    }
    flush(svc, &mut batch, &mut summary).await;

    // 5. Link-only updates, adopted and archived projects.
    patches.extend(adopt_patches(&assigned, &existing_projects));
    patches.extend(archive_patches(&existing_projects));
    if let Err(e) = svc.patch(&patches).await {
        tracing::warn!("patching {} crawled docs failed: {e}", patches.len());
        summary.errors += patches.len();
    }

    // 6. Deletions: stored crawler docs no qualifying file produced this pass.
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

/// Read one qualifying file and queue it: a full upsert when its content
/// changed, a link-only patch when only its project changed.
#[allow(clippy::too_many_arguments)]
fn ingest(
    svc: &MemoryService,
    c: &Candidate,
    entities: Vec<String>,
    state: &HashMap<String, CrawledDoc>,
    seen: &mut HashSet<String>,
    batch: &mut Vec<MemoryItem>,
    patches: &mut Vec<Value>,
    summary: &mut CrawlSummary,
) {
    let path_str = c.path.to_string_lossy().to_string();
    let id = path_id(&path_str);
    let content = match std::fs::read_to_string(&c.path) {
        Ok(text) if !text.trim().is_empty() => text,
        _ => {
            summary.skipped += 1; // empty, binary, or unreadable
            return;
        }
    };
    seen.insert(id.clone());
    let prev = state.get(&id);
    let existing = prev.map(|p| (p.hash.as_str(), p.created_at));
    match svc.prepare_crawled(
        &path_str,
        content,
        c.ty,
        c.scope.clone(),
        None,
        existing,
        entities.clone(),
    ) {
        Some(item) => {
            batch.push(item);
            summary.indexed += 1;
        }
        None => {
            if prev.map(|p| p.entities != entities).unwrap_or(false) {
                patches.push(json!({ "id": id, "entities": entities }));
            }
            summary.skipped += 1;
        }
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
    let entities = crawler.project_ids_for(&scope);
    svc.upsert_crawled(&path.to_string_lossy(), content, ty, scope, None, entities)
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

    #[test]
    fn readme_headline_takes_heading_and_first_paragraph() {
        let md = "# memd\n\n[![CI](x)](y)\n<div>\n\nUniversal local memory\nfor every LLM tool.\n\n## Install\n";
        assert_eq!(
            readme_headline(md),
            "memd — Universal local memory for every LLM tool."
        );
        assert_eq!(readme_headline("Just text."), "Just text.");
        assert_eq!(readme_headline("# Only a title\n"), "Only a title");
        assert_eq!(readme_headline(""), "");
        let long = format!("# T\n\n{}", "word ".repeat(200));
        assert!(readme_headline(&long).chars().count() <= 400);
    }

    #[test]
    fn project_ids_resolve_collisions_and_keep_existing_claims() {
        let taken = HashMap::from([(
            "entity_console".to_string(),
            PathBuf::from("/p/cloud/console"),
        )]);
        let repos = vec![
            PathBuf::from("/p/cloud/console"),
            PathBuf::from("/p/demos/console"),
            PathBuf::from("/p/memd"),
            PathBuf::from("/q/demos/console"),
        ];
        let got: HashMap<PathBuf, String> = assign_project_ids(&repos, &taken, &HashSet::new())
            .into_iter()
            .collect();
        assert_eq!(
            got[&PathBuf::from("/p/cloud/console")],
            "entity_console",
            "existing claim kept"
        );
        assert_eq!(
            got[&PathBuf::from("/p/demos/console")],
            "entity_demos-console"
        );
        assert_eq!(
            got[&PathBuf::from("/q/demos/console")],
            "entity_demos-console-2"
        );
        assert_eq!(got[&PathBuf::from("/p/memd")], "entity_memd");
    }

    #[test]
    fn project_entity_items_are_crawler_owned_projects() {
        let it = project_entity_item(
            "entity_memd",
            Path::new("/p/memd"),
            Some("# memd\n\nMemory daemon."),
            Some(5),
            9,
        );
        assert_eq!(it.r#type, "entity");
        assert_eq!(it.source, "crawler");
        assert_eq!(it.scope, "/p/memd");
        assert_eq!(it.created_at, 5);
        assert_eq!(it.knowledge.kind_of.as_deref(), Some("project"));
        assert_eq!(it.knowledge.path.as_deref(), Some("/p/memd"));
        assert_eq!(it.knowledge.name.as_deref(), Some("memd"));
        assert_eq!(it.summary.as_deref(), Some("memd — Memory daemon."));
        let other = project_entity_item("entity_memd", Path::new("/p/memd"), None, None, 9);
        assert_ne!(it.content_hash, other.content_hash);
    }

    #[test]
    fn crawler_never_writes_a_project_an_agent_took_over() {
        let assigned = vec![
            (PathBuf::from("/p/memd"), "entity_memd".to_string()),
            (PathBuf::from("/p/lab"), "entity_lab".to_string()),
            (PathBuf::from("/p/new"), "entity_new".to_string()),
        ];
        let existing = vec![
            serde_json::json!({ "id": "entity_memd", "path": "/p/memd", "source": "mcp" }),
            serde_json::json!({ "id": "entity_lab", "path": "/p/lab", "source": "crawler" }),
        ];
        let ids: Vec<String> = crawler_writable(&assigned, &existing)
            .into_iter()
            .map(|(_, id)| id)
            .collect();
        assert_eq!(ids, vec!["entity_lab", "entity_new"]);
    }

    #[test]
    fn only_agent_owned_projects_with_missing_paths_are_archived() {
        let existing = vec![
            serde_json::json!({ "kind_of": "project", "id": "entity_gone", "path": "/definitely/missing/xyz", "source": "mcp" }),
            serde_json::json!({ "kind_of": "project", "id": "entity_done", "path": "/definitely/missing/xyz", "source": "mcp", "status": "archived" }),
            serde_json::json!({ "kind_of": "project", "id": "entity_crawled", "path": "/definitely/missing/xyz", "source": "crawler" }),
            serde_json::json!({ "kind_of": "project", "id": "entity_here", "path": "/", "source": "cli" }),
        ];
        assert_eq!(
            archive_patches(&existing),
            vec![serde_json::json!({ "id": "entity_gone", "status": "archived" })]
        );
    }

    #[test]
    fn agent_entities_block_their_id_for_repositories() {
        // Review Critical 1: an agent's company must never become a crawled project.
        let existing = vec![
            serde_json::json!({ "id": "entity_console", "kind_of": "company", "source": "mcp" }),
        ];
        let (taken, blocked) = project_claims(&existing);
        let got = assign_project_ids(&[PathBuf::from("/p/demos/console")], &taken, &blocked);
        assert_eq!(got[0].1, "entity_demos-console");
        assert!(
            crawler_writable(&got, &existing)
                .iter()
                .all(|(_, id)| id != "entity_console")
        );
    }

    #[test]
    fn stubs_are_adopted_and_filled_by_the_crawler() {
        let existing = vec![
            serde_json::json!({ "id": "entity_memd", "kind_of": "concept", "source": "mcp", "status": "stub" }),
        ];
        let (taken, blocked) = project_claims(&existing);
        let got = assign_project_ids(&[PathBuf::from("/p/memd")], &taken, &blocked);
        assert_eq!(got[0].1, "entity_memd");
        assert_eq!(crawler_writable(&got, &existing).len(), 1);
    }

    #[test]
    fn agent_projects_without_a_path_are_adopted_not_overwritten() {
        // Review Important 2: the crawler gives an agent's project its path.
        let existing =
            vec![serde_json::json!({ "id": "entity_memd", "kind_of": "project", "source": "mcp" })];
        let (taken, blocked) = project_claims(&existing);
        let got = assign_project_ids(&[PathBuf::from("/p/memd")], &taken, &blocked);
        assert_eq!(got[0].1, "entity_memd");
        assert!(crawler_writable(&got, &existing).is_empty());
        assert_eq!(
            adopt_patches(&got, &existing),
            vec![serde_json::json!({ "id": "entity_memd", "path": "/p/memd" })]
        );
    }

    #[test]
    fn a_moved_repository_reclaims_its_id() {
        // Review Important 5: a crawled project whose old path is gone frees its id.
        let dir = tempfile::tempdir().unwrap();
        let new = dir.path().join("b/foo");
        std::fs::create_dir_all(&new).unwrap();
        let old = dir.path().join("a/foo").to_string_lossy().to_string();
        let existing = vec![
            serde_json::json!({ "id": "entity_foo", "kind_of": "project", "source": "crawler", "path": old }),
        ];
        let (taken, blocked) = project_claims(&existing);
        let got = assign_project_ids(&[new], &taken, &blocked);
        assert_eq!(got[0].1, "entity_foo");
    }
}
