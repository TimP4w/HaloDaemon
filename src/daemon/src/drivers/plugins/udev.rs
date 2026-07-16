// SPDX-License-Identifier: GPL-3.0-or-later
//! Deterministic Linux udev rules derived from plugin hardware declarations.

use std::collections::{BTreeMap, BTreeSet};

use super::PluginManifest;

const BASE_RULES: &str = include_str!("../../../../../udev/60-halod.rules");

#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
enum NodeKind {
    Hidraw,
    Usb,
}

impl NodeKind {
    fn render(self, vid: u16, pid: u16) -> String {
        match self {
            Self::Hidraw => format!(
                "KERNEL==\"hidraw*\", ATTRS{{idVendor}}==\"{vid:04x}\", ATTRS{{idProduct}}==\"{pid:04x}\", TAG+=\"uaccess\""
            ),
            Self::Usb => format!(
                "SUBSYSTEM==\"usb\", ATTRS{{idVendor}}==\"{vid:04x}\", ATTRS{{idProduct}}==\"{pid:04x}\", TAG+=\"uaccess\""
            ),
        }
    }
}

type RuleKey = (NodeKind, u16, u16);

/// Assemble daemon-owned baseline rules with access rules inferred from the
/// same scoped HID/USB/SMBus declarations that bound each plugin at runtime.
pub fn assemble(manifests: &[PluginManifest]) -> String {
    let mut rules: BTreeMap<RuleKey, BTreeSet<String>> = BTreeMap::new();
    let mut chipset_i2c = BTreeSet::new();
    let mut gpu_i2c: BTreeMap<u16, BTreeSet<String>> = BTreeMap::new();
    for manifest in manifests.iter().filter(|manifest| {
        manifest.platforms.is_empty() || manifest.platforms.iter().any(|p| p == "linux")
    }) {
        let label = format!("{} ({})", manifest.display_name(), manifest.plugin_id);
        let raw_usb = manifest.transports.usb.is_some();

        for device in &manifest.devices {
            if let Some(hid) = &device.r#match.hid {
                if let Some(vid) = hid.vid {
                    for pid in hid.pids.iter().copied().chain(hid.pid) {
                        insert(&mut rules, (NodeKind::Hidraw, vid, pid), &label);
                        if raw_usb {
                            insert(&mut rules, (NodeKind::Usb, vid, pid), &label);
                        }
                    }
                }
            }
            if let Some(usb) = &device.r#match.usb {
                insert(&mut rules, (NodeKind::Usb, usb.vid, usb.pid), &label);
            }
            if let Some(smbus) = &device.r#match.smbus {
                match smbus.bus.as_str() {
                    "chipset" => {
                        chipset_i2c.insert(label.clone());
                    }
                    "gpu" => {
                        for vendor in smbus.pci_match.iter().filter_map(|pci| pci.vendor) {
                            gpu_i2c.entry(vendor).or_default().insert(label.clone());
                        }
                    }
                    _ => {}
                }
            }
        }

        if let Some(usb) = &manifest.transports.usb {
            for device in &usb.devices {
                if let (Some(vid), Some(pid)) = (device.vid, device.pid) {
                    insert(&mut rules, (NodeKind::Usb, vid, pid), &label);
                }
            }
        }
    }

    let mut output = BASE_RULES.trim_end().to_owned();
    if !rules.is_empty() || !chipset_i2c.is_empty() || !gpu_i2c.is_empty() {
        output.push_str(
            "\n\n# ─── Plugin devices (generated) ──────────────────────────────────────────────\n",
        );
        for ((kind, vid, pid), plugins) in rules {
            output.push_str("\n# ");
            output.push_str(&plugins.into_iter().collect::<Vec<_>>().join(", "));
            output.push('\n');
            output.push_str(&kind.render(vid, pid));
            output.push('\n');
        }
        if !chipset_i2c.is_empty() {
            output.push_str("\n# ");
            output.push_str(&chipset_i2c.into_iter().collect::<Vec<_>>().join(", "));
            output.push_str(" — chipset SMBus\n");
            output.push_str(
                "SUBSYSTEM==\"i2c-dev\", DRIVERS==\"i801_smbus|piix4_smbus\", GROUP=\"halod\", MODE=\"0660\"\n",
            );
        }
        for (vendor, plugins) in gpu_i2c {
            output.push_str("\n# ");
            output.push_str(&plugins.into_iter().collect::<Vec<_>>().join(", "));
            output.push_str(" — GPU SMBus\n");
            output.push_str(&format!(
                "SUBSYSTEM==\"i2c-dev\", ATTRS{{vendor}}==\"0x{vendor:04x}\", GROUP=\"halod\", MODE=\"0660\"\n"
            ));
        }
    } else {
        output.push('\n');
    }
    output
}

pub fn status(rules: &str, manifests: &[PluginManifest]) -> halod_shared::types::UdevRulesStatus {
    #[cfg(target_os = "linux")]
    {
        const PATHS: &[&str] = &[
            "/etc/udev/rules.d/60-halod.rules",
            "/run/udev/rules.d/60-halod.rules",
            "/usr/local/lib/udev/rules.d/60-halod.rules",
            "/usr/lib/udev/rules.d/60-halod.rules",
            "/lib/udev/rules.d/60-halod.rules",
        ];
        status_at_paths(rules, manifests, PATHS.iter().map(std::path::Path::new))
    }
    #[cfg(not(target_os = "linux"))]
    {
        let _ = (rules, manifests);
        halod_shared::types::UdevRulesStatus::default()
    }
}

#[cfg(any(target_os = "linux", test))]
fn status_at_paths<'a>(
    rules: &str,
    manifests: &[PluginManifest],
    paths: impl IntoIterator<Item = &'a std::path::Path>,
) -> halod_shared::types::UdevRulesStatus {
    let installed = paths.into_iter().find_map(|path| {
        std::fs::read(path)
            .ok()
            .map(|contents| (path.display().to_string(), contents))
    });
    let generated_rule_count = device_rule_count(rules);
    let mut contributing_plugin_ids = manifests
        .iter()
        .filter(|manifest| device_rule_count(&assemble(std::slice::from_ref(manifest))) > 0)
        .map(|manifest| manifest.plugin_id.clone())
        .collect::<Vec<_>>();
    contributing_plugin_ids.sort();
    let mut plugins_requiring_update = manifests
        .iter()
        .filter(|manifest| {
            let plugin_rules = assemble(std::slice::from_ref(manifest));
            let required = generated_device_rules(&plugin_rules);
            !required.is_empty()
                && installed.as_ref().is_none_or(|(_, contents)| {
                    let installed = String::from_utf8_lossy(contents);
                    required
                        .iter()
                        .any(|rule| !installed.lines().any(|line| line == *rule))
                })
        })
        .map(|manifest| manifest.plugin_id.clone())
        .collect::<Vec<_>>();
    plugins_requiring_update.sort();
    match installed {
        Some((path, contents)) => halod_shared::types::UdevRulesStatus {
            supported: true,
            current: contents == rules.as_bytes(),
            installed_path: Some(path),
            generated_rule_count,
            plugins_requiring_update,
            contributing_plugin_ids,
        },
        None => halod_shared::types::UdevRulesStatus {
            supported: true,
            current: false,
            installed_path: None,
            generated_rule_count,
            plugins_requiring_update,
            contributing_plugin_ids,
        },
    }
}

#[cfg(any(target_os = "linux", test))]
fn device_rule_count(rules: &str) -> usize {
    generated_device_rules(rules).len()
}

#[cfg(any(target_os = "linux", test))]
fn generated_device_rules(rules: &str) -> Vec<&str> {
    rules
        .lines()
        .filter(|line| {
            (line.contains("idVendor") && line.contains("idProduct"))
                || line.starts_with("SUBSYSTEM==\"i2c-dev\"")
        })
        .collect()
}

fn insert(rules: &mut BTreeMap<RuleKey, BTreeSet<String>>, key: RuleKey, plugin: &str) {
    rules.entry(key).or_default().insert(plugin.to_owned());
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::plugins::parse_manifest_from_dir;

    fn plugin(root: &std::path::Path, id: &str, manifest: &str) -> PluginManifest {
        let dir = root.join(id);
        std::fs::create_dir(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), format!("id: {id}\n{manifest}")).unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        parse_manifest_from_dir(&dir).unwrap()
    }

    #[test]
    fn derives_hid_primary_usb_and_companion_rules() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = plugin(
            temp.path(),
            "cooler",
            "name: Cooler\npermissions: [hid, usb]\ndevices:\n  - vendor: Acme\n    model: One\n    match: { hid: { vid: 0x1234, pids: [0x0002, 0x0001] } }\ntransports:\n  hid: {}\n  usb:\n    devices:\n      - { id: primary, control: {} }\n      - { id: screen, vid: 0xabcd, pid: 0x0102, control: {} }\n",
        );

        let output = assemble(&[manifest]);
        assert!(output.contains(
            "KERNEL==\"hidraw*\", ATTRS{idVendor}==\"1234\", ATTRS{idProduct}==\"0001\""
        ));
        assert!(output
            .contains("SUBSYSTEM==\"usb\", ATTRS{idVendor}==\"1234\", ATTRS{idProduct}==\"0002\""));
        assert!(output
            .contains("SUBSYSTEM==\"usb\", ATTRS{idVendor}==\"abcd\", ATTRS{idProduct}==\"0102\""));
    }

    #[test]
    fn excludes_windows_only_plugins_and_deduplicates_rules() {
        let temp = tempfile::tempdir().unwrap();
        let linux = plugin(
            temp.path(),
            "linux_device",
            "permissions: [hid]\ndevices:\n  - vendor: Acme\n    model: One\n    match: { hid: { vid: 1, pid: 2 } }\ntransports: { hid: {} }\n",
        );
        let also_linux = plugin(
            temp.path(),
            "also_linux_device",
            "permissions: [hid]\ndevices:\n  - vendor: Acme\n    model: Two\n    match: { hid: { vid: 1, pid: 2 } }\ntransports: { hid: {} }\n",
        );
        let windows = plugin(
            temp.path(),
            "windows_device",
            "platforms: [windows]\npermissions: [hid]\ndevices:\n  - vendor: Acme\n    model: Win\n    match: { hid: { vid: 3, pid: 4 } }\ntransports: { hid: {} }\n",
        );

        let output = assemble(&[windows, also_linux, linux]);
        assert_eq!(output.matches("ATTRS{idProduct}==\"0002\"").count(), 1);
        assert!(!output.contains("ATTRS{idProduct}==\"0004\""));
    }

    #[test]
    fn baseline_hwmon_rule_is_driver_agnostic() {
        let output = assemble(&[]);
        assert!(output.contains("SUBSYSTEM==\"hwmon\""));
        assert!(output.contains("/sys%p/pwm[1-7]"));
        assert!(!output.contains("ATTR{name}"));
        assert!(!output.contains("SUBSYSTEM==\"i2c-dev\""));
    }

    #[test]
    fn derives_chipset_and_gpu_i2c_rules_from_smbus_matches() {
        let temp = tempfile::tempdir().unwrap();
        let manifest = plugin(
            temp.path(),
            "smbus_rgb",
            "permissions: [smbus]\ndevices:\n  - vendor: Acme\n    model: DRAM\n    match:\n      smbus: { bus: chipset, addresses: [0x70] }\n  - vendor: Acme\n    model: GPU\n    match:\n      smbus:\n        bus: gpu\n        addresses: [0x67]\n        pci_match:\n          - { vendor: 0x10de, sub_vendor: 0x1043 }\n          - { vendor: 0x1002, sub_vendor: 0x1043 }\n          - { vendor: 0x10de, sub_vendor: 0x19da }\n",
        );

        let output = assemble(&[manifest]);
        assert_eq!(
            output
                .matches("DRIVERS==\"i801_smbus|piix4_smbus\"")
                .count(),
            1
        );
        assert_eq!(output.matches("ATTRS{vendor}==\"0x10de\"").count(), 1);
        assert_eq!(output.matches("ATTRS{vendor}==\"0x1002\"").count(), 1);
        assert_eq!(output.matches("GROUP=\"halod\"").count(), 3);
        assert!(!output.contains("GROUP=\"i2c\""));
        assert_eq!(device_rule_count(&output), 3);
    }

    #[test]
    fn status_rule_count_ignores_baseline_rules() {
        let rules = assemble(&[]);
        assert_eq!(device_rule_count(&rules), 0);
    }

    #[test]
    fn status_uses_first_installed_file_and_detects_drift() {
        let temp = tempfile::tempdir().unwrap();
        let override_path = temp.path().join("etc.rules");
        let vendor_path = temp.path().join("usr.rules");
        std::fs::write(&override_path, "stale").unwrap();
        std::fs::write(&vendor_path, "current").unwrap();

        let paths = [override_path.as_path(), vendor_path.as_path()];
        let stale = status_at_paths("current", &[], paths);
        assert!(!stale.current);
        assert_eq!(stale.installed_path.as_deref(), override_path.to_str());

        std::fs::write(&override_path, "current").unwrap();
        assert!(status_at_paths("current", &[], paths).current);
    }

    #[test]
    fn status_identifies_only_plugins_with_missing_device_rules() {
        let temp = tempfile::tempdir().unwrap();
        let first = plugin(
            temp.path(),
            "first",
            "permissions: [hid]\ndevices:\n  - vendor: Acme\n    model: One\n    match: { hid: { vid: 1, pid: 2 } }\ntransports: { hid: {} }\n",
        );
        let second = plugin(
            temp.path(),
            "second",
            "permissions: [hid]\ndevices:\n  - vendor: Acme\n    model: Two\n    match: { hid: { vid: 3, pid: 4 } }\ntransports: { hid: {} }\n",
        );
        let installed_path = temp.path().join("installed.rules");
        std::fs::write(&installed_path, assemble(std::slice::from_ref(&first))).unwrap();

        let manifests = [first, second];
        let expected = assemble(&manifests);
        let status = status_at_paths(&expected, &manifests, [installed_path.as_path()]);

        assert!(!status.current);
        assert_eq!(status.plugins_requiring_update, ["second"]);
        assert_eq!(status.contributing_plugin_ids, ["first", "second"]);
    }
}
