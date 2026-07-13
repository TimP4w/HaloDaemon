// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::sync::Arc;

use crate::drivers::Device;
use crate::state::AppState;
use halod_shared::types::{VisibilityState, DEFAULT_PROFILE_NAME};

/// Load the device's effective saved state, then re-apply its per-zone RGB
/// transforms (which `load_state` does not cover). Shared by both register paths.
pub(super) async fn restore_saved_state(app: &Arc<AppState>, device: &Arc<dyn Device>) {
    let saved = {
        let cfg = app.config.read().await;
        Some(cfg.effective_device_state(device.id())).filter(|v| !v.is_null())
    };
    if let Some(state) = saved {
        log::debug!("[{}] restoring saved state", device.name());
        device.load_state(&state).await;
    }
    if let Some(rgb) = device.as_rgb() {
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
pub(super) fn clear_engine_slots(device: &Arc<dyn Device>) {
    if let Some(s) = device.as_rgb() {
        s.set_canvas_zones(vec![]);
    }
    if let Some(s) = device.as_fan() {
        s.clear_fan_curve();
    }
    if let Some(s) = device.as_lcd() {
        s.set_lcd_template_id(None);
    }
}

/// If the device is marked `Disabled` in config, push it to `app.devices`
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
    restore_saved_state(app, device).await;
    // clear any engine slots restored by load_state
    clear_engine_slots(device);
    log::info!(
        "[{}] registered as disabled, skipping initialize()",
        device.name()
    );
    app.devices.write().await.push(device.clone());
    true
}

/// Run `device.initialize()` and surface failures as a user-visible error notification.
/// Returns the original result so callers keep existing Ok/Err branching.
pub async fn init_device(app: &Arc<AppState>, device: &Arc<dyn Device>) -> Result<bool> {
    match device.initialize().await {
        Ok(v) => Ok(v),
        Err(e) => {
            crate::platform::notify::send(
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
///   1. Dedup    — skip silently if same id already in app.devices
///   2. Disabled — register without init if user-disabled
///   3. Init     — initialize; skip if Err or Ok(false)
///   4. State    — load saved state (sensor visibility, fan curves, …)
///   5. Push     — add to app.devices
///   6. Hook     — call device.after_register
///
/// Returns `true` when the device is now active in app.devices.
pub async fn register_device(app: &Arc<AppState>, device: Arc<dyn Device>) -> bool {
    let device_id = device.id().to_owned();
    if !claim_registration(app, &device_id).await {
        return false;
    }
    if register_if_disabled(app, &device).await {
        finish_registration(app, &device_id).await;
        return true;
    }
    match init_device(app, &device).await {
        Ok(true) => {}
        _ => {
            finish_registration(app, &device_id).await;
            return false;
        }
    }
    // baseline before overrides
    ensure_default_baseline(app, device.as_ref()).await;
    restore_saved_state(app, &device).await;
    app.devices.write().await.push(device.clone());
    finish_registration(app, &device_id).await;
    log::info!("[{}] registered", device.name());
    if let Some(hook) = device.as_post_register_hook() {
        hook.on_registered(Arc::clone(app)).await;
    }
    true
}

/// Atomically reserve a device id across the asynchronous initialization
/// window. The reservation lock is held while consulting `devices`, closing
/// the check/insert race between concurrent transport scanners.
async fn claim_registration(app: &Arc<AppState>, id: &str) -> bool {
    let mut active = app.device_registrations.lock().await;
    if active.contains(id) || app.devices.read().await.iter().any(|d| d.id() == id) {
        return false;
    }
    active.insert(id.to_owned());
    true
}

async fn finish_registration(app: &Arc<AppState>, id: &str) {
    app.device_registrations.lock().await.remove(id);
}

/// Register `device`, then — if it's a `Controller` — discover and register
/// its children too. Shared by every scanner that hosts children (HID hubs,
/// the plugin-integration scanner): register the parent first so children can
/// resolve it (e.g. as a `ChainHub`/`FanHub`), then walk `discover_children()`.
/// Returns whether the parent itself was registered.
pub async fn register_device_and_children(app: &Arc<AppState>, device: Arc<dyn Device>) -> bool {
    if !register_device(app, device.clone()).await {
        return false;
    }
    if let Some(ctrl) = device.as_controller() {
        for child in ctrl.discover_children().await {
            register_device(app, child).await;
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
                || marker.starts_with("chain_")
        })
}

/// The mirror of [`register_device_and_children`]: close and drop `root_id` plus
/// every child registered alongside it (see [`is_registered_child`]). Used for a
/// scoped reload of one integration or plugin. Returns the removed ids so the
/// caller can prune their (shared) HID-tracking entry.
pub async fn unregister_device_and_children(app: &Arc<AppState>, root_id: &str) -> Vec<String> {
    let removed: Vec<Arc<dyn Device>> = {
        let mut devices = app.devices.write().await;
        let mut removed = Vec::new();
        devices.retain(|d| {
            if d.id() == root_id || is_registered_child(d.id(), root_id) {
                removed.push(d.clone());
                false
            } else {
                true
            }
        });
        removed
    };
    for device in &removed {
        device.close().await;
    }
    removed.iter().map(|d| d.id().to_owned()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::registry::config::DeviceRecord;
    use crate::test_support::MockDevice;
    use halod_shared::types::VisibilityState;
    use std::sync::{atomic::Ordering, Arc};

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    #[test]
    fn clear_engine_slots_empties_all_capability_slots() {
        let mock = MockDevice::new("dev-1").with_fan().with_rgb().with_lcd();
        mock.fan
            .as_ref()
            .unwrap()
            .set_fan_curve(crate::cooling::config::FanCurveRecord {
                sensor_id: None,
                points: vec![(20.0, 25.0), (80.0, 100.0)],
            });
        mock.rgb
            .as_ref()
            .unwrap()
            .set_canvas_zones(vec![crate::config::PlacedZone {
                device_id: "dev-1".into(),
                zone_id: "ring".into(),
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

        assert!(device.as_fan().unwrap().fan_curve().is_none());
        assert!(device.as_rgb().unwrap().canvas_zones().is_empty());
        assert!(device.as_lcd().unwrap().lcd_template_id().is_none());
    }

    #[tokio::test]
    async fn register_device_skips_already_registered_id() {
        let app = make_app();
        let first = Arc::new(MockDevice::new("dev-1")) as Arc<dyn Device>;
        app.devices.write().await.push(first);

        let second = Arc::new(MockDevice::new("dev-1"));
        let load = Arc::clone(&second.load_called);
        let result = register_device(&app, Arc::new(MockDevice::new("dev-1"))).await;

        assert!(!result, "should return false for duplicate id");
        assert_eq!(
            app.devices.read().await.len(),
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
        assert_eq!(app.device_registrations.lock().await.len(), 1);
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
        }
        // .fail() makes initialize() Err: if it were called, result would be false.
        let device = Arc::new(MockDevice::new("dev-1").fail());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(result, "disabled device should return true");
        assert_eq!(
            app.devices.read().await.len(),
            1,
            "disabled device must be pushed"
        );
    }

    #[tokio::test]
    async fn register_device_skips_when_init_returns_false() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev-1").ok_false());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(!result);
        assert!(
            app.devices.read().await.is_empty(),
            "Ok(false) must not push"
        );
    }

    #[tokio::test]
    async fn register_device_skips_when_init_errors() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev-1").fail());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(!result);
        assert!(app.devices.read().await.is_empty(), "Err must not push");
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
        assert_eq!(app.devices.read().await.len(), 1);
        assert!(
            load.load(Ordering::SeqCst),
            "load_state must be called when saved state exists"
        );
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
            let mut gaming = crate::profiles::config::Profile::default();
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
            device.fan.as_ref().unwrap().fan_curve().is_none(),
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
                        "canvas_zones": [{
                            "device_id": "dev-1",
                            "zone_id": "ring",
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
            device.rgb.as_ref().unwrap().canvas_zones().is_empty(),
            "canvas_zones must be cleared after registering a disabled device, \
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
            let mut devices = app.devices.write().await;
            devices.push(root.clone());
            devices.push(child1.clone());
            devices.push(child2.clone());
            devices.push(unrelated.clone());
        }

        unregister_device_and_children(&app, "openrgb-127_0_0_1_6742").await;

        let remaining = app.devices.read().await;
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
            let mut devices = app.devices.write().await;
            devices.push(root.clone());
            devices.push(accessory.clone());
            devices.push(link.clone());
            devices.push(sibling.clone());
        }

        unregister_device_and_children(&app, "kraken-0").await;

        let remaining = app.devices.read().await;
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
