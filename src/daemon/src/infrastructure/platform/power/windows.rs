// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]
//! Resume detection via a callback-mode suspend/resume notification, which —
//! unlike `WM_POWERBROADCAST` — needs no window and so works in the service.

use anyhow::{bail, Result};
use tokio::sync::mpsc::{self, UnboundedReceiver, UnboundedSender};
use windows::Win32::Foundation::{ERROR_SUCCESS, HANDLE};
use windows::Win32::System::Power::{
    PowerRegisterSuspendResumeNotification, DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS,
};
use windows::Win32::UI::WindowsAndMessaging::{
    DEVICE_NOTIFY_CALLBACK, PBT_APMRESUMEAUTOMATIC, PBT_APMRESUMESUSPEND,
};

pub fn resume_events() -> Result<UnboundedReceiver<()>> {
    let (tx, rx) = mpsc::unbounded_channel();
    let context = Box::into_raw(Box::new(tx));
    let params = Box::into_raw(Box::new(DEVICE_NOTIFY_SUBSCRIBE_PARAMETERS {
        Callback: Some(on_power_event),
        Context: context.cast(),
    }));

    let mut registration: *mut core::ffi::c_void = std::ptr::null_mut();
    let status = unsafe {
        PowerRegisterSuspendResumeNotification(
            DEVICE_NOTIFY_CALLBACK,
            HANDLE(params.cast()),
            &mut registration,
        )
    };
    if status != ERROR_SUCCESS {
        // Windows never saw the pointers, so reclaiming them is safe.
        drop(unsafe { Box::from_raw(params) });
        drop(unsafe { Box::from_raw(context) });
        bail!("PowerRegisterSuspendResumeNotification failed: {status:?}");
    }
    // Windows dereferences both for the life of the registration, which lasts
    // as long as the process; the registration handle is deliberately dropped.
    Ok(rx)
}

unsafe extern "system" fn on_power_event(
    context: *const core::ffi::c_void,
    event: u32,
    _setting: *const core::ffi::c_void,
) -> u32 {
    if event == PBT_APMRESUMEAUTOMATIC || event == PBT_APMRESUMESUSPEND {
        let tx = unsafe { &*context.cast::<UnboundedSender<()>>() };
        let _ = tx.send(());
    }
    ERROR_SUCCESS.0
}
