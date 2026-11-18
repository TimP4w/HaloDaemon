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

/// Refuse to run the user daemon as root on Unix.
#[cfg(unix)]
pub(crate) fn refuse_if_elevated() -> anyhow::Result<()> {
    if is_elevated() {
        anyhow::bail!("refusing to run HaloDaemon as root; run it as the desktop user");
    }
    Ok(())
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
/// the broker — see [`crate::infrastructure::drivers::transports::register_ops`]. Declining the
/// prompt is surfaced as an error by the caller (register-bus devices become
/// unavailable, HID/network keep working).
#[cfg(windows)]
pub(crate) fn spawn_broker_elevated(
    bootstrap_token: &str,
    coordinator_sid: &str,
    coordinator_session: u32,
) -> anyhow::Result<()> {
    use anyhow::{anyhow, Context};
    use halod_shared::win32::{quote_arg, run_elevated};

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
    let parameters = [
        format!("--bootstrap-token={bootstrap_token}"),
        format!("--coordinator-sid={coordinator_sid}"),
        format!("--coordinator-session={coordinator_session}"),
    ]
    .iter()
    .map(|argument| quote_arg(argument))
    .collect::<Vec<_>>()
    .join(" ");
    run_elevated(&broker, &parameters, dir)
        .map_err(|e| anyhow!("ShellExecuteExW(runas) for halod-broker: {e}"))?;
    log::info!("launched halod-broker with a UAC prompt");
    Ok(())
}
