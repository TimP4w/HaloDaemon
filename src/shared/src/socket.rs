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
    let path = runtime_dir().0.join(crate::app::SOCKET_FILENAME);
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

    // Serialise all tests that touch XDG_RUNTIME_DIR so parallel test threads
    // don't race on process-global env state.
    static ENV_LOCK: std::sync::Mutex<()> = std::sync::Mutex::new(());

    #[test]
    fn socket_path_is_under_runtime_dir() {
        let _guard = ENV_LOCK.lock().unwrap();
        let path = socket_path();
        assert!(path.ends_with("/halod.sock"), "unexpected path: {path}");
    }

    #[test]
    fn fallback_dir_is_per_user_when_xdg_unset() {
        let _guard = ENV_LOCK.lock().unwrap();
        let prev = std::env::var_os("XDG_RUNTIME_DIR");
        // SAFETY: ENV_LOCK ensures no other test thread reads XDG_RUNTIME_DIR
        // concurrently; the value is restored before the lock is released.
        unsafe { std::env::remove_var("XDG_RUNTIME_DIR") };

        let (dir, is_fallback) = runtime_dir();
        let uid = current_uid();
        assert!(is_fallback);
        assert_eq!(dir.file_name().unwrap(), format!("halod-{uid}").as_str());

        unsafe {
            match prev {
                Some(v) => std::env::set_var("XDG_RUNTIME_DIR", v),
                None => std::env::remove_var("XDG_RUNTIME_DIR"),
            }
        }
    }
}
