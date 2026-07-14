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
    #[cfg(windows)]
    let mut f = open_owner_only_temp(tmp)?;
    #[cfg(not(windows))]
    let mut f = {
        let mut opts = std::fs::OpenOptions::new();
        opts.write(true).create_new(true);
        #[cfg(unix)]
        {
            use std::os::unix::fs::OpenOptionsExt as _;
            opts.mode(0o600);
        }
        opts.open(tmp)
            .with_context(|| format!("creating {}", tmp.display()))?
    };
    f.write_all(bytes).context("writing temp file")?;
    f.sync_all().context("syncing temp file")?;
    drop(f);
    replace_file(tmp, dst)?;
    #[cfg(unix)]
    if let Ok(dir) = std::fs::File::open(dst.parent().unwrap_or_else(|| Path::new("."))) {
        let _ = dir.sync_all();
    }
    Ok(())
}

#[cfg(windows)]
fn open_owner_only_temp(path: &Path) -> Result<std::fs::File> {
    use std::os::windows::ffi::OsStrExt as _;
    use std::os::windows::io::FromRawHandle as _;
    use windows::core::PCWSTR;
    use windows::Win32::Foundation::{LocalFree, GENERIC_WRITE, HLOCAL};
    use windows::Win32::Security::Authorization::ConvertStringSecurityDescriptorToSecurityDescriptorW;
    use windows::Win32::Security::{PSECURITY_DESCRIPTOR, SECURITY_ATTRIBUTES};
    use windows::Win32::Storage::FileSystem::{
        CreateFileW, CREATE_NEW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_MODE,
    };

    let (sid, _) = halod_hwaccess::winsec::current_process_identity()?;
    let sddl: Vec<u16> = format!("D:P(A;;GA;;;{sid})(A;;GA;;;SY)")
        .encode_utf16()
        .chain(Some(0))
        .collect();
    let mut descriptor = PSECURITY_DESCRIPTOR::default();
    unsafe {
        ConvertStringSecurityDescriptorToSecurityDescriptorW(
            PCWSTR(sddl.as_ptr()),
            1,
            &mut descriptor,
            None,
        )
    }
    .map_err(|e| anyhow::anyhow!("building owner-only temp-file DACL: {e}"))?;
    let attributes = SECURITY_ATTRIBUTES {
        nLength: std::mem::size_of::<SECURITY_ATTRIBUTES>() as u32,
        lpSecurityDescriptor: descriptor.0,
        bInheritHandle: false.into(),
    };
    let path_w: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
    let handle = unsafe {
        CreateFileW(
            PCWSTR(path_w.as_ptr()),
            GENERIC_WRITE.0,
            FILE_SHARE_MODE(0),
            Some(&attributes),
            CREATE_NEW,
            FILE_ATTRIBUTE_NORMAL,
            None,
        )
    };
    unsafe {
        let _ = LocalFree(Some(HLOCAL(descriptor.0)));
    }
    let handle = handle.map_err(|e| anyhow::anyhow!("creating {}: {e}", path.display()))?;
    // SAFETY: CreateFileW returned a newly owned handle; File assumes sole
    // ownership and closes it exactly once.
    Ok(unsafe { std::fs::File::from_raw_handle(handle.0) })
}

#[cfg(not(windows))]
fn replace_file(tmp: &Path, dst: &Path) -> Result<()> {
    std::fs::rename(tmp, dst).with_context(|| format!("replacing {}", dst.display()))
}

#[cfg(windows)]
fn replace_file(tmp: &Path, dst: &Path) -> Result<()> {
    use std::os::windows::ffi::OsStrExt as _;
    use windows::core::PCWSTR;
    use windows::Win32::Storage::FileSystem::{
        MoveFileExW, MOVEFILE_REPLACE_EXISTING, MOVEFILE_WRITE_THROUGH,
    };

    let tmp: Vec<u16> = tmp.as_os_str().encode_wide().chain(Some(0)).collect();
    let dst_w: Vec<u16> = dst.as_os_str().encode_wide().chain(Some(0)).collect();
    let mut last_error = None;
    // Indexers and antivirus commonly hold a destination briefly without
    // FILE_SHARE_DELETE. Retry those transient collisions, but never delete the
    // old destination first: readers always observe either complete version.
    for attempt in 0..40 {
        // SAFETY: both nul-terminated path buffers outlive the call.
        match unsafe {
            MoveFileExW(
                PCWSTR(tmp.as_ptr()),
                PCWSTR(dst_w.as_ptr()),
                MOVEFILE_REPLACE_EXISTING | MOVEFILE_WRITE_THROUGH,
            )
        } {
            Ok(()) => return Ok(()),
            Err(error) => {
                let code = error.code().0 as u32 & 0xffff;
                if !matches!(code, 5 | 32 | 33) || attempt == 39 {
                    last_error = Some(error);
                    break;
                }
                last_error = Some(error);
                std::thread::sleep(std::time::Duration::from_millis(25));
            }
        }
    }
    Err(anyhow::anyhow!(
        "replacing {} with MoveFileExW(REPLACE_EXISTING|WRITE_THROUGH): {}",
        dst.display(),
        last_error.expect("replacement loop always runs")
    ))
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

    // A Windows handle opened without FILE_SHARE_DELETE prevents replacement.
    // The helper must fail safely (old destination intact, no temp debris), and
    // replacement must work as soon as the interfering handle is released.
    #[cfg(windows)]
    #[test]
    fn reader_interference_fails_safely_then_replaces_after_close() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        atomic_write_str(&p, "old").unwrap();
        let reader = std::fs::File::open(&p).unwrap();
        assert!(atomic_write_str(&p, "new").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old");
        drop(reader);
        atomic_write_str(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
    }

    #[cfg(windows)]
    #[test]
    fn writer_interference_fails_safely_then_replaces_after_close() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("f.txt");
        atomic_write_str(&p, "old").unwrap();
        let writer = std::fs::OpenOptions::new().write(true).open(&p).unwrap();
        assert!(atomic_write_str(&p, "new").is_err());
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old");
        drop(writer);
        atomic_write_str(&p, "new").unwrap();
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "new");
    }

    // Repeated in-place replacement leaves exactly the last write and no temp
    // debris, including when the destination already exists each round.
    #[cfg(windows)]
    #[test]
    fn repeated_replacement_leaves_only_the_final_content() {
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("cfg.yaml");
        for i in 0..25 {
            atomic_write_str(&p, &format!("gen {i}")).unwrap();
            assert_eq!(std::fs::read_to_string(&p).unwrap(), format!("gen {i}"));
        }
        let leftovers = std::fs::read_dir(dir.path())
            .unwrap()
            .filter_map(|e| e.ok())
            .filter(|e| e.file_name().to_string_lossy().contains(".tmp"))
            .count();
        assert_eq!(leftovers, 0);
    }

    #[cfg(windows)]
    fn open_without_delete_sharing(path: &Path) -> windows::Win32::Foundation::HANDLE {
        use std::os::windows::ffi::OsStrExt as _;
        use windows::core::PCWSTR;
        use windows::Win32::Foundation::GENERIC_READ;
        use windows::Win32::Storage::FileSystem::{
            CreateFileW, FILE_ATTRIBUTE_NORMAL, FILE_SHARE_READ, OPEN_EXISTING,
        };
        let path: Vec<u16> = path.as_os_str().encode_wide().chain(Some(0)).collect();
        unsafe {
            CreateFileW(
                PCWSTR(path.as_ptr()),
                GENERIC_READ.0,
                FILE_SHARE_READ,
                None,
                OPEN_EXISTING,
                FILE_ATTRIBUTE_NORMAL,
                None,
            )
        }
        .unwrap()
    }

    #[cfg(windows)]
    #[test]
    fn retries_transient_indexer_or_antivirus_interference() {
        use windows::Win32::Foundation::CloseHandle;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("config.yaml");
        atomic_write_str(&p, "old").unwrap();
        let held = open_without_delete_sharing(&p).0 as usize;
        let release = std::thread::spawn(move || {
            std::thread::sleep(std::time::Duration::from_millis(150));
            unsafe { CloseHandle(windows::Win32::Foundation::HANDLE(held as *mut _)).unwrap() };
        });
        atomic_write_str(&p, "new").unwrap();
        release.join().unwrap();
        assert_eq!(std::fs::read_to_string(p).unwrap(), "new");
    }

    #[cfg(windows)]
    #[test]
    fn failed_locked_replacement_preserves_destination_and_cleans_temp() {
        use windows::Win32::Foundation::CloseHandle;
        let dir = tempfile::tempdir().unwrap();
        let p = dir.path().join("secret.json");
        atomic_write_str(&p, "old").unwrap();
        let held = open_without_delete_sharing(&p);
        assert!(atomic_write_str(&p, "new").is_err());
        unsafe { CloseHandle(held).unwrap() };
        assert_eq!(std::fs::read_to_string(&p).unwrap(), "old");
        assert_eq!(
            std::fs::read_dir(dir.path())
                .unwrap()
                .filter_map(|entry| entry.ok())
                .filter(|entry| entry.file_name().to_string_lossy().contains(".tmp"))
                .count(),
            0
        );
    }
}
