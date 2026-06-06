use anyhow::Result;
use std::sync::Arc;

use crate::drivers::Device;
use crate::state::AppState;
use halod_protocol::types::VisibilityState;

/// If the device is marked `Disabled` in config, push it to `app.devices`
/// without calling `initialize()` and return `true`.
/// Returns `false` when not disabled so the caller can proceed with init.
pub async fn register_if_disabled(app: &Arc<AppState>, device: &Arc<dyn Device>) -> bool {
    let is_disabled = {
        let cfg = app.config.read().await;
        cfg.known_devices
            .get(&device.id())
            .map(|r| r.active_state == VisibilityState::Disabled)
            .unwrap_or(false)
    };
    if !is_disabled {
        return false;
    }
    device.set_active_state(VisibilityState::Disabled);
    let saved = {
        let cfg = app.config.read().await;
        cfg.active_profile_data()
            .device_states
            .get(&device.id())
            .cloned()
    };
    if let Some(s) = saved {
        device.load_state(&s).await;
    }
    // load_state restores engine-participation slots (fan curve, canvas zones,
    // LCD template) from the user's last Visible-state save — set_device_visibility
    // skips persist_device_state on the Visible→Disabled transition, so saved
    // state still reflects the pre-disable participation. Clear those slots now
    // to mirror visibility.rs's behaviour; otherwise the fan/canvas/LCD engines
    // would see a curve/zones/template on a disabled device and the serializer
    // would emit it to the UI on the very first broadcast after startup.
    if let Some(s) = device.as_fan() {
        s.clear_fan_curve();
    }
    if let Some(s) = device.as_rgb() {
        s.set_canvas_zones(vec![]);
    }
    if let Some(s) = device.as_lcd() {
        s.set_lcd_template_id(None);
    }
    if let Some(rgb) = device.as_rgb() {
        let transforms = {
            let cfg = app.config.read().await;
            cfg.device_transforms.get(&device.id()).cloned().unwrap_or_default()
        };
        if !transforms.is_empty() {
            rgb.set_zone_transforms(transforms);
        }
    }
    log::info!("[{}] registered as disabled — skipping initialize()", device.name());
    app.devices.lock().await.push(device.clone());
    true
}

/// Run `device.initialize()` and surface failures as a user-visible error notification.
/// Returns the original result so callers keep existing Ok/Err branching.
pub async fn init_device(app: &Arc<AppState>, device: &Arc<dyn Device>) -> Result<bool> {
    match device.initialize().await {
        Ok(v) => Ok(v),
        Err(e) => {
            crate::notify::error(
                app,
                format!("Failed to initialize {}", device.name()),
                e.to_string(),
            )
            .await;
            Err(e)
        }
    }
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
    if app.devices.lock().await.iter().any(|d| d.id() == device.id()) {
        return false;
    }
    if register_if_disabled(app, &device).await {
        return true;
    }
    match init_device(app, &device).await {
        Ok(true) => {}
        _ => return false,
    }
    let saved = {
        let cfg = app.config.read().await;
        cfg.active_profile_data()
            .device_states
            .get(&device.id())
            .cloned()
    };
    if let Some(state) = saved {
        log::debug!("[{}] restoring saved state", device.name());
        device.load_state(&state).await;
    }
    if let Some(rgb) = device.as_rgb() {
        let transforms = {
            let cfg = app.config.read().await;
            cfg.device_transforms.get(&device.id()).cloned().unwrap_or_default()
        };
        if !transforms.is_empty() {
            rgb.set_zone_transforms(transforms);
        }
    }
    app.devices.lock().await.push(device.clone());
    log::info!("[{}] registered", device.name());
    device.after_register(Arc::clone(app)).await;
    true
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::{Config, DeviceRecord};
    use crate::test_support::MockDevice;
    use halod_protocol::types::VisibilityState;
    use std::sync::{
        atomic::Ordering,
        Arc,
    };

    fn make_app() -> Arc<AppState> {
        Arc::new(AppState::new(Config::default()))
    }

    // ── register_device tests ─────────────────────────────────────────────────

    #[tokio::test]
    async fn register_device_skips_already_registered_id() {
        let app = make_app();
        let first = Arc::new(MockDevice::new("dev-1")) as Arc<dyn Device>;
        app.devices.lock().await.push(first);

        let second = Arc::new(MockDevice::new("dev-1"));
        let load = Arc::clone(&second.load_called);
        let result = register_device(&app, Arc::new(MockDevice::new("dev-1"))).await;

        assert!(!result, "should return false for duplicate id");
        assert_eq!(app.devices.lock().await.len(), 1, "must not push a second copy");
        assert!(!load.load(Ordering::SeqCst), "load_state must not be called");
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
        // .fail() means initialize() returns Err — if it were called, register_device
        // would return false, making the assert!(result) below fail.
        let device = Arc::new(MockDevice::new("dev-1").fail());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(result, "disabled device should return true");
        assert_eq!(app.devices.lock().await.len(), 1, "disabled device must be pushed");
    }

    #[tokio::test]
    async fn register_device_skips_when_init_returns_false() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev-1").ok_false());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(!result);
        assert!(app.devices.lock().await.is_empty(), "Ok(false) must not push");
    }

    #[tokio::test]
    async fn register_device_skips_when_init_errors() {
        let app = make_app();
        let device = Arc::new(MockDevice::new("dev-1").fail());
        let result = register_device(&app, device as Arc<dyn Device>).await;

        assert!(!result);
        assert!(app.devices.lock().await.is_empty(), "Err must not push");
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
        assert_eq!(app.devices.lock().await.len(), 1);
        assert!(load.load(Ordering::SeqCst), "load_state must be called when saved state exists");
    }

    #[tokio::test]
    async fn register_device_does_not_load_state_when_none_saved() {
        let app = make_app();
        let mock = Arc::new(MockDevice::new("dev-1"));
        let load = Arc::clone(&mock.load_called);
        register_device(&app, mock as Arc<dyn Device>).await;

        assert!(!load.load(Ordering::SeqCst), "load_state must not be called when no saved state");
    }

    // ── disabled fan engine-slot clearing ─────────────────────────────────────

    #[tokio::test]
    async fn register_disabled_device_clears_fan_curve_restored_from_saved_state() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "dev-1".to_string(),
                DeviceRecord {
                    name:         String::new(),
                    vendor:       String::new(),
                    model:        String::new(),
                    active_state: VisibilityState::Disabled,
                },
            );
            // Saved state from when the device was last Visible — set_device_visibility
            // skips persist on the Visible→Disabled transition, so the saved state
            // still holds the pre-disable curve.
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

    // ── disabled rgb canvas-zone clearing ─────────────────────────────────────

    #[tokio::test]
    async fn register_disabled_device_clears_canvas_zones_restored_from_saved_state() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "dev-1".to_string(),
                DeviceRecord {
                    name:         String::new(),
                    vendor:       String::new(),
                    model:        String::new(),
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

    // ── disabled lcd template-id clearing ─────────────────────────────────────

    #[tokio::test]
    async fn register_disabled_device_clears_lcd_template_restored_from_saved_state() {
        let app = make_app();
        {
            let mut cfg = app.config.write().await;
            cfg.known_devices.insert(
                "dev-1".to_string(),
                DeviceRecord {
                    name:         String::new(),
                    vendor:       String::new(),
                    model:        String::new(),
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
}
