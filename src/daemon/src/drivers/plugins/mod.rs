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

mod bytebuf;
mod device;
mod manifest;
mod sandbox;
mod transport_api;
mod worker;

pub use device::LuaDevice;
pub use manifest::{parse_manifest, PluginManifest};

use std::path::Path;
use std::sync::{Arc, RwLock};

use crate::drivers::transports::hid::HidTransport;
use crate::drivers::transports::Transport;
use crate::drivers::Device;
use crate::registry::discovery::DiscoveryHandle;

static PLUGIN_REGISTRY: RwLock<Vec<PluginManifest>> = RwLock::new(Vec::new());

/// (Re)load every `*.lua` in `dir` into the registry, replacing prior contents.
/// A malformed plugin is logged and skipped — it never aborts loading or the
/// daemon. Missing directory is normal (no plugins installed).
pub fn load_all(dir: &Path) {
    let mut manifests = Vec::new();
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

/// Stable device id from a matched manifest + handle.
fn device_id(manifest: &PluginManifest, handle: &DiscoveryHandle<'_>) -> String {
    let suffix = match handle {
        DiscoveryHandle::Hid {
            serial: Some(s), ..
        } => (*s).to_owned(),
        DiscoveryHandle::Hid { idx, .. } => idx.to_string(),
        _ => "0".to_owned(),
    };
    format!("{}-{}", manifest.id_prefix(), suffix)
}

/// Open the HID transport a plugin declares against the matched handle's path.
fn open_hid_transport(
    manifest: &PluginManifest,
    handle: &DiscoveryHandle<'_>,
) -> anyhow::Result<Arc<dyn Transport>> {
    let DiscoveryHandle::Hid { path, .. } = handle else {
        anyhow::bail!("plugin '{}' matched a non-HID handle", manifest.plugin_id);
    };
    let hid = manifest.transports.hid.clone().unwrap_or_default();
    let transport = HidTransport::open(
        path,
        Some(hid.report_size),
        hid.timeout_ms,
        hid.feature_report,
        None,
    )?;
    Ok(Arc::new(transport))
}

/// Build a device from a matched manifest and the handle that matched it.
/// Device-only plugins need no runtime/transport; capability plugins open their
/// transport and spawn a worker. Returns `None` if the transport can't be opened
/// (so a native driver can still claim the hardware).
fn build_device(
    manifest: &PluginManifest,
    handle: &DiscoveryHandle<'_>,
) -> Option<Arc<dyn Device>> {
    let id = device_id(manifest, handle);
    if !manifest.needs_worker() {
        return Some(Arc::new(LuaDevice::device_only(id, manifest)));
    }
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        log::warn!(
            "plugin '{}' needs a worker but no runtime is available",
            manifest.plugin_id
        );
        return None;
    };
    match open_hid_transport(manifest, handle) {
        Ok(transport) => Some(Arc::new(LuaDevice::with_transport(
            id, manifest, transport, runtime,
        ))),
        Err(e) => {
            log::warn!(
                "plugin '{}' transport open failed: {e:#}",
                manifest.plugin_id
            );
            None
        }
    }
}

/// Match a handle against a given manifest slice (pure — used by tests and by
/// [`match_handle`]).
pub fn match_in(
    manifests: &[PluginManifest],
    handle: &DiscoveryHandle<'_>,
) -> Option<Arc<dyn Device>> {
    manifests
        .iter()
        .find(|m| m.match_spec.matches(handle))
        .and_then(|m| build_device(m, handle))
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
}
