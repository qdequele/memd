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
