// SPDX-License-Identifier: GPL-3.0-or-later
//! Elevation helpers (Windows).
//!
//! Since the privilege split, `halod.exe --worker` **never** self-elevates —
//! doing so would put the plugin/network code back at Administrator, defeating
//! the whole point. Chipset SMBus / PawnIO register access instead goes through
//! the elevated `halod-broker` process. "Elevation" here therefore means one of
//! two things:
//!   - [`is_elevated`] — a read-only check still used by the action executor and
//!     the debug usecase.
//!   - [`spawn_broker_elevated`] — on a **dev run** (no installed service to
//!     launch the broker), bring the broker up ourselves with one UAC prompt.

/// Whether the current process runs with elevated privilege. On Unix the daemon
/// is meant to run per-user; a non-zero effective UID is normal and `false`.
/// Running as root is treated as elevated so configuration-triggered process
/// launches (Command/OpenApp) are refused rather than run as root.
#[cfg(unix)]
pub(crate) fn is_elevated() -> bool {
    // SAFETY: geteuid is always safe and never fails.
    unsafe { libc::geteuid() == 0 }
}

/// Emit a loud, one-time warning if the daemon is running as root on Unix.
#[cfg(unix)]
pub(crate) fn warn_if_elevated() {
    // TODO: we should probably just refuse to run elevated.
    // We have the broker on windows, on linux we really don't have any usecase where running elevated is warranted.
    // so let's close this hole and decrease the attack surface.
    if is_elevated() {
        log::warn!(
            "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
        );
        log::warn!(
            "!!!!!!! HaloDaemon IS RUNNING AS ROOT - THIS IS DANGEROUS AND UNSUPPORTED !!!!!!!"
        );
        log::warn!(
            "!!!!!!! Run it as your normal user. Command/OpenApp button actions are     !!!!!!!"
        );
        log::warn!(
            "!!!!!!! DISABLED while elevated so a button mapping can't run as root.      !!!!!!!"
        );
        log::warn!(
            "!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!!"
        );
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

/// Launch `halod-broker.exe` (next to this executable) elevated via a UAC
/// prompt. Used only on a dev run, where no supervisor service exists to spawn
/// the broker — see [`crate::drivers::transports::register_ops`]. Declining the
/// prompt is surfaced as an error by the caller (register-bus devices become
/// unavailable, HID/network keep working).
#[cfg(windows)]
pub(crate) fn spawn_broker_elevated(
    bootstrap_token: &str,
    coordinator_sid: &str,
    coordinator_session: u32,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use windows::core::PCWSTR;
    use windows::Win32::UI::Shell::{ShellExecuteExW, SEE_MASK_NOASYNC, SHELLEXECUTEINFOW};
    use windows::Win32::UI::WindowsAndMessaging::SW_SHOWNORMAL;

    let exe = std::env::current_exe().context("locating halod.exe")?;
    let dir = exe
        .parent()
        .ok_or_else(|| anyhow!("cannot resolve install directory"))?;
    let broker = dir.join("halod-broker.exe");
    if !broker.exists() {
        return Err(anyhow!(
            "halod-broker.exe not found next to {}",
            exe.display()
        ));
    }

    let verb = wide("runas");
    let file = wide_path(&broker);
    let dir_w = wide_path(dir);
    let parameters = [
        format!("--bootstrap-token={bootstrap_token}"),
        format!("--coordinator-sid={coordinator_sid}"),
        format!("--coordinator-session={coordinator_session}"),
    ]
    .iter()
    .map(|argument| quote_arg(argument))
    .collect::<Vec<_>>()
    .join(" ");
    let parameters = wide(&parameters);

    let mut info = SHELLEXECUTEINFOW {
        cbSize: std::mem::size_of::<SHELLEXECUTEINFOW>() as u32,
        fMask: SEE_MASK_NOASYNC,
        lpVerb: PCWSTR(verb.as_ptr()),
        lpFile: PCWSTR(file.as_ptr()),
        lpParameters: PCWSTR(parameters.as_ptr()),
        lpDirectory: PCWSTR(dir_w.as_ptr()),
        nShow: SW_SHOWNORMAL.0,
        ..Default::default()
    };

    // SAFETY: `info` and every string buffer it points at outlive the call.
    unsafe { ShellExecuteExW(&mut info) }
        .map_err(|e| anyhow!("ShellExecuteExW(runas) for halod-broker: {e}"))?;
    log::info!("[elevation] launched halod-broker with a UAC prompt");
    Ok(())
}

#[cfg(windows)]
use crate::platform::win32::{wide, wide_path};

/// Quote a single argument for `CreateProcess`/`ShellExecuteExW` `lpParameters`
/// using the CommandLineToArgvW escaping rules:
/// - Wrap in double quotes if the argument is empty or contains spaces/tabs/quotes.
/// - Backslashes before a `"` (or the closing quote) are doubled.
///
#[cfg(windows)]
pub(crate) fn quote_arg(arg: &str) -> String {
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
