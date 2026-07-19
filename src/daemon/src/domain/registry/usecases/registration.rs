// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::device::Device;
use halod_shared::types::{ConnectionType, VisibilityState, DEFAULT_PROFILE_NAME};

/// Close a device after releasing any global input state it owns. Every
/// registry removal path must use this instead of calling `Device::close`
/// directly so a disconnected layer-shift key cannot remain latched.
pub async fn close_device(app: &Arc<AppState>, device: &Arc<dyn Device>) {
    app.input.layer_shift_clear_device(device.id());
    device.close().await;
}

/// Load the device's effective saved state, then re-apply its per-zone RGB
/// transforms (which `load_state` does not cover). Active devices only: state
/// restoration may perform hardware I/O and must never run for a disabled one.
pub(super) async fn restore_saved_state(app: &Arc<AppState>, device: &Arc<dyn Device>) {
    let saved = {
        let cfg = app.config.read().await;
        Some(cfg.effective_device_state(device.id())).filter(|v| !v.is_null())
    };
    if let Some(state) = saved {
        log::debug!("[{}] restoring saved state", device.name());
        device.load_state(&state).await;
    }
    if let Some(rgb) = device.as_lighting() {
        let transforms = {
            let cfg = app.config.read().await;
            cfg.device_transforms
                .get(device.id())
                .cloned()
                .unwrap_or_default()
        };
        if !transforms.is_empty() {
            rgb.set_zone_transforms(transforms);
        }
    }
}

/// Empty every engine-participation slot so engines treat the device as
/// non-participating. Shared by the disabled-register and visibility paths.
pub(crate) fn clear_engine_slots(device: &Arc<dyn Device>) {
    if let Some(s) = device.as_lighting() {
        s.set_canvas_zones(vec![]);
    }
    if let Some(s) = device.as_cooling() {
        s.clear_curves();
    }
    if let Some(s) = device.as_lcd() {
        s.set_lcd_template_id(None);
    }
}

/// If the device is marked `Disabled` in config, push it to `app.device_registry`
/// without calling `initialize()` and return `true`.
/// Returns `false` when not disabled so the caller can proceed with init.
pub async fn register_if_disabled(app: &Arc<AppState>, device: &Arc<dyn Device>) -> bool {
    let is_disabled = {
        let cfg = app.config.read().await;
        cfg.known_devices
            .get(device.id())
            .map(|r| r.active_state == VisibilityState::Disabled)
            .unwrap_or(false)
    };
    if !is_disabled {
        return false;
    }
    device.set_active_state(VisibilityState::Disabled);
    // Constructors may seed participation slots even though saved state is not
    // restored here. Keep the disabled placeholder inert for every engine.
    clear_engine_slots(device);
    log::info!(
        "[{}] registered as disabled, skipping initialize()",
        device.name()
    );
    app.device_registry.write().await.push(device.clone());
    true
}

/// Seed a keyboard device's layout slot from persisted config before
/// `initialize()`, so the device resolves the right variant/language and LED
/// positions on its first init. No-op for devices without the slot.
pub async fn seed_keyboard_layout(app: &Arc<AppState>, device: &Arc<dyn Device>) {
    let Some(slot) = device.keyboard_layout_slot() else {
        return;
    };
    let selection = {
        let cfg = app.config.read().await;
        cfg.keyboard_layouts.get(device.id()).copied()
    };
    if let Some(selection) = selection {
        slot.set_selection(selection);
    }
}

/// Run `device.initialize()` and surface failures as a user-visible error notification.
/// Returns the original result so callers keep existing Ok/Err branching.
pub async fn init_device(app: &Arc<AppState>, device: &Arc<dyn Device>) -> Result<bool> {
    match device.initialize().await {
        Ok(v) => {
            if let Some(plugin_id) = device.owning_plugin_id() {
                app.registry.clear_init_error(&plugin_id, device.id());
            }
            Ok(v)
        }
        Err(e) => {
            if let Some(plugin_id) = device.owning_plugin_id() {
                app.registry.report_init_error(
                    &plugin_id,
                    device.id(),
                    format!("{}: {e:#}", device.name()),
                );
            }
            crate::application::notifications::send(
                app,
                halod_shared::types::NotificationCode::DeviceInitFailed {
                    device: device.name().to_string(),
                    detail: e.to_string(),
                },
            )
            .await;
            Err(e)
        }
    }
}

/// Seed `default` with `device`'s current state if it has none yet, so every
/// device always has a baseline to inherit from. No-op otherwise.
async fn ensure_default_baseline(app: &Arc<AppState>, device: &dyn Device) {
    let device_id = device.id().to_owned();
    let already_seeded = {
        let cfg = app.config.read().await;
        cfg.profiles
            .get(DEFAULT_PROFILE_NAME)
            .map(|p| p.device_states.contains_key(&device_id))
            .unwrap_or(false)
    };
    if already_seeded {
        return;
    }
    let baseline = device.save_state().await;
    if baseline.is_null() {
        return;
    }
    log::debug!("[{device_id}] seeding default-profile baseline");
    {
        let mut cfg = app.config.write().await;
        cfg.profiles
            .entry(DEFAULT_PROFILE_NAME.to_string())
            .or_default()
            .device_states
            .insert(device_id, baseline);
    }
    app.request_config_save();
}

/// Register a device through the full discovery lifecycle:
///   1. Dedup    — skip silently if same id already in app.device_registry
///   2. Disabled — register without init if user-disabled
///   3. Init     — initialize; skip if Err or Ok(false)
///   4. State    — load saved state (sensor visibility, fan curves, …)
///   5. Push     — add to app.device_registry
///
/// Returns `true` when the device is now active in app.device_registry.
pub async fn register_device(app: &Arc<AppState>, device: Arc<dyn Device>) -> bool {
    let device_id = device.id().to_owned();
    if !claim_registration(app, &device_id).await {
        return false;
    }
    if register_if_disabled(app, &device).await {
        finish_registration(app, &device_id).await;
        return true;
    }
    seed_keyboard_layout(app, &device).await;
    match init_device(app, &device).await {
        Ok(true) => {}
        _ => {
            finish_registration(app, &device_id).await;
            return false;
        }
    }
    if !prefer_wired_transport(app, &device).await {
        finish_registration(app, &device_id).await;
        return false;
    }
    // baseline before overrides
    ensure_default_baseline(app, device.as_ref()).await;
    restore_saved_state(app, &device).await;
    app.device_registry.write().await.push(device.clone());
    log_registration_conflicts(app, &device).await;
    finish_registration(app, &device_id).await;
    log::info!("[{}] registered", device.name());
    true
}

/// Keep one plugin-owned representation of a physical device when its wired
/// HID interface and receiver child are both visible. Native Logitech devices
/// switch transports in-place; Lua plugin devices cannot, so the wired instance
/// displaces the same-plugin wireless instance until receiver reconciliation
/// creates it again after the cable is removed.
async fn prefer_wired_transport(app: &Arc<AppState>, device: &Arc<dyn Device>) -> bool {
    let Some(plugin_id) = device.owning_plugin_id() else {
        return true;
    };
    let identity = device.identity();
    if identity.serial.is_none() {
        return true;
    }
    let Some(connection_type) = device.wire_connection_type().await else {
        return true;
    };

    let candidates: Vec<Arc<dyn Device>> = app
        .device_registry
        .read()
        .await
        .iter()
        .filter(|other| {
            other.owning_plugin_id().as_deref() == Some(plugin_id.as_str())
                && matches!(
                    crate::domain::registry::identity::compare(
                        &identity,
                        &device.conflict_origin(),
                        &other.identity(),
                        &other.conflict_origin(),
                    ),
                    crate::domain::registry::identity::MatchEvidence::ConfirmedSerial
                )
        })
        .cloned()
        .collect();

    let mut wireless = Vec::new();
    for candidate in candidates {
        match (connection_type, candidate.wire_connection_type().await) {
            (ConnectionType::Wireless, Some(ConnectionType::Wired)) => {
                log::info!(
                    "[{}] keeping wired device '{}' instead of wireless duplicate '{}'",
                    plugin_id,
                    candidate.id(),
                    device.id()
                );
                close_device(app, device).await;
                return false;
            }
            (ConnectionType::Wired, Some(ConnectionType::Wireless)) => wireless.push(candidate),
            _ => {}
        }
    }
    if wireless.is_empty() {
        return true;
    }

    let displaced_ids: std::collections::HashSet<String> =
        wireless.iter().map(|d| d.id().to_owned()).collect();
    app.device_registry
        .write()
        .await
        .retain(|d| !displaced_ids.contains(d.id()));
    {
        let mut owners = app.device_registry.children.lock().await;
        for children in owners.values_mut() {
            children.retain(|id| !displaced_ids.contains(id));
        }
    }
    for displaced in wireless {
        log::info!(
            "[{}] wired device '{}' replaced wireless duplicate '{}'",
            plugin_id,
            device.id(),
            displaced.id()
        );
        close_device(app, &displaced).await;
    }
    true
}

async fn log_registration_conflicts(app: &Arc<AppState>, device: &Arc<dyn Device>) {
    if device.active_state() == VisibilityState::Disabled || !device.is_live() {
        return;
    }
    let identity = device.identity();
    if identity.is_empty() {
        return;
    }
    let origin = device.conflict_origin();
    for other in app.device_registry.read().await.iter() {
        if other.id() == device.id()
            || other.active_state() == VisibilityState::Disabled
            || !other.is_live()
            || other.integration_id().is_some()
        {
            continue;
        }
        match crate::domain::registry::identity::compare(
            &identity,
            &origin,
            &other.identity(),
            &other.conflict_origin(),
        ) {
            crate::domain::registry::identity::MatchEvidence::ConfirmedSerial
            | crate::domain::registry::identity::MatchEvidence::ConfirmedLocation => log::warn!(
                "device '{}' may conflict with '{}' (same physical hardware)",
                device.id(),
                other.id()
            ),
            crate::domain::registry::identity::MatchEvidence::PossibleUsb => log::info!(
                "device '{}' may conflict with '{}' (matching VID/PID)",
                device.id(),
                other.id()
            ),
            _ => {}
        }
    }
}

/// Atomically reserve a device id across the asynchronous initialization
/// window. The reservation lock is held while consulting `devices`, closing
/// the check/insert race between concurrent transport scanners.
async fn claim_registration(app: &Arc<AppState>, id: &str) -> bool {
    let mut active = app.device_registry.registrations.lock().await;
    if active.contains(id)
        || app
            .device_registry
            .read()
            .await
            .iter()
            .any(|d| d.id() == id)
    {
        return false;
    }
    active.insert(id.to_owned());
    true
}

async fn finish_registration(app: &Arc<AppState>, id: &str) {
    app.device_registry.registrations.lock().await.remove(id);
}

/// Register `device`, then — if it's a `Controller` — discover and register
/// its children too. Shared by every scanner that hosts children (HID hubs,
/// the plugin-integration scanner): register the parent first so children can
/// resolve it (e.g. as a `LightingDivisionHub`/`FanHub`), then walk `discover_children()`.
/// Returns whether the parent itself was registered.
pub async fn register_device_and_children(app: &Arc<AppState>, device: Arc<dyn Device>) -> bool {
    if !register_device(app, device.clone()).await {
        return false;
    }
    if device.active_state() == VisibilityState::Disabled {
        return true;
    }
    if let Some(ctrl) = device.as_controller() {
        let mut child_ids = std::collections::HashSet::new();
        for child in ctrl.discover_children().await {
            let child_id = child.id().to_owned();
            if register_device(app, child).await {
                child_ids.insert(child_id);
            }
        }
        if !child_ids.is_empty() {
            app.device_registry
                .children
                .lock()
                .await
                .entry(device.id().to_owned())
                .or_default()
                .extend(child_ids);
        }
    }
    true
}

/// True if `id` is a child registered alongside root `root_id`: `_ctrl_`
/// (integration controllers), `_acc_` (chain accessories), or `_chain_` (chain
/// links). Matching the markers rather than a bare `{root_id}_` prefix stops two
/// roots differing only by a trailing `_<suffix>` from tearing each other down.
fn is_registered_child(id: &str, root_id: &str) -> bool {
    id.strip_prefix(root_id)
        .and_then(|rest| rest.strip_prefix('_'))
        .is_some_and(|marker| {
            marker.starts_with("ctrl_")
                || marker.starts_with("acc_")
                || marker.starts_with("cooling_")
                || marker.starts_with("chain_")
        })
}

/// The mirror of [`register_device_and_children`]: close and drop `root_id` plus
/// every child registered alongside it (see [`is_registered_child`]). Used for a
/// scoped reload of one integration or plugin. Returns the removed ids so the
/// caller can prune their (shared) HID-tracking entry.
pub async fn unregister_device_and_children(app: &Arc<AppState>, root_id: &str) -> Vec<String> {
    let explicit_children = {
        let mut owners = app.device_registry.children.lock().await;
        let owned = owners.remove(root_id).unwrap_or_default();
        for children in owners.values_mut() {
            children.remove(root_id);
            for id in &owned {
                children.remove(id);
            }
        }
        owned
    };
    let removed: Vec<Arc<dyn Device>> = {
        let mut devices = app.device_registry.write().await;
        let mut removed = Vec::new();
        devices.retain(|d| {
            if d.id() == root_id
                || explicit_children.contains(d.id())
                || is_registered_child(d.id(), root_id)
            {
                removed.push(d.clone());
                false
            } else {
                true
            }
        });
        removed
    };
    // Dynamic children share their root's Lua worker. Close them first so
    // close_child hooks run before the root close hook terminates the worker.
    for device in removed.iter().filter(|device| device.id() != root_id) {
        close_device(app, device).await;
    }
    for device in removed.iter().filter(|device| device.id() == root_id) {
        close_device(app, device).await;
    }
    removed.iter().map(|d| d.id().to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::device::CapabilityRef;
    use crate::domain::registry::identity::DeviceIdentity;
    use crate::domain::registry::model::DeviceRecord;
    use crate::test_support::MockDevice;
    use halod_shared::types::VisibilityState;
    use std::sync::{atomic::Ordering, Arc};

    struct PluginTransportDevice {
        id: String,
        connection_type: ConnectionType,
        closed: std::sync::atomic::AtomicBool,
    }

    impl PluginTransportDevice {
        fn new(id: &str, connection_type: ConnectionType) -> Self {
            Self {
                id: id.into(),
                connection_type,
                closed: std::sync::atomic::AtomicBool::new(false),
            }
        }
    }

    #[async_trait::async_trait]
    impl Device for PluginTransportDevice {
        fn id(&self) -> &str {
            &self.id
        }
        fn name(&self) -> &str {
            &self.id
        }
        fn vendor(&self) -> &str {
            "Logitech"
        }
        fn model(&self) -> &str {
            "Test"
        }
        async fn initialize(&self) -> Result<bool> {
            Ok(true)
        }
        async fn close(&self) {
            self.closed.store(true, Ordering::SeqCst);
        }
        fn owning_plugin_id(&self) -> Option<String> {
            Some("logitech".into())
        }
        fn identity(&self) -> DeviceIdentity {
            DeviceIdentity::serial(Some("AABB1122".into()))
        }
        async fn wire_connection_type(&self) -> Option<ConnectionType> {
            Some(self.connection_type)
        }
        fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
            Vec::new()
        }
    }

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    #[tokio::test]
    async fn wired_plugin_device_replaces_same_serial_wireless_child() {
        let app = make_app();
        let wireless = Arc::new(PluginTransportDevice::new(
            "logitech_AABB1122",
            ConnectionType::Wireless,
        ));
        assert!(register_device(&app, wireless.clone()).await);
        app.device_registry
            .children
            .lock()
            .await
            .entry("logitech-receiver".into())
            .or_default()
            .insert(wireless.id().into());

        let wired = Arc::new(PluginTransportDevice::new(
            "logitech-046d-c095-0",
            ConnectionType::Wired,
        ));
        assert!(register_device(&app, wired.clone()).await);

        let devices = app.device_registry.read().await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id(), wired.id());
        drop(devices);
        assert!(wireless.closed.load(Ordering::SeqCst));
        assert!(!app
            .device_registry
            .children
            .lock()
            .await
            .get("logitech-receiver")
            .unwrap()
            .contains(wireless.id()));
    }

    #[tokio::test]
    async fn wireless_plugin_child_does_not_displace_same_serial_wired_device() {
        let app = make_app();
        let wired = Arc::new(PluginTransportDevice::new(
            "logitech-046d-c095-0",
            ConnectionType::Wired,
        ));
        assert!(register_device(&app, wired.clone()).await);

        let wireless = Arc::new(PluginTransportDevice::new(
            "logitech_AABB1122",
            ConnectionType::Wireless,
        ));
        assert!(!register_device(&app, wireless.clone()).await);

        let devices = app.device_registry.read().await;
        assert_eq!(devices.len(), 1);
        assert_eq!(devices[0].id(), wired.id());
        assert!(wireless.closed.load(Ordering::SeqCst));
    }

    #[test]
    fn clear_engine_slots_empties_all_capability_slots() {
        let mock = MockDevice::new("dev-1").with_fan().with_rgb().with_lcd();
        mock.fan.as_ref().unwrap().set_curve(
            "default".to_string(),
            crate::domain::cooling::model::FanCurveRecord {
                sensor_id: None,
                points: vec![(20.0, 25.0), (80.0, 100.0)],
            },
        );
        mock.rgb
            .as_ref()
            .unwrap()
            .set_canvas_zones(vec![crate::config::PlacedZone {
                device_id: "dev-1".into(),
                channel_id: "ring".into(),
                x: 0.0,
                y: 0.0,
                w: 1.0,
                h: 1.0,
                rotation: 0.0,
                effect: None,
                sampling_mode: Default::default(),
            }]);
        mock.lcd
            .as_ref()
            .unwrap()
            .set_lcd_template_id(Some("clock".into()));
        let device = Arc::new(mock) as Arc<dyn Device>;

        clear_engine_slots(&device);

        assert!(device.as_cooling().unwrap().curve("default").is_none());
        assert!(device.as_lighting().unwrap().placed_channels().is_empty());
        assert!(device.as_lcd().unwrap().lcd_template_id().is_none());
    }

    #[tokio::test]
    async fn register_device_skips_already_registered_id() {
        let app = make_app();
        let first = Arc::new(MockDevice::new("dev-1")) as Arc<dyn Device>;
        app.device_registry.write().await.push(first);

        let second = Arc::new(MockDevice::new("dev-1"));
        let load = Arc::clone(&second.load_called);
        let result = register_device(&app, Arc::new(MockDevice::new("dev-1"))).await;

        assert!(!result, "should return false for duplicate id");
        assert_eq!(
            app.device_registry.read().await.len(),
            1,
            "must not push a second copy"
        );
        assert!(
            !load.load(Ordering::SeqCst),
            "load_state must not be called"
        );
    }

    #[tokio::test]
    async fn concurrent_registration_claims_allow_only_one_owner() {
        let app = make_app();
        let (a, b) = tokio::join!(
            claim_registration(&app, "steelseries-1"),
            claim_registration(&app, "steelseries-1")
        );

        assert_ne!(a, b, "exactly one concurrent scanner must own the id");
        assert_eq!(app.device_registry.registrations.lock().await.len(), 1);
        finish_registration(&app, "steelseries-1").await;
    }

    #[tokio::test]
    async fn register_device_registers_disabled_device_without_init() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "dev-1".to_string(),
                DeviceRecord {
                    name: String::new(),
                    vendor: String::new(),
                    model: String::new(),
                    active_state: VisibilityState::Disabled,
                },
            );
            cfg.active_profile_data_mut().device_states.insert(
                "dev-1".into(),
                serde_json::json!({
                    "rgb": {
                        "state": {
                            "mode": "static",
                            "color": {"r": 255, "g": 0, "b": 0}
                        }
                    }
                }),
            );
        }
        let device = Arc::new(MockDevice::new("dev-1").with_rgb().init_panics());
        let result = register_device(&app, device.clone() as Arc<dyn Device>).await;

        assert!(result, "disabled device should return true");
        assert_eq!(
            app.device_registry.read().await.len(),
            1,
            "disabled device must be pushed"
        );
        assert!(
            !device.load_called.load(std::sync::atomic::Ordering::SeqCst),
            "disabled registration must not restore state"
        );
        assert_eq!(
            device
                .rgb_apply_count
                .load(std::sync::atomic::Ordering::SeqCst),
            0,
            "saved RGB state must not be sent to disabled hardware"
        );
    }

    #[tokio::test]
    async fn register_device_skips_when_init_returns_false() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev-1").ok_false());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(!result);
        assert!(
            app.device_registry.read().await.is_empty(),
            "Ok(false) must not push"
        );
    }

    #[tokio::test]
    async fn register_device_skips_when_init_errors() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev-1").fail());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(!result);
        assert!(
            app.device_registry.read().await.is_empty(),
            "Err must not push"
        );
    }

    #[tokio::test]
    async fn register_device_loads_state_and_pushes_active_device() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.active_profile_data_mut()
                .device_states
                .insert("dev-1".to_string(), serde_json::json!({"x": 1}));
        }
        let mock = Arc::new(MockDevice::new("dev-1"));
        let load = Arc::clone(&mock.load_called);
        let result = register_device(&app, mock as Arc<dyn Device>).await;

        assert!(result);
        assert_eq!(app.device_registry.read().await.len(), 1);
        assert!(
            load.load(Ordering::SeqCst),
            "load_state must be called when saved state exists"
        );
    }

    #[tokio::test]
    async fn register_device_seeds_keyboard_layout_before_init() {
        use halod_shared::keyboard::{KeyVariant, KeyboardLayoutSelection};
        use halod_shared::types::KeyboardLayout;
        let app = make_app();
        app.config.write().await.keyboard_layouts.insert(
            "kbd".into(),
            KeyboardLayoutSelection {
                variant: Some(KeyVariant::Iso),
                language: Some(KeyboardLayout::CH),
            },
        );
        let mock = Arc::new(MockDevice::new("kbd").with_keyboard_layout());
        register_device(&app, mock.clone() as Arc<dyn Device>).await;

        let sel = mock.keyboard_layout.as_ref().unwrap().selection();
        assert_eq!(sel.variant, Some(KeyVariant::Iso));
        assert_eq!(sel.language, Some(KeyboardLayout::CH));
    }

    #[tokio::test]
    async fn register_device_does_not_load_state_when_none_saved() {
        let app = make_app();
        let mock = Arc::new(MockDevice::new("dev-1"));
        let load = Arc::clone(&mock.load_called);
        register_device(&app, mock as Arc<dyn Device>).await;

        assert!(
            !load.load(Ordering::SeqCst),
            "load_state must not be called when no saved state"
        );
    }

    #[tokio::test]
    async fn register_device_seeds_default_baseline_when_absent() {
        let app = make_app();
        let mock = MockDevice::new("dev-1").with_choice();
        mock.choice.as_ref().unwrap().record("mode", 1);
        let mock = Arc::new(mock);
        register_device(&app, mock as Arc<dyn Device>).await;

        let cfg = app.config.read().await;
        let dflt = cfg.profiles.get(DEFAULT_PROFILE_NAME).unwrap();
        let entry = dflt
            .device_states
            .get("dev-1")
            .expect("default must hold a baseline after registration");
        assert_eq!(entry["choice"]["mode"], 1);
    }

    #[tokio::test]
    async fn register_device_does_not_overwrite_existing_default_baseline() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.profiles
                .get_mut(DEFAULT_PROFILE_NAME)
                .unwrap()
                .device_states
                .insert("dev-1".into(), serde_json::json!({ "choice": {"mode": 9} }));
        }
        let mock = MockDevice::new("dev-1").with_choice();
        mock.choice.as_ref().unwrap().record("mode", 1);
        let mock = Arc::new(mock);
        register_device(&app, mock as Arc<dyn Device>).await;

        let cfg = app.config.read().await;
        let entry = &cfg
            .profiles
            .get(DEFAULT_PROFILE_NAME)
            .unwrap()
            .device_states["dev-1"];
        assert_eq!(
            entry["choice"]["mode"], 9,
            "existing baseline must be preserved"
        );
    }

    #[tokio::test]
    async fn register_device_seeds_baseline_not_active_profile_override() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            let mut gaming = crate::domain::profiles::model::Profile::default();
            gaming
                .device_states
                .insert("dev-1".into(), serde_json::json!({ "choice": {"mode": 7} }));
            cfg.profiles.insert("Gaming".into(), gaming);
            cfg.active_profile = "Gaming".into();
        }
        let mock = MockDevice::new("dev-1").with_choice();
        mock.choice.as_ref().unwrap().record("mode", 1);
        let mock = Arc::new(mock);
        register_device(&app, mock as Arc<dyn Device>).await;

        let cfg = app.config.read().await;
        let dflt = &cfg
            .profiles
            .get(DEFAULT_PROFILE_NAME)
            .unwrap()
            .device_states["dev-1"];
        assert_eq!(dflt["choice"]["mode"], 1);
        let gaming = &cfg.profiles.get("Gaming").unwrap().device_states["dev-1"];
        assert_eq!(gaming["choice"]["mode"], 7);
    }

    #[tokio::test]
    async fn register_disabled_device_clears_fan_curve_restored_from_saved_state() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "dev-1".to_string(),
                DeviceRecord {
                    name: String::new(),
                    vendor: String::new(),
                    model: String::new(),
                    active_state: VisibilityState::Disabled,
                },
            );
            // Saved state from when the device was last Visible (persist is
            // skipped on the Visible→Disabled transition).
            cfg.active_profile_data_mut().device_states.insert(
                "dev-1".to_string(),
                serde_json::json!({
                    "fan_curve": {
                        "sensor_id": "cpu",
                        "points": [[20.0, 25.0], [80.0, 100.0]],
                    }
                }),
            );
        }
        let device = Arc::new(MockDevice::new("dev-1").with_fan().init_panics());
        let result = register_device(&app, device.clone() as Arc<dyn Device>).await;

        assert!(result, "disabled device should register");
        assert!(
            device.fan.as_ref().unwrap().curve("default").is_none(),
            "fan_curve must be cleared after registering a disabled device, \
             so the fan engine and serializer treat it as non-participating"
        );
    }

    #[tokio::test]
    async fn register_disabled_device_clears_canvas_zones_restored_from_saved_state() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "dev-1".to_string(),
                DeviceRecord {
                    name: String::new(),
                    vendor: String::new(),
                    model: String::new(),
                    active_state: VisibilityState::Disabled,
                },
            );
            cfg.active_profile_data_mut().device_states.insert(
                "dev-1".to_string(),
                serde_json::json!({
                    "rgb": {
                        "placed_channels": [{
                            "device_id": "dev-1",
                            "channel_id": "ring",
                            "x": 0.0, "y": 0.0, "w": 1.0, "h": 1.0, "rotation": 0.0
                        }]
                    }
                }),
            );
        }
        let device = Arc::new(MockDevice::new("dev-1").with_rgb().init_panics());
        let result = register_device(&app, device.clone() as Arc<dyn Device>).await;

        assert!(result, "disabled device should register");
        assert!(
            device.rgb.as_ref().unwrap().placed_channels().is_empty(),
            "placed_channels must be cleared after registering a disabled device, \
             so the canvas engine treats it as non-participating"
        );
    }

    #[tokio::test]
    async fn register_disabled_device_clears_lcd_template_restored_from_saved_state() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "dev-1".to_string(),
                DeviceRecord {
                    name: String::new(),
                    vendor: String::new(),
                    model: String::new(),
                    active_state: VisibilityState::Disabled,
                },
            );
            cfg.active_profile_data_mut().device_states.insert(
                "dev-1".to_string(),
                serde_json::json!({
                    "lcd": {
                        "template_id": "clock",
                        "params": {},
                        "brightness": 50,
                        "rotation": 0,
                        "mode": "image",
                        "active_image": null,
                    }
                }),
            );
        }
        let device = Arc::new(MockDevice::new("dev-1").with_lcd().init_panics());
        let result = register_device(&app, device.clone() as Arc<dyn Device>).await;

        assert!(result, "disabled device should register");
        assert!(
            device.lcd.as_ref().unwrap().lcd_template_id().is_none(),
            "lcd template id must be cleared after registering a disabled device, \
             so the LCD engine treats it as non-participating"
        );
    }

    #[tokio::test]
    async fn unregister_device_and_children_removes_only_the_matching_subtree() {
        let app = make_app();
        let root = Arc::new(MockDevice::new("openrgb-127_0_0_1_6742"));
        let child1 = Arc::new(MockDevice::new("openrgb-127_0_0_1_6742_ctrl_0"));
        let child2 = Arc::new(MockDevice::new("openrgb-127_0_0_1_6742_ctrl_1"));
        let unrelated = Arc::new(MockDevice::new("other-device"));
        {
            let mut devices = app.device_registry.write().await;
            devices.push(root.clone());
            devices.push(child1.clone());
            devices.push(child2.clone());
            devices.push(unrelated.clone());
        }

        unregister_device_and_children(&app, "openrgb-127_0_0_1_6742").await;

        let remaining = app.device_registry.read().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id(), "other-device");
        drop(remaining);

        assert!(root.closed.load(Ordering::SeqCst));
        assert!(child1.closed.load(Ordering::SeqCst));
        assert!(child2.closed.load(Ordering::SeqCst));
        assert!(!unrelated.closed.load(Ordering::SeqCst));
    }

    #[tokio::test]
    async fn unregister_device_and_children_removes_chain_accessory_and_link_children() {
        // A device plugin's chain children (auto-detected `_acc_` accessories and
        // manually-added `_chain_` links) must be torn down with the parent —
        // otherwise they linger after a disable and collide with freshly-built
        // children on re-enable, leaving the plugin in a broken state.
        let app = make_app();
        let root = Arc::new(MockDevice::new("kraken-0"));
        let accessory = Arc::new(MockDevice::new("kraken-0_acc_0_19"));
        let link = Arc::new(MockDevice::new("kraken-0_chain_0_abcd"));
        // A sibling whose id merely shares a leading substring must survive.
        let sibling = Arc::new(MockDevice::new("kraken-0b"));
        {
            let mut devices = app.device_registry.write().await;
            devices.push(root.clone());
            devices.push(accessory.clone());
            devices.push(link.clone());
            devices.push(sibling.clone());
        }

        unregister_device_and_children(&app, "kraken-0").await;

        let remaining = app.device_registry.read().await;
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining[0].id(), "kraken-0b");
        drop(remaining);

        assert!(root.closed.load(Ordering::SeqCst));
        assert!(
            accessory.closed.load(Ordering::SeqCst),
            "_acc_ child removed"
        );
        assert!(link.closed.load(Ordering::SeqCst), "_chain_ child removed");
        assert!(!sibling.closed.load(Ordering::SeqCst), "sibling survives");
    }

    #[test]
    fn is_registered_child_matches_only_known_markers() {
        let root = "openrgb-a_1";
        assert!(is_registered_child("openrgb-a_1_ctrl_0", root));
        assert!(is_registered_child("openrgb-a_1_acc_0_19", root));
        assert!(is_registered_child("openrgb-a_1_chain_0_uuid", root));
        assert!(
            is_registered_child("openrgb-a_1_ctrl_0_acc_0_19", root),
            "nested"
        );
        // Another root that differs only by a trailing `_<suffix>` is NOT a child.
        assert!(!is_registered_child("openrgb-a_1_2", root));
        assert!(!is_registered_child("openrgb-a_1", root), "the root itself");
        assert!(!is_registered_child("openrgb-a_1b", root));
    }
}
