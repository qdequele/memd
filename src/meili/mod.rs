//! Meilisearch manager: download/pin a binary, run it as a child process on a
//! dedicated localhost port, and health-check it. No Docker (PRD decision #2).

pub mod client;

pub use client::MeiliClient;

use crate::config::Config;
use crate::paths;
use anyhow::{Context, Result, bail};
use futures_util::StreamExt;
use std::path::PathBuf;
use std::time::Duration;
use tokio::io::AsyncWriteExt;
use tokio::process::{Child, Command};

/// Resolve the release asset name for the current platform.
fn asset_name() -> Result<&'static str> {
    Ok(match (std::env::consts::OS, std::env::consts::ARCH) {
        ("macos", "aarch64") => "meilisearch-macos-apple-silicon",
        ("macos", "x86_64") => "meilisearch-macos-amd64",
        ("linux", "x86_64") => "meilisearch-linux-amd64",
        ("linux", "aarch64") => "meilisearch-linux-aarch64",
        (os, arch) => bail!("unsupported platform for managed Meilisearch: {os}/{arch}"),
    })
}

/// Path to the pinned binary for `version` inside memd's bin dir.
pub fn binary_path(version: &str) -> Result<PathBuf> {
    Ok(paths::bin_dir()?.join(format!("meilisearch-{version}")))
}

/// Read the version recorded in the Meilisearch database's `VERSION` file
/// (e.g. `1.45.1`), if a database exists. Used to detect engine/db mismatches.
pub fn db_version() -> Result<Option<String>> {
    let vfile = paths::meili_db_dir()?.join("VERSION");
    match std::fs::read_to_string(&vfile) {
        Ok(s) => {
            // The file may store "major.minor.patch" or comma-separated parts.
            let v = s.trim().replace(',', ".");
            Ok(if v.is_empty() { None } else { Some(v) })
        }
        Err(_) => Ok(None),
    }
}

/// Download the pinned Meilisearch binary if it is not already present.
pub async fn ensure_binary(cfg: &Config) -> Result<PathBuf> {
    let version = &cfg.meilisearch.version;
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

/// Spawn the managed Meilisearch as a child process.
///
/// `stdout`/`stderr` are inherited so the daemon's log redirection captures
/// them. The caller owns the returned [`Child`] and its lifecycle.
pub async fn spawn(cfg: &Config) -> Result<Child> {
    let bin = ensure_binary(cfg).await?;
    let db = paths::meili_db_dir()?;
    std::fs::create_dir_all(&db)?;

    // Meilisearch creates its dump and snapshot directories relative to the
    // current working directory by default. Under launchd the cwd is `/`
    // (read-only on macOS → EROFS), so pin them to absolute paths inside our
    // data dir. This keeps the daemon working no matter how it is launched.
    let data = paths::data_dir()?;
    let dumps = data.join("dumps");
    let snapshots = data.join("snapshots");
    std::fs::create_dir_all(&dumps)?;
    std::fs::create_dir_all(&snapshots)?;

    let addr = format!("{}:{}", cfg.meilisearch.host, cfg.meilisearch.port);
    tracing::info!("starting Meilisearch on {addr}");

    let child = Command::new(&bin)
        .current_dir(&data)
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
        .arg("production")
        .kill_on_drop(true)
        .spawn()
        .with_context(|| format!("spawning {}", bin.display()))?;
    Ok(child)
}

/// Wait until the instance reports healthy, up to `timeout`.
pub async fn wait_healthy(client: &MeiliClient, timeout: Duration) -> Result<()> {
    let deadline = tokio::time::Instant::now() + timeout;
    while tokio::time::Instant::now() < deadline {
        if client.is_healthy().await {
            return Ok(());
        }
        tokio::time::sleep(Duration::from_millis(250)).await;
    }
    bail!("Meilisearch did not become healthy within {:?}", timeout)
}
