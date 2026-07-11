// SPDX-License-Identifier: GPL-3.0-or-later
//! Discovery for config-instantiated integration plugins (e.g. an OpenRGB SDK
//! client): unlike every other `TransportScanner`, this one never touches a
//! hardware bus. It connects to whatever host/port each enabled, permission-
//! satisfied `Integration` plugin's own config declares, builds a headless
//! `LuaDevice` root, and lets `Controller::discover_children()` enumerate the
//! individual devices the remote service reports.

use std::sync::Arc;

use crate::registry::discovery::{DiscoveryHandle, TransportScanner};
use crate::registry::usecases::registration::register_device_and_children;
use crate::state::AppState;

use super::device::LuaDevice;
use super::manifest::PluginManifest;
use super::transport::PluginIo;
use super::{granted_for, resolved_config_for};

/// Sanitize a config value for use in a device id: keep it stable and
/// collision-resistant without leaking odd characters into the id.
fn sanitize(value: &str) -> String {
    value
        .chars()
        .map(|c| if c.is_ascii_alphanumeric() { c } else { '_' })
        .collect()
}

/// Stable device id for an integration root, derived from its own config
/// rather than a discovery handle (it has none) — so two servers configured
/// for the same plugin (a future multi-instance setup) can't collide.
fn root_device_id(
    manifest: &PluginManifest,
    config: &std::collections::HashMap<String, String>,
) -> String {
    let tcp = manifest.transports.tcp.clone().unwrap_or_default();
    let host = config.get(&tcp.host_key).cloned().unwrap_or_default();
    let port = config.get(&tcp.port_key).cloned().unwrap_or_default();
    format!(
        "{}-{}_{}",
        manifest.id_prefix(),
        sanitize(&host),
        sanitize(&port)
    )
}

/// A harmless placeholder handle: the `tcp` transport backend's `open` never
/// reads it (it reads host/port from the plugin's config instead), but the
/// shared `PluginTransportDescriptor::open` signature requires one.
fn placeholder_handle<'a>() -> DiscoveryHandle<'a> {
    DiscoveryHandle::Hid {
        vid: 0,
        pid: 0,
        path: "",
        serial: None,
        idx: 0,
        usage_page: 0,
        usage: 0,
        interface_number: None,
    }
}

async fn build_and_register(app: &Arc<AppState>, manifest: PluginManifest) {
    let Ok(runtime) = tokio::runtime::Handle::try_current() else {
        log::warn!(
            "integration plugin '{}' needs a runtime but none is available",
            manifest.plugin_id
        );
        return;
    };
    let granted = granted_for(&manifest.plugin_id);
    let config = resolved_config_for(&manifest.plugin_id, &granted);
    let id = root_device_id(&manifest, &config);

    if app.devices.read().await.iter().any(|d| d.id() == id) {
        return;
    }

    // The connect can block for the plugin's full timeout (default 5s) — run
    // it on a blocking-pool thread so a slow/unreachable server only stalls
    // this one scanner pass, not the whole async runtime.
    let open_manifest = manifest.clone();
    let open_config = config.clone();
    let opened = tokio::task::spawn_blocking(move || {
        super::transport::descriptor_for("tcp")
            .map(|d| (d.open)(&open_manifest, &placeholder_handle(), &open_config))
    })
    .await;

    let transport: PluginIo = match opened {
        Ok(Some(Ok(t))) => t,
        Ok(Some(Err(e))) => {
            log::warn!(
                "integration plugin '{}' connect failed: {e:#}",
                manifest.plugin_id
            );
            return;
        }
        Ok(None) => {
            log::error!("integration plugin '{}': no 'tcp' transport backend registered — this is a daemon bug", manifest.plugin_id);
            return;
        }
        Err(e) => {
            log::warn!(
                "integration plugin '{}': connect task panicked: {e}",
                manifest.plugin_id
            );
            return;
        }
    };

    let device = Arc::new_cyclic(|weak| {
        let mut dev = LuaDevice::integration_root(id, &manifest, transport, runtime);
        dev.set_self_ref(weak.clone());
        dev
    });
    register_device_and_children(app, device).await;
}

async fn discover(app: Arc<AppState>) {
    for manifest in super::integration_manifests() {
        build_and_register(&app, manifest).await;
    }
}

inventory::submit!(TransportScanner {
    name: "plugin-integrations",
    platform: None,
    scan: |app| Box::pin(discover(app)),
});
