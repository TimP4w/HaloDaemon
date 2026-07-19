// SPDX-License-Identifier: GPL-3.0-or-later
//! Host requirements inferred from plugin transports. Manifests only declare
//! presence-only command requirements that cannot be inferred from a command
//! transport (for example, the OpenRGB executable used by a TCP integration).

use std::collections::HashSet;

use halod_shared::types::{
    PluginRequirement, PluginRequirementStatus, RequirementFailureReason, RequirementImpact,
};

use super::{PluginManifest, RequirementDef};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct DerivedRequirement {
    pub requirement: PluginRequirement,
    pub impact: RequirementImpact,
    pub feature: Option<String>,
}

impl DerivedRequirement {
    fn key(&self) -> String {
        match &self.requirement {
            PluginRequirement::Command { executable } => {
                format!("command:{}", executable.trim().to_ascii_lowercase())
            }
            PluginRequirement::KernelModule { name } => {
                format!("kernel_module:{}", name.trim().to_ascii_lowercase())
            }
            PluginRequirement::PawnIo => "pawnio".into(),
            PluginRequirement::LinuxI2c => "linux_i2c".into(),
            PluginRequirement::LinuxHwmon { access } => format!("linux_hwmon:{access}"),
        }
    }
}

fn applies_to(platforms: &[String], os: &str) -> bool {
    platforms.is_empty() || platforms.iter().any(|platform| platform == os)
}

fn explicit_requirement(def: &RequirementDef) -> DerivedRequirement {
    let requirement = match def.kind {
        super::RequirementDefKind::Command => PluginRequirement::Command {
            executable: def.name.clone(),
        },
        super::RequirementDefKind::KernelModule => PluginRequirement::KernelModule {
            name: def.name.clone(),
        },
    };
    DerivedRequirement {
        requirement,
        impact: RequirementImpact::Block,
        feature: None,
    }
}

/// Derive readiness checks from the authority the plugin already declares.
pub fn derive_for(manifest: &PluginManifest, os: &str) -> Vec<DerivedRequirement> {
    if !applies_to(&manifest.platforms, os) {
        return Vec::new();
    }

    let mut out = Vec::new();
    if let Some(command) = &manifest.transports.command {
        out.extend(
            command
                .commands
                .iter()
                .map(|executable| DerivedRequirement {
                    requirement: PluginRequirement::Command {
                        executable: executable.clone(),
                    },
                    impact: RequirementImpact::Block,
                    feature: None,
                }),
        );
    }

    // Explicit command checks are for non-command integrations. If a manifest
    // redundantly lists a transport command, the inferred blocking check wins.
    out.extend(
        manifest
            .requirements
            .iter()
            .filter(|requirement| applies_to(&requirement.platforms, os))
            .map(explicit_requirement),
    );

    let uses_smbus = manifest
        .devices
        .iter()
        .any(|device| device.transport == "smbus");
    if os == "windows"
        && (uses_smbus
            || manifest.transports.amd_smn.is_some()
            || manifest.transports.lpcio.is_some())
    {
        out.push(DerivedRequirement {
            requirement: PluginRequirement::PawnIo,
            impact: RequirementImpact::Block,
            feature: None,
        });
    }
    if os == "linux" && uses_smbus {
        out.push(DerivedRequirement {
            requirement: PluginRequirement::LinuxI2c,
            impact: RequirementImpact::Block,
            feature: None,
        });
    }
    if os == "linux" && manifest.transports.hwmon.is_some() {
        out.push(DerivedRequirement {
            requirement: PluginRequirement::LinuxHwmon {
                access: "read".into(),
            },
            impact: RequirementImpact::Block,
            feature: None,
        });
        if manifest
            .capabilities
            .iter()
            .any(|capability| capability == "cooling")
        {
            out.push(DerivedRequirement {
                requirement: PluginRequirement::LinuxHwmon {
                    access: "pwm".into(),
                },
                impact: RequirementImpact::Degrade,
                feature: Some("fan_control".into()),
            });
        }
    }

    let mut seen = HashSet::new();
    out.retain(|requirement| seen.insert(requirement.key()));
    out
}

/// Evaluate requirements while allowing command resolution to be injected in
/// tests. Platform facilities use their small, independently tested probes.
pub fn evaluate_with(
    manifest: &PluginManifest,
    os: &str,
    resolve_command: &dyn Fn(&str) -> bool,
) -> Vec<PluginRequirementStatus> {
    derive_for(manifest, os)
        .into_iter()
        .map(|derived| {
            let (satisfied, reason) = match &derived.requirement {
                PluginRequirement::Command { executable } => {
                    if resolve_command(executable) {
                        (true, None)
                    } else {
                        (false, Some(RequirementFailureReason::NotFound))
                    }
                }
                PluginRequirement::KernelModule { name } => {
                    super::probes::probe_module(&super::probes::RealModuleEnv, name)
                }
                PluginRequirement::PawnIo => super::probes::probe_pawnio(),
                PluginRequirement::LinuxI2c => super::probes::probe_linux_i2c(),
                PluginRequirement::LinuxHwmon { access } => {
                    super::probes::probe_linux_hwmon(access)
                }
            };
            PluginRequirementStatus {
                requirement: derived.requirement,
                impact: derived.impact,
                satisfied,
                reason,
                feature: derived.feature,
            }
        })
        .collect()
}

pub fn evaluate(manifest: &PluginManifest) -> Vec<PluginRequirementStatus> {
    evaluate_with(manifest, std::env::consts::OS, &|executable| {
        crate::domain::plugin::engine::command_resolve::resolve(executable).is_some()
    })
}

pub fn blocking_missing(statuses: &[PluginRequirementStatus]) -> Vec<PluginRequirementStatus> {
    statuses
        .iter()
        .filter(|status| status.impact == RequirementImpact::Block && !status.satisfied)
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::{Path, PathBuf};

    fn manifest(yaml_extra: &str) -> PluginManifest {
        let tmp = tempfile::tempdir().unwrap();
        let dir = write_plugin_dir(tmp.path(), "reqplug", yaml_extra);
        super::super::parse_manifest_from_dir(&dir).unwrap()
    }

    fn write_plugin_dir(root: &Path, id: &str, yaml_extra: &str) -> PathBuf {
        let dir = root.join(id);
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(dir.join("plugin.yaml"), format!("id: {id}\n{yaml_extra}")).unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        dir
    }

    #[test]
    fn command_transport_is_automatic_and_explicit_command_is_still_supported() {
        let transport = manifest(
            "type: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [nvidia-smi]\n",
        );
        assert!(matches!(
            &derive_for(&transport, "linux")[0].requirement,
            PluginRequirement::Command { executable } if executable == "nvidia-smi"
        ));

        let integration = manifest(
            "type: integration\npermissions: [network]\ntransports:\n  tcp: {}\nrequirements:\n  - { kind: command, name: openrgb }\n",
        );
        assert!(matches!(
            &derive_for(&integration, "linux")[0].requirement,
            PluginRequirement::Command { executable } if executable == "openrgb"
        ));
    }

    #[test]
    fn explicit_kernel_module_is_retained() {
        let integration = manifest(
            "type: integration\nplatforms: [linux]\npermissions: [network]\ntransports:\n  tcp: {}\nrequirements:\n  - { kind: kernel_module, name: v4l2loopback, platforms: [linux] }\n",
        );
        assert!(matches!(
            &derive_for(&integration, "linux")[0].requirement,
            PluginRequirement::KernelModule { name } if name == "v4l2loopback"
        ));
    }

    #[test]
    fn missing_transport_command_blocks_activation() {
        let manifest = manifest(
            "type: integration\npermissions: [command]\ntransports:\n  command:\n    commands: [present, missing]\n",
        );
        let statuses = evaluate_with(&manifest, "linux", &|name| name == "present");
        let missing = blocking_missing(&statuses);
        assert_eq!(missing.len(), 1);
        assert_eq!(missing[0].reason, Some(RequirementFailureReason::NotFound));
    }

    #[test]
    fn hardware_requirements_are_inferred_from_transports() {
        let smbus = manifest(
            "platforms: [linux, windows]\npermissions: [smbus]\ndevices:\n  - vendor: Test\n    model: Chip\n    match:\n      smbus: { bus: chipset, addresses: [0x58] }\n",
        );
        assert!(matches!(
            derive_for(&smbus, "linux")[0].requirement,
            PluginRequirement::LinuxI2c
        ));
        assert!(matches!(
            derive_for(&smbus, "windows")[0].requirement,
            PluginRequirement::PawnIo
        ));

        let pawnio = manifest(
            "platforms: [windows]\npermissions: [amd_smn]\ndevices:\n  - vendor: AMD\n    model: CPU\n    match:\n      amd_smn: { any: true }\ntransports:\n  amd_smn: {}\n",
        );
        assert!(matches!(
            derive_for(&pawnio, "windows")[0].requirement,
            PluginRequirement::PawnIo
        ));
    }

    #[test]
    fn hwmon_read_and_fan_control_are_inferred() {
        let manifest = manifest(
            "type: integration\nplatforms: [linux]\npermissions: [hwmon]\ncapabilities: [sensors, cooling]\ntransports:\n  hwmon: {}\n",
        );
        let requirements = derive_for(&manifest, "linux");
        assert_eq!(requirements.len(), 2);
        assert_eq!(requirements[0].impact, RequirementImpact::Block);
        assert_eq!(requirements[1].impact, RequirementImpact::Degrade);
        assert_eq!(requirements[1].feature.as_deref(), Some("fan_control"));
    }

    #[test]
    fn unsupported_platform_has_no_requirements() {
        let manifest = manifest(
            "type: integration\nplatforms: [windows]\npermissions: [command]\ntransports:\n  command:\n    commands: [tool]\n",
        );
        assert!(derive_for(&manifest, "linux").is_empty());
    }
}
