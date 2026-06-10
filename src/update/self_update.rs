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
    let result = download_and_swap(url, expected_version, &tmp, installed).await;
    if result.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    result
}

/// The fallible middle of [`apply`]: download, chmod, verify, swap.
async fn download_and_swap(
    url: &str,
    expected_version: &str,
    tmp: &Path,
    installed: &Path,
) -> Result<()> {
    download(url, tmp).await?;
    #[cfg(unix)]
    {
        use std::os::unix::fs::PermissionsExt;
        let mut perms = std::fs::metadata(tmp)?.permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(tmp, perms)?;
    }
    // `--version` runs a subprocess: do it off the async runtime.
    let tmp_owned = tmp.to_path_buf();
    let installed_owned = installed.to_path_buf();
    let expected = expected_version.to_string();
    tokio::task::spawn_blocking(move || verify_and_swap(&tmp_owned, &installed_owned, &expected))
        .await
        .context("verify task panicked")?
}

/// Stream `url` to `dest`.
async fn download(url: &str, dest: &Path) -> Result<()> {
    let client = reqwest::Client::builder()
        .timeout(Duration::from_secs(600))
        .https_only(true)
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

/// Run `<bin> --version` (10s timeout) and check the output mentions
/// `expected_version`. Catches corrupt or wrong-architecture downloads before
/// anything is touched.
fn verify_version(bin: &Path, expected_version: &str) -> Result<()> {
    verify_version_with_timeout(bin, expected_version, Duration::from_secs(10))
}

/// Inner implementation of version verification with a configurable timeout.
fn verify_version_with_timeout(
    bin: &Path,
    expected_version: &str,
    timeout: Duration,
) -> Result<()> {
    use std::process::Stdio;
    // Retry ETXTBSY (os error 26): on Linux, exec of a just-written file fails
    // with "text file busy" while any concurrently-forked child still holds a
    // write fd inherited across fork (tokio worker threads, parallel tests).
    // The fd closes as soon as that child execs, so a short retry suffices.
    let mut attempts = 0;
    let mut child = loop {
        match std::process::Command::new(bin)
            .arg("--version")
            .stdout(Stdio::piped())
            .stderr(Stdio::null())
            .spawn()
        {
            Ok(child) => break child,
            Err(e) if e.raw_os_error() == Some(26) && attempts < 20 => {
                attempts += 1;
                std::thread::sleep(Duration::from_millis(50));
            }
            Err(e) => {
                return Err(e).with_context(|| format!("running {} --version", bin.display()));
            }
        }
    };
    let deadline = std::time::Instant::now() + timeout;
    // Reading stdout after exit is safe here: --version output is far below
    // the pipe buffer size, so the child never blocks on a full pipe.
    let status = loop {
        match child.try_wait()? {
            Some(status) => break status,
            None if std::time::Instant::now() > deadline => {
                let _ = child.kill();
                let _ = child.wait();
                bail!("downloaded binary hung in --version ({timeout:?} timeout)");
            }
            None => std::thread::sleep(Duration::from_millis(50)),
        }
    };
    let mut stdout = String::new();
    if let Some(mut out) = child.stdout.take() {
        use std::io::Read;
        let _ = out.read_to_string(&mut stdout);
    }
    let expected = expected_version.trim_start_matches('v');
    if !status.success() || !stdout.contains(expected) {
        bail!(
            "downloaded binary failed verification (status {status:?}, output {:?}, expected version {expected})",
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

    #[test]
    fn verify_times_out_on_hung_binary() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("memd.new");
        std::fs::write(&p, "#!/bin/sh\nsleep 60\n").unwrap();
        let mut perms = std::fs::metadata(&p).unwrap().permissions();
        perms.set_mode(0o755);
        std::fs::set_permissions(&p, perms).unwrap();
        let err =
            verify_version_with_timeout(&p, "v0.2.0", Duration::from_millis(200)).unwrap_err();
        assert!(err.to_string().contains("timeout"), "got: {err}");
    }
}
