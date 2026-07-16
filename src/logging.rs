//! Daemon log management: a size-capped rotating file writer and helpers to
//! keep `memd.log` bounded. Rotation keeps exactly one previous file
//! (`memd.log.1`), so disk use never exceeds ~2× the cap.

use anyhow::Result;
use std::io::Write;
use std::path::{Path, PathBuf};

/// Maximum size of the active log file before it is rotated.
pub const MAX_LOG_BYTES: u64 = 50 * 1024 * 1024;

/// Rename `path` to `path.1` (replacing any previous rotation).
fn rotate(path: &Path) -> std::io::Result<()> {
    let mut rotated = path.as_os_str().to_owned();
    rotated.push(".1");
    std::fs::rename(path, PathBuf::from(rotated))
}

/// Rotate `path` out of the way if it has grown past `max_bytes`. Called once
/// at daemon startup, before any writer opens the file, so a log left oversized
/// by a previous run (or by the pre-rotation era) is bounded immediately.
pub fn rotate_if_oversized(path: &Path, max_bytes: u64) -> Result<()> {
    if let Ok(meta) = std::fs::metadata(path)
        && meta.len() > max_bytes
    {
        rotate(path)?;
    }
    Ok(())
}

/// A `Write` impl that appends to `path` and rotates it to `path.1` once
/// `max_bytes` is exceeded, then continues in a fresh file. It owns its file
/// handle and re-opens after rotation, so rotation works even while writing.
pub struct SizeRotatingWriter {
    path: PathBuf,
    max_bytes: u64,
    file: std::fs::File,
    written: u64,
}

impl SizeRotatingWriter {
    pub fn new(path: PathBuf, max_bytes: u64) -> Result<Self> {
        let file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&path)?;
        let written = file.metadata().map(|m| m.len()).unwrap_or(0);
        Ok(Self {
            path,
            max_bytes,
            file,
            written,
        })
    }

    fn rotate_and_reopen(&mut self) -> std::io::Result<()> {
        self.file.flush()?;
        rotate(&self.path)?;
        self.file = std::fs::OpenOptions::new()
            .create(true)
            .append(true)
            .open(&self.path)?;
        self.written = 0;
        Ok(())
    }
}

impl Write for SizeRotatingWriter {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        if self.written >= self.max_bytes {
            // Best-effort: a failed rotation must not take logging down.
            let _ = self.rotate_and_reopen();
        }
        let n = self.file.write(buf)?;
        self.written += n as u64;
        Ok(n)
    }

    fn flush(&mut self) -> std::io::Result<()> {
        self.file.flush()
    }
}

/// Caps how many times a repetitive event is logged. The first `max` calls to
/// [`LogLimiter::should_log`] return true; everything after is counted as
/// suppressed so the caller can emit one roll-up line at the end.
pub struct LogLimiter {
    max: usize,
    seen: usize,
}

impl LogLimiter {
    pub fn new(max: usize) -> Self {
        Self { max, seen: 0 }
    }

    pub fn should_log(&mut self) -> bool {
        self.seen += 1;
        self.seen <= self.max
    }

    /// How many events were suppressed (0 while under the cap).
    pub fn suppressed(&self) -> usize {
        self.seen.saturating_sub(self.max)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn tmp_dir(name: &str) -> PathBuf {
        let dir = std::env::temp_dir().join(format!("memd-logging-test-{name}"));
        let _ = std::fs::remove_dir_all(&dir);
        std::fs::create_dir_all(&dir).unwrap();
        dir
    }

    #[test]
    fn writer_rotates_when_cap_exceeded() {
        let dir = tmp_dir("rotate");
        let log = dir.join("memd.log");
        let mut w = SizeRotatingWriter::new(log.clone(), 100).unwrap();

        // 20 lines of 10 bytes = 200 bytes: must rotate at least once.
        for _ in 0..20 {
            w.write_all(b"0123456789").unwrap();
        }
        w.flush().unwrap();

        let rotated = dir.join("memd.log.1");
        assert!(rotated.exists(), "expected {} to exist", rotated.display());
        assert!(
            std::fs::metadata(&log).unwrap().len() <= 110,
            "active log should have been reset by rotation"
        );
    }

    #[test]
    fn writer_keeps_only_one_rotated_file() {
        let dir = tmp_dir("keep-one");
        let log = dir.join("memd.log");
        let mut w = SizeRotatingWriter::new(log.clone(), 50).unwrap();

        for _ in 0..40 {
            w.write_all(b"0123456789").unwrap();
        }
        w.flush().unwrap();

        // Multiple rotations happened; only .1 may exist.
        assert!(dir.join("memd.log.1").exists());
        assert!(!dir.join("memd.log.2").exists());
        assert!(!dir.join("memd.log.1.1").exists());
    }

    #[test]
    fn writer_counts_preexisting_bytes_toward_cap() {
        let dir = tmp_dir("preexisting");
        let log = dir.join("memd.log");
        std::fs::write(&log, vec![b'x'; 90]).unwrap();

        let mut w = SizeRotatingWriter::new(log.clone(), 100).unwrap();
        w.write_all(b"0123456789").unwrap(); // reaches 100
        w.write_all(b"after").unwrap(); // must land in a fresh file
        w.flush().unwrap();

        assert!(dir.join("memd.log.1").exists());
        assert_eq!(std::fs::read_to_string(&log).unwrap(), "after");
    }

    #[test]
    fn rotate_if_oversized_moves_large_file() {
        let dir = tmp_dir("startup");
        let log = dir.join("memd.log");
        std::fs::write(&log, vec![b'x'; 200]).unwrap();

        rotate_if_oversized(&log, 100).unwrap();

        assert!(!log.exists());
        assert_eq!(
            std::fs::metadata(dir.join("memd.log.1")).unwrap().len(),
            200
        );
    }

    #[test]
    fn rotate_if_oversized_leaves_small_and_missing_files_alone() {
        let dir = tmp_dir("startup-small");
        let log = dir.join("memd.log");
        std::fs::write(&log, b"tiny").unwrap();

        rotate_if_oversized(&log, 100).unwrap();
        assert!(log.exists());
        assert!(!dir.join("memd.log.1").exists());

        // A missing file is fine too.
        rotate_if_oversized(&dir.join("nope.log"), 100).unwrap();
    }

    #[test]
    fn limiter_allows_first_n_then_suppresses() {
        let mut l = LogLimiter::new(3);
        assert!(l.should_log());
        assert!(l.should_log());
        assert!(l.should_log());
        assert_eq!(l.suppressed(), 0);
        assert!(!l.should_log());
        assert!(!l.should_log());
        assert_eq!(l.suppressed(), 2);
    }
}
