// SPDX-License-Identifier: GPL-3.0-or-later
use std::sync::{Arc, OnceLock};
use tokio::sync::RwLock;

use halod_shared::types::{
    finite_or, CanvasState, EffectDef, LightingOverviewState as WireLightingState,
    DEFAULT_SAMPLE_RADIUS,
};

use crate::config::{Config, PlacedZone};
use crate::domain::lighting::engine::RgbEngine;

fn sanitize_zone(mut p: PlacedZone) -> PlacedZone {
    p.sanitize();
    p
}

struct Engine {
    handle: Arc<RgbEngine>,
}

/// Cached custom Designer effects, refreshed from disk after save/delete.
pub struct CustomEffectsState {
    pub effects: RwLock<Vec<EffectDef>>,
}

impl CustomEffectsState {
    pub fn new() -> Self {
        Self {
            effects: RwLock::new(
                crate::domain::lighting::usecases::custom_effects::list_custom_effects(),
            ),
        }
    }

    /// Re-read saved effects from disk into the cache.
    pub async fn refresh(&self) {
        let effects = crate::domain::lighting::usecases::custom_effects::list_custom_effects();
        *self.effects.write().await = effects;
    }
}

/// Lighting state: the RGB/canvas engine handle, its runtime config channel,
/// and the custom Designer effects cache. The engine is set once at startup
/// (see `main.rs`) after `AppState` already exists, so accessors are fallible
/// until then — callers gate on `AppState::engines_ready`.
pub struct LightingState {
    engine: OnceLock<Engine>,
    pub custom_effects: CustomEffectsState,
}

impl Default for LightingState {
    fn default() -> Self {
        Self {
            engine: OnceLock::new(),
            custom_effects: CustomEffectsState::new(),
        }
    }
}

impl LightingState {
    pub fn engine(&self) -> Option<&Arc<RgbEngine>> {
        self.engine.get().map(|e| &e.handle)
    }

    pub fn set_engine(&self, handle: Arc<RgbEngine>) {
        let _ = self.engine.set(Engine { handle });
    }

    pub async fn snapshot(
        &self,
        registry: &crate::domain::plugin::Registry,
        cfg: &Config,
        placed_zones: Vec<PlacedZone>,
    ) -> WireLightingState {
        let cs = cfg.effective_canvas_state();
        let custom_direct_effects = self.custom_effects.effects.read().await.clone();

        WireLightingState {
            canvas: CanvasState {
                available_effects: RgbEngine::available_effect_descriptors(registry),
                available_direct_effects: RgbEngine::direct_effect_descriptors(registry),
                custom_direct_effects,
                effects: cs.effects.clone(),
                default_effect: cs.default_effect.clone(),
                placed_zones: placed_zones.into_iter().map(sanitize_zone).collect(),
                sample_radius: finite_or(cs.sample_radius, DEFAULT_SAMPLE_RADIUS),
            },
            targets: cfg.active_profile_data().lighting.targets.clone(),
            // Overwritten by the serializer from the persisted config.
            config: Default::default(),
        }
    }
}

#[cfg(test)]
mod override_map_tests {
    use super::sanitize_zone;
    use crate::config::PlacedZone;

    #[test]
    fn placed_zone_sanitize_preserves_finite_fields() {
        let p = PlacedZone {
            device_id: "dev1".into(),
            channel_id: "zoneA".into(),
            x: 0.1,
            y: 0.2,
            w: 0.3,
            h: 0.4,
            rotation: 1.5,
            effect: Some("bars".into()),
            sampling_mode: Default::default(),
        };
        let w = sanitize_zone(p.clone());
        assert_eq!(w.device_id, p.device_id);
        assert_eq!(w.channel_id, p.channel_id);
        assert_eq!(w.x, p.x);
        assert_eq!(w.y, p.y);
        assert_eq!(w.w, p.w);
        assert_eq!(w.h, p.h);
        assert_eq!(w.rotation, p.rotation);
        assert_eq!(w.effect, p.effect);
    }

    #[test]
    fn placed_zone_nan_inf_replaced_with_fallbacks() {
        let p = PlacedZone {
            device_id: "dev1".into(),
            channel_id: "zoneA".into(),
            x: f32::NAN,
            y: f32::INFINITY,
            w: f32::NEG_INFINITY,
            h: f32::NAN,
            rotation: f32::NAN,
            effect: None,
            sampling_mode: Default::default(),
        };
        let w = sanitize_zone(p);
        assert!(w.x.is_finite());
        assert!(w.y.is_finite());
        assert!(w.w.is_finite());
        assert!(w.h.is_finite());
        assert_eq!(w.x, 0.0);
        assert_eq!(w.y, 0.0);
        assert_eq!(w.w, 0.15);
        assert_eq!(w.h, 0.15);
    }
}

#[cfg(test)]
mod snapshot_tests {
    use super::*;

    #[tokio::test]
    async fn snapshot_includes_placed_zones_from_devices() {
        let app = Arc::new(crate::application::state::AppState::new(Config::default()));
        let state = LightingState::default();
        let cfg = Config::default();
        let channels = vec![PlacedZone {
            device_id: "rgb_dev".to_string(),
            channel_id: "zone_1".to_string(),
            x: 10.0,
            y: 20.0,
            w: 100.0,
            h: 50.0,
            rotation: 0.0,
            effect: None,
            sampling_mode: Default::default(),
        }];

        let wire = state.snapshot(&app.registry, &cfg, channels).await;

        assert_eq!(wire.canvas.placed_zones.len(), 1);
        assert_eq!(wire.canvas.placed_zones[0].device_id, "rgb_dev");
        assert_eq!(wire.canvas.placed_zones[0].channel_id, "zone_1");
        assert_eq!(wire.canvas.placed_zones[0].x, 10.0);
        assert_eq!(wire.canvas.placed_zones[0].y, 20.0);
    }
}
