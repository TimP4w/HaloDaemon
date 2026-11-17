// SPDX-License-Identifier: GPL-3.0-or-later
//! Match host capabilities and connected hardware against *disabled* plugins
//! to recommend one the user could enable. This is deliberately independent of the activation matcher
//! (`make_device`), which excludes disabled plugins — a recommendation is
//! exactly the opposite population. Runs at startup only (no continuous
//! probing).

use std::collections::HashSet;

use crate::infrastructure::drivers::transports::smbus::PciMatch;
use halod_plugin_signing::{CatalogPackage, HOST_PLUGIN_API};
use halod_shared::debug_info::HidEntryDebugInfo;
use halod_shared::types::{
    HidIdentity, PackageMatches, PluginRecommendation, PluginRecommendationMatch,
};

use super::manifest::PluginManifest;
use crate::infrastructure::drivers::transports::smbus::BusInfo;
use halod_shared::ids::PluginId;

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

fn hid_identity_accepts(identity: &HidIdentity, e: &HidEntryDebugInfo) -> bool {
    identity.vid == e.vid
        && identity.pids.contains(&e.pid)
        && identity.usage_page.is_none_or(|up| up == e.usage_page)
        && identity.usage.is_none_or(|u| u == e.usage)
        && identity.interface.is_none_or(|iface| iface == e.interface)
}

#[derive(Debug, Clone, Copy)]
pub struct Candidate<'a> {
    pub plugin_id: &'a PluginId,
    pub plugin_name: &'a str,
    pub matches: &'a PackageMatches,
    pub install_from: Option<&'a str>,
}

pub fn candidates<'a>(
    manifests: &'a [PluginManifest],
    is_enabled: &dyn Fn(&PluginId) -> bool,
    catalog: impl IntoIterator<Item = (&'a str, &'a CatalogPackage)>,
) -> Vec<Candidate<'a>> {
    let mut out: Vec<Candidate<'a>> = manifests
        .iter()
        .filter(|m| !is_enabled(&m.plugin_id))
        .map(|m| Candidate {
            plugin_id: &m.plugin_id,
            plugin_name: &m.declarations.name,
            matches: &m.declarations.matches,
            install_from: None,
        })
        .collect();
    for (repo, entry) in catalog {
        if entry.api.is_compatible_with(HOST_PLUGIN_API)
            && !manifests.iter().any(|m| m.plugin_id == entry.id)
            && !out.iter().any(|c| c.plugin_id == entry.id)
        {
            out.push(Candidate {
                plugin_id: &entry.id,
                plugin_name: &entry.name,
                matches: &entry.matches,
                install_from: Some(repo),
            });
        }
    }
    out
}

pub fn recommendations(
    candidates: &[Candidate<'_>],
    hid: &[HidEntryDebugInfo],
    usb: &[UsbEntry],
    gpu_smbus: &[BusInfo],
    command_available: &dyn Fn(&str) -> bool,
) -> Vec<PluginRecommendation> {
    let mut out = Vec::new();
    let mut seen: HashSet<(&PluginId, PluginRecommendationMatch)> = HashSet::new();
    for candidate in candidates
        .iter()
        .filter(|c| c.matches.supports_platform(std::env::consts::OS))
    {
        let mut hits = Vec::new();
        if candidate.plugin_id == "halo_effects" || candidate.plugin_id == "halo_lcd" {
            hits.push(PluginRecommendationMatch::Always);
        }
        let m = candidate.matches;
        hits.extend(
            m.commands
                .iter()
                .filter(|executable| command_available(executable))
                .map(|executable| PluginRecommendationMatch::Command {
                    executable: executable.clone(),
                }),
        );
        for identity in &m.hid {
            hits.extend(
                hid.iter()
                    .filter(|e| hid_identity_accepts(identity, e))
                    .map(|e| PluginRecommendationMatch::Hid {
                        vid: e.vid,
                        pid: e.pid,
                    }),
            );
        }
        for identity in &m.usb {
            hits.extend(
                usb.iter()
                    .filter(|entry| {
                        entry.vid == identity.vid
                            && identity.pids.contains(&entry.pid)
                            && entry.interfaces.contains(&identity.interface)
                    })
                    .map(|entry| PluginRecommendationMatch::Usb {
                        vid: entry.vid,
                        pid: entry.pid,
                    }),
            );
        }
        hits.extend(
            gpu_smbus
                .iter()
                .filter(|bus| {
                    m.smbus_gpu
                        .iter()
                        .any(|identity| PciMatch::from(identity).accepts(bus))
                })
                .map(|bus| PluginRecommendationMatch::SmbusGpu {
                    pci_vendor: bus.pci_vendor,
                    pci_device: bus.pci_device,
                    pci_sub_vendor: bus.pci_sub_vendor,
                    pci_sub_device: bus.pci_sub_device,
                }),
        );
        for hardware in hits {
            if seen.insert((candidate.plugin_id, hardware.clone())) {
                out.push(PluginRecommendation {
                    plugin_id: candidate.plugin_id.clone(),
                    plugin_name: candidate.plugin_name.to_owned(),
                    hardware,
                    install_from: candidate.install_from.map(str::to_owned),
                    accessible: true,
                });
            }
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn recommendations(
        manifests: &[PluginManifest],
        is_enabled: &dyn Fn(&PluginId) -> bool,
        hid: &[HidEntryDebugInfo],
        usb: &[UsbEntry],
        gpu_smbus: &[BusInfo],
        command_available: &dyn Fn(&str) -> bool,
    ) -> Vec<PluginRecommendation> {
        super::recommendations(
            &candidates(manifests, is_enabled, []),
            hid,
            usb,
            gpu_smbus,
            command_available,
        )
    }

    fn catalog_entry(id: &str, api: &str, hid: Vec<HidIdentity>) -> CatalogPackage {
        CatalogPackage {
            id: id.parse().unwrap(),
            name: format!("{id} name"),
            version: "1.0.0".parse().unwrap(),
            api: api.parse().unwrap(),
            path: format!("packages/{id}.tar.gz"),
            sha256: "a".repeat(64),
            size: 1,
            matches: PackageMatches {
                hid,
                ..Default::default()
            },
            uses: Default::default(),
        }
    }

    fn cooler_identity() -> HidIdentity {
        HidIdentity {
            vid: 0x1e71,
            pids: vec![0x2007],
            usage_page: None,
            usage: None,
            interface: None,
        }
    }

    #[test]
    fn a_catalog_only_package_is_recommended_with_its_repo() {
        let entry = catalog_entry("acme_cooler", "3.0", vec![cooler_identity()]);
        let found = super::recommendations(
            &candidates(&[], &|_| false, [("official", &entry)]),
            &[hid_entry(0x1e71, 0x2007)],
            &[],
            &[],
            &|_| false,
        );
        assert_eq!(found.len(), 1);
        assert_eq!(found[0].plugin_name, "acme_cooler name");
        assert_eq!(found[0].install_from.as_deref(), Some("official"));
    }

    #[test]
    fn an_installed_or_incompatible_package_is_not_recommended_from_the_catalog() {
        let installed = manifest("acme_cooler", COOLER);
        let entry = catalog_entry("acme_cooler", "3.0", vec![cooler_identity()]);
        let future = catalog_entry("future_cooler", "9.0", vec![cooler_identity()]);
        let listed = candidates(
            std::slice::from_ref(&installed),
            &|_| true,
            [("official", &entry), ("official", &future)],
        );
        assert!(listed.is_empty());
        let disabled = candidates(
            std::slice::from_ref(&installed),
            &|_| false,
            [("official", &entry)],
        );
        assert_eq!(disabled.len(), 1);
        assert_eq!(disabled[0].install_from, None);
    }

    fn manifest(id: &str, yaml_extra: &str) -> PluginManifest {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(tmp.path(), id, yaml_extra);
        super::super::manifest::parse_manifest_from_dir(&dir).unwrap()
    }

    fn write_plugin_dir(root: &Path, id: &str, yaml_extra: &str) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            format!("id: {id}\nversion: 1.0.0\napi: \"3.0\"\n{yaml_extra}"),
        )
        .unwrap();
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

    const COOLER: &str = "capabilities: [lighting]\ndevices:\n  - vendor: Acme\n    model: Cooler\n    type: led_strip\n    match:\n      hid: { vid: 0x1e71, pid: 0x2007 }\n";

    #[test]
    fn concrete_match_recommends_a_disabled_plugin() {
        let m = manifest("acme_cooler", COOLER);
        let recs = recommendations(
            &[m],
            &|_| false,
            &[hid_entry(0x1e71, 0x2007)],
            &[],
            &[],
            &|_| false,
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(recs[0].plugin_id, "acme_cooler");
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
        let m = manifest("acme_cooler", COOLER);
        // Already enabled → not a recommendation.
        assert!(recommendations(
            std::slice::from_ref(&m),
            &|id| id == "acme_cooler",
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
            "capabilities: [lighting]\ndevices:\n  - vendor: X\n    model: Y\n    type: led_strip\n    match:\n      hid: { any: true }\n",
        );
        assert!(
            recommendations(&[m], &|_| false, &[hid_entry(1, 2)], &[], &[], &|_| false,).is_empty()
        );
    }

    #[test]
    fn usage_constraint_is_honored_and_duplicates_collapse() {
        let m = manifest(
            "usage_gated",
            "capabilities: [lighting]\ndevices:\n  - vendor: X\n    model: Y\n    type: led_strip\n    match:\n      hid: { vid: 0x1234, pid: 0x5678, usage_page: 0xffab }\n",
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
            "capabilities: [controls]\ndevices:\n  - vendor: X\n    model: Y\n    type: monitor\n    match:\n      usb: { vid: 0x1234, pid: 0x5678, interface: 2 }\ntransports:\n  usb:\n    devices:\n      - id: primary\n        interface: 2\n        control: { max_transfer_size: 64, max_timeout_ms: 1000 }\n",
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
    fn usb_family_pids_recommend_each_declared_pid() {
        let m = manifest(
            "usb_family",
            "capabilities: [controls]\ndevices:\n  - vendor: X\n    model: Y\n    type: monitor\n    match:\n      usb: { vid: 0x1234, pids: [0x1, 0x2], interface: 0 }\ntransports:\n  usb:\n    devices:\n      - id: primary\n        interface: 0\n        control: { max_transfer_size: 64, max_timeout_ms: 1000 }\n",
        );
        let family_member = UsbEntry {
            vid: 0x1234,
            pid: 0x2,
            interfaces: vec![0],
        };
        let recs = recommendations(
            std::slice::from_ref(&m),
            &|_| false,
            &[],
            &[family_member],
            &[],
            &|_| false,
        );
        assert_eq!(recs.len(), 1);
        assert_eq!(
            recs[0].hardware,
            PluginRecommendationMatch::Usb {
                vid: 0x1234,
                pid: 0x2
            }
        );
        let stranger = UsbEntry {
            vid: 0x1234,
            pid: 0x9,
            interfaces: vec![0],
        };
        assert!(recommendations(&[m], &|_| false, &[], &[stranger], &[], &|_| false).is_empty());
    }

    #[test]
    fn gpu_smbus_match_is_passive_and_pci_gated() {
        let m = manifest(
            "gpu_smbus",
            "capabilities: [lighting]\ndevices:\n  - vendor: X\n    model: GPU\n    type: gpu\n    match:\n      smbus:\n        bus: gpu\n        addresses: [0x67]\n        pci_match: [{ vendor: 0x10de, sub_vendor: 0x1043 }]\n",
        );
        let bus = BusInfo {
            bus_number: 1,
            adapter_name: "GPU i2c".into(),
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
            "type: integration\ntransports:\n  tcp: {}\nconfig:\n  fields:\n    - { key: host, label: Host, kind: host }\n    - { key: port, label: Port, kind: port }\n",
        );
        let recs = recommendations(&[m], &|_| false, &[], &[], &[], &|_| false);
        assert!(matches!(
            recs[0].hardware,
            PluginRecommendationMatch::Always
        ));
    }

    #[test]
    fn stock_lcd_is_always_recommended_when_disabled() {
        let m = manifest(
            "halo_lcd",
            "type: integration\ntransports:\n  tcp: {}\nconfig:\n  fields:\n    - { key: host, label: Host, kind: host }\n    - { key: port, label: Port, kind: port }\n",
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
            "gpu_tool",
            "devices:\n  - vendor: Acme\n    model: any\n    match:\n      command: nvidia-smi\ntransports:\n  command:\n    commands: [nvidia-smi]\n",
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
