// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{Context, Result};
use std::sync::Arc;

use crate::profiles::device_state::persist_device_state;
use crate::registry::require_device_owned_id;
use crate::registry::usecases::settings;
use crate::{ipc, state::AppState};
use halod_shared::types::RgbState;
use halod_shared::zone_transform::ZoneContentTransform;

pub async fn rgb_apply(id: String, state: RgbState, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let rgb = device
        .as_rgb()
        .with_context(|| format!("device '{id}' does not support RGB"))?;

    // A non-Engine state takes the device off the canvas.
    let left_canvas = !matches!(state, RgbState::Engine) && !rgb.canvas_zones().is_empty();
    if left_canvas {
        rgb.set_canvas_zones(Vec::new());
    }

    let needs_engine = matches!(state, RgbState::DirectEffect { .. });

    let leaving_engine = !needs_engine
        && matches!(
            rgb.current_state(),
            Some(RgbState::Engine | RgbState::DirectEffect { .. })
        );

    rgb.apply(state.clone()).await?;
    persist_device_state(&app, device.as_ref()).await;

    if left_canvas {
        ipc::broadcast_state(&app).await;
    }

    if needs_engine {
        settings::set_engine_config(
            halod_shared::commands::EngineKind::Canvas,
            Some(true),
            None,
            None,
            None,
            Arc::clone(&app),
        )
        .await?;
    } else if leaving_engine {
        tokio::time::sleep(std::time::Duration::from_millis(
            super::canvas::STOP_DRAIN_MS,
        ))
        .await;
        rgb.apply(state).await?;
    }
    Ok(())
}

/// Set a zone's LED-content transform. The transform is a persistent per-device
/// setting, applied to all daemon-driven output for that zone.
pub async fn set_zone_transform(
    id: String,
    zone_id: String,
    transform: ZoneContentTransform,
    app: Arc<AppState>,
) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let rgb = device.as_rgb().context("device does not support RGB")?;

    rgb.set_zone_transform(zone_id.clone(), transform);

    if let Some(state) = rgb.current_state() {
        rgb.apply(state).await?;
    }

    {
        let mut cfg = app.config.write().await;
        cfg.device_transforms
            .entry(device.id().to_owned())
            .or_default()
            .insert(zone_id, transform);
    }
    app.request_config_save();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::drivers::RgbCapability;
    use crate::test_support::MockDevice;
    use halod_shared::types::RgbColor;
    #[tokio::test]
    async fn set_zone_transform_stores_transform_in_config() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);

        set_zone_transform(
            "dev1".into(),
            "ring".into(),
            ZoneContentTransform {
                reverse: true,
                led_offset: 4,
                flip_h: false,
                flip_v: false,
                swap_rings: false,
            },
            app.clone(),
        )
        .await
        .unwrap();

        let cfg = app.config.read().await;
        let t = cfg
            .device_transforms
            .get("dev1")
            .and_then(|m| m.get("ring"))
            .copied()
            .expect("transform should be stored in config");
        assert!(t.reverse);
        assert_eq!(t.led_offset, 4);
        assert!(!t.flip_h && !t.flip_v);

        let device_transforms = dev.zone_transforms();
        let device_t = device_transforms
            .get("ring")
            .copied()
            .expect("transform should be stored in device");
        assert!(device_t.reverse);
        assert_eq!(device_t.led_offset, 4);
    }

    #[tokio::test]
    async fn set_zone_transform_reapplies_current_state() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);

        let color = RgbColor {
            r: 10,
            g: 20,
            b: 30,
        };
        dev.apply(RgbState::Static { color }).await.unwrap();

        set_zone_transform(
            "dev1".into(),
            "ring".into(),
            ZoneContentTransform {
                flip_h: true,
                ..Default::default()
            },
            app.clone(),
        )
        .await
        .unwrap();

        // The state survived the transform change (re-applied, not cleared).
        assert!(
            matches!(dev.current_state(), Some(RgbState::Static { color: c }) if c == color),
            "current state should remain the re-applied static colour"
        );
    }

    #[tokio::test]
    async fn set_zone_transform_without_state_is_ok() {
        // No current state → nothing to re-apply, but the call still succeeds and
        // stores the transform.
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);

        set_zone_transform(
            "dev1".into(),
            "ring".into(),
            ZoneContentTransform {
                flip_v: true,
                ..Default::default()
            },
            app,
        )
        .await
        .unwrap();
        assert!(dev.zone_transforms().get("ring").unwrap().flip_v);
    }

    #[tokio::test]
    async fn rgb_apply_applies_state_and_persists() {
        let dev = std::sync::Arc::new(MockDevice::new("dev1").with_rgb());
        let app = std::sync::Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as std::sync::Arc<dyn crate::drivers::Device>);

        let state = RgbState::Static {
            color: RgbColor { r: 255, g: 0, b: 0 },
        };
        rgb_apply("dev1".into(), state.clone(), app.clone())
            .await
            .unwrap();

        let current = dev.rgb_state().current_state();
        assert!(
            matches!(current, Some(RgbState::Static { color }) if color.r == 255 && color.g == 0 && color.b == 0)
        );
    }

    #[tokio::test]
    async fn rgb_apply_clears_canvas_placement() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);

        dev.rgb
            .as_ref()
            .unwrap()
            .set_canvas_zones(vec![crate::config::PlacedZone {
                device_id: "dev1".into(),
                zone_id: "ring".into(),
                x: 0.0,
                y: 0.0,
                w: 0.1,
                h: 0.1,
                rotation: 0.0,
                effect: None,
                sampling_mode: Default::default(),
            }]);

        rgb_apply(
            "dev1".into(),
            RgbState::Static {
                color: RgbColor { r: 1, g: 2, b: 3 },
            },
            app,
        )
        .await
        .unwrap();

        assert!(
            dev.rgb.as_ref().unwrap().canvas_zones().is_empty(),
            "applying a per-device effect removes the device from the canvas"
        );
    }

    #[tokio::test]
    async fn rgb_apply_engine_state_keeps_canvas_placement() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);

        dev.rgb
            .as_ref()
            .unwrap()
            .set_canvas_zones(vec![crate::config::PlacedZone {
                device_id: "dev1".into(),
                zone_id: "ring".into(),
                x: 0.0,
                y: 0.0,
                w: 0.1,
                h: 0.1,
                rotation: 0.0,
                effect: None,
                sampling_mode: Default::default(),
            }]);

        rgb_apply("dev1".into(), RgbState::Engine, app)
            .await
            .unwrap();

        assert_eq!(dev.rgb.as_ref().unwrap().canvas_zones().len(), 1);
    }

    #[tokio::test]
    async fn rgb_apply_leaving_direct_effect_reapplies_after_drain() {
        // A tick already in flight for the old DirectEffect can still write a
        // stale frame after our apply below; the drain + re-apply must make
        // the final write ours (see `super::canvas::STOP_DRAIN_MS`).
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);
        dev.apply(RgbState::DirectEffect {
            id: "breathing".into(),
            params: Default::default(),
        })
        .await
        .unwrap();
        dev.rgb_apply_count
            .store(0, std::sync::atomic::Ordering::SeqCst);

        let black = RgbState::Static {
            color: RgbColor { r: 0, g: 0, b: 0 },
        };
        rgb_apply("dev1".into(), black.clone(), app).await.unwrap();

        assert_eq!(
            dev.rgb_apply_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must re-apply once after the drain, not just once up front"
        );
        assert!(matches!(
            dev.rgb.as_ref().unwrap().current_state(),
            Some(RgbState::Static { color }) if color.r == 0 && color.g == 0 && color.b == 0
        ));
    }

    #[tokio::test]
    async fn rgb_apply_static_to_static_does_not_drain_again() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::drivers::Device>);
        dev.apply(RgbState::Static {
            color: RgbColor { r: 1, g: 2, b: 3 },
        })
        .await
        .unwrap();
        dev.rgb_apply_count
            .store(0, std::sync::atomic::Ordering::SeqCst);

        rgb_apply(
            "dev1".into(),
            RgbState::Static {
                color: RgbColor { r: 4, g: 5, b: 6 },
            },
            app,
        )
        .await
        .unwrap();

        assert_eq!(
            dev.rgb_apply_count
                .load(std::sync::atomic::Ordering::SeqCst),
            1,
            "no extra drain re-apply when the device wasn't engine-driven"
        );
    }

    #[tokio::test]
    async fn rgb_apply_errors_when_device_lacks_rgb_capability() {
        let dev = std::sync::Arc::new(MockDevice::new("dev1"));
        let app = std::sync::Arc::new(AppState::new(Config::default()));
        app.devices
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::drivers::Device>);

        let err = rgb_apply(
            "dev1".into(),
            RgbState::Static {
                color: RgbColor { r: 0, g: 0, b: 0 },
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("RGB"));
    }
}
