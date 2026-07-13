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
/// `XDG_RUNTIME_DIR` is already a per-user `0700` dir. When unset, fall back to
/// a per-user (`uid`-suffixed) subdirectory of the temp dir rather than the
/// world-accessible temp dir itself; the daemon must create it `0700`.
#[cfg(unix)]
pub fn runtime_dir() -> (PathBuf, bool) {
    runtime_dir_from(
        std::env::var_os("XDG_RUNTIME_DIR"),
        std::env::temp_dir(),
        current_uid(),
    )
}

/// Pure runtime-directory resolver used by startup and tests. Keeping the
/// process environment at this boundary avoids mutating it in concurrent tests.
#[cfg(unix)]
pub fn runtime_dir_from(
    xdg_runtime_dir: Option<std::ffi::OsString>,
    temp_dir: PathBuf,
    uid: u32,
) -> (PathBuf, bool) {
    match xdg_runtime_dir {
        Some(dir) if !dir.is_empty() => (PathBuf::from(dir), false),
        _ => (temp_dir.join(format!("halod-{uid}")), true),
    }
}

#[cfg(unix)]
pub fn socket_path() -> String {
    socket_path_in(&runtime_dir().0)
}

#[cfg(unix)]
pub fn socket_path_in(runtime_dir: &std::path::Path) -> String {
    let path = runtime_dir.join(crate::app::SOCKET_FILENAME);
    path.to_str()
        .expect("socket path (XDG_RUNTIME_DIR/halod.sock) contains non-UTF-8 bytes")
        .to_owned()
}

#[cfg(windows)]
pub fn socket_path() -> String {
    crate::app::PIPE_NAME.to_string()
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
        let uid = 1234;
        let (dir, is_fallback) = runtime_dir_from(None, PathBuf::from("/tmp"), uid);
        assert!(is_fallback);
        assert_eq!(dir.file_name().unwrap(), format!("halod-{uid}").as_str());
        assert_eq!(socket_path_in(&dir), format!("/tmp/halod-{uid}/halod.sock"));
    }
}
