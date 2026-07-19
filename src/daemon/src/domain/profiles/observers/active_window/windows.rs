// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(windows)]

use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use std::sync::atomic::{AtomicU32, AtomicU64, Ordering};
use std::sync::{Arc, Mutex, OnceLock};
use tokio::sync::mpsc;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{
    GetCurrentThreadId, OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetWindowThreadProcessId, PostThreadMessageW, TranslateMessage,
    EVENT_SYSTEM_FOREGROUND, MSG, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS, WM_QUIT,
};

use super::FocusEvent;

/// Global bridge — `WINEVENT_OUTOFCONTEXT` delivers on a system thread, so
/// `OnceLock<Mutex<…>>` replaces `thread_local!`.
static BRIDGE: OnceLock<Mutex<Option<std::sync::mpsc::SyncSender<FocusEvent>>>> = OnceLock::new();
static BRIDGE_GENERATION: AtomicU64 = AtomicU64::new(0);

unsafe extern "system" fn hook_proc(
    _hook: HWINEVENTHOOK,
    event: u32,
    hwnd: HWND,
    id_object: i32,
    id_child: i32,
    _id_event_thread: u32,
    _time: u32,
) {
    log::trace!(
        "[FocusWatcher/Windows] hook_proc fired: event=0x{:x} hwnd={:?} id_object={} id_child={}",
        event,
        hwnd.0,
        id_object,
        id_child
    );
    let event = resolve_foreground(hwnd);
    if let Some(guard) = BRIDGE.get() {
        if let Ok(lock) = guard.lock() {
            if let Some(tx) = &*lock {
                match tx.send(event) {
                    Ok(()) => log::trace!("[FocusWatcher/Windows] event sent to bridge"),
                    Err(e) => log::warn!("[FocusWatcher/Windows] bridge send failed: {e}"),
                }
            } else {
                log::warn!("[FocusWatcher/Windows] BRIDGE is None in hook_proc thread");
            }
        }
    } else {
        log::warn!("[FocusWatcher/Windows] BRIDGE not yet initialized in hook_proc thread");
    }
}

fn resolve_foreground(hwnd: HWND) -> FocusEvent {
    if hwnd.is_invalid() {
        log::debug!("[FocusWatcher/Windows] resolve_foreground: hwnd invalid → NoApp");
        return FocusEvent::NoApp;
    }
    unsafe {
        let mut pid: u32 = 0;
        GetWindowThreadProcessId(hwnd, Some(&mut pid));
        if pid == 0 {
            log::debug!("[FocusWatcher/Windows] resolve_foreground: pid=0 → NoApp");
            return FocusEvent::NoApp;
        }
        let handle = match OpenProcess(PROCESS_QUERY_LIMITED_INFORMATION, false, pid) {
            Ok(h) => h,
            Err(e) => {
                log::debug!(
                    "[FocusWatcher/Windows] resolve_foreground: OpenProcess(pid={}) failed: {e} → NoApp",
                    pid
                );
                return FocusEvent::NoApp;
            }
        };
        let mut buf = vec![0u16; 1024];
        let mut size = buf.len() as u32;
        let ok = QueryFullProcessImageNameW(
            handle,
            PROCESS_NAME_WIN32,
            windows::core::PWSTR(buf.as_mut_ptr()),
            &mut size,
        );
        let _ = CloseHandle(handle);
        if let Err(e) = &ok {
            log::debug!(
                "[FocusWatcher/Windows] resolve_foreground: QueryFullProcessImageNameW failed: {e} → NoApp"
            );
            return FocusEvent::NoApp;
        }
        if size == 0 {
            log::debug!(
                "[FocusWatcher/Windows] resolve_foreground: QueryFullProcessImageNameW returned size=0 → NoApp"
            );
            return FocusEvent::NoApp;
        }
        let path = OsString::from_wide(&buf[..size as usize]);
        let basename = Path::new(&path)
            .file_name()
            .and_then(|f| f.to_str())
            .map(super::normalize_name)
            .unwrap_or_default();
        if basename.is_empty() {
            log::debug!("[FocusWatcher/Windows] resolve_foreground: empty basename → NoApp");
            FocusEvent::NoApp
        } else {
            log::debug!(
                "[FocusWatcher/Windows] resolve_foreground: AppFocused process_name={}",
                basename
            );
            FocusEvent::AppFocused {
                process_name: basename,
            }
        }
    }
}

pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    log::info!("[FocusWatcher/Windows] spawn() called, initializing WinEvent hook");
    let (bridge_tx, bridge_rx) = std::sync::mpsc::sync_channel::<FocusEvent>(32);
    let (tx, rx) = mpsc::channel::<FocusEvent>(32);
    let generation = BRIDGE_GENERATION.fetch_add(1, Ordering::AcqRel) + 1;
    *BRIDGE.get_or_init(|| Mutex::new(None)).lock().unwrap() = Some(bridge_tx);
    let hook_thread_id = Arc::new(AtomicU32::new(0));
    let hook_thread_id_for_hook = Arc::clone(&hook_thread_id);

    std::thread::spawn(move || {
        log::info!("[FocusWatcher/Windows] hook thread started");
        hook_thread_id_for_hook.store(unsafe { GetCurrentThreadId() }, Ordering::Release);
        let still_active = BRIDGE_GENERATION.load(Ordering::Acquire) == generation
            && BRIDGE
                .get()
                .and_then(|bridge| bridge.lock().ok())
                .is_some_and(|bridge| bridge.is_some());
        if !still_active {
            return;
        }

        unsafe {
            let hook = SetWinEventHook(
                EVENT_SYSTEM_FOREGROUND,
                EVENT_SYSTEM_FOREGROUND,
                None,
                Some(hook_proc),
                0,
                0,
                WINEVENT_OUTOFCONTEXT | WINEVENT_SKIPOWNPROCESS,
            );
            if hook.is_invalid() {
                log::error!("[FocusWatcher/Windows] SetWinEventHook failed (hook handle invalid)");
                return;
            }
            log::info!(
                "[FocusWatcher/Windows] SetWinEventHook OK (hook={:?}); entering message loop",
                hook.0
            );
            let mut msg = MSG::default();
            loop {
                let ret = GetMessageW(&mut msg, None, 0, 0);
                log::trace!(
                    "[FocusWatcher/Windows] GetMessageW returned {} (msg={:#x})",
                    ret.0,
                    msg.message
                );
                if ret.0 == 0 {
                    log::info!("[FocusWatcher/Windows] GetMessageW returned WM_QUIT, exiting loop");
                    break;
                }
                if ret.0 == -1 {
                    log::error!("[FocusWatcher/Windows] GetMessageW error, exiting loop");
                    break;
                }
                let _ = TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            log::warn!("[FocusWatcher/Windows] message loop exited; unhooking");
            let _ = UnhookWinEvent(hook);
        }
    });

    // Bridge: forward from std::sync::mpsc to tokio mpsc
    let bridge = tokio::task::spawn_blocking(move || {
        log::debug!("[FocusWatcher/Windows] bridge forwarder task started");
        loop {
            if tx.is_closed() {
                break;
            }
            match bridge_rx.recv_timeout(std::time::Duration::from_millis(100)) {
                Ok(event) if tx.blocking_send(event).is_err() => break,
                Ok(_) | Err(std::sync::mpsc::RecvTimeoutError::Timeout) => {}
                Err(std::sync::mpsc::RecvTimeoutError::Disconnected) => break,
            }
        }
        if BRIDGE_GENERATION.load(Ordering::Acquire) == generation {
            if let Some(bridge) = BRIDGE.get() {
                *bridge.lock().unwrap() = None;
            }
        }
        let thread_id = hook_thread_id.load(Ordering::Acquire);
        if thread_id != 0 {
            unsafe {
                let _ =
                    PostThreadMessageW(thread_id, WM_QUIT, Default::default(), Default::default());
            }
        }
        log::warn!("[FocusWatcher/Windows] bridge forwarder exited (bridge_rx closed)");
    });
    tokio::spawn(async move {
        if let Err(e) = bridge.await {
            log::warn!("[FocusWatcher/Windows] bridge forwarder panicked: {e}");
        }
    });

    Ok(rx)
}
