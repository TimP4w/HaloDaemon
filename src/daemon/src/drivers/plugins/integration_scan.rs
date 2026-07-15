// SPDX-License-Identifier: GPL-3.0-or-later
//! Discovery for config-instantiated integration plugins (e.g. an OpenRGB SDK
//! client): unlike every other `TransportScanner`, this one never touches a
//! hardware bus. It connects to whatever host/port each enabled, permission-
//! satisfied `Integration` plugin's own config declares, builds a headless
//! `LuaDevice` root, and lets `Controller::discover_children()` enumerate the
//! individual devices the remote service reports.

use std::sync::Arc;

use anyhow::Result;
use halod_shared::types::Permission;

use crate::ipc::broadcast_state;
use crate::registry::discovery::{DiscoveryHandle, TransportScanner};
use crate::registry::usecases::registration::register_device_and_children;
use crate::state::AppState;

use super::device::{LuaDevice, LuaDeviceParts, LuaDeviceSpawnParts, LuaDeviceWorker};
use super::manifest::PluginManifest;
use super::transport::PluginIo;

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

/// Open one fresh `tcp` connection to the configured server. Blocks for the
/// transport timeout, so callers run it off-runtime. Shared by discovery and the
/// reconnect watcher's probe.
pub(super) fn open_probe(
    manifest: &PluginManifest,
    config: &std::collections::HashMap<String, String>,
    granted: &[Permission],
) -> Result<PluginIo> {
    match super::transport::descriptor_for("tcp") {
        Some(d) => (d.open)(manifest, &placeholder_handle(), config, granted, None),
        None => anyhow::bail!("integration plugin: no 'tcp' transport backend registered"),
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
    let plugin_id = manifest.plugin_id.clone();
    let granted = app.registry.granted_for(&manifest.plugin_id);
    let config =
        app.registry
            .resolved_config_for(app.secret_store.as_ref(), &manifest.plugin_id, &granted);
    let id = root_device_id(&manifest, &config);

    if app.devices.read().await.iter().any(|d| d.id() == id) {
        return;
    }

    // Opens one fresh connection to the configured server. A real connect can
    // block for the transport's timeout, so callers run it off the async runtime.
    let open_manifest = manifest.clone();
    let open_config = config.clone();
    let open_granted = granted.clone();
    let open_transport: Arc<dyn Fn() -> Result<PluginIo> + Send + Sync> =
        Arc::new(move || open_probe(&open_manifest, &open_config, &open_granted));

    // Created before the transport opens, so `OpeningTransport` is real — if
    // connect fails, this Arc is simply dropped unobserved.
    let runtime_state = Arc::new(std::sync::Mutex::new(
        super::device::RuntimeState::OpeningTransport,
    ));

    // Drive every controller over the *one* root connection: a slot-shared
    // `Transport` serialises each controller's frame write behind the same
    // socket lock, so sibling controllers (e.g. the sticks of a DRAM kit) land
    // in the server's per-controller queues back-to-back and stay in phase
    // instead of drifting across independent connections. One connection is
    // also the safe case for servers that crash when a client opens one socket
    // per controller (e.g. OpenRGB).
    let transport: PluginIo = match tokio::task::spawn_blocking({
        let root_open = open_transport.clone();
        move || root_open()
    })
    .await
    {
        Ok(Ok(t)) => t,
        Ok(Err(e)) => {
            log::warn!(
                "integration plugin '{}' connect failed: {e:#}",
                manifest.plugin_id
            );
            report_connect_failure(app, &manifest, format!("{e:#}")).await;
            return;
        }
        Err(e) => {
            log::warn!(
                "integration plugin '{}': connect task panicked: {e}",
                manifest.plugin_id
            );
            report_connect_failure(app, &manifest, format!("connect task panicked: {e}")).await;
            return;
        }
    };
    // The user may have disabled the integration while the blocking connect
    // was in flight. Drop the newly-opened transport and never register a root
    // from that stale activation attempt.
    if app.registry.integration_manifest(&plugin_id).is_none() {
        return;
    }
    match &transport {
        PluginIo::Stream { .. } => {}
        _ => {
            log::error!(
                "integration plugin '{}': root transport is not a stream",
                manifest.plugin_id
            );
            report_connect_failure(app, &manifest, "root transport is not a stream".into()).await;
            return;
        }
    }

    let notify = Arc::downgrade(app);
    let device = Arc::new_cyclic(move |weak| {
        let mut dev = LuaDevice::new(LuaDeviceParts {
            id,
            manifest: &manifest,
            spec: None,
            notify,
            runtime: Some(runtime_state),
            worker: LuaDeviceWorker::Spawn(Box::new(LuaDeviceSpawnParts {
                dev_match: super::worker::DevMatch {
                    transport: "tcp".to_owned(),
                    ..Default::default()
                },
                transport,
                handle: runtime,
                granted,
                config,
            })),
        });
        dev.set_self_ref(weak.clone());
        dev
    });
    register_device_and_children(app, device).await;
    app.registry.clear_connect_error(&plugin_id);
}

/// Emit a deduplicated connect-failure notification + persisted plugin issue,
/// then push a fresh state frame so the plugin page reflects it immediately.
async fn report_connect_failure(app: &Arc<AppState>, manifest: &PluginManifest, detail: String) {
    app.registry
        .report_connect_error(app, &manifest.plugin_id, &manifest.display_name(), detail)
        .await;
    broadcast_state(app).await;
}

async fn discover(app: Arc<AppState>) {
    for manifest in app.registry.integration_manifests() {
        build_and_register(&app, manifest).await;
    }
}

/// Connect and register a single integration by plugin id, for a scoped
/// reconnect (enable toggle, config change) that must not touch any other
/// device. No-op if `plugin_id` isn't currently an enabled, permission-
/// satisfied integration.
pub(crate) async fn discover_one(app: &Arc<AppState>, plugin_id: &str) {
    if let Some(manifest) = app.registry.integration_manifest(plugin_id) {
        build_and_register(app, manifest).await;
    }
}

inventory::submit!(TransportScanner {
    name: "plugin-integrations",
    detail: halod_shared::types::DiscoveryDetail::PluginIntegrations,
    platform: None,
    scan: |app| Box::pin(discover(app)),
});
