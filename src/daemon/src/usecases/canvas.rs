use anyhow::Result;
use serde_json::Value;
use std::sync::Arc;

use super::parse_params;
use crate::{config, ipc, state::AppState};
use halod_protocol::types::RgbState;

/// Set the active canvas effect, update config, and signal the engine immediately.
pub async fn set_effect(msg: Value, app: Arc<AppState>) -> Result<()> {
    let effect_id = msg["effect_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing effect_id"))?
        .to_string();
    let params = parse_params(&msg["params"]);

    // Update persisted config (briefly).
    let cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.active_profile_data_mut().canvas_state.active_effect =
            Some((effect_id.clone(), params.clone()));
        cfg.clone()
    };
    app.request_config_save(cfg_snap);

    app.engines.get().unwrap().canvas.set_effect(&effect_id, &params).await;

    ipc::broadcast_state(app).await;
    Ok(())
}

/// Add (or replace) a zone placement on the canvas.
pub async fn place_zone(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device_id = msg["device_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing device_id"))?
        .to_string();
    let zone_id = msg["zone_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing zone_id"))?
        .to_string();
    let x = msg["x"].as_f64().unwrap_or(0.0) as f32;
    let y = msg["y"].as_f64().unwrap_or(0.0) as f32;
    let w = msg["w"].as_f64().unwrap_or(0.15) as f32;
    let h = msg["h"].as_f64().unwrap_or(0.15) as f32;
    let rotation = msg["rotation"].as_f64().unwrap_or(0.0) as f32;

    let device = {
        let devices = app.devices.lock().await;
        devices
            .iter()
            .find(|d| d.id() == device_id)
            .cloned()
            .ok_or_else(|| anyhow::anyhow!("device not found: {device_id}"))?
    };

    let rgb = device
        .as_rgb()
        .ok_or_else(|| anyhow::anyhow!("device does not support canvas engine: {device_id}"))?;

    // Update zone list on the device slot.
    let mut zones = rgb.canvas_zones();
    zones.retain(|z| !(z.device_id == device_id && z.zone_id == zone_id));
    zones.push(config::PlacedZone { device_id: device_id.clone(), zone_id, x, y, w, h, rotation });
    rgb.set_canvas_zones(zones);

    // Mark the device as engine-controlled so the lighting widget reflects this.
    if let Some(rgb) = device.as_rgb() {
        let _ = rgb.apply(RgbState::Engine).await;
    }

    super::persist_device_state(&app, device.as_ref()).await;

    ipc::broadcast_state(app).await;
    Ok(())
}

/// Remove a zone from the canvas.
pub async fn remove_zone(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device_id = msg["device_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing device_id"))?;
    let zone_id = msg["zone_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing zone_id"))?;

    let device = {
        let devices = app.devices.lock().await;
        devices.iter().find(|d| d.id() == device_id).cloned()
    };

    if let Some(device) = device {
        if let Some(rgb) = device.as_rgb() {
            let mut zones = rgb.canvas_zones();
            zones.retain(|z| !(z.device_id == device_id && z.zone_id == zone_id));
            rgb.set_canvas_zones(zones);
            super::persist_device_state(&app, device.as_ref()).await;
        }
    }

    ipc::broadcast_state(app).await;
    Ok(())
}

/// Update the canvas position (and optionally size/rotation) of an existing zone.
/// Called on every drag-end — skips broadcast to avoid flooding.
pub async fn move_zone(msg: Value, app: Arc<AppState>) -> Result<()> {
    let device_id = msg["device_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing device_id"))?;
    let zone_id = msg["zone_id"]
        .as_str()
        .ok_or_else(|| anyhow::anyhow!("missing zone_id"))?;
    let x = msg["x"].as_f64().ok_or_else(|| anyhow::anyhow!("missing x"))? as f32;
    let y = msg["y"].as_f64().ok_or_else(|| anyhow::anyhow!("missing y"))? as f32;
    let new_w = msg["w"].as_f64().map(|v| v as f32);
    let new_h = msg["h"].as_f64().map(|v| v as f32);
    let new_rotation = msg["rotation"].as_f64().map(|v| v as f32);

    let device = {
        let devices = app.devices.lock().await;
        devices.iter().find(|d| d.id() == device_id).cloned()
    };

    if let Some(device) = device {
        if let Some(rgb) = device.as_rgb() {
            let mut zones = rgb.canvas_zones();
            if let Some(z) = zones
                .iter_mut()
                .find(|z| z.device_id == device_id && z.zone_id == zone_id)
            {
                z.x = x;
                z.y = y;
                if let Some(w) = new_w { z.w = w; }
                if let Some(h) = new_h { z.h = h; }
                if let Some(r) = new_rotation { z.rotation = r; }
            }
            rgb.set_canvas_zones(zones);
            super::persist_device_state(&app, device.as_ref()).await;
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::{RgbCapability, RgbStateSlot};
    use crate::state::AppState;
    use async_trait::async_trait;
    use halod_protocol::types::{RgbColor, RgbDescriptor, RgbState, RgbStatus};
    use serde_json::json;

    struct MockCanvasDevice {
        id: String,
        rgb: RgbStateSlot,
    }

    impl MockCanvasDevice {
        fn new(id: &str) -> Arc<Self> {
            Arc::new(Self { id: id.to_string(), rgb: RgbStateSlot::default() })
        }
    }

    #[async_trait]
    impl crate::drivers::Device for MockCanvasDevice {
        fn id(&self) -> String { self.id.clone() }
        fn name(&self) -> &str { "mock" }
        fn vendor(&self) -> &str { "mock" }
        fn model(&self) -> &str { "mock" }
        async fn initialize(&self) -> anyhow::Result<bool> { Ok(true) }
        async fn close(&self) {}
        fn capabilities(&self) -> Vec<crate::drivers::CapabilityRef<'_>> {
            vec![crate::drivers::CapabilityRef::Rgb(self)]
        }
    }

    #[async_trait]
    impl RgbCapability for MockCanvasDevice {
        fn descriptor(&self) -> &RgbDescriptor {
            static DESC: std::sync::OnceLock<RgbDescriptor> = std::sync::OnceLock::new();
            DESC.get_or_init(|| RgbDescriptor { zones: vec![], native_effects: vec![] })
        }
        async fn apply(&self, _state: RgbState) -> anyhow::Result<()> { Ok(()) }
        fn current_state(&self) -> Option<RgbState> { None }
        async fn write_frame(&self, _zone_id: &str, _colors: &[RgbColor]) -> anyhow::Result<()> { Ok(()) }
        fn serialize_rgb(&self) -> RgbStatus {
            RgbStatus {
                descriptor: self.descriptor().clone(),
                state: None,
                zone_transforms: std::collections::HashMap::new(),
                chainable_channels: vec![],
            }
        }
        fn rgb_state(&self) -> &RgbStateSlot {
            &self.rgb
        }
    }

    fn make_app(device: Arc<MockCanvasDevice>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.devices.try_lock().unwrap().push(device as Arc<dyn crate::drivers::Device>);
        app
    }

    #[tokio::test]
    async fn place_zone_adds_zone_to_device_slot() {
        let dev = MockCanvasDevice::new("dev0");
        let app = make_app(dev.clone());
        let msg = json!({"device_id": "dev0", "zone_id": "ring", "x": 0.1, "y": 0.2, "w": 0.3, "h": 0.4, "rotation": 0.0});
        place_zone(msg, app).await.unwrap();
        let zones = dev.rgb.canvas_zones();
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].zone_id, "ring");
        assert!((zones[0].x - 0.1).abs() < 1e-5);
        assert!((zones[0].y - 0.2).abs() < 1e-5);
    }

    #[tokio::test]
    async fn place_zone_replaces_existing_zone() {
        let dev = MockCanvasDevice::new("dev0");
        let app = make_app(dev.clone());
        let msg1 = json!({"device_id": "dev0", "zone_id": "ring", "x": 0.1, "y": 0.1});
        let msg2 = json!({"device_id": "dev0", "zone_id": "ring", "x": 0.9, "y": 0.9});
        place_zone(msg1, app.clone()).await.unwrap();
        place_zone(msg2, app).await.unwrap();
        let zones = dev.rgb.canvas_zones();
        assert_eq!(zones.len(), 1, "duplicate zone should be replaced not appended");
        assert!((zones[0].x - 0.9).abs() < 1e-5);
    }

    #[tokio::test]
    async fn place_zone_errors_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let msg = json!({"device_id": "ghost", "zone_id": "ring"});
        let err = place_zone(msg, app).await.unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn remove_zone_removes_zone_from_slot() {
        let dev = MockCanvasDevice::new("dev0");
        let app = make_app(dev.clone());
        let place_msg = json!({"device_id": "dev0", "zone_id": "ring", "x": 0.0, "y": 0.0});
        place_zone(place_msg, app.clone()).await.unwrap();
        assert_eq!(dev.rgb.canvas_zones().len(), 1);
        let remove_msg = json!({"device_id": "dev0", "zone_id": "ring"});
        remove_zone(remove_msg, app).await.unwrap();
        assert!(dev.rgb.canvas_zones().is_empty());
    }

    #[tokio::test]
    async fn move_zone_updates_position_in_slot() {
        let dev = MockCanvasDevice::new("dev0");
        let app = make_app(dev.clone());
        let place_msg = json!({"device_id": "dev0", "zone_id": "ring", "x": 0.1, "y": 0.1});
        place_zone(place_msg, app.clone()).await.unwrap();
        let move_msg = json!({"device_id": "dev0", "zone_id": "ring", "x": 0.5, "y": 0.6});
        move_zone(move_msg, app).await.unwrap();
        let zones = dev.rgb.canvas_zones();
        assert!((zones[0].x - 0.5).abs() < 1e-5);
        assert!((zones[0].y - 0.6).abs() < 1e-5);
    }

    #[tokio::test]
    async fn remove_zone_is_noop_for_offline_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let msg = json!({"device_id": "offline", "zone_id": "ring"});
        remove_zone(msg, app).await.unwrap();
    }
}

/// Set the global sampling radius (in pixmap pixels) for the canvas engine.
pub async fn set_sample_radius(msg: Value, app: Arc<AppState>) -> Result<()> {
    let radius = msg["radius"]
        .as_f64()
        .ok_or_else(|| anyhow::anyhow!("missing radius"))? as f32;
    let cfg_snap = {
        let mut cfg = app.config.write().await;
        cfg.active_profile_data_mut().canvas_state.sample_radius = radius.clamp(0.5, 64.0);
        cfg.clone()
    };
    app.request_config_save(cfg_snap);
    ipc::broadcast_state(app).await;
    Ok(())
}
