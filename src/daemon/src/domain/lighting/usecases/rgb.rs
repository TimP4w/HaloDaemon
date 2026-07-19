// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::{Context, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use crate::domain::profiles::device_state::persist_device_state;
use crate::domain::registry::require_device_owned_id;
use crate::domain::registry::usecases::settings;
use halod_shared::types::{LightingChannel, LightingState, ZoneTopology};
use halod_shared::zone_transform::ZoneContentTransform;

fn validate_channel_transform(
    channel: &LightingChannel,
    transform: &mut ZoneContentTransform,
) -> Result<()> {
    match &channel.topology {
        ZoneTopology::Ring => {
            anyhow::ensure!(
                !transform.flip_h && !transform.flip_v && !transform.swap_rings,
                "ring transforms cannot use geometric flips or swap_rings"
            );
            if !channel.leds.is_empty() {
                transform.led_offset = transform.led_offset.rem_euclid(channel.leds.len() as i32);
            }
        }
        ZoneTopology::Rings { count } => {
            anyhow::ensure!(
                !transform.flip_h && !transform.flip_v,
                "ring transforms cannot use geometric flips"
            );
            let ring_len = channel.leds.len() / usize::from((*count).max(1));
            anyhow::ensure!(ring_len > 0, "ring channel has no addressable LEDs");
            transform.led_offset = transform.led_offset.rem_euclid(ring_len as i32);
        }
        ZoneTopology::Linear | ZoneTopology::Grid | ZoneTopology::Keyboard { .. } => {
            anyhow::ensure!(
                !transform.reverse && transform.led_offset == 0 && !transform.swap_rings,
                "non-ring transforms only support geometric flips"
            );
        }
    }
    Ok(())
}

pub async fn lighting_apply(id: String, state: LightingState, app: Arc<AppState>) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lighting = device
        .as_lighting()
        .with_context(|| format!("device '{id}' does not support lighting"))?;

    // A non-Engine state takes the device off the canvas.
    let left_canvas =
        !matches!(state, LightingState::Engine) && !lighting.placed_channels().is_empty();
    if left_canvas {
        lighting.set_canvas_zones(Vec::new());
    }

    let needs_engine = matches!(state, LightingState::DirectEffect { .. });

    let leaving_engine = !needs_engine
        && matches!(
            lighting.current_state(),
            Some(LightingState::Engine | LightingState::DirectEffect { .. })
        );

    lighting.apply(state.clone()).await?;
    persist_device_state(&app, device.as_ref()).await;

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
        lighting.apply(state).await?;
    }
    app.record_change(crate::domain::events::Change::LightingDevice(id))
        .await;
    Ok(())
}

/// Set a zone's LED-content transform. The transform is a persistent per-device
/// setting, applied to all daemon-driven output for that zone.
pub async fn set_channel_transform(
    id: String,
    channel_id: String,
    mut transform: ZoneContentTransform,
    app: Arc<AppState>,
) -> Result<()> {
    let device = require_device_owned_id(&id, &app).await?;
    let lighting = device
        .as_lighting()
        .context("device does not support lighting")?;

    let channel = lighting
        .descriptor()
        .channels
        .iter()
        .find(|channel| channel.id == channel_id)
        .ok_or_else(|| anyhow::anyhow!("channel '{channel_id}' not found on device '{id}'"))?;
    validate_channel_transform(channel, &mut transform)?;

    lighting.set_channel_transform(channel_id.clone(), transform);

    if let Some(state) = lighting.current_state() {
        lighting.apply(state).await?;
    }

    {
        let mut cfg = app.config.write().await;
        cfg.device_transforms
            .entry(device.id().to_owned())
            .or_default()
            .insert(channel_id, transform);
    }
    app.request_config_save();
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::device::LightingCapability;
    use crate::test_support::MockDevice;
    use halod_shared::types::RgbColor;
    #[tokio::test]
    async fn set_channel_transform_stores_transform_in_config() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::domain::device::Device>);

        set_channel_transform(
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

        let device_transforms = dev.channel_transforms();
        let device_t = device_transforms
            .get("ring")
            .copied()
            .expect("transform should be stored in device");
        assert!(device_t.reverse);
        assert_eq!(device_t.led_offset, 4);
    }

    #[tokio::test]
    async fn set_channel_transform_reapplies_current_state() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::domain::device::Device>);

        let color = RgbColor {
            r: 10,
            g: 20,
            b: 30,
        };
        dev.apply(LightingState::Static { color }).await.unwrap();

        set_channel_transform(
            "dev1".into(),
            "ring".into(),
            ZoneContentTransform {
                reverse: true,
                ..Default::default()
            },
            app.clone(),
        )
        .await
        .unwrap();

        // The state survived the transform change (re-applied, not cleared).
        assert!(
            matches!(dev.current_state(), Some(LightingState::Static { color: c }) if c == color),
            "current state should remain the re-applied static colour"
        );
    }

    #[tokio::test]
    async fn set_channel_transform_without_state_is_ok() {
        // No current state → nothing to re-apply, but the call still succeeds and
        // stores the transform.
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::domain::device::Device>);

        set_channel_transform(
            "dev1".into(),
            "ring".into(),
            ZoneContentTransform {
                reverse: true,
                ..Default::default()
            },
            app,
        )
        .await
        .unwrap();
        assert!(dev.channel_transforms().get("ring").unwrap().reverse);
    }

    #[tokio::test]
    async fn rgb_apply_applies_state_and_persists() {
        let dev = std::sync::Arc::new(MockDevice::new("dev1").with_rgb());
        let app = std::sync::Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as std::sync::Arc<dyn crate::domain::device::Device>);

        let state = LightingState::Static {
            color: RgbColor { r: 255, g: 0, b: 0 },
        };
        lighting_apply("dev1".into(), state.clone(), app.clone())
            .await
            .unwrap();

        let current = dev.lighting_state().current_state();
        assert!(
            matches!(current, Some(LightingState::Static { color }) if color.r == 255 && color.g == 0 && color.b == 0)
        );
    }

    #[tokio::test]
    async fn rgb_apply_clears_canvas_placement() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::domain::device::Device>);

        dev.rgb
            .as_ref()
            .unwrap()
            .set_canvas_zones(vec![crate::config::PlacedZone {
                device_id: "dev1".into(),
                channel_id: "ring".into(),
                x: 0.0,
                y: 0.0,
                w: 0.1,
                h: 0.1,
                rotation: 0.0,
                effect: None,
                sampling_mode: Default::default(),
            }]);

        lighting_apply(
            "dev1".into(),
            LightingState::Static {
                color: RgbColor { r: 1, g: 2, b: 3 },
            },
            app,
        )
        .await
        .unwrap();

        assert!(
            dev.rgb.as_ref().unwrap().placed_channels().is_empty(),
            "applying a per-device effect removes the device from the canvas"
        );
    }

    #[tokio::test]
    async fn rgb_apply_engine_state_keeps_canvas_placement() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::domain::device::Device>);

        dev.rgb
            .as_ref()
            .unwrap()
            .set_canvas_zones(vec![crate::config::PlacedZone {
                device_id: "dev1".into(),
                channel_id: "ring".into(),
                x: 0.0,
                y: 0.0,
                w: 0.1,
                h: 0.1,
                rotation: 0.0,
                effect: None,
                sampling_mode: Default::default(),
            }]);

        lighting_apply("dev1".into(), LightingState::Engine, app)
            .await
            .unwrap();

        assert_eq!(dev.rgb.as_ref().unwrap().placed_channels().len(), 1);
    }

    #[tokio::test]
    async fn rgb_apply_leaving_direct_effect_reapplies_after_drain() {
        // A tick already in flight for the old DirectEffect can still write a
        // stale frame after our apply below; the drain + re-apply must make
        // the final write ours (see `super::canvas::STOP_DRAIN_MS`).
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::domain::device::Device>);
        dev.apply(LightingState::DirectEffect {
            id: "breathing".into(),
            params: Default::default(),
        })
        .await
        .unwrap();
        dev.rgb_apply_count
            .store(0, std::sync::atomic::Ordering::SeqCst);

        let black = LightingState::Static {
            color: RgbColor { r: 0, g: 0, b: 0 },
        };
        lighting_apply("dev1".into(), black.clone(), app)
            .await
            .unwrap();

        assert_eq!(
            dev.rgb_apply_count
                .load(std::sync::atomic::Ordering::SeqCst),
            2,
            "must re-apply once after the drain, not just once up front"
        );
        assert!(matches!(
            dev.rgb.as_ref().unwrap().current_state(),
            Some(LightingState::Static { color }) if color.r == 0 && color.g == 0 && color.b == 0
        ));
    }

    #[tokio::test]
    async fn rgb_apply_static_to_static_does_not_drain_again() {
        let dev = Arc::new(MockDevice::new("dev1").with_rgb());
        let app = Arc::new(AppState::new(Config::default()));
        app.device_registry
            .write()
            .await
            .push(dev.clone() as Arc<dyn crate::domain::device::Device>);
        dev.apply(LightingState::Static {
            color: RgbColor { r: 1, g: 2, b: 3 },
        })
        .await
        .unwrap();
        dev.rgb_apply_count
            .store(0, std::sync::atomic::Ordering::SeqCst);

        lighting_apply(
            "dev1".into(),
            LightingState::Static {
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
        app.device_registry
            .write()
            .await
            .push(dev as std::sync::Arc<dyn crate::domain::device::Device>);

        let err = lighting_apply(
            "dev1".into(),
            LightingState::Static {
                color: RgbColor { r: 0, g: 0, b: 0 },
            },
            app,
        )
        .await
        .unwrap_err();
        assert!(err.to_string().contains("lighting"));
    }
}
