// SPDX-License-Identifier: GPL-3.0-or-later
//! The single durable atomic-write primitive for all persistence paths.

use anyhow::{Context, Result};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};

static TMP_SEQ: AtomicU64 = AtomicU64::new(0);

/// Atomically write `bytes` to `path`: a uniquely-named temp in the same dir,
/// created owner-only, fully written and fsync'd, then renamed over the
/// destination (replacing it on every platform), then the parent dir fsync'd on
/// Unix. Sync/permission failures propagate; the temp is removed on any error.
pub fn atomic_write(path: &Path, bytes: &[u8]) -> Result<()> {
    let parent = path.parent().unwrap_or_else(|| Path::new("."));
    std::fs::create_dir_all(parent).with_context(|| format!("creating {}", parent.display()))?;
    let base = path.file_name().and_then(|n| n.to_str()).unwrap_or("tmp");
    let seq = TMP_SEQ.fetch_add(1, Ordering::Relaxed);
    let tmp = parent.join(format!(".{base}.{}.{seq}.tmp", std::process::id()));
    let res = write_and_replace(&tmp, path, bytes);
    if res.is_err() {
        let _ = std::fs::remove_file(&tmp);
    }
    res
}

/// Convenience for string payloads.
pub fn atomic_write_str(path: &Path, contents: &str) -> Result<()> {
    atomic_write(path, contents.as_bytes())
}

fn write_and_replace(tmp: &Path, dst: &Path, bytes: &[u8]) -> Result<()> {
    use std::io::Write as _;
    let mut opts = std::fs::OpenOptions::new();
    opts.write(true).create_new(true);
    #[cfg(unix)]
    {
        use std::os::unix::fs::OpenOptionsExt as _;
        opts.mode(0o600);
    }
    let mut f = opts
        .open(tmp)
        .with_context(|| format!("creating {}", tmp.display()))?;
    f.write_all(bytes).context("writing temp file")?;
    f.sync_all().context("syncing temp file")?;
    drop(f);
    // rename replaces an existing destination on Unix and on Windows
    // (MoveFileEx REPLACE_EXISTING).
    std::fs::rename(tmp, dst).with_context(|| format!("replacing {}", dst.display()))?;
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(dst.parent().unwrap_or_else(|| Path::new("."))) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn writes_and_replaces_existing() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        atomic_write_str(&p, "one").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "one");
        atomic_write_str(&p, "two").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "two");
    }

    #[test]
    fn leaves_no_temp_files() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        atomic_write_str(&p, "x").unwrap();
        let tmps = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(tmps, 0);
    }

    #[cfg(unix)]
    #[test]
    fn creates_owner_only() {
        use std::os::unix::fs::PermissionsExt as _;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        atomic_write_str(&p, "x").unwrap();
        let mode = std::fs::metadata(&p).unwrap().permissions().mode() & 0o777;
        assert_eq!(mode, 0o600);
    }
}
