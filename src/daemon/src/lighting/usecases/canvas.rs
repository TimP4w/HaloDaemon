// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::sync::Arc;

use crate::profiles::device_state::persist_device_state;
use crate::registry::require_device_owned_id;
use crate::{config, ipc, state::AppState};
use halod_shared::commands::EngineKind;
use halod_shared::types::{EffectDef, RgbColor, RgbState, SamplingMode, ZoneTopology};

/// Default `(w, h)` for a freshly placed zone, in normalized canvas units. A
/// linear strip gets a short box so it doesn't float in empty space; every other
/// topology stays square.
fn default_zone_size(topology: Option<ZoneTopology>) -> (f64, f64) {
    match topology {
        Some(ZoneTopology::Linear) => (0.2, 0.05),
        _ => (0.15, 0.15),
    }
}

/// Topology of a device's RGB zone, if the device exists and exposes it.
async fn zone_topology(
    device_id: &str,
    zone_id: &str,
    app: &Arc<AppState>,
) -> Option<ZoneTopology> {
    let device = require_device_owned_id(device_id, app).await.ok()?;
    let rgb = device.as_rgb()?;
    rgb.descriptor()
        .zones
        .iter()
        .find(|z| z.id == zone_id)
        .map(|z| z.topology.clone())
}

pub async fn upsert_effect(
    instance_id: String,
    mut def: EffectDef,
    app: Arc<AppState>,
) -> Result<()> {
    def.name = def
        .name
        .map(|n| n.trim().to_string())
        .filter(|n| !n.is_empty());
    {
        let mut cfg = app.config.write().await;
        cfg.canvas_state_for_edit().effects.insert(instance_id, def);
    }
    app.request_config_save();
    ipc::broadcast_state(&app).await;
    Ok(())
}

/// Zones referencing this instance fall back to the canvas default.
pub async fn remove_effect(instance_id: String, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        let cs = cfg.canvas_state_for_edit();
        cs.effects.remove(&instance_id);
        if cs.default_effect.as_deref() == Some(instance_id.as_str()) {
            cs.default_effect = None;
        }
    }
    app.request_config_save();
    ipc::broadcast_state(&app).await;
    Ok(())
}

pub async fn set_default_effect(instance_id: Option<String>, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        cfg.canvas_state_for_edit().default_effect = instance_id;
    }
    app.request_config_save();
    ipc::broadcast_state(&app).await;
    Ok(())
}

/// Mutate the device's canvas zone list and persist, returning the device so
/// callers can do extra work (e.g. mark it engine-controlled). Errors for an
/// unknown / non-canvas device; callers treating offline as a no-op discard it.
async fn modify_canvas_zones(
    device_id: &str,
    app: &Arc<AppState>,
    mutate: impl FnOnce(&mut Vec<config::PlacedZone>),
) -> Result<Arc<dyn crate::drivers::Device>> {
    let device = require_device_owned_id(device_id, app).await?;
    let rgb = device
        .as_rgb()
        .ok_or_else(|| anyhow::anyhow!("device does not support canvas engine: {device_id}"))?;

    let mut zones = rgb.canvas_zones();
    mutate(&mut zones);
    rgb.set_canvas_zones(zones);

    persist_device_state(app, device.as_ref()).await;
    Ok(device)
}

/// Add (or replace) a zone placement on the canvas.
#[allow(clippy::too_many_arguments)]
pub async fn place_zone(
    device_id: String,
    zone_id: String,
    x: Option<f64>,
    y: Option<f64>,
    w: Option<f64>,
    h: Option<f64>,
    rotation: Option<f64>,
    effect: Option<String>,
    sampling_mode: Option<SamplingMode>,
    app: Arc<AppState>,
) -> Result<()> {
    // Default placement size depends on the zone's topology: a linear strip is
    // a thin line, so a square box leaves it floating in empty space.
    let (def_w, def_h) = default_zone_size(zone_topology(&device_id, &zone_id, &app).await);
    let x = x.unwrap_or(0.0) as f32;
    let y = y.unwrap_or(0.0) as f32;
    let w = w.unwrap_or(def_w) as f32;
    let h = h.unwrap_or(def_h) as f32;
    let rotation = rotation.unwrap_or(0.0) as f32;

    let device = modify_canvas_zones(&device_id, &app, |zones| {
        zones.retain(|z| !(z.device_id == device_id && z.zone_id == zone_id));
        zones.push(config::PlacedZone {
            device_id: device_id.clone(),
            zone_id,
            x,
            y,
            w,
            h,
            rotation,
            effect,
            sampling_mode: sampling_mode.unwrap_or_default(),
        });
    })
    .await?;

    if let Some(rgb) = device.as_rgb() {
        let _ = rgb.apply(RgbState::Engine).await;
    }

    crate::registry::usecases::settings::set_engine_config(
        EngineKind::Canvas,
        Some(true),
        None,
        None,
        None,
        Arc::clone(&app),
    )
    .await?;

    ipc::broadcast_state(&app).await;
    Ok(())
}

/// Remove a zone from the canvas.
pub async fn remove_zone(device_id: String, zone_id: String, app: Arc<AppState>) -> Result<()> {
    let _ = modify_canvas_zones(&device_id, &app, |zones| {
        zones.retain(|z| !(z.device_id == device_id && z.zone_id == zone_id));
    })
    .await;

    ipc::broadcast_state(&app).await;
    Ok(())
}

/// Update the canvas position (and optionally size/rotation) of an existing zone.
/// Called on every drag-end — skips broadcast to avoid flooding.
#[allow(clippy::too_many_arguments)]
pub async fn move_zone(
    device_id: String,
    zone_id: String,
    x: f64,
    y: f64,
    w: Option<f64>,
    h: Option<f64>,
    rotation: Option<f64>,
    effect: Option<String>,
    sampling_mode: Option<SamplingMode>,
    app: Arc<AppState>,
) -> Result<()> {
    let x = x as f32;
    let y = y as f32;
    let new_w = w.map(|v| v as f32);
    let new_h = h.map(|v| v as f32);
    let new_rotation = rotation.map(|v| v as f32);

    let _ = modify_canvas_zones(&device_id, &app, |zones| {
        if let Some(z) = zones
            .iter_mut()
            .find(|z| z.device_id == device_id && z.zone_id == zone_id)
        {
            z.x = x;
            z.y = y;
            if let Some(w) = new_w {
                z.w = w;
            }
            if let Some(h) = new_h {
                z.h = h;
            }
            if let Some(r) = new_rotation {
                z.rotation = r;
            }
            if let Some(effect) = effect {
                z.effect = Some(effect);
            }
            if let Some(mode) = sampling_mode {
                z.sampling_mode = mode;
            }
        }
    })
    .await;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use crate::test_support::MockDevice;

    fn canvas_zones(dev: &MockDevice) -> Vec<config::PlacedZone> {
        dev.rgb.as_ref().unwrap().canvas_zones()
    }

    fn make_app(device: Arc<MockDevice>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .try_write()
            .unwrap()
            .push(device as Arc<dyn crate::drivers::Device>);
        app
    }

    #[test]
    fn default_zone_size_thins_only_linear() {
        let default_size = default_zone_size(None);
        let linear_size = default_zone_size(Some(ZoneTopology::Linear));

        // Linear zones are shorter (in the cross-axis) than every other
        // topology, which all get the same square default.
        assert!(linear_size.1 < default_size.1);
        for topology in [ZoneTopology::Ring, ZoneTopology::Grid] {
            assert_eq!(default_zone_size(Some(topology)), default_size);
        }
    }

    #[tokio::test]
    async fn place_zone_adds_zone_to_device_slot() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        place_zone(
            "dev0".into(),
            "ring".into(),
            Some(0.1),
            Some(0.2),
            Some(0.3),
            Some(0.4),
            Some(0.0),
            Some("bars".into()),
            None,
            app.clone(),
        )
        .await
        .unwrap();
        let zones = canvas_zones(&dev);
        assert_eq!(zones.len(), 1);
        assert_eq!(zones[0].zone_id, "ring");
        assert!((zones[0].x - 0.1).abs() < 1e-5);
        assert!((zones[0].y - 0.2).abs() < 1e-5);
        assert_eq!(zones[0].effect.as_deref(), Some("bars"));
        assert!(
            app.config.read().await.global.engine_canvas_enabled,
            "placing a zone must enable the canvas engine so it animates"
        );
    }

    #[tokio::test]
    async fn place_zone_replaces_existing_zone() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        place_zone(
            "dev0".into(),
            "ring".into(),
            Some(0.1),
            Some(0.1),
            None,
            None,
            None,
            None,
            None,
            app.clone(),
        )
        .await
        .unwrap();
        place_zone(
            "dev0".into(),
            "ring".into(),
            Some(0.9),
            Some(0.9),
            None,
            None,
            None,
            None,
            None,
            app,
        )
        .await
        .unwrap();
        let zones = canvas_zones(&dev);
        assert_eq!(
            zones.len(),
            1,
            "duplicate zone should be replaced not appended"
        );
        assert!((zones[0].x - 0.9).abs() < 1e-5);
    }

    #[tokio::test]
    async fn place_zone_errors_on_missing_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = place_zone(
            "ghost".into(),
            "ring".into(),
            None,
            None,
            None,
            None,
            None,
            None,
            None,
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("ghost"));
    }

    #[tokio::test]
    async fn remove_zone_removes_zone_from_slot() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        place_zone(
            "dev0".into(),
            "ring".into(),
            Some(0.0),
            Some(0.0),
            None,
            None,
            None,
            None,
            None,
            app.clone(),
        )
        .await
        .unwrap();
        assert_eq!(canvas_zones(&dev).len(), 1);
        remove_zone("dev0".into(), "ring".into(), app)
            .await
            .unwrap();
        assert!(canvas_zones(&dev).is_empty());
    }

    #[tokio::test]
    async fn move_zone_updates_position_and_effect_in_slot() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        place_zone(
            "dev0".into(),
            "ring".into(),
            Some(0.1),
            Some(0.1),
            None,
            None,
            None,
            None,
            None,
            app.clone(),
        )
        .await
        .unwrap();
        move_zone(
            "dev0".into(),
            "ring".into(),
            0.5,
            0.6,
            None,
            None,
            None,
            Some("bars".into()),
            None,
            app,
        )
        .await
        .unwrap();
        let zones = canvas_zones(&dev);
        assert!((zones[0].x - 0.5).abs() < 1e-5);
        assert!((zones[0].y - 0.6).abs() < 1e-5);
        assert_eq!(zones[0].effect.as_deref(), Some("bars"));
    }

    #[tokio::test]
    async fn remove_zone_is_noop_for_offline_device() {
        let app = Arc::new(AppState::new(Config::default()));
        remove_zone("offline".into(), "ring".into(), app)
            .await
            .unwrap();
    }

    #[tokio::test]
    async fn move_zone_is_noop_for_offline_device() {
        let app = Arc::new(AppState::new(Config::default()));
        move_zone(
            "offline".into(),
            "ring".into(),
            0.5,
            0.6,
            None,
            None,
            None,
            None,
            None,
            app,
        )
        .await
        .unwrap();
    }

    #[tokio::test]
    async fn stop_disables_engine_and_blanks_engine_mode_device() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        // Placing a zone puts the device into Engine mode.
        place_zone(
            "dev0".into(),
            "ring".into(),
            Some(0.0),
            Some(0.0),
            None,
            None,
            None,
            None,
            None,
            app.clone(),
        )
        .await
        .unwrap();
        assert!(matches!(
            dev.rgb.as_ref().unwrap().current_state(),
            Some(RgbState::Engine)
        ));

        stop(app.clone()).await.unwrap();

        assert!(!app.config.read().await.global.engine_canvas_enabled);
        assert!(matches!(
            dev.rgb.as_ref().unwrap().current_state(),
            Some(RgbState::Static {
                color: RgbColor { r: 0, g: 0, b: 0 }
            })
        ));
    }

    #[tokio::test]
    async fn stop_blanks_direct_effect_device() {
        use crate::drivers::RgbCapability;
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        // A direct (software) effect is also engine-driven, so stop must blank it.
        dev.apply(RgbState::DirectEffect {
            id: "breathing".into(),
            params: Default::default(),
        })
        .await
        .unwrap();

        stop(app.clone()).await.unwrap();

        assert!(matches!(
            dev.rgb.as_ref().unwrap().current_state(),
            Some(RgbState::Static {
                color: RgbColor { r: 0, g: 0, b: 0 }
            })
        ));
    }

    fn def(effect_id: &str) -> EffectDef {
        EffectDef {
            effect_id: effect_id.into(),
            name: None,
            params: Default::default(),
        }
    }

    #[tokio::test]
    async fn upsert_effect_stores_instance_in_config() {
        let app = Arc::new(AppState::new(Config::default()));
        upsert_effect("bars".into(), def("screen_sampler"), app.clone())
            .await
            .unwrap();
        let cs = app.config.read().await.effective_canvas_state();
        assert_eq!(cs.effects["bars"].effect_id, "screen_sampler");
    }

    #[tokio::test]
    async fn upsert_effect_trims_name_and_drops_blank() {
        let app = Arc::new(AppState::new(Config::default()));
        let named = EffectDef {
            name: Some("  Desk glow  ".into()),
            ..def("screen_sampler")
        };
        upsert_effect("bars".into(), named, app.clone())
            .await
            .unwrap();
        let cs = app.config.read().await.effective_canvas_state();
        assert_eq!(cs.effects["bars"].name.as_deref(), Some("Desk glow"));

        let blank = EffectDef {
            name: Some("   ".into()),
            ..def("screen_sampler")
        };
        upsert_effect("bars".into(), blank, app.clone())
            .await
            .unwrap();
        let cs = app.config.read().await.effective_canvas_state();
        assert_eq!(cs.effects["bars"].name, None);
    }

    #[tokio::test]
    async fn remove_effect_clears_default_when_it_matches() {
        let app = Arc::new(AppState::new(Config::default()));
        upsert_effect("bars".into(), def("screen_sampler"), app.clone())
            .await
            .unwrap();
        set_default_effect(Some("bars".into()), app.clone())
            .await
            .unwrap();
        remove_effect("bars".into(), app.clone()).await.unwrap();
        let cs = app.config.read().await.effective_canvas_state();
        assert!(cs.effects.is_empty());
        assert!(
            cs.default_effect.is_none(),
            "default must clear with its instance"
        );
    }

    #[tokio::test]
    async fn set_default_effect_updates_config() {
        let app = Arc::new(AppState::new(Config::default()));
        set_default_effect(Some("bars".into()), app.clone())
            .await
            .unwrap();
        assert_eq!(
            app.config
                .read()
                .await
                .effective_canvas_state()
                .default_effect,
            Some("bars".into())
        );
    }

    #[tokio::test]
    async fn set_sample_radius_clamps_to_lower_bound() {
        let app = Arc::new(AppState::new(Config::default()));
        // set_sample_radius calls canvas_state_for_edit() which needs a profile.
        app.config.write().await.canvas_state_for_edit();
        set_sample_radius(0.0, app.clone()).await.unwrap();
        assert_eq!(
            app.config
                .read()
                .await
                .effective_canvas_state()
                .sample_radius,
            0.5
        );
    }

    #[tokio::test]
    async fn set_sample_radius_clamps_to_upper_bound() {
        let app = Arc::new(AppState::new(Config::default()));
        app.config.write().await.canvas_state_for_edit();
        set_sample_radius(100.0, app.clone()).await.unwrap();
        assert_eq!(
            app.config
                .read()
                .await
                .effective_canvas_state()
                .sample_radius,
            64.0
        );
    }

    #[tokio::test]
    async fn set_sample_radius_stores_value_within_bounds() {
        let app = Arc::new(AppState::new(Config::default()));
        app.config.write().await.canvas_state_for_edit();
        set_sample_radius(10.0, app.clone()).await.unwrap();
        assert_eq!(
            app.config
                .read()
                .await
                .effective_canvas_state()
                .sample_radius,
            10.0
        );
    }
}

/// How long to wait after disabling the canvas engine before blanking, so any
/// tick already in flight finishes first and can't re-light the LEDs we revert.
/// Also used by [`super::rgb::rgb_apply`] for the same reason when a single
/// device leaves an engine-driven state.
pub(crate) const STOP_DRAIN_MS: u64 = 60;

/// Stop the canvas engine: disable it, then blank every RGB device it was
/// driving (revert from `Engine` mode to static black).
pub async fn stop(app: Arc<AppState>) -> Result<()> {
    // Disable the engine (persists config + pushes the run config to the engine).
    crate::registry::usecases::settings::set_engine_config(
        EngineKind::Canvas,
        Some(false),
        None,
        None,
        None,
        Arc::clone(&app),
    )
    .await?;

    // A `watch::send` only enqueues — a tick already executing will run to
    // completion and could re-apply Engine mode + push a frame. Let it drain
    // before blanking so the revert is the last write the LEDs receive.
    tokio::time::sleep(std::time::Duration::from_millis(STOP_DRAIN_MS)).await;

    let black = RgbState::Static {
        color: RgbColor { r: 0, g: 0, b: 0 },
    };
    let devices = app.devices.read().await.clone();
    for device in devices {
        if let Some(rgb) = device.as_rgb() {
            // `DirectEffect` is engine-driven too, so blank it alongside `Engine`.
            if matches!(
                rgb.current_state(),
                Some(RgbState::Engine | RgbState::DirectEffect { .. })
            ) {
                let _ = rgb.apply(black.clone()).await;
                persist_device_state(&app, device.as_ref()).await;
            }
        }
    }

    ipc::broadcast_state(&app).await;
    Ok(())
}

/// Set the global sampling radius (in pixmap pixels) for the canvas engine.
pub async fn set_sample_radius(radius: f64, app: Arc<AppState>) -> Result<()> {
    let radius = radius as f32;
    {
        let mut cfg = app.config.write().await;
        cfg.canvas_state_for_edit().sample_radius = radius.clamp(0.5, 64.0);
    }
    app.request_config_save();
    ipc::broadcast_state(&app).await;
    Ok(())
}
