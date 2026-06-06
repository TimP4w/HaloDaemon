//! Debug / diagnostics types exchanged on the IPC channel.
//!
//! The daemon emits a `DebugInfo` snapshot on demand (`get_debug_info`); the UI
//! renders it in the per-device debug dialog and the Settings → Debug panel.
//! Everything is plain strings so future fields don't require protocol bumps.

use serde::{Deserialize, Serialize};

/// Process-wide diagnostics: OS, elevation, daemon build, helper presence.
#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct SystemDebugInfo {
    /// "linux", "windows"
    pub os: String, // TODO: make this an enum
    /// Free-form version string, e.g. kernel release on Linux or Windows build.
    pub os_version: String,
    /// `geteuid() == 0` on Unix; `IsUserAnAdmin` on Windows.
    pub running_elevated: bool,
    /// PawnIO driver detected on Windows; `None` outside Windows.
    pub pawnio_present: Option<bool>,
    /// `udev/60-halod.rules` reachable on Linux; `None` outside Linux.
    pub udev_rules_present: Option<bool>,
    pub daemon_version: String,
    /// `CARGO_PKG_NAME` plus features compiled in.
    pub daemon_build: String,
}

/// One enumerated HID interface from the OS, regardless of whether HaloDaemon
/// recognises it. Lets the debug UI explain "the device is plugged in, but no
/// descriptor matched".
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct HidEntryDebugInfo {
    pub vid: u16,
    pub pid: u16,
    pub path: String,
    pub interface: i32,
    #[serde(default)]
    pub serial: String,
    pub usage_page: u16,
    pub usage: u16,
    #[serde(default)]
    pub manufacturer: String,
    #[serde(default)]
    pub product: String,
    /// `Some(device_id)` when a HaloDaemon device claims this HID entry.
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub matched_device_id: Option<String>,
}

/// Per-device diagnostic record. Common fields are filled by the daemon from
/// the device's id/vendor/model; `fields` is a free-form list of extras
/// surfaced by the driver (wpid, host mode, firmware version…).
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DeviceDebugInfo {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub connected: bool,
    /// Transport layer the device is registered through: "hid", "smbus",
    /// "smbus_gpu", "usb_control", "hwmon", "child". "unknown" means
    /// the driver did not override `debug_transport()`.
    pub transport: String,
    /// Free-form key/value pairs, ordered as the driver returned them.
    #[serde(default)]
    pub fields: Vec<(String, String)>,
}

/// One SMBus controller discovered on the system, whether or not a HaloDaemon
/// driver claims it. Surfaces the "PawnIO is loaded but I see no DRAM RGB"
/// failure mode that no other panel shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbusBusDebugInfo {
    /// "chipset" (PawnIO on Windows / `/dev/i2c-*` on Linux) or "gpu" (NvAPI).
    pub kind: String,
    pub bus_number: u8,
    pub adapter_name: String,
    pub pci_vendor: u16,
    pub pci_device: u16,
}

#[derive(Debug, Clone, Default, Serialize, Deserialize)]
pub struct DebugInfo {
    pub system: SystemDebugInfo,
    pub devices: Vec<DeviceDebugInfo>,
    pub hid_entries: Vec<HidEntryDebugInfo>,
    #[serde(default)]
    pub smbus_buses: Vec<SmbusBusDebugInfo>,
}
