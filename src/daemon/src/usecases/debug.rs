//! `get_debug_info` — produce a snapshot of system + device + HID-bus state for
//! the debug UI. Cross-platform: extra fields are best-effort and absent on
//! platforms where they don't apply (e.g. PawnIO is Windows-only).
//!
//! Output shape mirrors `halod_protocol::debug_info::DebugInfo`. The
//! response goes straight back to the requesting client on the `debug_info`
//! channel — it is not broadcast.

use anyhow::Result;
use serde_json::{json, Value};
use std::sync::Arc;

use crate::drivers::transports::smbus::{BusInfo, SmBusTransport};
use crate::ipc::ClientHandle;
use crate::state::AppState;
use halod_protocol::debug_info::{
    DebugInfo, DeviceDebugInfo, HidEntryDebugInfo, SmbusBusDebugInfo, SystemDebugInfo,
};

pub async fn get_debug_info(_msg: Value, client: ClientHandle, app: Arc<AppState>) -> Result<()> {
    let info = collect(app).await;
    let value = serde_json::to_value(&info)?;
    client.send_json(&json!({ "type": "debug_info", "data": value }));
    Ok(())
}

async fn collect(app: Arc<AppState>) -> DebugInfo {
    // `collect_system` shells out to `cmd /c ver` on Windows and
    // `enumerate_hid` can take 50–200 ms walking the hidapi device list — both
    // run on `spawn_blocking` so they don't stall the IPC executor while the
    // debug snapshot is being built.
    let system_task = tokio::task::spawn_blocking(collect_system);
    let hid_task = tokio::task::spawn_blocking(enumerate_hid);
    let tracking_keys = snapshot_tracking_keys(&app).await;
    let system = system_task.await.unwrap_or_default();
    let hid_raw = hid_task.await.unwrap_or_default();

    let device_list = app.devices.lock().await.clone();
    let mut devices = Vec::with_capacity(device_list.len());
    for d in &device_list {
        devices.push(build_device_debug(d.as_ref(), &tracking_keys, &hid_raw).await);
    }

    // Cross-reference: mark HID entries claimed by a registered device.
    let hid_entries: Vec<HidEntryDebugInfo> = hid_raw
        .into_iter()
        .map(|mut e| {
            e.matched_device_id = tracking_keys
                .iter()
                .find(|(_, key)| matches_hid_key(key, &e))
                .map(|(id, _)| id.clone());
            e
        })
        .collect();

    let smbus_buses = enumerate_smbus().await;

    DebugInfo {
        system,
        devices,
        hid_entries,
        smbus_buses,
    }
}

async fn enumerate_smbus() -> Vec<SmbusBusDebugInfo> {
    let (chipset, gpu) = SmBusTransport::enumerate_for_debug().await;
    let mut out: Vec<SmbusBusDebugInfo> = chipset
        .into_iter()
        .map(|b| smbus_to_wire(b, "chipset"))
        .collect();
    out.extend(gpu.into_iter().map(|b| smbus_to_wire(b, "gpu")));
    out
}

fn smbus_to_wire(b: BusInfo, kind: &str) -> SmbusBusDebugInfo {
    SmbusBusDebugInfo {
        kind: kind.to_string(),
        bus_number: b.bus_number,
        adapter_name: b.adapter_name,
        pci_vendor: b.pci_vendor,
        pci_device: b.pci_device,
    }
}

// ── Tracking-map snapshot ────────────────────────────────────────────────────

/// `(device_id, hid_key)` pairs for every device the HID transport is tracking,
/// in either Primary or WiredOverride form. The HID key has the shape
/// `vid:pid:serial` (all hex, serial may be empty).
async fn snapshot_tracking_keys(app: &Arc<AppState>) -> Vec<(String, String)> {
    use crate::state::HidTrackingEntry;
    let tracking = app.hid_device_tracking.lock().await;
    let mut out = Vec::new();
    for (key, entry) in tracking.iter() {
        match entry {
            HidTrackingEntry::Primary(arcs) => {
                for d in arcs {
                    out.push((d.id(), key.clone()));
                }
            }
            HidTrackingEntry::WiredOverride(d) => {
                out.push((d.id(), key.clone()));
            }
        }
    }
    out
}

/// Returns true when `key` (a `vid:pid:serial` tracking key) refers to the same
/// HID interface as `entry`.
fn matches_hid_key(key: &str, entry: &HidEntryDebugInfo) -> bool {
    let mut parts = key.splitn(3, ':');
    let Some(vid) = parts.next().and_then(|s| u16::from_str_radix(s, 16).ok()) else {
        return false;
    };
    let Some(pid) = parts.next().and_then(|s| u16::from_str_radix(s, 16).ok()) else {
        return false;
    };
    let serial = parts.next().unwrap_or("");
    vid == entry.vid && pid == entry.pid && serial == entry.serial
}

// ── Per-device record ────────────────────────────────────────────────────────

async fn build_device_debug(
    device: &dyn crate::drivers::Device,
    tracking_keys: &[(String, String)],
    hid_entries: &[HidEntryDebugInfo],
) -> DeviceDebugInfo {
    let wire = device.serialize().await;
    let mut fields = Vec::new();

    // Resolve the HID key for this device, if any, then surface the matching
    // hidapi entry's transport details so the UI shows path/interface/usage
    // for every HID-backed device without each driver re-implementing it.
    let hid_key = tracking_keys
        .iter()
        .find(|(id, _)| id == &device.id())
        .map(|(_, k)| k.clone());

    // Driver-declared transport wins; HID-tracked devices implicitly get "hid"
    // (with the matching hidapi entry surfaced as fields). Anything else means
    // the driver forgot to override `debug_transport()` — surface "unknown" so
    // the gap is visible in the debug UI rather than silently mislabelled.
    let (transport, hid_match) = match device.debug_transport() {
        Some(t) => (t.to_string(), None),
        None => match hid_key.as_deref() {
            Some(key) => {
                let hid = hid_entries.iter().find(|e| matches_hid_key(key, e));
                ("hid".to_string(), hid)
            }
            None => ("unknown".to_string(), None),
        },
    };

    if let Some(h) = hid_match {
        fields.push(("vid".to_string(), format!("{:04x}", h.vid)));
        fields.push(("pid".to_string(), format!("{:04x}", h.pid)));
        fields.push(("path".to_string(), h.path.clone()));
        fields.push(("interface".to_string(), h.interface.to_string()));
        fields.push((
            "usage".to_string(),
            format!("{:04x}:{:04x}", h.usage_page, h.usage),
        ));
        if !h.serial.is_empty() {
            fields.push(("hid_serial".to_string(), h.serial.clone()));
        }
        if !h.manufacturer.is_empty() {
            fields.push(("hid_manufacturer".to_string(), h.manufacturer.clone()));
        }
        if !h.product.is_empty() {
            fields.push(("hid_product".to_string(), h.product.clone()));
        }
    }

    if let Some(serial) = &wire.serial_number {
        fields.push(("serial_number".to_string(), serial.clone()));
    }

    // Driver-specific extras (wpid, host mode, firmware, …).
    for (k, v) in device.debug_info_extra() {
        fields.push((k, v));
    }

    DeviceDebugInfo {
        id: wire.id,
        name: wire.name,
        vendor: wire.vendor,
        model: wire.model,
        connected: wire.connected,
        transport,
        fields,
    }
}

// ── HID enumeration ──────────────────────────────────────────────────────────

fn enumerate_hid() -> Vec<HidEntryDebugInfo> {
    let api = match hidapi::HidApi::new() {
        Ok(a) => a,
        Err(e) => {
            log::warn!("[debug] HidApi::new failed: {e}");
            return Vec::new();
        }
    };
    api.device_list()
        .map(|d| HidEntryDebugInfo {
            vid: d.vendor_id(),
            pid: d.product_id(),
            path: d.path().to_string_lossy().into_owned(),
            interface: d.interface_number(),
            serial: d
                .serial_number()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            usage_page: d.usage_page(),
            usage: d.usage(),
            manufacturer: d
                .manufacturer_string()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            product: d.product_string().map(|s| s.to_string()).unwrap_or_default(),
            matched_device_id: None,
        })
        .collect()
}

// ── System info ──────────────────────────────────────────────────────────────

fn collect_system() -> SystemDebugInfo {
    SystemDebugInfo {
        os: std::env::consts::OS.to_string(),
        os_version: os_version(),
        running_elevated: is_elevated(),
        pawnio_present: pawnio_present(),
        udev_rules_present: udev_rules_present(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        daemon_build: daemon_build_string(),
    }
}

fn daemon_build_string() -> String {
    env!("CARGO_PKG_NAME").to_string()
}

#[cfg(target_os = "linux")]
fn os_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "windows")]
fn os_version() -> String {
    // Pull build info from `cmd /c ver` — it's cheap, has no extra deps, and
    // (unlike `GetVersionEx`) reports the real build past Windows 8. If the
    // process can't spawn `cmd`, fall back to an empty string.
    match std::process::Command::new("cmd").args(["/c", "ver"]).output() {
        Ok(out) if out.status.success() => {
            String::from_utf8_lossy(&out.stdout).trim().to_string()
        }
        _ => String::new(),
    }
}

#[cfg(not(any(target_os = "linux", target_os = "windows")))]
fn os_version() -> String {
    String::new()
}

#[cfg(unix)]
fn is_elevated() -> bool {
    // SAFETY: geteuid is always safe; it takes no args and never fails.
    unsafe { libc::geteuid() == 0 }
}

#[cfg(windows)]
fn is_elevated() -> bool {
    use windows::Win32::Foundation::{CloseHandle, HANDLE};
    use windows::Win32::Security::{
        GetTokenInformation, TokenElevation, TOKEN_ELEVATION, TOKEN_QUERY,
    };
    use windows::Win32::System::Threading::{GetCurrentProcess, OpenProcessToken};

    // SAFETY: standard token-query sequence; handle closed before return.
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

#[cfg(not(any(unix, windows)))]
fn is_elevated() -> bool {
    false
}

/// Whether the PawnIO kernel driver helper DLL is reachable. Only meaningful
/// on Windows; returns `None` elsewhere so the UI hides the row.
#[cfg(windows)]
fn pawnio_present() -> Option<bool> {
    let candidates = [
        "PawnIOLib.dll".to_string(),
        r"C:\Program Files\PawnIO\PawnIOLib.dll".to_string(),
    ];
    let mut paths: Vec<String> = candidates.to_vec();
    if let Ok(pf) = std::env::var("ProgramFiles") {
        paths.push(format!(r"{pf}\PawnIO\PawnIOLib.dll"));
    }
    // SAFETY: libloading::Library::new on a missing DLL just returns Err.
    let found = paths
        .iter()
        .any(|p| unsafe { libloading::Library::new(p).is_ok() });
    Some(found)
}

#[cfg(not(windows))]
fn pawnio_present() -> Option<bool> {
    None
}

/// Whether `60-halod.rules` is installed in one of the standard udev
/// directories. `None` outside Linux.
#[cfg(target_os = "linux")]
fn udev_rules_present() -> Option<bool> {
    let candidates = [
        "/etc/udev/rules.d/60-halod.rules",
        "/lib/udev/rules.d/60-halod.rules",
        "/usr/lib/udev/rules.d/60-halod.rules",
        "/run/udev/rules.d/60-halod.rules",
    ];
    Some(candidates.iter().any(|p| std::path::Path::new(p).exists()))
}

#[cfg(not(target_os = "linux"))]
fn udev_rules_present() -> Option<bool> {
    None
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn matches_hid_key_pairs_vid_pid_serial() {
        let entry = HidEntryDebugInfo {
            vid: 0x046D,
            pid: 0xC095,
            path: "p".into(),
            interface: 0,
            serial: "ABC".into(),
            usage_page: 0,
            usage: 0,
            manufacturer: String::new(),
            product: String::new(),
            matched_device_id: None,
        };
        assert!(matches_hid_key("046d:c095:ABC", &entry));
        // Different serial — no match.
        assert!(!matches_hid_key("046d:c095:XYZ", &entry));
        // Empty serial entry against an empty-serial tracking key.
        let no_serial = HidEntryDebugInfo {
            serial: String::new(),
            ..entry.clone()
        };
        assert!(matches_hid_key("046d:c095:", &no_serial));
    }

}
