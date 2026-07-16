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

/// Spawn the managed Meilisearch as a child process.
///
/// `stdout`/`stderr` are piped; callers must drain them (see
/// [`forward_output`]) or the child blocks once the pipe buffer fills. The
/// caller owns the returned [`Child`] and its lifecycle.
pub async fn spawn(cfg: &Config) -> Result<Child> {
    spawn_with_import(cfg, None).await
}

/// Command-line arguments for the managed Meilisearch. `--log-level WARN`
/// keeps it from logging every HTTP request — at INFO an unhealthy instance
/// once grew the daemon log to 9 GB.
fn build_args(
    cfg: &Config,
    db: &std::path::Path,
    dumps: &std::path::Path,
    snapshots: &std::path::Path,
    import_dump: Option<&std::path::Path>,
) -> Vec<std::ffi::OsString> {
    let addr = format!("{}:{}", cfg.meilisearch.host, cfg.meilisearch.port);
    let mut args: Vec<std::ffi::OsString> = vec![
        "--db-path".into(),
        db.into(),
        "--dump-dir".into(),
        dumps.into(),
        "--snapshot-dir".into(),
        snapshots.into(),
        "--http-addr".into(),
        addr.into(),
        "--master-key".into(),
        cfg.meilisearch.master_key.clone().into(),
        "--no-analytics".into(),
        "--env".into(),
        "production".into(),
        "--log-level".into(),
        "WARN".into(),
    ];
    if let Some(dump) = import_dump {
        args.push("--import-dump".into());
        args.push(dump.into());
    }
    args
}

/// Drain the child's stdout/stderr into our tracing log so Meilisearch output
/// goes through the daemon's size-capped writer instead of an unbounded
/// supervisor redirect. Must be called once per spawned child.
pub fn forward_output(child: &mut Child) {
    use tokio::io::{AsyncBufReadExt, BufReader};
    let streams: [Option<Box<dyn tokio::io::AsyncRead + Send + Unpin>>; 2] = [
        child
            .stdout
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
        child
            .stderr
            .take()
            .map(|s| Box::new(s) as Box<dyn tokio::io::AsyncRead + Send + Unpin>),
    ];
    for stream in streams.into_iter().flatten() {
        tokio::spawn(async move {
            let mut lines = BufReader::new(stream).lines();
            while let Ok(Some(line)) = lines.next_line().await {
                tracing::info!(target: "meilisearch", "{}", strip_ansi(&line));
            }
        });
    }
}

/// Remove ANSI SGR escape sequences (`ESC [ … m`) — Meilisearch colors its
/// log lines even when writing to a pipe, and raw escapes would litter our
/// log file.
fn strip_ansi(s: &str) -> String {
    let mut out = String::with_capacity(s.len());
    let mut chars = s.chars();
    while let Some(c) = chars.next() {
        if c == '\x1b' {
            // Skip "[<params>m"; tolerate a bare ESC by dropping just it.
            if chars.clone().next() == Some('[') {
                for c2 in chars.by_ref() {
                    if c2 == 'm' {
                        break;
                    }
                }
            }
        } else {
            out.push(c);
        }
    }
    out
}

/// Detects a wedged engine: the process is alive but health checks keep
/// failing. Fires once `threshold` consecutive checks have failed; a healthy
/// check resets the streak.
pub struct WedgeDetector {
    threshold: u32,
    consecutive: u32,
}

impl WedgeDetector {
    pub fn new(threshold: u32) -> Self {
        Self {
            threshold,
            consecutive: 0,
        }
    }

    /// Record one health-check result. Returns true while wedged.
    pub fn observe(&mut self, healthy: bool) -> bool {
        if healthy {
            self.consecutive = 0;
        } else {
            self.consecutive = self.consecutive.saturating_add(1);
        }
        self.consecutive >= self.threshold
    }
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

    tracing::info!(
        "starting Meilisearch on {}:{}",
        cfg.meilisearch.host,
        cfg.meilisearch.port
    );

    let child = Command::new(&bin)
        .current_dir(&data)
        .args(build_args(cfg, &db, &dumps, &snapshots, import_dump))
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    #[test]
    fn wedge_detector_fires_after_threshold_consecutive_failures() {
        let mut d = WedgeDetector::new(3);
        assert!(!d.observe(false));
        assert!(!d.observe(false));
        assert!(d.observe(false), "third consecutive failure = wedged");
        assert!(d.observe(false), "stays wedged while failures continue");
    }

    #[test]
    fn wedge_detector_resets_on_healthy() {
        let mut d = WedgeDetector::new(2);
        assert!(!d.observe(false));
        assert!(!d.observe(true), "healthy check resets the streak");
        assert!(!d.observe(false));
        assert!(d.observe(false));
    }

    #[test]
    fn spawn_args_cap_meilisearch_log_level() {
        let cfg = Config::default();
        let args = build_args(
            &cfg,
            Path::new("/db"),
            Path::new("/dumps"),
            Path::new("/snapshots"),
            None,
        );
        let pos = args
            .iter()
            .position(|a| a == "--log-level")
            .expect("--log-level must be passed so Meilisearch does not log every request");
        assert_eq!(args[pos + 1], "WARN");
    }

    #[test]
    fn strip_ansi_removes_color_codes_and_keeps_text() {
        assert_eq!(
            strip_ansi("\x1b[2m2026-07-15T23:48:05Z\x1b[0m \x1b[33m WARN\x1b[0m boom"),
            "2026-07-15T23:48:05Z  WARN boom"
        );
        assert_eq!(strip_ansi("plain text"), "plain text");
    }

    #[test]
    fn spawn_args_include_import_dump_when_given() {
        let cfg = Config::default();
        let args = build_args(
            &cfg,
            Path::new("/db"),
            Path::new("/dumps"),
            Path::new("/snapshots"),
            Some(Path::new("/dumps/x.dump")),
        );
        assert!(args.iter().any(|a| a == "--import-dump"));
    }
}
