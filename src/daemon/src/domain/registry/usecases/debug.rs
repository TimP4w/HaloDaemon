// SPDX-License-Identifier: GPL-3.0-or-later
//! `get_debug_info` — snapshot of system + device + HID-bus state for the debug
//! UI. Extra fields are best-effort and absent where they don't apply (e.g.
//! PawnIO is Windows-only). Response goes back to the requesting client on the
//! `debug_info` channel — not broadcast.

use anyhow::Result;
use serde_json::json;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::infrastructure::drivers::transports::smbus::{BusInfo, SmBusTransport};
use crate::infrastructure::ipc::ClientHandle;
use halod_shared::debug_info::{
    DebugInfo, DependencyRule, DependencyStatus, DeviceDebugInfo, HidEntryDebugInfo, OsKind,
    SmbusBusDebugInfo, SmbusBusKind as DebugSmbusBusKind, SystemDebugInfo,
};

pub async fn get_debug_info(client: ClientHandle, app: Arc<AppState>) -> Result<()> {
    let info = collect(app).await;
    let value = serde_json::to_value(&info)?;
    client.send_json(&json!({ "type": "debug_info", "data": value }));
    Ok(())
}

async fn collect(app: Arc<AppState>) -> DebugInfo {
    let system_task = tokio::task::spawn_blocking(collect_system);
    let hid_task = tokio::task::spawn_blocking(enumerate_hid);
    let tracking_keys = snapshot_tracking_keys(&app).await;
    let system = system_task.await.unwrap_or_else(|_| collect_system());
    let hid_raw = hid_task.await.unwrap_or_default();

    let device_list = app.device_registry.read().await.clone();
    let mut devices = Vec::with_capacity(device_list.len());
    for d in &device_list {
        devices.push(build_device_debug(d.as_ref(), &tracking_keys, &hid_raw).await);
    }

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
    let dependencies = collect_dependencies(&app).await;

    DebugInfo {
        system,
        devices,
        hid_entries,
        smbus_buses,
        dependencies,
    }
}

/// Detect the external runtime dependencies relevant to the host the daemon
/// runs on. Only rows that apply to the current platform are emitted, so the
/// UI never shows e.g. a Windows-only helper on Linux.
async fn collect_dependencies(_app: &Arc<AppState>) -> Vec<DependencyStatus> {
    let ffmpeg = crate::domain::lcd::engine::video::ffmpeg_available();
    #[cfg_attr(not(target_os = "linux"), allow(unused_mut))]
    let mut deps = tokio::task::spawn_blocking(move || collect_binary_dependencies(ffmpeg))
        .await
        .unwrap_or_default();

    #[cfg(target_os = "linux")]
    if let Some(d) = gnome_extension_dependency().await {
        deps.push(d);
    }

    deps
}

struct HealthRule {
    id: DependencyRule,
    required: bool,
    platform: &'static str,
    check: Box<dyn Fn() -> bool>,
}

fn health_rules(ffmpeg_available: bool) -> Vec<HealthRule> {
    let mut rules = vec![HealthRule {
        id: DependencyRule::Ffmpeg,
        required: false,
        platform: "All",
        check: Box::new(move || ffmpeg_available),
    }];

    #[cfg(target_os = "linux")]
    rules.extend([
        HealthRule {
            id: DependencyRule::Pactl,
            required: false,
            platform: "Linux",
            check: Box::new(|| binary_on_path("pactl")),
        },
        HealthRule {
            id: DependencyRule::NvidiaSmi,
            required: false,
            platform: "Linux",
            check: Box::new(|| binary_on_path("nvidia-smi")),
        },
        HealthRule {
            id: DependencyRule::UdevRules,
            required: true,
            platform: "Linux",
            check: Box::new(|| udev_rules_present().unwrap_or(false)),
        },
        HealthRule {
            id: DependencyRule::I2cAccess,
            required: false,
            platform: "Linux",
            check: Box::new(i2c_reachable),
        },
        HealthRule {
            id: DependencyRule::HalodGroup,
            required: false,
            platform: "Linux",
            check: Box::new(halod_group_assigned),
        },
        HealthRule {
            id: DependencyRule::Powerprofilesctl,
            required: false,
            platform: "Linux",
            check: Box::new(|| binary_on_path("powerprofilesctl")),
        },
        HealthRule {
            id: DependencyRule::XdgDesktopPortal,
            required: false,
            platform: "Linux",
            check: Box::new(xdg_desktop_portal_present),
        },
    ]);

    #[cfg(target_os = "windows")]
    rules.extend([
        HealthRule {
            id: DependencyRule::Pawnio,
            required: false,
            platform: "Windows",
            check: Box::new(|| pawnio_present().unwrap_or(false)),
        },
        HealthRule {
            id: DependencyRule::Broker,
            required: false,
            platform: "Windows",
            check: Box::new(|| broker_service_present().unwrap_or(false)),
        },
    ]);

    rules
}

fn collect_binary_dependencies(ffmpeg_available: bool) -> Vec<DependencyStatus> {
    health_rules(ffmpeg_available)
        .into_iter()
        .map(|r| DependencyStatus {
            id: r.id,
            present: (r.check)(),
            required: r.required,
            platform: r.platform.to_string(),
            fix_variant: String::new(),
        })
        .collect()
}

/// Whether `name` resolves to a regular file on any `PATH` entry. Avoids
/// spawning the binary just to learn it exists.
#[cfg(target_os = "linux")]
fn binary_on_path(name: &str) -> bool {
    std::env::var_os("PATH")
        .map(|paths| std::env::split_paths(&paths).any(|dir| dir.join(name).is_file()))
        .unwrap_or(false)
}

/// Whether xdg-desktop-portal is installed, via its D-Bus activation file in any
/// XDG data dir. The portal lives in libexec (not on `PATH`), and the service
/// file is present whether or not a session is currently running it.
#[cfg(target_os = "linux")]
fn xdg_desktop_portal_present() -> bool {
    let dirs = std::env::var("XDG_DATA_DIRS")
        .unwrap_or_else(|_| "/usr/local/share:/usr/share".to_string());
    portal_service_in_dirs(&dirs)
}

#[cfg(target_os = "linux")]
fn portal_service_in_dirs(data_dirs: &str) -> bool {
    const SERVICE: &str = "dbus-1/services/org.freedesktop.portal.Desktop.service";
    std::env::split_paths(data_dirs).any(|dir| dir.join(SERVICE).is_file())
}

/// True when a `/dev/i2c-*` node exists and is read/write openable now (covers a
/// missing `i2c-dev` module or `halod` group membership).
#[cfg(target_os = "linux")]
fn i2c_reachable() -> bool {
    let Ok(entries) = std::fs::read_dir("/dev") else {
        return false;
    };
    entries.flatten().any(|e| {
        let name = e.file_name();
        let is_i2c = name
            .to_str()
            .and_then(|n| n.strip_prefix("i2c-"))
            .is_some_and(|rest| !rest.is_empty() && rest.bytes().all(|b| b.is_ascii_digit()));
        is_i2c
            && std::fs::OpenOptions::new()
                .read(true)
                .write(true)
                .open(e.path())
                .is_ok()
    })
}

/// Whether the daemon process has the `halod` group assigned. Group lookup uses
/// NSS rather than parsing `/etc/group`, so centrally managed users work too.
#[cfg(target_os = "linux")]
fn halod_group_assigned() -> bool {
    let name = c"halod";
    let Some(gid) = lookup_group_gid(name) else {
        return false;
    };
    // SAFETY: `getegid` has no preconditions.
    let effective = unsafe { libc::getegid() };
    if effective == gid {
        return true;
    }
    // SAFETY: a zero-sized query requires a null output pointer.
    let count = unsafe { libc::getgroups(0, std::ptr::null_mut()) };
    if count <= 0 {
        return false;
    }
    let mut groups = vec![0; count as usize];
    // SAFETY: `groups` has capacity for `count` group IDs.
    let written = unsafe { libc::getgroups(count, groups.as_mut_ptr()) };
    written >= 0 && gid_is_assigned(gid, effective, &groups[..written as usize])
}

#[cfg(target_os = "linux")]
fn gid_is_assigned(gid: libc::gid_t, effective: libc::gid_t, groups: &[libc::gid_t]) -> bool {
    effective == gid || groups.contains(&gid)
}

#[cfg(target_os = "linux")]
fn lookup_group_gid(name: &std::ffi::CStr) -> Option<libc::gid_t> {
    const MAX_BUFFER: usize = 1024 * 1024;
    // SAFETY: `sysconf` has no pointer arguments or additional preconditions.
    let initial = unsafe { libc::sysconf(libc::_SC_GETGR_R_SIZE_MAX) };
    let initial = usize::try_from(initial)
        .unwrap_or(16 * 1024)
        .clamp(1024, MAX_BUFFER);
    let mut buffer = vec![0_u8; initial];
    loop {
        let mut group = std::mem::MaybeUninit::<libc::group>::uninit();
        let mut result = std::ptr::null_mut();
        // SAFETY: all pointers reference live storage for the duration of the
        // call, and `buffer.len()` matches the writable buffer allocation.
        let rc = unsafe {
            libc::getgrnam_r(
                name.as_ptr(),
                group.as_mut_ptr(),
                buffer.as_mut_ptr().cast(),
                buffer.len(),
                &mut result,
            )
        };
        if rc == libc::ERANGE && buffer.len() < MAX_BUFFER {
            buffer.resize((buffer.len() * 2).min(MAX_BUFFER), 0);
            continue;
        }
        if rc != 0 || result.is_null() {
            return None;
        }
        // SAFETY: POSIX guarantees the group structure was initialized when
        // `getgrnam_r` succeeds and returns a non-null result.
        return Some(unsafe { group.assume_init() }.gr_gid);
    }
}

#[cfg(target_os = "linux")]
async fn gnome_extension_dependency() -> Option<DependencyStatus> {
    use crate::domain::profiles::observers::active_window::gnome_shell::{
        extension_status, ExtensionStatus,
    };
    let status = tokio::time::timeout(std::time::Duration::from_secs(5), extension_status())
        .await
        .ok()??; // None on timeout or non-GNOME session
    let (present, fix_variant) = match status {
        ExtensionStatus::Enabled => (true, ""),
        ExtensionStatus::Disabled => (false, "disabled"),
        ExtensionStatus::Missing => (false, "missing"),
    };
    Some(DependencyStatus {
        id: DependencyRule::GnomeExtension,
        present,
        required: false,
        platform: "GNOME".to_string(),
        fix_variant: fix_variant.to_string(),
    })
}

async fn enumerate_smbus() -> Vec<SmbusBusDebugInfo> {
    let (chipset, gpu) = SmBusTransport::enumerate_for_debug().await;
    let mut out: Vec<SmbusBusDebugInfo> = chipset
        .into_iter()
        .map(|b| smbus_to_wire(b, DebugSmbusBusKind::Chipset))
        .collect();
    out.extend(
        gpu.into_iter()
            .map(|b| smbus_to_wire(b, DebugSmbusBusKind::Gpu)),
    );
    out
}

fn smbus_to_wire(b: BusInfo, kind: DebugSmbusBusKind) -> SmbusBusDebugInfo {
    SmbusBusDebugInfo {
        kind,
        bus_number: b.bus_number,
        adapter_name: b.adapter_name,
        pci_vendor: b.pci_vendor,
        pci_device: b.pci_device,
    }
}

/// `(device_id, hid_key)` pairs for every device the HID transport is tracking.
/// The HID key has the shape
/// `vid:pid:serial` (all hex, serial may be empty).
async fn snapshot_tracking_keys(app: &Arc<AppState>) -> Vec<(String, String)> {
    use crate::application::state::HidTrackingEntry;
    let tracking = app.hid.snapshot().await;
    let mut out = Vec::new();
    for (key, entry) in &tracking {
        let HidTrackingEntry::Primary(arcs) = entry;
        for d in arcs {
            out.push((d.id().to_owned(), key.clone()));
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

async fn build_device_debug(
    device: &dyn crate::infrastructure::drivers::Device,
    tracking_keys: &[(String, String)],
    hid_entries: &[HidEntryDebugInfo],
) -> DeviceDebugInfo {
    let wire = device.serialize().await;
    let mut fields = source_fields(device);

    let hid_key = tracking_keys
        .iter()
        .find(|(id, _)| id == device.id())
        .map(|(_, k)| k.clone());

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

/// Ownership metadata is deliberately emitted for every device, not merely
/// Lua-backed devices. It makes a debug export answer the practical question
/// behind a duplicate report: which component is talking to this hardware?
fn source_fields(device: &dyn crate::infrastructure::drivers::Device) -> Vec<(String, String)> {
    use crate::domain::registry::identity::DeviceOrigin;

    let origin = device.conflict_origin();
    let mut fields = match (&origin, device.integration_id()) {
        (_, Some(id)) => vec![
            ("source_kind".into(), "integration_root".into()),
            ("integration_id".into(), id),
        ],
        (DeviceOrigin::Integration(id), None) => vec![
            ("source_kind".into(), "integration_controller".into()),
            ("integration_id".into(), id.clone()),
        ],
        (DeviceOrigin::Plugin(id), None) => vec![
            ("source_kind".into(), "plugin".into()),
            ("plugin_id".into(), id.clone()),
        ],
        (DeviceOrigin::Builtin, None) => vec![("source_kind".into(), "builtin".into())],
    };
    if let Some(plugin_id) = device.owning_plugin_id() {
        if !fields.iter().any(|(key, _)| key == "plugin_id") {
            fields.push(("plugin_id".into(), plugin_id));
        }
    }
    fields
}

pub(crate) fn enumerate_hid() -> Vec<HidEntryDebugInfo> {
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
            serial: d.serial_number().map(|s| s.to_string()).unwrap_or_default(),
            usage_page: d.usage_page(),
            usage: d.usage(),
            manufacturer: d
                .manufacturer_string()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            product: d
                .product_string()
                .map(|s| s.to_string())
                .unwrap_or_default(),
            matched_device_id: None,
        })
        .collect()
}

fn os_kind() -> OsKind {
    #[cfg(target_os = "linux")]
    {
        OsKind::Linux
    }
    #[cfg(target_os = "windows")]
    {
        OsKind::Windows
    }
    #[cfg(not(any(target_os = "linux", target_os = "windows")))]
    {
        OsKind::Linux
    }
}

fn collect_system() -> SystemDebugInfo {
    SystemDebugInfo {
        os: os_kind(),
        os_version: os_version(),
        running_elevated: is_elevated(),
        pawnio_present: pawnio_present(),
        udev_rules_present: udev_rules_present(),
        daemon_version: env!("CARGO_PKG_VERSION").to_string(),
        daemon_build: daemon_build_string(),
    }
}

fn daemon_build_string() -> String {
    concat!(env!("CARGO_PKG_NAME"), " ", env!("CARGO_PKG_VERSION")).to_string()
}

#[cfg(target_os = "linux")]
fn os_version() -> String {
    std::fs::read_to_string("/proc/sys/kernel/osrelease")
        .map(|s| s.trim().to_string())
        .unwrap_or_default()
}

#[cfg(target_os = "windows")]
fn os_version() -> String {
    // `cmd /c ver` reports the real build past Windows 8 (unlike `GetVersionEx`).
    match std::process::Command::new("cmd")
        .args(["/c", "ver"])
        .output()
    {
        Ok(out) if out.status.success() => String::from_utf8_lossy(&out.stdout).trim().to_string(),
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
    Some(halod_hwaccess::pawnio::installation_present())
}

#[cfg(not(windows))]
fn pawnio_present() -> Option<bool> {
    None
}

/// Whether the elevated `HalodBroker` service is registered with the SCM. Since
/// the privilege split the worker never runs elevated itself — chipset SMBus /
/// PawnIO access goes through this on-demand service, which the worker starts
/// without a UAC prompt. If it isn't installed, register-bus access falls back
/// to a per-session UAC prompt (a dev run), so its absence is what limits
/// chipset RGB/sensors — not the worker's own elevation. `None` off Windows.
#[cfg(windows)]
fn broker_service_present() -> Option<bool> {
    use halod_hwaccess::proto::BROKER_SERVICE_NAME;
    use windows_service::service::ServiceAccess;
    use windows_service::service_manager::{ServiceManager, ServiceManagerAccess};

    let manager =
        ServiceManager::local_computer(None::<&str>, ServiceManagerAccess::CONNECT).ok()?;
    Some(
        manager
            .open_service(BROKER_SERVICE_NAME, ServiceAccess::QUERY_STATUS)
            .is_ok(),
    )
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
    fn source_fields_identify_builtin_plugin_and_integration_roots() {
        use crate::test_support::MockDevice;

        assert!(source_fields(&MockDevice::new("builtin"))
            .contains(&("source_kind".into(), "builtin".into())));
        assert!(
            source_fields(&MockDevice::new("plugin").with_owning_plugin_id("foo"))
                .contains(&("plugin_id".into(), "foo".into()))
        );
        assert!(
            source_fields(&MockDevice::new("root").with_integration_id("openrgb"))
                .contains(&("integration_id".into(), "openrgb".into()))
        );
    }

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
        assert!(!matches_hid_key("046d:c095:XYZ", &entry));
        let no_serial = HidEntryDebugInfo {
            serial: String::new(),
            ..entry.clone()
        };
        assert!(matches_hid_key("046d:c095:", &no_serial));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn binary_on_path_finds_common_tool_and_rejects_garbage() {
        // `sh` is guaranteed on PATH in any POSIX environment (incl. CI).
        assert!(binary_on_path("sh"));
        assert!(!binary_on_path("halod-nonexistent-binary-xyz"));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn binary_dependencies_includes_platform_rows() {
        use DependencyRule::*;
        let deps = collect_binary_dependencies(true);
        assert!(deps.iter().any(|d| d.id == Ffmpeg && d.present));
        let deps_false = collect_binary_dependencies(false);
        assert!(deps_false.iter().any(|d| d.id == Ffmpeg && !d.present));
        assert!(deps.iter().any(|d| d.id == Pactl));
        assert!(deps.iter().any(|d| d.id == UdevRules && d.required));
        assert!(deps.iter().any(|d| d.id == I2cAccess));
        assert!(deps.iter().any(|d| d.id == HalodGroup));
        assert!(deps.iter().any(|d| d.id == Powerprofilesctl));
        assert!(deps.iter().any(|d| d.id == XdgDesktopPortal));
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn portal_service_in_dirs_detects_service_file() {
        let dir = std::env::temp_dir().join("halod-xdp-test");
        let svc = dir.join("dbus-1/services");
        std::fs::create_dir_all(&svc).unwrap();
        let file = svc.join("org.freedesktop.portal.Desktop.service");
        let _ = std::fs::remove_file(&file);

        let dirs = format!("/nonexistent-halod-xyz:{}", dir.display());
        assert!(!portal_service_in_dirs(&dirs));
        std::fs::write(&file, b"[D-BUS Service]").unwrap();
        assert!(portal_service_in_dirs(&dirs));

        let _ = std::fs::remove_dir_all(&dir);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn group_assignment_accepts_effective_or_supplementary_gid() {
        assert!(gid_is_assigned(42, 42, &[]));
        assert!(gid_is_assigned(42, 1, &[2, 42]));
        assert!(!gid_is_assigned(42, 1, &[2, 3]));
    }
}
