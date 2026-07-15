//! Debug / diagnostics types exchanged on the IPC channel.
//!
//! The daemon emits a `DebugInfo` snapshot on demand (`get_debug_info`); the UI
//! renders it in the per-device debug dialog and the Settings → Debug panel.

use serde::{Deserialize, Serialize};

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum OsKind {
    Linux,
    Windows,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum SmbusBusKind {
    Chipset,
    Gpu,
}

/// Process-wide diagnostics: OS, elevation, daemon build, helper presence.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SystemDebugInfo {
    pub os: OsKind,
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
    /// "smbus_gpu", "usb", "hwmon", "child". "unknown" means
    /// the driver did not override `debug_transport()`.
    pub transport: String,
    /// Free-form key/value pairs, ordered as the driver returned them.
    #[serde(default)]
    pub fields: Vec<(String, String)>,
}

/// A well-known dependency the daemon can report on. The `snake_case`
/// `Serialize` output doubles as the `depcheck.rules.<key>.*` i18n key segment
/// the UI looks up (`DependencyRule::i18n_key`), so the daemon emits a typed
/// value and the UI matches it exhaustively — no free-text slug can drift.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum DependencyRule {
    Ffmpeg,
    Pactl,
    NvidiaSmi,
    UdevRules,
    I2cAccess,
    HalodGroup,
    Powerprofilesctl,
    XdgDesktopPortal,
    Pawnio,
    Broker,
    GnomeExtension,
}

impl DependencyRule {
    /// Every variant, so tests and callers can iterate without a hand-copied
    /// list that could drift from the enum.
    pub const ALL: &'static [DependencyRule] = &[
        DependencyRule::Ffmpeg,
        DependencyRule::Pactl,
        DependencyRule::NvidiaSmi,
        DependencyRule::UdevRules,
        DependencyRule::I2cAccess,
        DependencyRule::HalodGroup,
        DependencyRule::Powerprofilesctl,
        DependencyRule::XdgDesktopPortal,
        DependencyRule::Pawnio,
        DependencyRule::Broker,
        DependencyRule::GnomeExtension,
    ];

    /// The `depcheck.rules.<key>.*` i18n key segment for this rule.
    pub fn i18n_key(self) -> &'static str {
        match self {
            DependencyRule::Ffmpeg => "ffmpeg",
            DependencyRule::Pactl => "pactl",
            DependencyRule::NvidiaSmi => "nvidia_smi",
            DependencyRule::UdevRules => "udev_rules",
            DependencyRule::I2cAccess => "i2c_access",
            DependencyRule::HalodGroup => "halod_group",
            DependencyRule::Powerprofilesctl => "powerprofilesctl",
            DependencyRule::XdgDesktopPortal => "xdg_desktop_portal",
            DependencyRule::Pawnio => "pawnio",
            DependencyRule::Broker => "broker",
            DependencyRule::GnomeExtension => "gnome_extension",
        }
    }
}

/// One external runtime dependency (a CLI, helper, or platform integration)
/// the daemon relies on, and whether it was detected. Built by the daemon for
/// the host it actually runs on, so the UI only ever sees rows that apply to
/// the current platform.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DependencyStatus {
    /// Which dependency this row reports on.
    pub id: DependencyRule,
    /// Whether the dependency was found / is usable right now.
    pub present: bool,
    /// `true` when its absence breaks core functionality; `false` when it only
    /// gates an optional feature. Drives the "missing" severity in the UI.
    pub required: bool,
    /// Platform relevance label, e.g. `All`, `Linux`, `GNOME`, `Windows`.
    pub platform: String,
    /// Selects a fix-message variant when the resolution depends on runtime
    /// state, looked up as `depcheck.rules.<id>.fix_<fix_variant>`; empty
    /// picks the plain `depcheck.rules.<id>.fix` key.
    #[serde(default)]
    pub fix_variant: String,
}

/// One SMBus controller discovered on the system, whether or not a HaloDaemon
/// driver claims it. Surfaces the "PawnIO is loaded but I see no DRAM RGB"
/// failure mode that no other panel shows.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct SmbusBusDebugInfo {
    pub kind: SmbusBusKind,
    pub bus_number: u8,
    pub adapter_name: String,
    pub pci_vendor: u16,
    pub pci_device: u16,
}

#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct DebugInfo {
    pub system: SystemDebugInfo,
    pub devices: Vec<DeviceDebugInfo>,
    pub hid_entries: Vec<HidEntryDebugInfo>,
    #[serde(default)]
    pub smbus_buses: Vec<SmbusBusDebugInfo>,
    /// External runtime dependencies detected on the host, in display order.
    #[serde(default)]
    pub dependencies: Vec<DependencyStatus>,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn system_debug_info_round_trip() {
        let s = SystemDebugInfo {
            os: OsKind::Linux,
            os_version: "6.8.0".into(),
            running_elevated: true,
            pawnio_present: None,
            udev_rules_present: Some(true),
            daemon_version: "1.0.0".into(),
            daemon_build: "halod +default".into(),
        };
        let json = serde_json::to_string(&s).unwrap();
        let back: SystemDebugInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.os, OsKind::Linux);
        assert_eq!(back.os_version, "6.8.0");
        assert!(back.running_elevated);
        assert!(back.pawnio_present.is_none());
        assert_eq!(back.udev_rules_present, Some(true));
        assert_eq!(back.daemon_version, "1.0.0");
        assert_eq!(back.daemon_build, "halod +default");
    }

    #[test]
    fn system_debug_info_defaults_optional_fields() {
        let json = r#"{"os":"linux","os_version":"6.8.0","running_elevated":false,"daemon_version":"1.0.0","daemon_build":"halod +default"}"#;
        let back: SystemDebugInfo = serde_json::from_str(json).unwrap();
        assert!(back.pawnio_present.is_none());
        assert!(back.udev_rules_present.is_none());
    }

    #[test]
    fn hid_entry_debug_info_round_trip() {
        let e = HidEntryDebugInfo {
            vid: 0x046d,
            pid: 0xc52b,
            path: "/dev/hidraw0".into(),
            interface: 1,
            serial: "A1B2C3".into(),
            usage_page: 0xff00,
            usage: 0x0001,
            manufacturer: "Logitech".into(),
            product: "G Pro".into(),
            matched_device_id: Some("logitech-gpro-1".into()),
        };
        let json = serde_json::to_string(&e).unwrap();
        let back: HidEntryDebugInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.vid, 0x046d);
        assert_eq!(back.pid, 0xc52b);
        assert_eq!(back.path, "/dev/hidraw0");
        assert_eq!(back.interface, 1);
        assert_eq!(back.serial, "A1B2C3");
        assert_eq!(back.usage_page, 0xff00);
        assert_eq!(back.usage, 0x0001);
        assert_eq!(back.manufacturer, "Logitech");
        assert_eq!(back.product, "G Pro");
        assert_eq!(back.matched_device_id, Some("logitech-gpro-1".into()));
    }

    #[test]
    fn hid_entry_debug_info_serde_default_fields() {
        let json =
            r#"{"vid":1,"pid":2,"path":"/dev/hidraw0","interface":0,"usage_page":0,"usage":0}"#;
        let back: HidEntryDebugInfo = serde_json::from_str(json).unwrap();
        // #[serde(default)] fields
        assert_eq!(back.serial, "");
        assert_eq!(back.manufacturer, "");
        assert_eq!(back.product, "");
        assert!(back.matched_device_id.is_none());
    }

    #[test]
    fn hid_entry_debug_info_skips_none_matched_device_id() {
        let e = HidEntryDebugInfo {
            vid: 1,
            pid: 2,
            path: "/dev/hidraw0".into(),
            interface: 0,
            serial: "".into(),
            usage_page: 0,
            usage: 0,
            manufacturer: "".into(),
            product: "".into(),
            matched_device_id: None,
        };
        let json = serde_json::to_string(&e).unwrap();
        assert!(
            !json.contains("matched_device_id"),
            "None matched_device_id must be skipped: {json}"
        );
    }

    #[test]
    fn device_debug_info_round_trip() {
        let d = DeviceDebugInfo {
            id: "dev-1".into(),
            name: "Kraken X73".into(),
            vendor: "NZXT".into(),
            model: "X73".into(),
            connected: true,
            transport: "hid".into(),
            fields: vec![("firmware".into(), "3.2.1".into())],
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: DeviceDebugInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, "dev-1");
        assert_eq!(back.name, "Kraken X73");
        assert_eq!(back.vendor, "NZXT");
        assert_eq!(back.model, "X73");
        assert!(back.connected);
        assert_eq!(back.transport, "hid");
        assert_eq!(back.fields, vec![("firmware".into(), "3.2.1".into())]);
    }

    #[test]
    fn device_debug_info_defaults_fields_when_omitted() {
        let json = r#"{"id":"dev-1","name":"Kraken","vendor":"NZXT","model":"X73","connected":true,"transport":"hid"}"#;
        let back: DeviceDebugInfo = serde_json::from_str(json).unwrap();
        assert!(back.fields.is_empty());
    }

    #[test]
    fn smbus_bus_debug_info_round_trip() {
        let b = SmbusBusDebugInfo {
            kind: SmbusBusKind::Chipset,
            bus_number: 1,
            adapter_name: "SMBus PIIX4 adapter".into(),
            pci_vendor: 0x8086,
            pci_device: 0x1234,
        };
        let json = serde_json::to_string(&b).unwrap();
        let back: SmbusBusDebugInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.kind, SmbusBusKind::Chipset);
        assert_eq!(back.bus_number, 1);
        assert_eq!(back.adapter_name, "SMBus PIIX4 adapter");
        assert_eq!(back.pci_vendor, 0x8086);
        assert_eq!(back.pci_device, 0x1234);
    }

    #[test]
    fn debug_info_round_trip() {
        let di = DebugInfo {
            system: SystemDebugInfo {
                os: OsKind::Linux,
                os_version: "6.8.0".into(),
                running_elevated: true,
                pawnio_present: None,
                udev_rules_present: Some(false),
                daemon_version: "1.0.0".into(),
                daemon_build: "halod +default".into(),
            },
            devices: vec![DeviceDebugInfo {
                id: "dev-1".into(),
                name: "Kraken".into(),
                vendor: "NZXT".into(),
                model: "X73".into(),
                connected: true,
                transport: "hid".into(),
                fields: vec![],
            }],
            hid_entries: vec![HidEntryDebugInfo {
                vid: 0x046d,
                pid: 0xc52b,
                path: "/dev/hidraw0".into(),
                interface: 0,
                serial: "S/N".into(),
                usage_page: 0,
                usage: 0,
                manufacturer: "Logitech".into(),
                product: "G Pro".into(),
                matched_device_id: None,
            }],
            smbus_buses: vec![SmbusBusDebugInfo {
                kind: SmbusBusKind::Chipset,
                bus_number: 0,
                adapter_name: "SMBus".into(),
                pci_vendor: 0x8086,
                pci_device: 0x1234,
            }],
            dependencies: vec![DependencyStatus {
                id: DependencyRule::Ffmpeg,
                present: true,
                required: false,
                platform: "All".into(),
                fix_variant: String::new(),
            }],
        };
        let json = serde_json::to_string(&di).unwrap();
        let back: DebugInfo = serde_json::from_str(&json).unwrap();
        assert_eq!(back.system.os, OsKind::Linux);
        assert_eq!(back.devices.len(), 1);
        assert_eq!(back.hid_entries.len(), 1);
        assert_eq!(back.smbus_buses.len(), 1);
        assert_eq!(back.dependencies.len(), 1);
        assert_eq!(back.dependencies[0].id, DependencyRule::Ffmpeg);
        assert!(back.dependencies[0].present);
    }

    #[test]
    fn dependency_status_round_trip() {
        let d = DependencyStatus {
            id: DependencyRule::Pactl,
            present: false,
            required: false,
            platform: "Linux".into(),
            fix_variant: String::new(),
        };
        let json = serde_json::to_string(&d).unwrap();
        let back: DependencyStatus = serde_json::from_str(&json).unwrap();
        assert_eq!(back.id, DependencyRule::Pactl);
        assert!(!back.present);
        assert!(!back.required);
        assert_eq!(back.platform, "Linux");
        assert_eq!(back.fix_variant, "");
    }

    #[test]
    fn dependency_status_defaults_fix_variant() {
        let json = r#"{"id":"ffmpeg","present":true,"required":false,"platform":"All"}"#;
        let back: DependencyStatus = serde_json::from_str(json).unwrap();
        assert_eq!(back.fix_variant, "");
    }

    #[test]
    fn debug_info_defaults_smbus_buses_when_omitted() {
        let json = r#"{"system":{"os":"linux","os_version":"","running_elevated":false,"daemon_version":"","daemon_build":""},"devices":[],"hid_entries":[]}"#;
        let back: DebugInfo = serde_json::from_str(json).unwrap();
        assert!(
            back.smbus_buses.is_empty(),
            "smbus_buses must default to empty vec"
        );
    }
}
