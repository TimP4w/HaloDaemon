// SPDX-License-Identifier: GPL-3.0-or-later
//! Discovery for config-instantiated integration plugins (e.g. an OpenRGB SDK
//! client): unlike every other `TransportScanner`, this one never touches a
//! hardware bus. It connects to whatever host/port each enabled, permission-
//! satisfied `Integration` plugin's own config declares, builds a headless
//! `LuaDevice` root, and lets `Controller::discover_children()` enumerate the
//! individual devices the remote service reports.

use std::sync::Arc;

use anyhow::Result;

use crate::registry::discovery::{DiscoveryHandle, TransportScanner};
use crate::registry::usecases::registration::register_device_and_children;
use crate::state::AppState;

use super::device::{ChildWorkerFactory, LuaDevice};
use super::manifest::PluginManifest;
use super::transport::PluginIo;
use super::{granted_for, resolved_config_for};
use crate::drivers::transports::Transport;

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

    // Opens one fresh connection to the configured server. A real connect can
    // block for the transport's timeout, so callers run it off the async runtime.
    let open_manifest = manifest.clone();
    let open_config = config.clone();
    let open_transport: Arc<dyn Fn() -> Result<PluginIo> + Send + Sync> =
        Arc::new(move || match super::transport::descriptor_for("tcp") {
            Some(d) => (d.open)(&open_manifest, &placeholder_handle(), &open_config),
            None => anyhow::bail!("integration plugin: no 'tcp' transport backend registered"),
        });

    // Open a small pool of connections so writes to different controllers can
    // flow in parallel without overwhelming the server (one-per-controller
    // crashes some SDK servers — e.g. OpenRGB).  Round-robin controllers across
    // the pool by index.
    const POOL_SIZE: u32 = 4;
    let mut pool: Vec<Arc<dyn Transport>> = Vec::with_capacity(POOL_SIZE as usize);
    // Reuse the root's connection as pool[0].
    let root_open = open_transport.clone();
    let transport: PluginIo = match tokio::task::spawn_blocking(move || root_open()).await {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            log::warn!(
                "integration plugin '{}' connect failed: {e:#}",
                manifest.plugin_id
            );
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
    let first: Arc<dyn Transport> = match &transport {
        PluginIo::Stream { transport, .. } => transport.clone(),
        _ => {
            log::error!(
                "integration plugin '{}': root transport is not a stream",
                manifest.plugin_id
            );
            return;
        }
    };
    pool.push(first.clone());

    // Open remaining pool connections with a small stagger to avoid racing the
    // server's accept loop.
    for slot in 1..POOL_SIZE {
        let open = open_transport.clone();
        match tokio::task::spawn_blocking(move || open()).await {
            Ok(Ok(PluginIo::Stream { transport, .. })) => {
                log::info!(
                    "integration '{}': pool slot {}/{} connected",
                    manifest.plugin_id,
                    slot,
                    POOL_SIZE
                );
                pool.push(transport);
            }
            Ok(Ok(_)) => {}
            Ok(Err(e)) => log::warn!(
                "integration plugin '{}' pool connect failed: {e:#}",
                manifest.plugin_id
            ),
            Err(e) => log::warn!(
                "integration plugin '{}' pool connect task panicked: {e}",
                manifest.plugin_id
            ),
        }
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    let child_worker: ChildWorkerFactory = {
        let pool = Arc::new(pool);
        Arc::new(move |index| {
            let slot = &pool[(index % pool.len() as u32) as usize];
            Ok(PluginIo::Stream {
                transport: slot.clone(),
                bulk: None,
            })
        })
    };

    let device = Arc::new_cyclic(move |weak| {
        let mut dev = LuaDevice::integration_root(id, &manifest, transport, child_worker, runtime);
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

/// Connect and register a single integration by plugin id, for a scoped
/// reconnect (enable toggle, config change) that must not touch any other
/// device. No-op if `plugin_id` isn't currently an enabled, permission-
/// satisfied integration.
pub(crate) async fn discover_one(app: &Arc<AppState>, plugin_id: &str) {
    if let Some(manifest) = super::integration_manifest(plugin_id) {
        build_and_register(app, manifest).await;
    }
}

inventory::submit!(TransportScanner {
    name: "plugin-integrations",
    platform: None,
    scan: |app| Box::pin(discover(app)),
});
