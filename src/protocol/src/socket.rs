//! Shared IPC socket-path logic for the daemon and UI.

#[cfg(unix)]
use std::path::PathBuf;

/// Real uid of the current process.
#[cfg(unix)]
pub fn current_uid() -> u32 {
    // SAFETY: `getuid` is always safe — it reads the real uid and never fails.
    unsafe { libc::getuid() }
}

/// Directory that holds the Unix command socket, plus whether it is the
/// *fallback* location (i.e. `XDG_RUNTIME_DIR` was unset).
///
/// When `XDG_RUNTIME_DIR` is set (the normal systemd user-service case) it is
/// already a per-user `0700` directory (`/run/user/UID`), so the socket is
/// private with no extra work. When it is unset we fall back to a per-user
/// subdirectory of the temp dir rather than the
/// world-accessible temp dir itself; the `uid` suffix avoids cross-user
/// collisions, and the daemon is responsible for creating it `0700`.
#[cfg(unix)]
pub fn runtime_dir() -> (PathBuf, bool) {
    match std::env::var_os("XDG_RUNTIME_DIR") {
        Some(dir) if !dir.is_empty() => (PathBuf::from(dir), false),
        _ => {
            let uid = current_uid();
            (std::env::temp_dir().join(format!("halod-{uid}")), true)
        }
    }
}

#[cfg(unix)]
pub fn socket_path() -> String {
    runtime_dir()
        .0
        .join("halod.sock")
        .to_string_lossy()
        .into_owned()
}

#[cfg(windows)]
pub fn socket_path() -> String {
    r"\\.\pipe\halod".to_string()
}

#[cfg(not(any(unix, windows)))]
pub fn socket_path() -> String {
    String::new()
}

#[cfg(all(test, unix))]
mod tests {
    use super::*;

    #[test]
    fn socket_path_is_under_runtime_dir() {
        let path = socket_path();
        assert!(path.ends_with("/halod.sock"), "unexpected path: {path}");
    }

    #[test]
    fn fallback_dir_is_per_user_when_xdg_unset() {
        // Mutating process env is global; this test is the only one that touches
        // XDG_RUNTIME_DIR, and it restores the prior value before returning.
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: single-threaded test, no other thread reads the env here.
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };

        let (dir, is_fallback) = runtime_dir();
        let uid = current_uid();
        assert!(is_fallback);
        assert_eq!(dir.file_name().unwrap(), format!("halod-{uid}").as_str());

        // SAFETY: restoring the captured value, same single-threaded context.
        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }
}
