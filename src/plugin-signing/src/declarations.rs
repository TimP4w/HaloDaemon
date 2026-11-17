// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result};
use halod_shared::types::{
    HidIdentity, PackageMatches, PackageUses, PciIdentity, Permission, UsbIdentity,
};
use serde::de::IgnoredAny;
use serde::Deserialize;

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PackageDeclarations {
    pub name: String,
    pub matches: PackageMatches,
    pub uses: PackageUses,
}

pub fn read_declarations(manifest_yaml: &[u8]) -> Result<PackageDeclarations> {
    let declared: Declared =
        serde_yaml::from_slice(manifest_yaml).context("reading package declarations")?;
    Ok(declared.into_declarations())
}

#[derive(Deserialize)]
struct Declared {
    id: String,
    #[serde(default)]
    name: Option<String>,
    #[serde(default)]
    identity: DeclaredIdentity,
    #[serde(default)]
    platforms: Vec<String>,
    #[serde(default)]
    devices: Vec<DeclaredDevice>,
    #[serde(default)]
    transports: DeclaredTransports,
    #[serde(default)]
    setup: Option<DeclaredSetup>,
    #[serde(default)]
    config: Option<DeclaredConfig>,
    #[serde(default)]
    consumes: Vec<String>,
}

#[derive(Default, Deserialize)]
struct DeclaredIdentity {
    #[serde(default)]
    name: Option<String>,
}

#[derive(Deserialize)]
struct DeclaredDevice {
    #[serde(default, rename = "match")]
    matcher: DeclaredMatch,
}

#[derive(Default, Deserialize)]
struct DeclaredMatch {
    #[serde(default)]
    hid: Option<DeclaredHid>,
    #[serde(default)]
    usb: Option<DeclaredUsb>,
    #[serde(default)]
    smbus: Option<DeclaredSmbus>,
    #[serde(default)]
    command: Option<DeclaredCommandMatch>,
    #[serde(default)]
    amd_smn: Option<IgnoredAny>,
    #[serde(default)]
    lpcio: Option<IgnoredAny>,
}

#[derive(Deserialize)]
struct DeclaredHid {
    #[serde(default)]
    any: bool,
    #[serde(default)]
    vid: Option<u16>,
    #[serde(default)]
    pid: Option<u16>,
    #[serde(default)]
    pids: Vec<u16>,
    #[serde(default)]
    usage_page: Option<u16>,
    #[serde(default)]
    usage: Option<u16>,
    #[serde(default)]
    interface: Option<i32>,
}

#[derive(Deserialize)]
struct DeclaredUsb {
    vid: u16,
    #[serde(default)]
    pid: u16,
    #[serde(default)]
    pids: Vec<u16>,
    #[serde(default)]
    interface: u8,
}

#[derive(Deserialize)]
struct DeclaredSmbus {
    bus: String,
    #[serde(default)]
    pci_match: Vec<DeclaredPci>,
}

#[derive(Deserialize)]
struct DeclaredPci {
    #[serde(default)]
    vendor: Option<u16>,
    #[serde(default)]
    device: Option<u16>,
    #[serde(default)]
    sub_vendor: Option<u16>,
    #[serde(default)]
    sub_device: Option<u16>,
}

impl DeclaredPci {
    fn identity(&self) -> PciIdentity {
        PciIdentity {
            vendor: self.vendor,
            device: self.device,
            sub_vendor: self.sub_vendor,
            sub_device: self.sub_device,
        }
    }
}

#[derive(Deserialize)]
#[serde(untagged)]
enum DeclaredCommandMatch {
    Name(String),
    Detail { command: String },
}

#[derive(Default, Deserialize)]
struct DeclaredTransports {
    #[serde(default)]
    hid: Option<IgnoredAny>,
    #[serde(default)]
    tcp: Option<IgnoredAny>,
    #[serde(default)]
    usb: Option<IgnoredAny>,
    #[serde(default)]
    command: Option<DeclaredCommands>,
    #[serde(default)]
    hwmon: Option<IgnoredAny>,
    #[serde(default)]
    serial: Option<IgnoredAny>,
    #[serde(default)]
    amd_smn: Option<IgnoredAny>,
    #[serde(default)]
    lpcio: Option<IgnoredAny>,
    #[serde(default)]
    http: Option<IgnoredAny>,
    #[serde(default)]
    udp: Option<IgnoredAny>,
    #[serde(default)]
    audio_routing: Option<IgnoredAny>,
}

#[derive(Default, Deserialize)]
struct DeclaredCommands {
    #[serde(default)]
    commands: Vec<String>,
}

#[derive(Deserialize)]
struct DeclaredSetup {
    #[serde(default)]
    discovery: DeclaredDiscovery,
    #[serde(default)]
    auth: Option<DeclaredAuth>,
}

#[derive(Default, Deserialize)]
struct DeclaredDiscovery {
    #[serde(default)]
    mdns: Vec<IgnoredAny>,
    #[serde(default)]
    ssdp: Vec<IgnoredAny>,
    #[serde(default)]
    tcp: Vec<IgnoredAny>,
}

#[derive(Deserialize)]
struct DeclaredAuth {
    kind: String,
}

#[derive(Deserialize)]
struct DeclaredConfig {
    #[serde(default)]
    fields: Vec<DeclaredField>,
}

#[derive(Deserialize)]
struct DeclaredField {
    #[serde(default)]
    secure: bool,
}

impl DeclaredMatch {
    fn permission(&self) -> Option<Permission> {
        [
            (self.hid.is_some(), Permission::Hid),
            (self.usb.is_some(), Permission::Usb),
            (self.smbus.is_some(), Permission::Smbus),
            (self.command.is_some(), Permission::Command),
            (self.amd_smn.is_some(), Permission::AmdSmn),
            (self.lpcio.is_some(), Permission::Lpcio),
        ]
        .into_iter()
        .find_map(|(declared, permission)| declared.then_some(permission))
    }
}

impl DeclaredHid {
    fn identity(&self) -> Option<HidIdentity> {
        if self.any {
            return None;
        }
        let pids = if self.pids.is_empty() {
            self.pid.into_iter().collect()
        } else {
            self.pids.clone()
        };
        (!pids.is_empty()).then_some(HidIdentity {
            vid: self.vid?,
            pids,
            usage_page: self.usage_page,
            usage: self.usage,
            interface: self.interface,
        })
    }
}

impl DeclaredUsb {
    fn identity(&self) -> Option<UsbIdentity> {
        let pids = if self.pids.is_empty() {
            vec![self.pid]
        } else {
            self.pids.clone()
        };
        (self.vid != 0 && pids.iter().all(|pid| *pid != 0)).then_some(UsbIdentity {
            vid: self.vid,
            pids,
            interface: self.interface,
        })
    }
}

impl DeclaredCommandMatch {
    fn executable(&self) -> &str {
        match self {
            Self::Name(command) | Self::Detail { command } => command,
        }
    }
}

impl Declared {
    fn into_declarations(self) -> PackageDeclarations {
        let matchers = || self.devices.iter().map(|device| &device.matcher);
        let matches = PackageMatches {
            platforms: self.platforms.clone(),
            hid_any: matchers().any(|m| m.hid.as_ref().is_some_and(|hid| hid.any)),
            hid: matchers()
                .filter_map(|m| m.hid.as_ref()?.identity())
                .collect(),
            usb: matchers()
                .filter_map(|m| m.usb.as_ref()?.identity())
                .collect(),
            smbus_gpu: matchers()
                .filter_map(|m| m.smbus.as_ref().filter(|smbus| smbus.bus == "gpu"))
                .flat_map(|smbus| smbus.pci_match.iter().map(DeclaredPci::identity))
                .collect(),
            commands: matchers()
                .filter_map(|m| Some(m.command.as_ref()?.executable().to_owned()))
                .collect(),
        };
        let uses = PackageUses {
            permissions: self.permissions(),
            commands: self
                .transports
                .command
                .as_ref()
                .map(|command| command.commands.clone())
                .unwrap_or_default(),
            data_reads: self.data_reads(),
        };
        PackageDeclarations {
            name: self
                .name
                .or(self.identity.name)
                .unwrap_or_else(|| self.id.clone()),
            matches,
            uses,
        }
    }

    fn permissions(&self) -> Vec<Permission> {
        let t = &self.transports;
        let oauth = self
            .setup
            .as_ref()
            .and_then(|setup| setup.auth.as_ref())
            .is_some_and(|auth| auth.kind == "oauth2_pkce");
        let setup_network = oauth
            || self.setup.as_ref().is_some_and(|setup| {
                !setup.discovery.mdns.is_empty()
                    || !setup.discovery.ssdp.is_empty()
                    || !setup.discovery.tcp.is_empty()
            });
        let secure_field = self
            .config
            .as_ref()
            .is_some_and(|config| config.fields.iter().any(|field| field.secure));
        let declared = [
            (t.hid.is_some(), Permission::Hid),
            (t.usb.is_some(), Permission::Usb),
            (t.command.is_some(), Permission::Command),
            (t.hwmon.is_some(), Permission::Hwmon),
            (t.serial.is_some(), Permission::Serial),
            (t.amd_smn.is_some(), Permission::AmdSmn),
            (t.lpcio.is_some(), Permission::Lpcio),
            (
                t.tcp.is_some() || t.http.is_some() || t.udp.is_some() || setup_network,
                Permission::Network,
            ),
            (t.audio_routing.is_some(), Permission::AudioRouting),
            (oauth || secure_field, Permission::SecureStorage),
        ];
        let mut permissions: Vec<Permission> = self
            .devices
            .iter()
            .filter_map(|device| device.matcher.permission())
            .chain(declared.into_iter().filter_map(|(on, p)| on.then_some(p)))
            .collect();
        permissions.sort_unstable();
        permissions.dedup();
        permissions
    }

    fn data_reads(&self) -> Vec<String> {
        let own = format!("{}.", self.id);
        let mut reads: Vec<String> = self
            .consumes
            .iter()
            .filter(|key| !key.starts_with(&own))
            .cloned()
            .collect();
        reads.sort();
        reads.dedup();
        reads
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn declarations(extra: &str) -> PackageDeclarations {
        read_declarations(format!("id: demo\nversion: 1.0.0\napi: '3.0'\n{extra}").as_bytes())
            .unwrap()
    }

    #[test]
    fn name_falls_back_from_top_level_to_identity_to_id() {
        assert_eq!(declarations("name: Top\n").name, "Top");
        assert_eq!(declarations("identity: { name: Inner }\n").name, "Inner");
        assert_eq!(declarations("").name, "demo");
    }

    #[test]
    fn permissions_follow_declarations() {
        let cases = [
            ("devices:\n  - match: { hid: { vid: 1, pid: 2 } }\n", vec![Permission::Hid]),
            ("transports:\n  http: { origins: [https://a.example] }\n", vec![Permission::Network]),
            ("transports:\n  audio_routing: {}\n", vec![Permission::AudioRouting]),
            (
                "transports:\n  http: { origins: [https://a.example] }\nconfig:\n  fields:\n    - { key: token, label: Token, secure: true }\n",
                vec![Permission::Network, Permission::SecureStorage],
            ),
            (
                "setup:\n  modes: [automatic]\n  discovery: { mdns: [_x._tcp.local.] }\n",
                vec![Permission::Network],
            ),
            (
                "setup:\n  modes: [automatic]\n  discovery: { tcp: [\"127.0.0.1:6742\"] }\n",
                vec![Permission::Network],
            ),
            (
                "setup:\n  modes: [manual]\n  auth: { kind: oauth2_pkce }\n",
                vec![Permission::Network, Permission::SecureStorage],
            ),
            ("", vec![]),
        ];
        for (yaml, expected) in cases {
            assert_eq!(declarations(yaml).uses.permissions, expected, "{yaml}");
        }
    }

    #[test]
    fn data_reads_skip_own_keys_and_are_sorted_once() {
        let uses = declarations("consumes: [demo.x, b.y, a.z, b.y]\n").uses;
        assert_eq!(uses.data_reads, ["a.z", "b.y"]);
    }

    #[test]
    fn wildcards_and_vendor_only_matches_are_not_identities() {
        let matches = declarations(
            "devices:\n  - match: { hid: { any: true } }\n  - match: { hid: { vid: 1 } }\n  - match: { hid: { vid: 1, pid: 2, usage_page: 0xff00 } }\n  - match: { usb: { vid: 3, pids: [4, 5], interface: 2 } }\n  - match: { smbus: { bus: chipset, addresses: [0x20] } }\n  - match: { smbus: { bus: gpu, addresses: [0x67], pci_match: [{ vendor: 0x10de, confirmed: true }] } }\n  - match: { command: nvidia-smi }\n  - match: { command: { command: liquidctl } }\n",
        )
        .matches;
        assert_eq!(
            matches.hid,
            [HidIdentity {
                vid: 1,
                pids: vec![2],
                usage_page: Some(0xff00),
                usage: None,
                interface: None,
            }]
        );
        assert_eq!(
            matches.usb,
            [UsbIdentity {
                vid: 3,
                pids: vec![4, 5],
                interface: 2,
            }]
        );
        assert_eq!(matches.smbus_gpu[0].vendor, Some(0x10de));
        assert_eq!(matches.smbus_gpu.len(), 1);
        assert_eq!(matches.commands, ["nvidia-smi", "liquidctl"]);
    }
}
