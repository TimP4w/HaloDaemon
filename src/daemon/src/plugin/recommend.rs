// SPDX-License-Identifier: GPL-3.0-or-later
//! Match host capabilities and connected hardware against *disabled* plugins
//! to recommend one the user could enable. This is deliberately independent of the activation matcher
//! (`match_handle`), which excludes disabled/unconsented plugins — a
//! recommendation is exactly the opposite population. Runs at startup only (no
//! continuous probing); the enable action still routes through consent.

use std::collections::HashSet;

use halod_shared::debug_info::HidEntryDebugInfo;
use halod_shared::types::{PluginRecommendation, PluginRecommendationMatch};

use super::manifest::{HidMatch, PluginManifest};
use crate::drivers::transports::smbus::BusInfo;

/// The USB identity fields needed for recommendation matching. Enumeration is
/// descriptor-only and never claims an interface or opens an endpoint.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UsbEntry {
    pub vid: u16,
    pub pid: u16,
    pub interfaces: Vec<u8>,
}

pub fn enumerate_usb() -> Vec<UsbEntry> {
    use rusb::{Context, UsbContext};

    let Ok(context) = Context::new() else {
        return Vec::new();
    };
    context
        .devices()
        .map(|devices| {
            devices
                .iter()
                .filter_map(|device| {
                    let descriptor = device.device_descriptor().ok()?;
                    let mut interfaces: Vec<u8> = device
                        .active_config_descriptor()
                        .ok()
                        .map(|config| {
                            config
                                .interfaces()
                                .flat_map(|interface| interface.descriptors())
                                .map(|descriptor| descriptor.interface_number())
                                .collect()
                        })
                        .unwrap_or_default();
                    interfaces.sort_unstable();
                    interfaces.dedup();
                    Some(UsbEntry {
                        vid: descriptor.vendor_id(),
                        pid: descriptor.product_id(),
                        interfaces,
                    })
                })
                .collect()
        })
        .unwrap_or_default()
}

/// Whether a HID device matches a manifest's HID declaration *concretely* — a
/// real VID and PID, honoring any declared usage/interface constraint. A
/// wildcard (`any`) or a VID without a PID never recommends: it would match far
/// too broadly to suggest a specific plugin.
fn hid_match_concrete(m: &HidMatch, e: &HidEntryDebugInfo) -> bool {
    if m.any {
        return false;
    }
    let Some(vid) = m.vid else {
        return false;
    };
    if vid != e.vid {
        return false;
    }
    let pid_ok = m.pid == Some(e.pid) || m.pids.contains(&e.pid);
    if !pid_ok {
        return false;
    }
    if m.usage_page.is_some_and(|up| up != e.usage_page) {
        return false;
    }
    if m.usage.is_some_and(|u| u != e.usage) {
        return false;
    }
    if m.interface.is_some_and(|iface| iface != e.interface) {
        return false;
    }
    true
}

/// Recommendations for disabled plugins that are host defaults or whose
/// command, HID, USB, or passively identifiable GPU-SMBus declaration matches.
/// `is_enabled` excludes plugins the user already enabled.
pub fn recommendations(
    manifests: &[PluginManifest],
    is_enabled: &dyn Fn(&str) -> bool,
    hid: &[HidEntryDebugInfo],
    usb: &[UsbEntry],
    gpu_smbus: &[BusInfo],
    command_available: &dyn Fn(&str) -> bool,
) -> Vec<PluginRecommendation> {
    let mut out = Vec::new();
    let mut seen: HashSet<(String, PluginRecommendationMatch)> = HashSet::new();
    for m in manifests {
        if !m.supports_current_platform() || is_enabled(&m.plugin_id) {
            continue;
        }
        if m.plugin_id == "halo_effects" {
            let hardware = PluginRecommendationMatch::Always;
            seen.insert((m.plugin_id.clone(), hardware.clone()));
            out.push(PluginRecommendation {
                plugin_id: m.plugin_id.clone(),
                plugin_name: m.display_name().to_owned(),
                hardware,
                accessible: true,
            });
        }
        for spec in &m.devices {
            if let Some(command) = &spec.r#match.command {
                let executable = command.command();
                let hardware = PluginRecommendationMatch::Command {
                    executable: executable.to_owned(),
                };
                if command_available(executable)
                    && seen.insert((m.plugin_id.clone(), hardware.clone()))
                {
                    out.push(PluginRecommendation {
                        plugin_id: m.plugin_id.clone(),
                        plugin_name: m.display_name().to_owned(),
                        hardware,
                        accessible: true,
                    });
                }
            }
            if let Some(hm) = &spec.r#match.hid {
                for e in hid {
                    let hardware = PluginRecommendationMatch::Hid {
                        vid: e.vid,
                        pid: e.pid,
                    };
                    if hid_match_concrete(hm, e)
                        && seen.insert((m.plugin_id.clone(), hardware.clone()))
                    {
                        out.push(PluginRecommendation {
                            plugin_id: m.plugin_id.clone(),
                            plugin_name: m.display_name().to_owned(),
                            hardware,
                            accessible: true,
                        });
                    }
                }
            }

            if let Some(wanted) = &spec.r#match.usb {
                for entry in usb.iter().filter(|entry| {
                    entry.vid == wanted.vid
                        && entry.pid == wanted.pid
                        && entry.interfaces.contains(&wanted.interface)
                }) {
                    let hardware = PluginRecommendationMatch::Usb {
                        vid: entry.vid,
                        pid: entry.pid,
                    };
                    if seen.insert((m.plugin_id.clone(), hardware.clone())) {
                        out.push(PluginRecommendation {
                            plugin_id: m.plugin_id.clone(),
                            plugin_name: m.display_name().to_owned(),
                            hardware,
                            accessible: true,
                        });
                    }
                }
            }

            // SMBus device addresses cannot be probed before the user grants
            // that permission. A GPU declaration has a mandatory PCI allowlist,
            // however, so controller identity alone is a safe passive match.
            if spec.bus_kind() == Some(crate::drivers::transports::smbus::SmbusBusKind::Gpu) {
                for bus in gpu_smbus
                    .iter()
                    .filter(|bus| spec.pci_match.iter().any(|pci| pci.accepts(bus)))
                {
                    let hardware = PluginRecommendationMatch::SmbusGpu {
                        pci_vendor: bus.pci_vendor,
                        pci_device: bus.pci_device,
                        pci_sub_vendor: bus.pci_sub_vendor,
                        pci_sub_device: bus.pci_sub_device,
                    };
                    if seen.insert((m.plugin_id.clone(), hardware.clone())) {
                        out.push(PluginRecommendation {
                            plugin_id: m.plugin_id.clone(),
                            plugin_name: m.display_name().to_owned(),
                            hardware,
                            accessible: true,
                        });
                    }
                }
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn manifest(id: &str, yaml_extra: &str) -> PluginManifest {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(tmp.path(), id, yaml_extra);
        super::super::manifest::parse_manifest_from_dir(&dir).unwrap()
    }

    fn write_plugin_dir(root: &Path, id: &str, yaml_extra: &str) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), format!("id: {id}\n{yaml_extra}")).unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        dir
    }

    fn hid_entry(vid: u16, pid: u16) -> HidEntryDebugInfo {
        HidEntryDebugInfo {
            vid,
            pid,
            path: "/dev/hidraw0".into(),
            interface: 0,
            serial: String::new(),
            usage_page: 0xff00,
            usage: 1,
            manufacturer: String::new(),
            product: String::new(),
            matched_device_id: None,
        }
    }

    const KRAKEN: &str = "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: NZXT\n    model: Kraken\n    type: led_strip\n    match:\n      hid: { vid: 0x1e71, pid: 0x2007 }\n";

    #[test]
    fn concrete_match_recommends_a_disabled_plugin() {
        let m = manifest("nzxt_kraken", KRAKEN);
        let recs = recommendations(
            &[m],
            &|_| false,
            &[hid_entry(0x1e71, 0x2007)],
            &[],
            &[],
            &|_| false,
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].plugin_id, "nzxt_kraken");
        assert_eq!(
            recs[0].hardware,
            PluginRecommendationMatch::Hid {
                vid: 0x1e71,
                pid: 0x2007
            }
        );
    }

    #[test]
    fn enabled_plugin_and_mismatched_device_are_excluded() {
        let m = manifest("nzxt_kraken", KRAKEN);
        // Already enabled → not a recommendation.
        assert!(recommendations(
            std::slice::from_ref(&m),
            &|id| id == "nzxt_kraken",
            &[hid_entry(0x1e71, 0x2007)],
            &[],
            &[],
            &|_| false,
        )
        .is_empty());
        // Different device connected → no match.
        assert!(recommendations(
            &[m],
            &|_| false,
            &[hid_entry(0x046d, 0xc52b)],
            &[],
            &[],
            &|_| false,
        )
        .is_empty());
    }

    #[test]
    fn wildcard_match_never_recommends() {
        let m = manifest(
            "wild",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: X\n    model: Y\n    type: led_strip\n    match:\n      hid: { any: true }\n",
        );
        assert!(
            recommendations(&[m], &|_| false, &[hid_entry(1, 2)], &[], &[], &|_| false,).is_empty()
        );
    }

    #[test]
    fn usage_constraint_is_honored_and_duplicates_collapse() {
        let m = manifest(
            "usage_gated",
            "permissions: [hid]\ncapabilities: [rgb]\ndevices:\n  - vendor: X\n    model: Y\n    type: led_strip\n    match:\n      hid: { vid: 0x1234, pid: 0x5678, usage_page: 0xffab }\n",
        );
        // Wrong usage_page → no match.
        assert!(recommendations(
            std::slice::from_ref(&m),
            &|_| false,
            &[hid_entry(0x1234, 0x5678)],
            &[],
            &[],
            &|_| false,
        )
        .is_empty());

        // Two interfaces of the same device collapse to one recommendation.
        let mut e0 = hid_entry(0x1234, 0x5678);
        e0.usage_page = 0xffab;
        e0.interface = 0;
        let mut e1 = e0.clone();
        e1.interface = 1;
        let recs = recommendations(&[m], &|_| false, &[e0, e1], &[], &[], &|_| false);
        assert_eq!(recs.len(), 1);
    }

    #[test]
    fn usb_match_honors_interface() {
        let m = manifest(
            "usb",
            "permissions: [usb]\ncapabilities: [controls]\ndevices:\n  - vendor: X\n    model: Y\n    type: monitor\n    match:\n      usb: { vid: 0x1234, pid: 0x5678, interface: 2 }\ntransports:\n  usb:\n    devices:\n      - id: primary\n        interface: 2\n        control: { max_transfer_size: 64, max_timeout_ms: 1000 }\n",
        );
        let wrong = UsbEntry {
            vid: 0x1234,
            pid: 0x5678,
            interfaces: vec![0],
        };
        assert!(recommendations(
            std::slice::from_ref(&m),
            &|_| false,
            &[],
            &[wrong],
            &[],
            &|_| false,
        )
        .is_empty());
        let matching = UsbEntry {
            vid: 0x1234,
            pid: 0x5678,
            interfaces: vec![2],
        };
        let recs = recommendations(&[m], &|_| false, &[], &[matching], &[], &|_| false);
        assert!(matches!(
            recs[0].hardware,
            PluginRecommendationMatch::Usb { .. }
        ));
    }

    #[test]
    fn gpu_smbus_match_is_passive_and_pci_gated() {
        let m = manifest(
            "gpu_smbus",
            "permissions: [smbus]\ncapabilities: [rgb]\ndevices:\n  - vendor: X\n    model: GPU\n    type: gpu\n    match:\n      smbus:\n        bus: gpu\n        addresses: [0x67]\n        pci_match: [{ vendor: 0x10de, sub_vendor: 0x1043 }]\n",
        );
        let bus = BusInfo {
            bus_number: 1,
            adapter_name: "NVIDIA i2c".into(),
            pci_vendor: 0x10de,
            pci_device: 0x2684,
            pci_sub_vendor: 0x1043,
            pci_sub_device: 1,
        };
        let recs = recommendations(&[m], &|_| false, &[], &[], &[bus], &|_| false);
        assert_eq!(recs.len(), 1);
        assert!(matches!(
            recs[0].hardware,
            PluginRecommendationMatch::SmbusGpu { .. }
        ));
    }

    #[test]
    fn stock_effects_are_always_recommended_when_disabled() {
        let m = manifest(
            "halo_effects",
            "type: integration\npermissions: [network]\ntransports:\n  tcp: {}\n",
        );
        let recs = recommendations(&[m], &|_| false, &[], &[], &[], &|_| false);
        assert!(matches!(
            recs[0].hardware,
            PluginRecommendationMatch::Always
        ));
    }

    #[test]
    fn command_device_is_recommended_when_executable_resolves() {
        let m = manifest(
            "nvidia",
            "permissions: [command]\ndevices:\n  - vendor: NVIDIA\n    model: any\n    match:\n      command: nvidia-smi\ntransports:\n  command:\n    commands: [nvidia-smi]\n",
        );
        let recs = recommendations(&[m], &|_| false, &[], &[], &[], &|name| {
            name == "nvidia-smi"
        });
        assert!(matches!(
            &recs[0].hardware,
            PluginRecommendationMatch::Command { executable } if executable == "nvidia-smi"
        ));
    }
}
