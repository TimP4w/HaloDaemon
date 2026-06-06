#![cfg(windows)]

use std::cell::RefCell;
use std::ffi::OsString;
use std::os::windows::ffi::OsStringExt;
use std::path::Path;
use tokio::sync::mpsc;
use windows::Win32::Foundation::{CloseHandle, HWND};
use windows::Win32::System::Threading::{
    OpenProcess, QueryFullProcessImageNameW, PROCESS_NAME_WIN32,
    PROCESS_QUERY_LIMITED_INFORMATION,
};
use windows::Win32::UI::Accessibility::{SetWinEventHook, UnhookWinEvent, HWINEVENTHOOK};
use windows::Win32::UI::WindowsAndMessaging::{
    DispatchMessageW, GetMessageW, GetWindowThreadProcessId, TranslateMessage,
    EVENT_SYSTEM_FOREGROUND, MSG, WINEVENT_OUTOFCONTEXT, WINEVENT_SKIPOWNPROCESS,
};

use super::FocusEvent;

thread_local! {
    static BRIDGE: RefCell<Option<std::sync::mpsc::SyncSender<FocusEvent>>> = RefCell::new(None);
}

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
    BRIDGE.with(|cell| {
        if let Some(tx) = &*cell.borrow() {
            match tx.send(event) {
                Ok(()) => log::trace!("[FocusWatcher/Windows] event sent to bridge"),
                Err(e) => log::warn!("[FocusWatcher/Windows] bridge send failed: {e}"),
            }
        } else {
            log::warn!("[FocusWatcher/Windows] BRIDGE is None in hook_proc thread");
        }
    });
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
            .map(|s| super::normalize_name(s))
            .unwrap_or_default();
        if basename.is_empty() {
            log::debug!("[FocusWatcher/Windows] resolve_foreground: empty basename → NoApp");
            FocusEvent::NoApp
        } else {
            log::debug!(
                "[FocusWatcher/Windows] resolve_foreground: AppFocused process_name={}",
                basename
            );
            FocusEvent::AppFocused { process_name: basename }
        }
    }
}

pub async fn spawn() -> anyhow::Result<mpsc::Receiver<FocusEvent>> {
    log::info!("[FocusWatcher/Windows] spawn() called — initializing WinEvent hook");
    let (bridge_tx, bridge_rx) = std::sync::mpsc::sync_channel::<FocusEvent>(32);
    let (tx, rx) = mpsc::channel::<FocusEvent>(32);

    std::thread::spawn(move || {
        log::info!("[FocusWatcher/Windows] hook thread started");
        BRIDGE.with(|cell| *cell.borrow_mut() = Some(bridge_tx));
        log::debug!("[FocusWatcher/Windows] BRIDGE installed on hook thread");

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
                    log::info!("[FocusWatcher/Windows] GetMessageW returned WM_QUIT — exiting loop");
                    break;
                }
                if ret.0 == -1 {
                    log::error!(
                        "[FocusWatcher/Windows] GetMessageW error — exiting loop"
                    );
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
    tokio::task::spawn_blocking(move || {
        log::debug!("[FocusWatcher/Windows] bridge forwarder task started");
        while let Ok(event) = bridge_rx.recv() {
            if tx.blocking_send(event).is_err() {
                log::warn!("[FocusWatcher/Windows] tokio tx closed — bridge forwarder exiting");
                break;
            }
        }
        log::warn!("[FocusWatcher/Windows] bridge forwarder exited (bridge_rx closed)");
    });

    Ok(rx)
}
