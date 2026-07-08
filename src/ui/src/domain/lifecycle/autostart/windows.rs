// SPDX-License-Identifier: GPL-3.0-or-later
//! Windows autostart via an `HKCU\…\Run` registry value.
//!
//! The value points at the current executable launched with `--background`, so
//! it starts hidden to the tray at sign-in. This is the single in-app-owned
//! mechanism; the installer no longer creates a Startup-folder shortcut.

use std::io;

use windows::core::{w, PCWSTR};
use windows::Win32::Foundation::{ERROR_FILE_NOT_FOUND, ERROR_SUCCESS};
use windows::Win32::System::Registry::{
    RegDeleteKeyValueW, RegGetValueW, RegSetKeyValueW, HKEY_CURRENT_USER, REG_SZ, RRF_RT_REG_SZ,
};

const RUN_KEY: PCWSTR = w!("Software\\Microsoft\\Windows\\CurrentVersion\\Run");
const VALUE_NAME: PCWSTR = w!("HaloDaemon");

/// The command `set_enabled(true)` registers, shared with `is_enabled`.
fn expected_command(exe: &std::path::Path) -> String {
    format!(
        "\"{}\" {}",
        exe.to_string_lossy(),
        halod_shared::lifecycle::BACKGROUND_ARG
    )
}

fn read_run_value() -> Option<String> {
    let mut size: u32 = 0;
    // SAFETY: HKEY_CURRENT_USER is a predefined handle; RUN_KEY/VALUE_NAME are
    // `w!`-embedded NUL-terminated statics; no data buffer is written here.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            None,
            Some(&mut size),
        )
    };
    if status != ERROR_SUCCESS || size == 0 {
        return None;
    }

    let mut buf: Vec<u16> = vec![0; size.div_ceil(2) as usize];
    let mut actual_size = size;
    // SAFETY: `buf` is sized from the prior call's reported byte count and
    // outlives this call; `actual_size` caps the write to its capacity.
    let status = unsafe {
        RegGetValueW(
            HKEY_CURRENT_USER,
            RUN_KEY,
            VALUE_NAME,
            RRF_RT_REG_SZ,
            None,
            Some(buf.as_mut_ptr() as *mut core::ffi::c_void),
            Some(&mut actual_size),
        )
    };
    if status != ERROR_SUCCESS {
        return None;
    }
    while buf.last() == Some(&0) {
        buf.pop();
    }
    Some(String::from_utf16_lossy(&buf))
}

pub fn is_enabled() -> bool {
    let Ok(exe) = std::env::current_exe() else {
        return false;
    };
    read_run_value().is_some_and(|v| v == expected_command(&exe))
}

pub fn set_enabled(enable: bool) -> io::Result<()> {
    if enable {
        let exe = std::env::current_exe()?;
        let command = expected_command(&exe);
        let wide: Vec<u16> = command.encode_utf16().chain(std::iter::once(0)).collect();
        let bytes = (wide.len() * std::mem::size_of::<u16>()) as u32;
        // SAFETY: `wide` is NUL-terminated UTF-16 and outlives this call;
        // `bytes` is its exact byte length.
        let status = unsafe {
            RegSetKeyValueW(
                HKEY_CURRENT_USER,
                RUN_KEY,
                VALUE_NAME,
                REG_SZ.0,
                Some(wide.as_ptr() as *const core::ffi::c_void),
                bytes,
            )
        };
        win_result(status)
    } else {
        // SAFETY: HKEY_CURRENT_USER is a predefined handle; RUN_KEY/VALUE_NAME
        // are `w!`-embedded NUL-terminated statics.
        let status = unsafe { RegDeleteKeyValueW(HKEY_CURRENT_USER, RUN_KEY, VALUE_NAME) };
        // Already-absent is success (idempotent disable).
        if status == ERROR_FILE_NOT_FOUND {
            return Ok(());
        }
        win_result(status)
    }
}

fn win_result(status: windows::Win32::Foundation::WIN32_ERROR) -> io::Result<()> {
    if status == ERROR_SUCCESS {
        Ok(())
    } else {
        Err(io::Error::from_raw_os_error(status.0 as i32))
    }
}
