# memd Auto-Update Implementation Plan

> **For agentic workers:** REQUIRED SUB-SKILL: Use superpowers:subagent-driven-development (recommended) or superpowers:executing-plans to implement this plan task-by-task. Steps use checkbox (`- [ ]`) syntax for tracking.

**Goal:** memd checks GitHub daily and automatically updates both itself and its managed Meilisearch engine, with dump-based engine migration and full rollback on failure.

**Architecture:** A background tokio task in the daemon checks GitHub releases daily. Updates funnel through one mechanism: *prepare, then restart the daemon*. A memd self-update swaps the installed binary and restarts; an engine update dumps the live instance, downloads the new binary, writes a migration marker, and restarts — the new daemon applies the marker at startup (backup `data.ms` → boot new engine with `--import-dump` → verify → commit or rollback) before the normal engine spawn. Under launchd, "restart" is `exit(0)` (the plist already has `KeepAlive=true`); elsewhere the daemon re-execs itself.

**Tech Stack:** Rust (existing deps only: reqwest, tokio, serde, anyhow; tempfile in dev-deps). No new dependencies.

**Spec:** `docs/superpowers/specs/2026-06-10-auto-update-design.md`

**One conscious deviation from the spec:** the spec asked for an integration test running a real dump → `--import-dump` cycle. The `paths::*` helpers resolve to the user's real data dir with no injection point, so such a test would touch live user data; refactoring all path lookups for injection is out of scope (YAGNI). Instead the rollback/prune/marker/verify machinery is unit-tested against temp dirs, and Task 11 documents a manual end-to-end verification procedure.

---

## File Structure

| File | Responsibility |
|------|---------------|
| `src/update/mod.rs` | Create: module root — `UpdateState` (persisted JSON), `UpdateLock` (exclusive lock), `run_loop`/`tick` (daemon task) |
| `src/update/check.rs` | Create: GitHub release fetch, semver compare, platform asset name, `decide()` |
| `src/update/self_update.rs` | Create: memd binary download → `--version` verify → atomic swap |
| `src/update/engine.rs` | Create: engine migration — `prepare` (dump + download + marker), `apply_pending` (import + verify + rollback), backup pruning |
| `src/config.rs` | Modify: add `[update]` section (`auto: bool`, default true) |
| `src/paths.rs` | Modify: add `update_state_file`, `engine_migration_file`, `update_lock_file`, `dumps_dir` |
| `src/meili/mod.rs` | Modify: extract `download_binary(version)` from `ensure_binary`; add `spawn_with_import` |
| `src/meili/client.rs` | Modify: add `create_dump()` |
| `src/daemon.rs` | Modify: apply pending migration at startup; spawn updater task; restart channel; `restart_daemon()` |
| `src/main.rs` | Modify: declare `mod update`; add `Update { check }` subcommand |
| `src/cli.rs` | Modify: `cli::update`, update lines in `status`, service-restart helper |
| `docs/cli.mdx`, `docs/configuration.mdx`, `README.md` | Modify: document `memd update` and `[update]` |

---

### Task 1: `[update]` config section

**Files:**
- Modify: `src/config.rs`

- [ ] **Step 1: Write the failing tests**

Append at the end of `src/config.rs`:

```rust
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
```

- [ ] **Step 2: Run tests to verify they fail**

Run: `cargo test config::tests`
Expected: compile error — `no field `update` on type `Config``

- [ ] **Step 3: Implement**

In `src/config.rs`, add to the `Config` struct (after `pub crawler: CrawlerConfig,`):

```rust
    pub update: UpdateConfig,
```

Add after the `CrawlerConfig` struct definition:

```rust
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct UpdateConfig {
    /// Automatically apply memd + engine updates found by the daily check.
    pub auto: bool,
}

impl Default for UpdateConfig {
    fn default() -> Self {
        Self { auto: true }
    }
}
```

- [ ] **Step 4: Run tests to verify they pass**

Run: `cargo test config::tests`
Expected: `2 passed`

- [ ] **Step 5: Commit**

```bash
git add src/config.rs
git commit -m "feat(update): add [update] config section with auto=true default"
```

---

### Task 2: Path helpers

**Files:**
- Modify: `src/paths.rs`
- Modify: `src/meili/mod.rs:112-116` (reuse `dumps_dir`)

- [ ] **Step 1: Add the helpers**

Append to `src/paths.rs`:

```rust
/// Persisted auto-update state (last check, failed engine versions).
pub fn update_state_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("update-state.json"))
}

/// Marker describing a prepared engine migration, applied at daemon startup.
pub fn engine_migration_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("engine-migration.json"))
}

/// Lock taken while an update is being applied (daemon or `memd update`).
pub fn update_lock_file() -> Result<PathBuf> {
    Ok(data_dir()?.join("update.lock"))
}

/// Directory where the managed Meilisearch writes dumps (created on demand).
pub fn dumps_dir() -> Result<PathBuf> {
    let dir = data_dir()?.join("dumps");
    std::fs::create_dir_all(&dir)
        .with_context(|| format!("creating dumps dir {}", dir.display()))?;
    Ok(dir)
}
```

- [ ] **Step 2: Reuse `dumps_dir` in `meili::spawn`**

In `src/meili/mod.rs`, the block creating `dumps` currently reads:

```rust
    let data = paths::data_dir()?;
    let dumps = data.join("dumps");
    let snapshots = data.join("snapshots");
    std::fs::create_dir_all(&dumps)?;
    std::fs::create_dir_all(&snapshots)?;
```

Replace with:

```rust
    let data = paths::data_dir()?;
    let dumps = paths::dumps_dir()?;
    let snapshots = data.join("snapshots");
    std::fs::create_dir_all(&snapshots)?;
```

- [ ] **Step 3: Build**

Run: `cargo build`
Expected: success, no warnings about unused functions yet (they're `pub`).

- [ ] **Step 4: Commit**

```bash
git add src/paths.rs src/meili/mod.rs
git commit -m "feat(update): add update state/marker/lock/dumps path helpers"
```

---

### Task 3: `update` module — `UpdateState` and `UpdateLock`

**Files:**
- Create: `src/update/mod.rs`
- Modify: `src/main.rs` (declare module)

- [ ] **Step 1: Create the module with failing tests**

Create `src/update/mod.rs`:

```rust
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
                    .map(|t| t.elapsed().map(|d| d.as_secs() > 3600).unwrap_or(false))
                    .unwrap_or(true);
                if stale {
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
        let mut s = UpdateState::default();
        s.last_check_secs = 42;
        s.last_result = "ok".into();
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
```

- [ ] **Step 2: Declare the module**

In `src/main.rs`, add to the module list (alphabetical, after `mod paths;`):

```rust
mod update;
```

- [ ] **Step 3: Run tests**

Run: `cargo test update::tests`
Expected: `4 passed`

- [ ] **Step 4: Commit**

```bash
git add src/update/mod.rs src/main.rs
git commit -m "feat(update): UpdateState persistence and exclusive UpdateLock"
```

---

### Task 4: Release checking (`check.rs`)

**Files:**
- Create: `src/update/check.rs`
- Modify: `src/update/mod.rs` (declare submodule)

- [ ] **Step 1: Create `src/update/check.rs` with logic + tests**

```rust
//! Release checking: query GitHub for the latest memd and Meilisearch
//! releases and decide which single update (if any) to apply.

use super::UpdateState;
use anyhow::{Context, Result, bail};
use serde::Deserialize;

/// GitHub repo for memd itself.
pub const MEMD_REPO: &str = "qdequele/memd";
/// GitHub repo for the managed engine.
pub const ENGINE_REPO: &str = "meilisearch/meilisearch";

#[derive(Debug, Deserialize)]
pub struct Release {
    pub tag_name: String,
    #[serde(default)]
    pub draft: bool,
    #[serde(default)]
    pub prerelease: bool,
    #[serde(default)]
    pub assets: Vec<Asset>,
}

#[derive(Debug, Deserialize)]
pub struct Asset {
    pub name: String,
    pub browser_download_url: String,
}

/// Parse `v1.2.3` / `1.2.3` into a comparable triple. Returns `None` for
/// anything else (including `-rc` suffixes — stable releases only).
pub fn parse_semver(v: &str) -> Option<(u64, u64, u64)> {
    let v = v.trim().trim_start_matches('v');
    let mut parts = v.splitn(3, '.');
    let major = parts.next()?.parse().ok()?;
    let minor = parts.next()?.parse().ok()?;
    let patch = parts.next()?.parse().ok()?;
    Some((major, minor, patch))
}

/// True when `candidate` is a strictly newer parseable version than `current`.
pub fn is_newer(candidate: &str, current: &str) -> bool {
    match (parse_semver(candidate), parse_semver(current)) {
        (Some(c), Some(cur)) => c > cur,
        _ => false,
    }
}

/// The memd release asset name for this platform. Must match the names staged
/// by `.github/workflows/release.yml`.
pub fn memd_asset_name() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "memd-aarch64-apple-darwin",
        ("macos", "x86_64") => "memd-x86_64-apple-darwin",
        ("linux", "x86_64") => "memd-x86_64-unknown-linux-gnu",
        ("linux", "aarch64") => "memd-aarch64-unknown-linux-gnu",
        (os, arch) => bail!("unsupported platform for self-update: {os}/{arch}"),
    })
}

/// Fetch the latest release of `repo` from the GitHub API (unauthenticated;
/// GitHub requires a User-Agent).
pub async fn latest_release(repo: &str) -> Result<Release> {
    let url = format!("https://api.github.com/repos/{repo}/releases/latest");
    let resp = reqwest::Client::new()
        .get(&url)
        .header("User-Agent", concat!("memd/", env!("CARGO_PKG_VERSION")))
        .timeout(std::time::Duration::from_secs(30))
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !resp.status().is_success() {
        bail!("GitHub API error {} for {url}", resp.status());
    }
    resp.json::<Release>()
        .await
        .context("parsing GitHub release JSON")
}

/// One concrete update decision.
#[derive(Debug, PartialEq)]
pub enum Plan {
    None,
    /// Replace the memd binary with this release.
    Memd { version: String, asset_url: String },
    /// Migrate the engine to this version (tag, e.g. `v1.46.0`).
    Engine { version: String },
}

/// Decide the next update. memd takes priority over the engine so engine
/// migrations always run on the newest orchestration code; a memd release
/// without an asset for this platform falls through to the engine check.
pub fn decide(
    memd_release: Option<&Release>,
    engine_release: Option<&Release>,
    current_memd: &str,
    current_engine: &str,
    state: &UpdateState,
    asset_name: &str,
) -> Plan {
    if let Some(r) = memd_release
        && !r.draft
        && !r.prerelease
        && is_newer(&r.tag_name, current_memd)
        && let Some(a) = r.assets.iter().find(|a| a.name == asset_name)
    {
        return Plan::Memd {
            version: r.tag_name.clone(),
            asset_url: a.browser_download_url.clone(),
        };
    }
    if let Some(r) = engine_release
        && !r.draft
        && !r.prerelease
        && is_newer(&r.tag_name, current_engine)
        && !state.engine_version_failed(&r.tag_name)
    {
        return Plan::Engine {
            version: r.tag_name.clone(),
        };
    }
    Plan::None
}

#[cfg(test)]
mod tests {
    use super::*;

    fn release(tag: &str, prerelease: bool, assets: Vec<Asset>) -> Release {
        Release {
            tag_name: tag.into(),
            draft: false,
            prerelease,
            assets,
        }
    }

    fn asset(name: &str) -> Asset {
        Asset {
            name: name.into(),
            browser_download_url: format!("https://example.com/{name}"),
        }
    }

    #[test]
    fn semver_parses_with_and_without_v() {
        assert_eq!(parse_semver("v1.2.3"), Some((1, 2, 3)));
        assert_eq!(parse_semver("0.10.0"), Some((0, 10, 0)));
        assert_eq!(parse_semver("v1.46.0-rc.1"), None);
        assert_eq!(parse_semver("nightly"), None);
    }

    #[test]
    fn is_newer_compares_numerically() {
        assert!(is_newer("v0.10.0", "0.9.9"));
        assert!(is_newer("v1.46.0", "v1.45.1"));
        assert!(!is_newer("v1.45.1", "v1.45.1"));
        assert!(!is_newer("v1.44.0", "v1.45.1"));
        assert!(!is_newer("garbage", "v1.45.1"));
    }

    #[test]
    fn release_json_parses() {
        let raw = r#"{
            "tag_name": "v0.2.0",
            "draft": false,
            "prerelease": false,
            "assets": [
                {"name": "memd-aarch64-apple-darwin",
                 "browser_download_url": "https://github.com/qdequele/memd/releases/download/v0.2.0/memd-aarch64-apple-darwin"}
            ]
        }"#;
        let r: Release = serde_json::from_str(raw).unwrap();
        assert_eq!(r.tag_name, "v0.2.0");
        assert_eq!(r.assets.len(), 1);
    }

    #[test]
    fn decide_prefers_memd_over_engine() {
        let memd = release("v0.2.0", false, vec![asset("memd-test")]);
        let engine = release("v1.46.0", false, vec![]);
        let plan = decide(
            Some(&memd),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &UpdateState::default(),
            "memd-test",
        );
        assert!(matches!(plan, Plan::Memd { version, .. } if version == "v0.2.0"));
    }

    #[test]
    fn decide_falls_through_to_engine_when_memd_current_or_asset_missing() {
        let engine = release("v1.46.0", false, vec![]);
        // memd current.
        let memd_current = release("v0.1.0", false, vec![asset("memd-test")]);
        let plan = decide(
            Some(&memd_current),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &UpdateState::default(),
            "memd-test",
        );
        assert!(matches!(&plan, Plan::Engine { version } if version == "v1.46.0"));
        // memd newer but no asset for this platform.
        let memd_no_asset = release("v0.2.0", false, vec![]);
        let plan = decide(
            Some(&memd_no_asset),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &UpdateState::default(),
            "memd-test",
        );
        assert!(matches!(&plan, Plan::Engine { version } if version == "v1.46.0"));
    }

    #[test]
    fn decide_skips_prereleases_and_blacklisted_engines() {
        let memd_pre = release("v0.2.0", true, vec![asset("memd-test")]);
        let engine = release("v1.46.0", false, vec![]);
        let mut state = UpdateState::default();
        state.record_engine_failure("v1.46.0");
        let plan = decide(
            Some(&memd_pre),
            Some(&engine),
            "0.1.0",
            "v1.45.1",
            &state,
            "memd-test",
        );
        assert_eq!(plan, Plan::None);
    }

    #[test]
    fn decide_handles_missing_releases() {
        let plan = decide(None, None, "0.1.0", "v1.45.1", &UpdateState::default(), "x");
        assert_eq!(plan, Plan::None);
    }
}
```

- [ ] **Step 2: Declare the submodule**

In `src/update/mod.rs`, add after the module doc comment:

```rust
pub mod check;
```

- [ ] **Step 3: Run tests**

Run: `cargo test update::check`
Expected: `7 passed`

- [ ] **Step 4: Commit**

```bash
git add src/update/check.rs src/update/mod.rs
git commit -m "feat(update): GitHub release checker with semver compare and decide()"
```

---

### Task 5: Self-updater (`self_update.rs`)

**Files:**
- Create: `src/update/self_update.rs`
- Modify: `src/update/mod.rs` (declare submodule)

- [ ] **Step 1: Create `src/update/self_update.rs`**

```rust
//! Self-update: download the new memd binary next to the installed one,
//! verify it runs and reports the expected version, then atomically swap it
//! into place. The running process keeps its old inode — safe on Unix.

use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::path::Path;
use std::time::Duration;
use tokio::io::AsyncWriteExt;

/// Download `url` and swap it over `installed`. The temp file lives in the
/// same directory so the final rename is atomic (same filesystem).
pub async fn apply(url: &str, expected_version: &str, installed: &Path) -> Result<()> {
    if let Some(parent) = installed.parent() {
        std::fs::create_dir_all(parent)?;
    }
    let tmp = installed.with_extension("new");
    download(url, &tmp).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&tmp)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&tmp, perms)?;
    }
    verify_and_swap(&tmp, installed, expected_version)
}

/// Stream `url` to `dest`.
async fn download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let resp = client
        .get(url)
        .header("User-Agent", concat!("memd/", env!("CARGO_PKG_VERSION")))
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !resp.status().is_success() {
        bail!("download failed ({}) for {url}", resp.status());
    }
    let mut file = tokio::fs::File::create(dest)
        .await
        .with_context(|| format!("creating {}", dest.display()))?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    Ok(())
}

/// Verify `tmp` reports `expected_version`, then rename it over `installed`.
/// On verification failure the temp file is removed and `installed` is
/// untouched.
fn verify_and_swap(tmp: &Path, installed: &Path, expected_version: &str) -> Result<()> {
    if let Err(e) = verify_version(tmp, expected_version) {
        let _ = std::fs::remove_file(tmp);
        return Err(e);
    }
    std::fs::rename(tmp, installed)
        .with_context(|| format!("swapping {} into place", installed.display()))
}

/// Run `<bin> --version` and check the output mentions `expected_version`.
/// Catches corrupt or wrong-architecture downloads before anything is touched.
fn verify_version(bin: &Path, expected_version: &str) -> Result<()> {
    let out = std::process::Command::new(bin)
        .arg("--version")
        .output()
        .with_context(|| format!("running {} --version", bin.display()))?;
    let stdout = String::from_utf8_lossy(&out.stdout);
    let expected = expected_version.trim_start_matches('v');
    if !out.status.success() || !stdout.contains(expected) {
        bail!(
            "downloaded binary failed verification (status {:?}, output {:?}, expected version {expected})",
            out.status,
            stdout.trim()
        );
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;
    use std::path::PathBuf;

    /// A fake executable that prints `memd <version>`.
    fn fake_bin(dir: &Path, name: &str, version: &str) -> PathBuf {
        let p = dir.join(name);
        std::fs::write(&p, format!("#!/bin/sh\necho \"memd {version}\"\n")).unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
        p
    }

    #[test]
    fn verify_accepts_matching_version() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_bin(dir.path(), "memd.new", "0.2.0");
        assert!(verify_version(&bin, "v0.2.0").is_ok());
    }

    #[test]
    fn verify_rejects_wrong_version() {
        let dir = tempfile::tempdir().unwrap();
        let bin = fake_bin(dir.path(), "memd.new", "0.1.0");
        assert!(verify_version(&bin, "v0.2.0").is_err());
    }

    #[test]
    fn swap_replaces_installed_on_success() {
        let dir = tempfile::tempdir().unwrap();
        let installed = fake_bin(dir.path(), "memd", "0.1.0");
        let tmp = fake_bin(dir.path(), "memd.new", "0.2.0");
        verify_and_swap(&tmp, &installed, "v0.2.0").unwrap();
        assert!(!tmp.exists());
        let swapped = std::fs::read_to_string(&installed).unwrap();
        assert!(swapped.contains("0.2.0"));
    }

    #[test]
    fn failed_verify_leaves_installed_untouched_and_removes_tmp() {
        let dir = tempfile::tempdir().unwrap();
        let installed = fake_bin(dir.path(), "memd", "0.1.0");
        let tmp = fake_bin(dir.path(), "memd.new", "0.1.0"); // wrong version
        assert!(verify_and_swap(&tmp, &installed, "v0.2.0").is_err());
        assert!(!tmp.exists());
        let kept = std::fs::read_to_string(&installed).unwrap();
        assert!(kept.contains("0.1.0"));
    }
}
```

- [ ] **Step 2: Declare the submodule**

In `src/update/mod.rs`, after `pub mod check;`:

```rust
pub mod self_update;
```

- [ ] **Step 3: Run tests**

Run: `cargo test update::self_update`
Expected: `4 passed`

- [ ] **Step 4: Commit**

```bash
git add src/update/self_update.rs src/update/mod.rs
git commit -m "feat(update): self-updater with --version sanity check and atomic swap"
```

---

### Task 6: Meilisearch plumbing — `download_binary`, `spawn_with_import`, `create_dump`

**Files:**
- Modify: `src/meili/mod.rs`
- Modify: `src/meili/client.rs`

- [ ] **Step 1: Extract `download_binary(version)` from `ensure_binary`**

In `src/meili/mod.rs`, replace the whole `ensure_binary` function with:

```rust
/// Download the pinned Meilisearch binary if it is not already present.
pub async fn ensure_binary(cfg: &Config) -> Result<PathBuf> {
    download_binary(&cfg.meilisearch.version).await
}

/// Download the Meilisearch binary for `version` (a git tag like `v1.45.1`)
/// into memd's bin dir, if not already present. Binaries are stored per
/// version, so older ones remain available for rollback.
pub async fn download_binary(version: &str) -> Result<PathBuf> {
    let dest = binary_path(version)?;
    if dest.exists() {
        return Ok(dest);
    }

    let asset = asset_name()?;
    let url =
        format!("https://github.com/meilisearch/meilisearch/releases/download/{version}/{asset}");
    tracing::info!("downloading Meilisearch {version} from {url}");

    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .build()?;
    let resp = client
        .get(&url)
        .send()
        .await
        .with_context(|| format!("requesting {url}"))?;
    if !resp.status().is_success() {
        bail!("download failed ({}) for {url}", resp.status());
    }

    // Stream to a temp file, then atomically rename.
    let tmp = dest.with_extension("part");
    let mut file = tokio::fs::File::create(&tmp)
        .await
        .with_context(|| format!("creating {}", tmp.display()))?;
    let mut stream = resp.bytes_stream();
    while let Some(chunk) = stream.next().await {
        let chunk = chunk.context("reading download stream")?;
        file.write_all(&chunk).await?;
    }
    file.flush().await?;
    drop(file);
    tokio::fs::rename(&tmp, &dest).await?;

    // chmod +x.
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(&dest)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&dest, perms)?;
    }

    tracing::info!("Meilisearch binary ready at {}", dest.display());
    Ok(dest)
}
```

- [ ] **Step 2: Add `spawn_with_import`**

In `src/meili/mod.rs`, replace the `spawn` function with:

```rust
/// Spawn the managed Meilisearch as a child process.
///
/// `stdout`/`stderr` are inherited so the daemon's log redirection captures
/// them. The caller owns the returned [`Child`] and its lifecycle.
pub async fn spawn(cfg: &Config) -> Result<Child> {
    spawn_with_import(cfg, None).await
}

/// Like [`spawn`], but optionally boot with `--import-dump` (used by engine
/// migration; requires an empty database directory).
pub async fn spawn_with_import(
    cfg: &Config,
    import_dump: Option<&std::path::Path>,
) -> Result<Child> {
    let bin = ensure_binary(cfg).await?;
    let db = paths::meili_db_dir()?;
    std::fs::create_dir_all(&db)?;

    // Meilisearch creates its dump and snapshot directories relative to the
    // current working directory by default. Under launchd the cwd is `/`
    // (read-only on macOS → EROFS), so pin them to absolute paths inside our
    // data dir. This keeps the daemon working no matter how it is launched.
    let data = paths::data_dir()?;
    let dumps = paths::dumps_dir()?;
    let snapshots = data.join("snapshots");
    std::fs::create_dir_all(&snapshots)?;

    let addr = format!("{}:{}", cfg.meilisearch.host, cfg.meilisearch.port);
    tracing::info!("starting Meilisearch on {addr}");

    let mut cmd = Command::new(&bin);
    cmd.current_dir(&data)
        .arg("--db-path")
        .arg(&db)
        .arg("--dump-dir")
        .arg(&dumps)
        .arg("--snapshot-dir")
        .arg(&snapshots)
        .arg("--http-addr")
        .arg(&addr)
        .arg("--master-key")
        .arg(&cfg.meilisearch.master_key)
        .arg("--no-analytics")
        .arg("--env")
        .arg("production");
    if let Some(dump) = import_dump {
        cmd.arg("--import-dump").arg(dump);
    }
    let child = cmd
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;
    Ok(child)
}
```

(This supersedes the Task 2 edit to the dumps block — the new function body already uses `paths::dumps_dir()`.)

- [ ] **Step 3: Add `create_dump` to the client**

In `src/meili/client.rs`, add after the `stats` method:

```rust
    /// Trigger a dump of the whole instance and wait for it to finish.
    /// Returns the dump uid; the file is `<uid>.dump` in the dump dir.
    pub async fn create_dump(&self) -> Result<String> {
        let resp = self
            .http
            .post(self.url("/dumps"))
            .bearer_auth(&self.key)
            .send()
            .await?;
        let v = Self::json_or_err(resp).await.context("creating dump")?;
        let uid = v
            .get("taskUid")
            .and_then(|t| t.as_u64())
            .context("dump response had no taskUid")?;
        let task = self.wait_task(uid).await?;
        task.get("details")
            .and_then(|d| d.get("dumpUid"))
            .and_then(|s| s.as_str())
            .map(String::from)
            .context("dump task did not report a dumpUid")
    }
```

- [ ] **Step 4: Build and run all tests**

Run: `cargo build && cargo test`
Expected: builds cleanly; all existing tests still pass.

- [ ] **Step 5: Commit**

```bash
git add src/meili/mod.rs src/meili/client.rs
git commit -m "feat(update): version-addressed engine download, --import-dump spawn, create_dump"
```

---

### Task 7: Engine migration (`engine.rs`)

**Files:**
- Create: `src/update/engine.rs`
- Modify: `src/update/mod.rs` (declare submodule)

- [ ] **Step 1: Create `src/update/engine.rs`**

```rust
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

/// Marker file describing a prepared migration (`engine-migration.json`).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Migration {
    pub from: String,
    pub to: String,
    pub dump_path: PathBuf,
    /// Documents in the memories index when the dump was taken.
    pub expected_docs: u64,
}

impl Migration {
    pub fn load_from(path: &Path) -> Option<Migration> {
        std::fs::read_to_string(path)
            .ok()
            .and_then(|raw| serde_json::from_str(&raw).ok())
    }

    pub fn save_to(&self, path: &Path) -> Result<()> {
        let raw = serde_json::to_string_pretty(self)?;
        std::fs::write(path, raw).with_context(|| format!("writing {}", path.display()))
    }
}

/// While the daemon is up: dump the live instance, fetch the new binary, and
/// leave a marker for the next startup. Does not touch the running engine.
pub async fn prepare(cfg: &Config, client: &MeiliClient, to_version: &str) -> Result<()> {
    let dump_uid = client.create_dump().await.context("dumping live instance")?;
    let dump_path = paths::dumps_dir()?.join(format!("{dump_uid}.dump"));
    if !dump_path.exists() {
        bail!("dump file {} not found after dump task", dump_path.display());
    }
    let expected_docs = client
        .stats()
        .await
        .ok()
        .and_then(|s| s.get("numberOfDocuments").and_then(|n| n.as_u64()))
        .unwrap_or(0);
    meili::download_binary(to_version)
        .await
        .context("downloading new engine binary")?;
    let m = Migration {
        from: cfg.meilisearch.version.clone(),
        to: to_version.to_string(),
        dump_path,
        expected_docs,
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
}

/// Apply a pending migration, if any. Called at daemon startup BEFORE the
/// normal engine spawn. Migration failure is not an `Err`: the daemon must
/// always come up, on the old version if need be.
pub async fn apply_pending(cfg: &mut Config) -> Result<Applied> {
    let marker_path = paths::engine_migration_file()?;
    let Some(m) = Migration::load_from(&marker_path) else {
        return Ok(Applied::None);
    };
    // Single-shot: remove the marker first so a crash mid-migration cannot
    // loop the daemon into repeated migration attempts.
    let _ = std::fs::remove_file(&marker_path);

    tracing::info!("applying engine migration {} -> {}", m.from, m.to);
    let db = paths::meili_db_dir()?;
    let backup = db.with_file_name(format!("meili-data.bak.{}", now_secs()));
    if db.exists() {
        std::fs::rename(&db, &backup)
            .with_context(|| format!("backing up {}", db.display()))?;
    }

    match import_and_verify(cfg, &m).await {
        Ok(()) => {
            cfg.meilisearch.version = m.to.clone();
            cfg.save()?;
            prune_backups(&db)?;
            tracing::info!("engine migrated to {}", m.to);
            Ok(Applied::Migrated { to: m.to })
        }
        Err(e) => {
            let error = format!("{e:#}");
            tracing::error!("engine migration to {} failed: {error}", m.to);
            restore_backup(&db, &backup)?;
            let state_path = paths::update_state_file()?;
            let mut state = UpdateState::load_from(&state_path);
            state.record_engine_failure(&m.to);
            state.last_result = format!("engine {} migration failed: {error}", m.to);
            let _ = state.save_to(&state_path);
            Ok(Applied::RolledBack { to: m.to, error })
        }
    }
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
        let docs = client
            .stats()
            .await
            .ok()
            .and_then(|s| s.get("numberOfDocuments").and_then(|n| n.as_u64()))
            .unwrap_or(0);
        if docs < m.expected_docs {
            bail!("imported {docs} documents, expected at least {}", m.expected_docs);
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
        std::fs::rename(backup, db)
            .with_context(|| format!("restoring {}", backup.display()))?;
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
        if old.is_dir() {
            let _ = std::fs::remove_dir_all(old);
        } else {
            let _ = std::fs::remove_file(old);
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
        };
        m.save_to(&path).unwrap();
        let loaded = Migration::load_from(&path).unwrap();
        assert_eq!(loaded.to, "v1.46.0");
        assert_eq!(loaded.expected_docs, 123);
        assert!(Migration::load_from(&dir.path().join("missing.json")).is_none());
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
}
```

- [ ] **Step 2: Declare the submodule**

In `src/update/mod.rs`, after `pub mod check;`:

```rust
pub mod engine;
```

- [ ] **Step 3: Run tests**

Run: `cargo test update::engine`
Expected: `4 passed`

- [ ] **Step 4: Commit**

```bash
git add src/update/engine.rs src/update/mod.rs
git commit -m "feat(update): engine migration prepare/apply with rollback and backup pruning"
```

---

### Task 8: Updater loop (`run_loop` / `tick`)

**Files:**
- Modify: `src/update/mod.rs`

- [ ] **Step 1: Add the loop**

Append to `src/update/mod.rs` (before the `#[cfg(test)]` block):

```rust
pub const CHECK_INTERVAL_SECS: i64 = 24 * 3600;

/// Daemon background task: daily check, applying at most one update per
/// check. When an update was applied/prepared, sends a restart request and
/// returns — the daemon is about to go down.
pub async fn run_loop(cfg: crate::config::Config, restart_tx: tokio::sync::mpsc::Sender<&'static str>) {
    if !cfg.update.auto {
        tracing::info!("auto-update disabled in config ([update] auto = false)");
        return;
    }
    // Jittered initial delay: let the engine settle and de-synchronize checks
    // across machines (pid as a cheap entropy source — no rand dependency).
    let initial = std::time::Duration::from_secs(120 + (std::process::id() as u64 % 180));
    tokio::time::sleep(initial).await;

    loop {
        let Ok(state_path) = paths::update_state_file() else {
            return;
        };
        let mut state = UpdateState::load_from(&state_path);
        let now = crate::memory::model::now_secs();
        if now - state.last_check_secs >= CHECK_INTERVAL_SECS {
            match tick(&cfg, &mut state).await {
                Ok(Some(reason)) => {
                    // Deliberately NOT bumping last_check_secs: the restarted
                    // daemon re-checks promptly and picks up any further
                    // update (e.g. engine after a memd self-update).
                    let _ = state.save_to(&state_path);
                    let _ = restart_tx.send(reason).await;
                    return;
                }
                Ok(None) => {
                    state.last_check_secs = now;
                    let _ = state.save_to(&state_path);
                }
                Err(e) => {
                    tracing::warn!("update check failed: {e:#}");
                    state.last_check_secs = now;
                    state.last_result = format!("check failed: {e:#}");
                    let _ = state.save_to(&state_path);
                }
            }
        }
        tokio::time::sleep(std::time::Duration::from_secs(3600)).await;
    }
}

/// One check: decide and apply at most one update. Returns `Some(reason)`
/// when the daemon must restart to finish the job.
async fn tick(cfg: &crate::config::Config, state: &mut UpdateState) -> Result<Option<&'static str>> {
    let _lock = UpdateLock::acquire()?;
    let memd_rel = check::latest_release(check::MEMD_REPO).await.ok();
    let engine_rel = check::latest_release(check::ENGINE_REPO).await.ok();
    let asset = check::memd_asset_name()?;
    let plan = check::decide(
        memd_rel.as_ref(),
        engine_rel.as_ref(),
        env!("CARGO_PKG_VERSION"),
        &cfg.meilisearch.version,
        state,
        asset,
    );
    match plan {
        check::Plan::None => {
            state.last_result = "up to date".into();
            Ok(None)
        }
        check::Plan::Memd { version, asset_url } => {
            tracing::info!("memd update available: {version}");
            let installed = paths::installed_bin()?;
            self_update::apply(&asset_url, &version, &installed).await?;
            state.last_result = format!("memd updated to {version}");
            Ok(Some("memd self-update"))
        }
        check::Plan::Engine { version } => {
            tracing::info!("engine update available: {version}");
            let client = crate::meili::MeiliClient::new(
                cfg.meili_url(),
                cfg.meilisearch.master_key.clone(),
            );
            engine::prepare(cfg, &client, &version).await?;
            state.last_result = format!("engine migration to {version} prepared");
            Ok(Some("engine migration"))
        }
    }
}
```

- [ ] **Step 2: Build and test**

Run: `cargo build && cargo test update::`
Expected: builds cleanly; all `update::` tests pass.

- [ ] **Step 3: Commit**

```bash
git add src/update/mod.rs
git commit -m "feat(update): daily check loop applying at most one update per tick"
```

---

### Task 9: Daemon integration — startup migration, updater task, restart

**Files:**
- Modify: `src/daemon.rs`

- [ ] **Step 1: Rewrite `serve()` and `shutdown_signal`**

In `src/daemon.rs`, update the imports:

```rust
use crate::config::Config;
use crate::memory::MemoryService;
use crate::{crawler, mcp, meili, paths, update};
use anyhow::{Context, Result};
use std::time::Duration;
```

Replace `serve()` with:

```rust
/// Run the daemon until terminated. Blocks for the lifetime of the service.
pub async fn serve() -> Result<()> {
    let _guard = init_logging()?;
    let mut cfg = Config::load_or_init()?;

    write_pid()?;
    tracing::info!("memd daemon starting (pid {})", std::process::id());

    // 0. Apply a prepared engine migration before the engine starts. Failure
    //    rolls back and the daemon continues on the previous version.
    match update::engine::apply_pending(&mut cfg).await? {
        update::engine::Applied::Migrated { to } => {
            tracing::info!("engine updated to {to}");
        }
        update::engine::Applied::RolledBack { to, error } => {
            tracing::warn!("engine update to {to} rolled back: {error}");
        }
        update::engine::Applied::None => {}
    }

    // 1. Start the managed Meilisearch child process.
    let mut child = meili::spawn(&cfg).await?;
    let svc = MemoryService::from_config(&cfg);

    // 2. Wait for health, then ensure the index + local embedder.
    meili::wait_healthy(svc.client(), Duration::from_secs(60)).await?;
    svc.client()
        .ensure_index(&cfg.embedder.source, &cfg.embedder.model)
        .await
        .context("ensuring memories index")?;
    tracing::info!("Meilisearch ready; index configured");

    // 3. Crawler + watcher in the background.
    let crawl_cfg = cfg.clone();
    let crawl_svc = svc.clone();
    let crawler_task = tokio::spawn(async move {
        if let Err(e) = crawler::watch(crawl_cfg, crawl_svc).await {
            tracing::error!("crawler stopped: {e}");
        }
    });

    // 4. HTTP MCP endpoint.
    let addr = format!("{}:{}", cfg.mcp.host, cfg.mcp.port);
    let listener = tokio::net::TcpListener::bind(&addr)
        .await
        .with_context(|| format!("binding MCP endpoint on {addr}"))?;
    tracing::info!("MCP HTTP endpoint listening on http://{addr}/mcp");
    let app = mcp::router(svc.clone());

    let server = tokio::spawn(async move {
        if let Err(e) = axum::serve(listener, app).await {
            tracing::error!("MCP server error: {e}");
        }
    });

    // 5. Daily auto-update check; a prepared update requests a restart.
    let (restart_tx, mut restart_rx) = tokio::sync::mpsc::channel::<&'static str>(1);
    let upd_cfg = cfg.clone();
    let updater = tokio::spawn(async move {
        update::run_loop(upd_cfg, restart_tx).await;
    });

    // 6. Wait for a shutdown signal, child exit, or a restart request.
    let restart = shutdown_signal(&mut child, &mut restart_rx).await;

    tracing::info!("shutting down");
    crawler_task.abort();
    server.abort();
    updater.abort();
    let _ = child.kill().await;
    let _ = std::fs::remove_file(paths::pid_file()?);

    if restart {
        restart_daemon();
    }
    Ok(())
}

/// Exit so the supervisor relaunches us (launchd `KeepAlive`), or re-exec on
/// platforms without one. Never returns.
fn restart_daemon() -> ! {
    if crate::launchd::is_installed() {
        tracing::info!("exiting for restart; launchd will relaunch");
        std::process::exit(0);
    }
    #[cfg(unix)]
    {
        use std::os::unix::process::CommandExt;
        let exe = crate::cli::resolve_memd_exe();
        tracing::info!("re-exec {} serve", exe.display());
        let err = std::process::Command::new(exe).arg("serve").exec();
        tracing::error!("re-exec failed: {err}");
        std::process::exit(1);
    }
    #[cfg(not(unix))]
    std::process::exit(0);
}
```

Replace `shutdown_signal` with:

```rust
/// Block until SIGINT/SIGTERM, the Meilisearch child exiting, or a restart
/// request from the updater. Returns true when the daemon should restart
/// itself after cleanup.
async fn shutdown_signal(
    child: &mut tokio::process::Child,
    restart_rx: &mut tokio::sync::mpsc::Receiver<&'static str>,
) -> bool {
    #[cfg(unix)]
    {
        use tokio::signal::unix::{SignalKind, signal};
        let mut term = signal(SignalKind::terminate()).expect("SIGTERM handler");
        let mut int = signal(SignalKind::interrupt()).expect("SIGINT handler");
        tokio::select! {
            _ = term.recv() => { tracing::info!("received SIGTERM"); false }
            _ = int.recv() => { tracing::info!("received SIGINT"); false }
            status = child.wait() => { tracing::error!("Meilisearch exited: {status:?}"); false }
            reason = restart_rx.recv() => {
                tracing::info!("restart requested: {}", reason.unwrap_or("unknown"));
                true
            }
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            _ = tokio::signal::ctrl_c() => { tracing::info!("received ctrl-c"); false }
            status = child.wait() => { tracing::error!("Meilisearch exited: {status:?}"); false }
            reason = restart_rx.recv() => {
                tracing::info!("restart requested: {}", reason.unwrap_or("unknown"));
                true
            }
        }
    }
}
```

- [ ] **Step 2: Build and test**

Run: `cargo build && cargo test`
Expected: builds cleanly; all tests pass.

- [ ] **Step 3: Smoke-test the daemon in the foreground**

Run: `cargo run -- up --foreground` then Ctrl-C after "Meilisearch ready" appears in `memd logs` (or the terminal).
Expected: daemon starts and shuts down cleanly; no migration log lines (no marker present).

- [ ] **Step 4: Commit**

```bash
git add src/daemon.rs
git commit -m "feat(update): daemon applies pending migrations and runs the daily updater"
```

---

### Task 10: CLI — `memd update [--check]` and status lines

**Files:**
- Modify: `src/main.rs`
- Modify: `src/cli.rs`

- [ ] **Step 1: Add the subcommand**

In `src/main.rs`, add to `enum Command` (after the `Doctor` variant):

```rust
    /// Check GitHub for updates to memd and the managed engine; apply them.
    Update {
        /// Only report what would be updated.
        #[arg(long)]
        check: bool,
    },
}
```

Add to the `match cli.command` (after the `Doctor` arm):

```rust
        Command::Update { check } => cli::update(check).await,
```

- [ ] **Step 2: Implement `cli::update`**

Append to `src/cli.rs`:

```rust
/// Check for updates; apply them unless `check_only`. Manual counterpart of
/// the daemon's daily check — takes the same lock so the two never race.
pub async fn update(check_only: bool) -> Result<()> {
    let cfg = Config::load_or_init()?;
    let state_path = paths::update_state_file()?;
    let mut state = crate::update::UpdateState::load_from(&state_path);

    println!(
        "memd {} (engine pinned {}) — checking GitHub…",
        env!("CARGO_PKG_VERSION"),
        cfg.meilisearch.version
    );
    let memd_rel = crate::update::check::latest_release(crate::update::check::MEMD_REPO)
        .await
        .ok();
    let engine_rel = crate::update::check::latest_release(crate::update::check::ENGINE_REPO)
        .await
        .ok();
    let asset = crate::update::check::memd_asset_name()?;
    let plan = crate::update::check::decide(
        memd_rel.as_ref(),
        engine_rel.as_ref(),
        env!("CARGO_PKG_VERSION"),
        &cfg.meilisearch.version,
        &state,
        asset,
    );

    match plan {
        crate::update::check::Plan::None => {
            println!("Everything is up to date.");
            return Ok(());
        }
        crate::update::check::Plan::Memd { version, asset_url } => {
            if check_only {
                println!("memd {version} is available (run `memd update` to apply).");
                return Ok(());
            }
            let _lock = crate::update::UpdateLock::acquire()?;
            let installed = paths::installed_bin()?;
            crate::update::self_update::apply(&asset_url, &version, &installed).await?;
            println!("memd updated to {version} at {}", installed.display());
            state.last_result = format!("memd updated to {version}");
            state.save_to(&state_path)?;
            restart_service().await?;
            println!("Run `memd update` again to check for engine updates.");
        }
        crate::update::check::Plan::Engine { version } => {
            if check_only {
                println!(
                    "Meilisearch engine {version} is available (pinned {}).",
                    cfg.meilisearch.version
                );
                return Ok(());
            }
            let svc = MemoryService::from_config(&cfg);
            if !svc.client().is_healthy().await {
                bail!("the daemon must be running to migrate the engine — run `memd up` first");
            }
            let _lock = crate::update::UpdateLock::acquire()?;
            crate::update::engine::prepare(&cfg, svc.client(), &version).await?;
            state.last_result = format!("engine migration to {version} prepared");
            state.save_to(&state_path)?;
            println!("Engine migration to {version} prepared; restarting the daemon to apply…");
            restart_service().await?;
            println!("Check `memd status` — the import may take a moment on large stores.");
        }
    }
    Ok(())
}

/// Restart the daemon so a swapped binary / prepared migration takes effect.
async fn restart_service() -> Result<()> {
    if launchd::is_installed() {
        launchd::unload()?;
        tokio::time::sleep(Duration::from_millis(500)).await;
        launchd::load()?;
        println!("Service restarted.");
    } else {
        println!("No launchd service installed — restart the daemon manually (`memd down && memd up`).");
    }
    Ok(())
}
```

- [ ] **Step 3: Add update lines to `status`**

In `src/cli.rs::status`, after the `println!("Service: …")` block and before the `match crawler::last_summary()` block, insert:

```rust
    let upd = crate::update::UpdateState::load();
    if upd.last_check_secs > 0 {
        println!(
            "Updates:      last check {} — {}",
            format_ts(upd.last_check_secs),
            upd.last_result
        );
    } else {
        println!("Updates:      never checked (daily check runs in the daemon)");
    }
```

And add this helper near `health_label`:

```rust
/// Format unix seconds as RFC3339 for human output.
fn format_ts(secs: i64) -> String {
    time::OffsetDateTime::from_unix_timestamp(secs)
        .ok()
        .and_then(|t| {
            t.format(&time::format_description::well_known::Rfc3339)
                .ok()
        })
        .unwrap_or_else(|| secs.to_string())
}
```

- [ ] **Step 4: Build, test, try it**

Run: `cargo build && cargo test`
Expected: clean build, all tests pass.

Run: `cargo run -- update --check`
Expected: prints current versions and either "up to date" or an available version (hits the real GitHub API).

Run: `cargo run -- status`
Expected: includes an `Updates:` line.

- [ ] **Step 5: Commit**

```bash
git add src/main.rs src/cli.rs
git commit -m "feat(update): memd update [--check] command and status reporting"
```

---

### Task 11: Docs, final verification, manual test procedure

**Files:**
- Modify: `docs/cli.mdx`, `docs/configuration.mdx`, `README.md`

- [ ] **Step 1: Document the CLI command**

In `docs/cli.mdx`, add to the daemon-lifecycle table (or a new "Updates" section if the table doesn't fit):

```markdown
## Updates

| Command | Description |
|---------|-------------|
| `memd update` | Check GitHub and apply any memd / engine update now. |
| `memd update --check` | Report available updates without applying them. |

The daemon also checks once a day on its own and applies updates
automatically (disable with `[update] auto = false` in the config). Engine
upgrades use Meilisearch's dump → import flow and roll back automatically if
the migration fails — memories are never lost.
```

- [ ] **Step 2: Document the config section**

In `docs/configuration.mdx`, add alongside the other sections:

```markdown
## `[update]`

| Key | Default | Description |
|-----|---------|-------------|
| `auto` | `true` | Apply memd and engine updates found by the daemon's daily GitHub check. Set to `false` to update only via `memd update`. |
```

- [ ] **Step 3: README mention**

In `README.md`, add a bullet to the feature list (match the existing style):

```markdown
- **Self-updating** — a daily check keeps memd and its Meilisearch engine current; engine upgrades migrate via dump/import with automatic rollback.
```

- [ ] **Step 4: Full verification**

Run: `cargo fmt && cargo clippy --all-targets -- -D warnings && cargo test && cargo build --release`
Expected: no formatting diffs, no clippy warnings, all tests pass, release build succeeds.

- [ ] **Step 5: Manual end-to-end check (one-time, on the dev machine)**

1. `cargo run -- update --check` — confirms the GitHub checker works against real releases.
2. Self-update path: temporarily set `version = "0.0.1"` in `Cargo.toml`, `cargo build`, then `./target/debug/memd update` — it should download the latest published release, verify it, swap it into `~/.local/bin/memd`, and restart the service. Confirm with `~/.local/bin/memd --version`. Revert `Cargo.toml`.
3. Engine path: in `~/.config/memd/config.toml`, set `meilisearch.version` to the previous Meilisearch release (e.g. one minor below current latest), restart (`memd down && memd up`), then `memd update` — it should dump, prepare, restart, and `memd status` should show the new engine version with the same memory count as before.

- [ ] **Step 6: Commit**

```bash
git add docs/cli.mdx docs/configuration.mdx README.md
git commit -m "docs: document auto-update behavior, memd update command, [update] config"
```
