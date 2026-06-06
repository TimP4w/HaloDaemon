use serde_json::Value;
use std::collections::HashMap;
use std::sync::Arc;

use crate::config::{FanCurveRecord, PlacedZone};
use crate::drivers::Device;
use crate::engines::{canvas::CanvasEngine, fan_curve::preset_curves, lcd::LcdEngine};
use crate::state::AppState;
use halod_protocol::types::{
    AppRule, AppState as WireAppState, DeviceCapability, FanCurveStatus,
    GlobalConfig as WireGlobalConfig, LcdMode, LogEntry, WireCanvasState, WirePlacedZone,
};

pub async fn serialize_state(app: Arc<AppState>) -> Value {
    let disc = app.discovery.lock().await.clone();

    let device_list: Vec<Arc<dyn Device>> = app.devices.lock().await.clone();
    let mut devices = Vec::with_capacity(device_list.len());
    for d in &device_list {
        devices.push(d.serialize().await);
    }

    // Acquire config once for a consistent snapshot across all config reads.
    let cfg = app.config.read().await;

    // Patch active_state and name from the persisted DeviceRecord so the UI
    // reflects user preferences (hide, rename) without each device owning
    // config state. Devices with externally-owned names (chain links) keep
    // the name their own serialize() produced.
    for (device, wire) in device_list.iter().zip(devices.iter_mut()) {
        if let Some(record) = cfg.known_devices.get(&device.id()) {
            wire.active_state = record.active_state.clone();
            if !device.has_external_name() && !record.name.is_empty() {
                wire.name = record.name.clone();
            }
        }
    }

    let cs = &cfg.active_profile_data().canvas_state;
    let active_profile = cfg.active_profile.clone();
    let profiles = cfg.profile_names();
    let canvas_active_effect = cs.active_effect.clone();
    let canvas_sample_radius = cs.sample_radius;

    // Collect per-device engine state from device_list (already owned — no extra locks).
    let fan_curves_raw: Vec<(String, FanCurveRecord)> = device_list
        .iter()
        .filter_map(|d| Some((d.id(), d.as_fan()?.fan_curve()?)))
        .collect();

    let placed_zones: Vec<PlacedZone> = device_list
        .iter()
        .filter_map(|d| d.as_rgb())
        .flat_map(|s| s.canvas_zones())
        .collect();

    let lcd_device_templates: HashMap<String, String> = device_list
        .iter()
        .filter_map(|d| Some((d.id(), d.as_lcd()?.lcd_template_id()?)))
        .collect();

    // Patch LCD mode to Engine for devices currently driven by the LCD engine.
    for device in &mut devices {
        if lcd_device_templates.contains_key(&device.id) {
            for cap in &mut device.capabilities {
                if let DeviceCapability::Lcd(status) = cap {
                    status.mode = LcdMode::Engine;
                }
            }
        }
    }

    // Inject per-zone RGB transforms — stored per device in the RGB capability,
    // not produced by the device's own `serialize_rgb`.
    for (device, wire) in device_list.iter().zip(devices.iter_mut()) {
        if let Some(rgb) = device.as_rgb() {
            let transforms = rgb.zone_transforms();
            if transforms.is_empty() {
                continue;
            }
            for cap in &mut wire.capabilities {
                if let DeviceCapability::Rgb(status) = cap {
                    status.zone_transforms = transforms.clone();
                }
            }
        }
    }

    let statuses = app.fan_curve_statuses.lock().await;
    let fan_curves = fan_curves_raw
        .into_iter()
        .map(|(fan_id, record)| {
            let status = statuses.get(&fan_id).cloned().unwrap_or(FanCurveStatus::NoSensor);
            record.serialize(fan_id, status)
        })
        .collect();
    drop(statuses);

    let preset_curves_wire = preset_curves().iter().map(|p| p.serialize()).collect();

    let canvas = WireCanvasState {
        active_effect_id: canvas_active_effect.as_ref().map(|(id, _)| id.clone()),
        available_effects: CanvasEngine::available_effect_descriptors(),
        placed_zones: placed_zones
            .into_iter()
            .map(|p| WirePlacedZone {
                device_id: p.device_id,
                zone_id: p.zone_id,
                x: p.x,
                y: p.y,
                w: p.w,
                h: p.h,
                rotation: p.rotation,
            })
            .collect(),
        sample_radius: canvas_sample_radius,
    };

    let lcd_engine = LcdEngine::wire_state(lcd_device_templates);

    let g = &cfg.global;
    let global_config_wire = WireGlobalConfig {
        engine_fan_curve_enabled: g.engine_fan_curve_enabled,
        engine_fan_curve_tick_ms: g.engine_fan_curve_tick_ms,
        engine_canvas_enabled: g.engine_canvas_enabled,
        engine_canvas_fps: g.engine_canvas_fps,
        engine_lcd_enabled: g.engine_lcd_enabled,
        engine_lcd_fps: g.engine_lcd_fps,
        fan_failsafe_duty: g.fan_failsafe_duty,
        log_level: g.log_level.clone(),
        close_to_tray: g.close_to_tray,
    };

    let app_rules: Vec<AppRule> = cfg.app_rules.clone();

    // Drop the config guard before accessing log_buffer (std::sync::Mutex) to
    // avoid holding a tokio guard across a potentially-blocking std lock.
    drop(cfg);

    let log_entries: Vec<LogEntry> = app
        .log_buffer
        .lock()
        .map(|buf| {
            let skip = buf.len().saturating_sub(100);
            buf.iter().skip(skip).cloned().collect()
        })
        .unwrap_or_default();

    let focus_watcher_supported = app.focus_watcher_supported.load(std::sync::atomic::Ordering::Relaxed);

    let wire = WireAppState {
        discovery: disc,
        devices,
        active_profile,
        profiles,
        fan_curves,
        preset_curves: preset_curves_wire,
        canvas,
        lcd_engine,
        global_config: global_config_wire,
        log_entries,
        config_dir: crate::config::config_dir().display().to_string(),
        app_rules,
        focus_watcher_supported,
    };
    serde_json::to_value(wire).expect("protocol AppState should always serialize")
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::{
        config::{Config, FanCurveRecord, PlacedZone},
        drivers::Device,
        test_support::MockDevice,
    };
    use async_trait::async_trait;
    use halod_protocol::types::WireDevice;

    #[tokio::test]
    async fn serialize_empty_state() {
        let app = Arc::new(AppState::new(Config::default()));
        let value = serialize_state(app).await;
        let wire: halod_protocol::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices.len(), 0);
    }

    #[tokio::test]
    async fn serialize_with_one_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan()
                .with_rgb(),
        );
        app.devices.lock().await.push(dev);
        let value = serialize_state(app).await;
        let wire: halod_protocol::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices.len(), 1);
        assert_eq!(wire.devices[0].id, "test_device");
        assert_eq!(wire.devices[0].name, "Test Fan");
        assert_eq!(wire.devices[0].vendor, "Acme");
    }

    #[tokio::test]
    async fn serialize_fan_curve_from_device_trait() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = MockDevice::new("fan_dev")
            .with_name("Fan Device")
            .with_vendor("Acme")
            .with_model("Turbo Fan")
            .with_fan()
            .with_rgb();
        dev.fan.as_ref().unwrap().set_fan_curve(FanCurveRecord {
            sensor_id: Some("cpu_temp".to_string()),
            points: vec![(0.0, 30.0), (80.0, 90.0), (100.0, 100.0)],
        });
        let dev: Arc<dyn Device> = Arc::new(dev);
        app.devices.lock().await.push(dev);

        let value = serialize_state(app).await;
        let wire: halod_protocol::types::AppState = serde_json::from_value(value).unwrap();

        assert_eq!(wire.fan_curves.len(), 1);
        assert_eq!(wire.fan_curves[0].fan_id, "fan_dev");
        assert_eq!(wire.fan_curves[0].sensor_id.as_deref(), Some("cpu_temp"));
    }

    #[tokio::test]
    async fn serialize_patches_name_from_device_record() {
        use crate::config::DeviceRecord;
        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "test_device".to_string(),
            DeviceRecord {
                name: "My Fan".into(),
                vendor: "Acme".into(),
                model: "Fan 3000".into(),
                active_state: Default::default(),
            },
        );
        let app = Arc::new(AppState::new(cfg));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan()
                .with_rgb(),
        );
        app.devices.lock().await.push(dev);

        let value = serialize_state(app).await;
        let wire: halod_protocol::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices[0].name, "My Fan");
    }

    #[tokio::test]
    async fn serialize_keeps_device_name_when_no_record() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev: Arc<dyn Device> = Arc::new(
            MockDevice::new("test_device")
                .with_name("Test Fan")
                .with_vendor("Acme")
                .with_model("Fan 3000")
                .with_fan()
                .with_rgb(),
        );
        app.devices.lock().await.push(dev);

        let value = serialize_state(app).await;
        let wire: halod_protocol::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices[0].name, "Test Fan");
    }

    #[tokio::test]
    async fn serialize_keeps_device_name_when_device_has_external_name() {
        use crate::config::DeviceRecord;
        struct ExternalNameDevice {
            inner: MockDevice,
        }
        #[async_trait]
        impl Device for ExternalNameDevice {
            fn id(&self) -> String { self.inner.id() }
            fn name(&self) -> &str { self.inner.name() }
            fn vendor(&self) -> &str { self.inner.vendor() }
            fn model(&self) -> &str { self.inner.model() }
            fn capabilities(&self) -> Vec<crate::drivers::CapabilityRef<'_>> { vec![] }
            fn has_external_name(&self) -> bool { true }
            async fn initialize(&self) -> anyhow::Result<bool> { Ok(true) }
            async fn close(&self) {}
            async fn serialize(&self) -> WireDevice {
                let mut w = self.inner.serialize().await;
                w.name = "Chain Link 1".to_string();
                w
            }
        }
        let mut cfg = Config::default();
        cfg.known_devices.insert(
            "chain_dev".to_string(),
            DeviceRecord {
                name: "ARGB Strip".into(),
                vendor: "Generic".into(),
                model: "Chain Link".into(),
                active_state: Default::default(),
            },
        );
        let app = Arc::new(AppState::new(cfg));
        let dev: Arc<dyn Device> = Arc::new(ExternalNameDevice {
            inner: MockDevice::new("chain_dev")
                .with_name("ARGB Strip")
                .with_vendor("Generic")
                .with_model("Chain Link"),
        });
        app.devices.lock().await.push(dev);

        let value = serialize_state(app).await;
        let wire: halod_protocol::types::AppState = serde_json::from_value(value).unwrap();
        assert_eq!(wire.devices[0].name, "Chain Link 1");
    }

    #[tokio::test]
    async fn serialize_canvas_zones_from_device_trait() {
        let app = Arc::new(AppState::new(Config::default()));
        let dev = MockDevice::new("rgb_dev")
            .with_name("RGB Device")
            .with_vendor("Acme")
            .with_model("RGB Strip")
            .with_fan()
            .with_rgb();
        dev.rgb.as_ref().unwrap().set_canvas_zones(vec![PlacedZone {
            device_id: "rgb_dev".to_string(),
            zone_id: "zone_1".to_string(),
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
            rotation: 0.0,
        }]);
        let dev: Arc<dyn Device> = Arc::new(dev);
        app.devices.lock().await.push(dev);

        let value = serialize_state(app).await;
        let wire: halod_protocol::types::AppState = serde_json::from_value(value).unwrap();

        assert_eq!(wire.canvas.placed_zones.len(), 1);
        assert_eq!(wire.canvas.placed_zones[0].device_id, "rgb_dev");
        assert_eq!(wire.canvas.placed_zones[0].zone_id, "zone_1");
        assert_eq!(wire.canvas.placed_zones[0].x, 10.0);
        assert_eq!(wire.canvas.placed_zones[0].y, 20.0);
    }
}
