//! Startup self-elevation (Windows).
//!
//! Chipset SMBus access (DRAM and GPU RGB via PawnIO) requires Administrator
//! privileges; unelevated, `pawnio_open` fails with `E_ACCESSDENIED` and those
//! devices never appear. On startup we offer a UAC prompt to relaunch elevated;
//! accepting hands off to the elevated instance, declining is non-fatal.

/// Ensure the process is elevated, prompting via UAC if it is not.
///
/// No-op on non-Windows platforms — Linux SMBus access is governed by
/// `/dev/i2c-*` permissions (the `i2c` group), not process elevation.
#[cfg(not(windows))]
pub fn ensure_elevated() {}

#[cfg(windows)]
pub fn ensure_elevated() {
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    if is_elevated() {
        return;
    }

    let exe = match std::env::current_exe() {
        Ok(p) => p,
        Err(e) => {
            log::warn!("[elevation] cannot locate executable ({e}); continuing unelevated");
            return;
        }
    };

    // Preserve the working directory; `ShellExecuteExW` otherwise starts the
    // elevated process in System32, breaking CWD-relative lookups (config, the
    // PawnIO `pwnio/` module folder).
    let cwd = std::env::current_dir().unwrap_or_default();

    let params: String = std::env::args()
        .skip(1)
        .map(|a| quote_arg(&a))
        .collect::<Vec<_>>()
        .join(" ");

    let verb = wide("runas");
    let file = wide_path(&exe);
    let params_w = wide(&params);
    let dir_w = wide_path(&cwd);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: if params.is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(params_w.as_ptr())
        },
        lpDirectory: if cwd.as_os_str().is_empty() {
            PCWSTR::null()
        } else {
            PCWSTR(dir_w.as_ptr())
        },
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY: `info` and every string buffer it points at outlive the call.
    match unsafe { ShellExecuteExW(&mut info) } {
        Ok(()) => {
            // Hand off to the elevated instance so two daemons don't contend
            // for the IPC pipe.
            log::info!("[elevation] relaunched with Administrator privileges");
            std::process::exit(0);
        }
        Err(e) => {
            log::warn!(
                "[elevation] not granted Administrator privileges ({e}), continuing \
                 anyway; chipset SMBus (DRAM / GPU RGB) devices will be unavailable"
            );
        }
    }
}

/// Whether the current process token is elevated.
#[cfg(windows)]
pub(crate) fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard Win32 token-query sequence; the token handle is closed
    // before returning.
    unsafe {
        let mut token = HANDLE::default();
        if OpenProcessToken(GetCurrentProcess(), TOKEN_QUERY, &mut token).is_err() {
            return false;
        }
        let mut elevation = TOKEN_ELEVATION::default();
        let mut size = 0u32;
        let ok = GetTokenInformation(
            token,
            TokenElevation,
            Some(&mut elevation as *mut _ as *mut std::ffi::c_void),
            std::mem::size_of::<TOKEN_ELEVATION>() as u32,
            &mut size,
        );
        let _ = CloseHandle(token);
        ok.is_ok() && elevation.TokenIsElevated != 0
    }
}

#[cfg(windows)]
use crate::platform::win32::{wide, wide_path};

/// Quote a single argument for `CreateProcess`/`ShellExecuteExW` `lpParameters`
/// using the CommandLineToArgvW escaping rules:
/// - Wrap in double quotes if the argument is empty or contains spaces/tabs/quotes.
/// - Backslashes before a `"` (or the closing quote) are doubled.
#[cfg(windows)]
fn quote_arg(arg: &str) -> String {
    let needs_quoting = arg.is_empty() || arg.chars().any(|c| c == ' ' || c == '\t' || c == '"');
    if !needs_quoting {
        return arg.to_string();
    }
    let mut out = String::from('"');
    let mut backslashes: usize = 0;
    for c in arg.chars() {
        match c {
            '\\' => backslashes += 1,
            '"' => {
                // 2N backslashes + escaped quote
                for _ in 0..backslashes * 2 {
                    out.push('\\');
                }
                out.push_str("\\\"");
                backslashes = 0;
            }
            _ => {
                for _ in 0..backslashes {
                    out.push('\\');
                }
                out.push(c);
                backslashes = 0;
            }
        }
    }
    // Backslashes before the closing quote must be doubled.
    for _ in 0..backslashes * 2 {
        out.push('\\');
    }
    out.push('"');
    out
}

#[cfg(all(windows, test))]
mod tests {
    use super::quote_arg;

    #[test]
    fn plain_arg_no_quoting_needed() {
        assert_eq!(quote_arg("hello"), "hello");
    }

    #[test]
    fn empty_arg_wraps_in_quotes() {
        assert_eq!(quote_arg(""), "\"\"");
    }

    #[test]
    fn arg_with_space_wraps_in_quotes() {
        assert_eq!(quote_arg("two words"), "\"two words\"");
    }

    #[test]
    fn arg_with_double_quote_escapes() {
        assert_eq!(quote_arg(r#"a"b"#), "\"a\\\"b\"");
    }

    #[test]
    fn arg_with_backslashes_before_quote() {
        assert_eq!(quote_arg(r"a\"), "a\\");
    }

    #[test]
    fn arg_with_multiple_backslashes_before_quote() {
        assert_eq!(quote_arg(r##"a\\\"b"##), r#""a\\\\\\\"b""#);
    }

    #[test]
    fn arg_with_tab_requires_quoting() {
        assert_eq!(quote_arg("a\tb"), "\"a\tb\"");
    }
}
