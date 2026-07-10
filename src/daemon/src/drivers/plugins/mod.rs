// SPDX-License-Identifier: GPL-3.0-or-later
//! Device plugins: add a device without recompiling the daemon.
//!
//! A plugin is a Lua script in the plugins directory that declares a `match`
//! (which hardware it drives) and an `identity`, plus — in later steps —
//! callbacks that turn capability calls into transport bytes. Plugins expose
//! only *existing* capability kinds; Halo owns the capability taxonomy.
//!
//! Registration is at runtime (not the compile-time `inventory` path native
//! drivers use): `load_all` reads the plugins directory into `PLUGIN_REGISTRY`,
//! and `make_device` consults `match_handle` before the native descriptors, so
//! a plugin shadows a native driver for the same hardware.

mod backends;
mod bytebuf;
mod chain_leaf;
mod device;
mod manifest;
mod sandbox;
mod transport;
mod transport_api;
mod worker;

pub use device::LuaDevice;
pub use manifest::{parse_manifest, PluginManifest, ProbeMode};
pub use scan::plugin_smbus_scan_entries;
pub use worker::run_pre_scan;

use std::collections::HashSet;
use std::path::Path;
use std::sync::{Arc, RwLock};

use halod_shared::types::PluginInfo;

use crate::drivers::Device;
use crate::registry::discovery::DiscoveryHandle;

mod scan;

#[cfg(test)]
mod corsair_test;
#[cfg(test)]
mod ene_test;
#[cfg(test)]
mod zotac_test;

static PLUGIN_REGISTRY: RwLock<Vec<PluginManifest>> = RwLock::new(Vec::new());
/// Plugin ids the user disabled. `match_handle` skips these, so a disabled
/// plugin no longer shadows its native driver.
static DISABLED: RwLock<Option<HashSet<String>>> = RwLock::new(None);

/// Replace the disabled-plugin set (from `config.plugins_disabled`).
pub fn set_disabled(ids: &[String]) {
    *DISABLED.write().expect("plugin disabled set poisoned") = Some(ids.iter().cloned().collect());
}

fn is_disabled(plugin_id: &str) -> bool {
    DISABLED
        .read()
        .ok()
        .and_then(|g| g.as_ref().map(|s| s.contains(plugin_id)))
        .unwrap_or(false)
}

/// Every loaded plugin with its enable state, for the management UI.
pub fn list() -> Vec<PluginInfo> {
    let registry = match PLUGIN_REGISTRY.read() {
        Ok(g) => g,
        Err(_) => return Vec::new(),
    };
    registry
        .iter()
        .map(|m| PluginInfo {
            id: m.plugin_id.clone(),
            name: m.display_name().to_owned(),
            path: m.source_path.display().to_string(),
            capabilities: m.capability_labels(),
            enabled: !is_disabled(&m.plugin_id),
            author: m.author().to_owned(),
            version: m.version().to_owned(),
            description: m.description().to_owned(),
            targets: m.target_labels(),
            builtin: is_builtin(&m.plugin_id),
        })
        .collect()
}

/// A plugin compiled into the daemon binary (built-in), keyed by its stem.
/// Built-ins have no on-disk script in the plugins directory, so they can be
/// disabled but never deleted through the GUI.
pub fn is_builtin(plugin_id: &str) -> bool {
    BUILTIN_PLUGINS
        .iter()
        .any(|(name, _)| Path::new(name).file_stem().and_then(|s| s.to_str()) == Some(plugin_id))
}

/// Plugins shipped inside the daemon binary, loaded before directory plugins.
/// These replace what used to be native Rust drivers (e.g. ENE SMBus RGB); the
/// user can disable them like any other plugin.
const BUILTIN_PLUGINS: &[(&str, &str)] = &[
    ("ene_smbus.lua", include_str!("builtins/ene_smbus.lua")),
    (
        "corsair_dram.lua",
        include_str!("builtins/corsair_dram.lua"),
    ),
    (
        "zotac_spectra_gpu.lua",
        include_str!("builtins/zotac_spectra_gpu.lua"),
    ),
];

fn builtin_manifests() -> Vec<PluginManifest> {
    BUILTIN_PLUGINS
        .iter()
        .filter_map(|(name, src)| match parse_manifest(src, Path::new(name)) {
            Ok(m) => Some(m),
            Err(e) => {
                log::error!("built-in plugin '{name}' failed to parse: {e:#}");
                None
            }
        })
        .collect()
}

/// (Re)load the built-in plugins plus every `*.lua` in `dir` into the registry,
/// replacing prior contents. A malformed plugin is logged and skipped — it never
/// aborts loading or the daemon. Missing directory is normal (no plugins installed).
pub fn load_all(dir: &Path) {
    let mut manifests = builtin_manifests();
    match std::fs::read_dir(dir) {
        Ok(entries) => {
            for entry in entries.flatten() {
                let path = entry.path();
                if path.extension().and_then(|e| e.to_str()) != Some("lua") {
                    continue;
                }
                match load_one(&path) {
                    Ok(m) => {
                        log::info!(
                            "Loaded device plugin '{}' from {}",
                            m.plugin_id,
                            path.display()
                        );
                        manifests.push(m);
                    }
                    Err(e) => log::warn!("Skipping plugin {}: {e:#}", path.display()),
                }
            }
        }
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
            log::debug!("No plugins directory at {}", dir.display());
        }
        Err(e) => log::warn!("Cannot read plugins directory {}: {e}", dir.display()),
    }
    *PLUGIN_REGISTRY.write().expect("plugin registry poisoned") = manifests;
}

fn load_one(path: &Path) -> anyhow::Result<PluginManifest> {
    let source = std::fs::read_to_string(path)?;
    parse_manifest(&source, path)
}

/// Stable device id from a matched manifest + handle (suffix per transport).
fn device_id(
    manifest: &PluginManifest,
    spec: &manifest::MatchSpec,
    handle: &DiscoveryHandle<'_>,
) -> String {
    let suffix = transport::descriptor_for(&spec.transport)
        .map(|d| (d.id_suffix)(handle))
        .unwrap_or_else(|| "0".to_owned());
    format!("{}-{}", manifest.id_prefix(), suffix)
}

/// Build a device from a matched manifest, the spec that matched, and the
/// handle. Device-only plugins need no runtime/transport; capability plugins
/// open their transport and spawn a worker. Returns `None` if the transport
/// can't be opened (so a native driver can still claim the hardware).
fn build_device(
    manifest: &PluginManifest,
    spec: &manifest::MatchSpec,
    handle: &DiscoveryHandle<'_>,
) -> Option<Arc<dyn Device>> {
    let id = device_id(manifest, spec, handle);
    if !manifest.needs_worker() {
        return Some(Arc::new(LuaDevice::device_only(id, manifest, spec)));
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        log::warn!(
            "plugin '{}' needs a worker but no runtime is available",
            manifest.plugin_id
        );
        return None;
    };
    let transport =
        match transport::descriptor_for(&spec.transport).map(|d| (d.open)(manifest, handle)) {
            Some(Ok(t)) => t,
            Some(Err(e)) => {
                log::warn!(
                    "plugin '{}' transport open failed: {e:#}",
                    manifest.plugin_id
                );
                return None;
            }
            None => return None,
        };

    let dev_match = worker::DevMatch {
        transport: spec.transport.clone(),
        bus: spec.bus.clone(),
        addr: match handle {
            DiscoveryHandle::Smbus { addr, .. } => Some(*addr),
            _ => None,
        },
    };

    // `new_cyclic` so the device can hand its children a `FanHub` back-reference
    // (the chain machinery, mirrored on the native NZXT Kraken).
    let device = Arc::new_cyclic(|weak| {
        let mut dev = LuaDevice::with_transport(id, manifest, spec, dev_match, transport, runtime);
        dev.set_self_ref(weak.clone());
        dev
    });
    if manifest.chain.is_some() {
        let adapter: Arc<dyn crate::drivers::chain::ChainAdapter> = device.clone();
        let host = crate::drivers::chain::ChainHost::new(
            adapter,
            crate::drivers::CHAIN_LINK_KIND_NZXT_ARGB,
        );
        device.install_chain_host(host);
    }
    Some(device as Arc<dyn Device>)
}

/// Match a handle against a given manifest slice (pure — used by tests and by
/// [`match_handle`]).
pub fn match_in(
    manifests: &[PluginManifest],
    handle: &DiscoveryHandle<'_>,
) -> Option<Arc<dyn Device>> {
    manifests
        .iter()
        .filter(|m| !is_disabled(&m.plugin_id))
        .find_map(|m| m.match_spec_for(handle).map(|spec| (m, spec)))
        .and_then(|(m, spec)| build_device(m, spec, handle))
}

/// Match a discovery handle against every loaded plugin. Consulted by
/// `make_device` *before* the native descriptors so a plugin shadows native.
pub fn match_handle(handle: &DiscoveryHandle<'_>) -> Option<Arc<dyn Device>> {
    let registry = PLUGIN_REGISTRY.read().ok()?;
    match_in(&registry, handle)
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::Path;

    fn manifest() -> PluginManifest {
        let src = r#"
            return {
              match = { transport = "hid", vid = 0x1234, pid = 0x5678 },
              identity = { vendor = "Acme", model = "K1", name = "Acme K1" },
            }
        "#;
        parse_manifest(src, Path::new("acme_k1.lua")).unwrap()
    }

    fn hid<'a>(vid: u16, pid: u16, serial: Option<&'a str>, idx: usize) -> DiscoveryHandle<'a> {
        DiscoveryHandle::Hid {
            vid,
            pid,
            path: "p",
            serial,
            idx,
            usage_page: 0,
            usage: 0,
            interface_number: None,
        }
    }

    #[test]
    fn matching_handle_builds_device_with_identity() {
        let manifests = vec![manifest()];
        let dev = match_in(&manifests, &hid(0x1234, 0x5678, Some("SER"), 0)).expect("matches");
        assert_eq!(dev.vendor(), "Acme");
        assert_eq!(dev.name(), "Acme K1");
        assert_eq!(dev.id(), "acme_k1-SER");
    }

    #[test]
    fn device_id_falls_back_to_index_without_serial() {
        let manifests = vec![manifest()];
        let dev = match_in(&manifests, &hid(0x1234, 0x5678, None, 3)).expect("matches");
        assert_eq!(dev.id(), "acme_k1-3");
    }

    #[test]
    fn non_matching_handle_returns_none() {
        let manifests = vec![manifest()];
        assert!(match_in(&manifests, &hid(0x9999, 0x0000, None, 0)).is_none());
    }

    #[test]
    fn disabled_plugin_does_not_match() {
        // Unique id so toggling DISABLED can't perturb other parallel tests.
        let src = r#"
            return {
              match = { transport = "hid", vid = 0xAAAA, pid = 0xBBBB },
              identity = { vendor = "Acme", model = "K1" },
            }
        "#;
        let manifests = vec![parse_manifest(src, Path::new("disabled_only_plugin.lua")).unwrap()];
        let handle = hid(0xAAAA, 0xBBBB, Some("S"), 0);
        set_disabled(&["disabled_only_plugin".to_string()]);
        assert!(
            match_in(&manifests, &handle).is_none(),
            "disabled plugin must not shadow native"
        );
        set_disabled(&[]);
        assert!(match_in(&manifests, &handle).is_some());
    }

    #[test]
    fn ene_smbus_is_builtin_others_are_not() {
        assert!(is_builtin("ene_smbus"));
        assert!(!is_builtin("wled_udp"));
        assert!(!is_builtin("ene_smbus.lua")); // stem only, not the file name
    }

    #[test]
    fn shipped_example_plugin_parses() {
        // Guards the documented example against drift with the manifest schema.
        let src = include_str!("../../../../../plugins/examples/example_device.lua");
        let m = parse_manifest(src, Path::new("example_device.lua")).unwrap();
        assert_eq!(m.identity.vendor, "Example");
        assert_eq!(m.capability_labels(), vec!["RGB", "Fan", "Sensor"]);
        assert!(m.needs_worker());
        assert_eq!(m.poll.as_ref().map(|p| p.interval_ms), Some(500));
    }

    #[test]
    fn capability_labels_reflect_manifest_sections() {
        let src = r#"
            return {
              match = { transport = "hid", vid = 1, pid = 2 },
              identity = { vendor = "V", model = "M" },
              rgb = { zones = {} },
              sensor = {},
            }
        "#;
        let m = parse_manifest(src, Path::new("caps.lua")).unwrap();
        assert_eq!(m.capability_labels(), vec!["RGB", "Sensor"]);
    }
}
