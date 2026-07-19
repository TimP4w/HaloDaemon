// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::Result;
use std::sync::Arc;

use crate::profiles::device_state::persist_device_state;
use crate::registry::require_device_owned_id;
use crate::{config, state::AppState};
use halod_shared::commands::EngineKind;
use halod_shared::types::{
    EffectDef, EffectParamValue, LightingState, PlacedZone, RgbColor, SamplingMode, ZoneTopology,
};

const MAX_CANVAS_ID_LEN: usize = 64;
const MAX_CANVAS_NAME_LEN: usize = 256;
const MAX_CANVAS_PARAM_TEXT_LEN: usize = 4096;
const MAX_ZONE_SIZE: f32 = 4.0;

fn validate_canvas_id(what: &str, id: &str) -> Result<()> {
    anyhow::ensure!(
        !id.is_empty() && id.len() <= MAX_CANVAS_ID_LEN,
        "{what} must be 1..={MAX_CANVAS_ID_LEN} bytes"
    );
    anyhow::ensure!(
        id.chars()
            .all(|c| c.is_ascii_alphanumeric() || matches!(c, '.' | '_' | '-' | ':')),
        "{what} contains invalid characters"
    );
    Ok(())
}

fn validate_effect_def(instance_id: &str, def: &EffectDef, app: &AppState) -> Result<()> {
    validate_canvas_id("effect instance id", instance_id)?;
    validate_canvas_id("effect id", &def.effect_id)?;
    anyhow::ensure!(
        def.params.len() <= halod_shared::types::MAX_EFFECT_PARAMS,
        "effect has too many params"
    );
    if let Some(name) = &def.name {
        anyhow::ensure!(name.len() <= MAX_CANVAS_NAME_LEN, "effect name is too long");
    }
    for (key, value) in &def.params {
        validate_canvas_id("effect parameter id", key)?;
        match value {
            EffectParamValue::Float(v) => {
                anyhow::ensure!(v.is_finite(), "effect parameter '{key}' is not finite")
            }
            EffectParamValue::Str(v) => anyhow::ensure!(
                v.len() <= MAX_CANVAS_PARAM_TEXT_LEN && !v.contains('\0'),
                "effect parameter '{key}' is invalid"
            ),
            EffectParamValue::Steps(steps) => anyhow::ensure!(
                steps.len() <= halod_shared::types::MAX_EFFECT_PARAMS
                    && steps.iter().all(|s| s.value.is_finite()),
                "effect parameter '{key}' has invalid steps"
            ),
            EffectParamValue::Color(_) | EffectParamValue::Bool(_) => {}
        }
    }
    let known = crate::lighting::rgb_engine::RgbEngine::available_effect_descriptors(&app.registry)
        .iter()
        .any(|descriptor| descriptor.id == def.effect_id);
    anyhow::ensure!(known, "unknown canvas effect '{}'", def.effect_id);
    Ok(())
}

fn validate_placed_zone(
    zone: &PlacedZone,
    effects: &std::collections::HashMap<String, EffectDef>,
) -> Result<()> {
    validate_canvas_id("device id", &zone.device_id)?;
    validate_canvas_id("zone id", &zone.channel_id)?;
    anyhow::ensure!(
        zone.x.is_finite()
            && zone.y.is_finite()
            && zone.w.is_finite()
            && zone.h.is_finite()
            && zone.rotation.is_finite()
            && zone.w > 0.0
            && zone.h > 0.0
            && zone.w <= MAX_ZONE_SIZE
            && zone.h <= MAX_ZONE_SIZE,
        "canvas zone geometry is invalid"
    );
    if let Some(effect) = &zone.effect {
        validate_canvas_id("zone effect id", effect)?;
        anyhow::ensure!(
            effects.contains_key(effect),
            "unknown canvas effect instance '{effect}'"
        );
    }
    Ok(())
}

/// Default `(w, h)` for a freshly placed zone, in normalized canvas units. A
/// linear strip gets a short box so it doesn't float in empty space; every other
/// topology stays square.
fn default_zone_size(topology: Option<ZoneTopology>) -> (f64, f64) {
    match topology {
        Some(ZoneTopology::Linear) => (0.2, 0.05),
        _ => (0.15, 0.15),
    }
}

/// Topology of a device's RGB zone, erroring if the device/zone is unknown so a
/// bogus `channel_id` is never persisted.
async fn require_zone(
    device_id: &str,
    channel_id: &str,
    app: &Arc<AppState>,
) -> Result<ZoneTopology> {
    let device = require_device_owned_id(device_id, app).await?;
    let rgb = device
        .as_lighting()
        .ok_or_else(|| anyhow::anyhow!("device does not support canvas engine: {device_id}"))?;
    rgb.descriptor()
        .channels
        .iter()
        .find(|z| z.id == channel_id)
        .map(|z| z.topology.clone())
        .ok_or_else(|| anyhow::anyhow!("zone '{channel_id}' not found on device '{device_id}'"))
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
    validate_effect_def(&instance_id, &def, &app)?;
    {
        let mut cfg = app.config.write().await;
        let effects = &mut cfg.canvas_state_for_edit().effects;
        anyhow::ensure!(
            effects.contains_key(&instance_id)
                || effects.len() < halod_shared::types::MAX_CANVAS_EFFECTS,
            "too many canvas effects"
        );
        effects.insert(instance_id, def);
    }
    app.request_config_save();
    app.record_change(crate::services::effective_state::Change::Canvas)
        .await;
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
        for zone in &mut cs.placed_zones {
            if zone.effect.as_deref() == Some(instance_id.as_str()) {
                zone.effect = None;
            }
        }
    }
    app.request_config_save();
    app.record_change(crate::services::effective_state::Change::Canvas)
        .await;
    Ok(())
}

pub async fn set_default_effect(instance_id: Option<String>, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        let cs = cfg.canvas_state_for_edit();
        if let Some(id) = &instance_id {
            validate_canvas_id("default effect instance id", id)?;
            anyhow::ensure!(
                cs.effects.contains_key(id),
                "unknown canvas effect instance '{id}'"
            );
        }
        cs.default_effect = instance_id;
    }
    app.request_config_save();
    app.record_change(crate::services::effective_state::Change::Canvas)
        .await;
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
        .as_lighting()
        .ok_or_else(|| anyhow::anyhow!("device does not support canvas engine: {device_id}"))?;

    let mut channels = rgb.placed_channels();
    mutate(&mut channels);
    anyhow::ensure!(
        channels.len() <= halod_shared::types::MAX_PLACED_ZONES,
        "too many canvas channels"
    );
    let effects = app.config.read().await.effective_canvas_state().effects;
    for z in &channels {
        validate_placed_zone(z, &effects)?;
    }
    let mut placement_ids = std::collections::HashSet::with_capacity(channels.len());
    for z in &channels {
        anyhow::ensure!(
            placement_ids.insert((&z.device_id, &z.channel_id)),
            "canvas contains a duplicate device-zone placement"
        );
    }
    rgb.set_canvas_zones(channels);

    persist_device_state(app, device.as_ref()).await;
    Ok(device)
}

/// Add (or replace) a zone placement on the canvas.
#[allow(clippy::too_many_arguments)]
pub async fn place_zone(
    device_id: String,
    channel_id: String,
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
    let (def_w, def_h) =
        default_zone_size(Some(require_zone(&device_id, &channel_id, &app).await?));
    let x = x.unwrap_or(0.0) as f32;
    let y = y.unwrap_or(0.0) as f32;
    let w = w.unwrap_or(def_w) as f32;
    let h = h.unwrap_or(def_h) as f32;
    let rotation = rotation.unwrap_or(0.0) as f32;

    if let Some(effect_id) = &effect {
        let effects = &app.config.read().await.effective_canvas_state().effects;
        anyhow::ensure!(
            effects.contains_key(effect_id),
            "unknown canvas effect instance '{effect_id}'"
        );
    }
    let device = modify_canvas_zones(&device_id, &app, |channels| {
        channels.retain(|z| !(z.device_id == device_id && z.channel_id == channel_id));
        channels.push(config::PlacedZone {
            device_id: device_id.clone(),
            channel_id,
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

    if let Some(rgb) = device.as_lighting() {
        rgb.apply(LightingState::Engine).await?;
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

    app.record_change(crate::services::effective_state::Change::CanvasDevice(
        device_id,
    ))
    .await;
    Ok(())
}

/// Remove a zone from the canvas.
pub async fn remove_zone(device_id: String, channel_id: String, app: Arc<AppState>) -> Result<()> {
    let mut removed = false;
    modify_canvas_zones(&device_id, &app, |channels| {
        let old_len = channels.len();
        channels.retain(|z| !(z.device_id == device_id && z.channel_id == channel_id));
        removed = channels.len() != old_len;
    })
    .await?;
    anyhow::ensure!(
        removed,
        "zone '{channel_id}' is not placed on device '{device_id}'"
    );

    app.record_change(crate::services::effective_state::Change::CanvasDevice(
        device_id,
    ))
    .await;
    Ok(())
}

/// Update the canvas position (and optionally size/rotation) of an existing zone.
#[allow(clippy::too_many_arguments)]
pub async fn move_zone(
    device_id: String,
    channel_id: String,
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
    if let Some(effect_id) = &effect {
        let effects = &app.config.read().await.effective_canvas_state().effects;
        anyhow::ensure!(
            effects.contains_key(effect_id),
            "unknown canvas effect instance '{effect_id}'"
        );
    }

    let mut moved = false;
    modify_canvas_zones(&device_id, &app, |channels| {
        if let Some(z) = channels
            .iter_mut()
            .find(|z| z.device_id == device_id && z.channel_id == channel_id)
        {
            moved = true;
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
    .await?;
    anyhow::ensure!(
        moved,
        "zone '{channel_id}' is not placed on device '{device_id}'"
    );

    // The UI keeps an optimistic transform until the daemon confirms the new
    // placement. Report the semantic topology change so the retained lighting
    // and device records are committed together.
    app.record_change(crate::services::effective_state::Change::CanvasDevice(
        device_id,
    ))
    .await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::state::AppState;
    use crate::test_support::MockDevice;

    fn placed_channels(dev: &MockDevice) -> Vec<config::PlacedZone> {
        dev.rgb.as_ref().unwrap().placed_channels()
    }

    fn make_app(device: Arc<MockDevice>) -> Arc<AppState> {
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .try_write()
            .unwrap()
            .push(device as Arc<dyn crate::drivers::Device>);
        app
    }

    #[test]
    fn default_zone_size_thins_only_linear() {
        let default_size = default_zone_size(None);
        let linear_size = default_zone_size(Some(ZoneTopology::Linear));

        // Linear channels are shorter (in the cross-axis) than every other
        // topology, which all get the same square default.
        assert!(linear_size.1 < default_size.1);
        for topology in [ZoneTopology::Ring, ZoneTopology::Grid] {
            assert_eq!(default_zone_size(Some(topology)), default_size);
        }
    }

    #[tokio::test]
    async fn place_zone_rejects_unknown_zone_id() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        let err = place_zone(
            "dev0".into(),
            "nope".into(),
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
        assert!(err.to_string().contains("not found"), "{err}");
    }

    #[tokio::test]
    async fn upsert_effect_rejects_too_many_params() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev);
        let mut def = EffectDef {
            effect_id: "x".into(),
            name: None,
            params: std::collections::HashMap::new(),
        };
        for i in 0..halod_shared::types::MAX_EFFECT_PARAMS + 1 {
            def.params.insert(
                format!("k{i}"),
                halod_shared::types::EffectParamValue::Float(1.0),
            );
        }
        assert!(upsert_effect("inst".into(), def, app).await.is_err());
    }

    #[tokio::test]
    async fn place_zone_adds_zone_to_device_slot() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        upsert_effect("bars".into(), def("screen_sampler"), app.clone())
            .await
            .unwrap();
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
        let channels = placed_channels(&dev);
        assert_eq!(channels.len(), 1);
        assert_eq!(channels[0].channel_id, "ring");
        assert!((channels[0].x - 0.1).abs() < 1e-5);
        assert!((channels[0].y - 0.2).abs() < 1e-5);
        assert_eq!(channels[0].effect.as_deref(), Some("bars"));
        assert!(
            app.config.read().await.rgb.canvas_enabled,
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
        let channels = placed_channels(&dev);
        assert_eq!(
            channels.len(),
            1,
            "duplicate zone should be replaced not appended"
        );
        assert!((channels[0].x - 0.9).abs() < 1e-5);
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
        assert_eq!(placed_channels(&dev).len(), 1);
        remove_zone("dev0".into(), "ring".into(), app)
            .await
            .unwrap();
        assert!(placed_channels(&dev).is_empty());
    }

    #[tokio::test]
    async fn move_zone_updates_position_and_effect_in_slot() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        upsert_effect("bars".into(), def("screen_sampler"), app.clone())
            .await
            .unwrap();
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
        let channels = placed_channels(&dev);
        assert!((channels[0].x - 0.5).abs() < 1e-5);
        assert!((channels[0].y - 0.6).abs() < 1e-5);
        assert_eq!(channels[0].effect.as_deref(), Some("bars"));
    }

    #[tokio::test]
    async fn remove_zone_errors_for_unknown_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = remove_zone("offline".into(), "ring".into(), app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("offline"));
    }

    #[tokio::test]
    async fn move_zone_errors_for_unknown_device() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = move_zone(
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
        .unwrap_err();
        assert!(err.to_string().contains("offline"));
    }

    #[tokio::test]
    async fn move_and_remove_error_for_unplaced_zone() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev);

        let move_err = move_zone(
            "dev0".into(),
            "ring".into(),
            0.5,
            0.6,
            None,
            None,
            None,
            None,
            None,
            app.clone(),
        )
        .await
        .unwrap_err();
        assert!(move_err.to_string().contains("not placed"));

        let remove_err = remove_zone("dev0".into(), "ring".into(), app)
            .await
            .unwrap_err();
        assert!(remove_err.to_string().contains("not placed"));
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
            Some(LightingState::Engine)
        ));

        stop(app.clone()).await.unwrap();

        assert!(!app.config.read().await.rgb.canvas_enabled);
        assert!(matches!(
            dev.rgb.as_ref().unwrap().current_state(),
            Some(LightingState::Static {
                color: RgbColor { r: 0, g: 0, b: 0 }
            })
        ));
    }

    #[tokio::test]
    async fn stop_blanks_direct_effect_device() {
        use crate::drivers::LightingCapability;
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev.clone());
        // A direct (software) effect is also engine-driven, so stop must blank it.
        dev.apply(LightingState::DirectEffect {
            id: "breathing".into(),
            params: Default::default(),
        })
        .await
        .unwrap();

        stop(app.clone()).await.unwrap();

        assert!(matches!(
            dev.rgb.as_ref().unwrap().current_state(),
            Some(LightingState::Static {
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
    async fn set_default_effect_rejects_a_dangling_instance() {
        let app = Arc::new(AppState::new(Config::default()));
        assert!(set_default_effect(Some("missing".into()), app)
            .await
            .is_err());
    }

    #[tokio::test]
    async fn place_zone_rejects_a_dangling_effect() {
        let dev = Arc::new(MockDevice::new("dev0").with_rgb());
        let app = make_app(dev);
        assert!(place_zone(
            "dev0".into(),
            "ring".into(),
            None,
            None,
            None,
            None,
            None,
            Some("missing".into()),
            None,
            app,
        )
        .await
        .is_err());
    }

    #[tokio::test]
    async fn set_default_effect_updates_config() {
        let app = Arc::new(AppState::new(Config::default()));
        upsert_effect("bars".into(), def("screen_sampler"), app.clone())
            .await
            .unwrap();
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

    let black = LightingState::Static {
        color: RgbColor { r: 0, g: 0, b: 0 },
    };
    let devices = app.device_registry.read().await.clone();
    for device in devices {
        if let Some(rgb) = device.as_lighting() {
            // `DirectEffect` is engine-driven too, so blank it alongside `Engine`.
            if matches!(
                rgb.current_state(),
                Some(LightingState::Engine | LightingState::DirectEffect { .. })
            ) {
                let _ = rgb.apply(black.clone()).await;
                persist_device_state(&app, device.as_ref()).await;
            }
        }
    }

    app.record_change(crate::services::effective_state::Change::LightingTopology)
        .await;
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
    app.record_change(crate::services::effective_state::Change::Canvas)
        .await;
    Ok(())
}
