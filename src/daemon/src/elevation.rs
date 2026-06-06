//! Startup self-elevation (Windows).
//!
//! Chipset SMBus access — DRAM and GPU RGB via the PawnIO kernel driver —
//! requires Administrator privileges. When `halod` is launched
//! unelevated, `pawnio_open` fails with `E_ACCESSDENIED` (0x80070005) and those
//! devices silently never appear.
//!
//! On startup we therefore offer a UAC prompt to relaunch the process elevated.
//! Accepting hands control to the elevated instance; declining is non-fatal —
//! the daemon keeps running, just without chipset SMBus devices.

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

    // Preserve the working directory across the relaunch — `ShellExecuteExW`
    // otherwise starts the elevated process in System32, which breaks
    // CWD-relative lookups (config files, the PawnIO `pwnio/` module folder).
    let cwd = std::env::current_dir().unwrap_or_default();

    // Forward the original command-line arguments (argv[0] excluded).
    let params: String = std::env::args().skip(1).collect::<Vec<_>>().join(" ");

    let verb = wide("runas");
    let file = wide(&exe.to_string_lossy());
    let params_w = wide(&params);
    let dir_w = wide(&cwd.to_string_lossy());

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
            // The elevated instance is launched — hand off to it so two
            // daemons don't contend for the IPC pipe.
            log::info!("[elevation] relaunched with Administrator privileges");
            std::process::exit(0);
        }
        Err(e) => {
            log::warn!(
                "[elevation] not granted Administrator privileges ({e}) — continuing \
                 anyway; chipset SMBus (DRAM / GPU RGB) devices will be unavailable"
            );
        }
    }
}

/// Whether the current process token is elevated.
#[cfg(windows)]
fn is_elevated() -> bool {
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

/// Encode a string as a NUL-terminated UTF-16 buffer for the Win32 `*W` APIs.
#[cfg(windows)]
fn wide(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}
