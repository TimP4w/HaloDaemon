// SPDX-License-Identifier: GPL-3.0-or-later
use anyhow::{bail, Result};
use std::sync::Arc;

use crate::state::AppState;
use halod_shared::commands::OverrideTarget;
use halod_shared::types::DEFAULT_PROFILE_NAME;

/// Drop a tracked override from the active profile so the unit reverts to the
/// default profile, then re-apply effective state to the live devices.
pub async fn remove_profile_override(target: OverrideTarget, app: Arc<AppState>) -> Result<()> {
    {
        let mut cfg = app.config.write().await;
        if cfg.active_profile == DEFAULT_PROFILE_NAME {
            bail!("cannot remove overrides from the default profile");
        }
        log::info!(
            "[Profile] Removing override {target:?} from '{}'",
            cfg.active_profile
        );
        let profile = cfg.active_profile_data_mut();
        match target {
            OverrideTarget::DeviceCapability {
                device_id,
                state_key,
            } => {
                let empty = if let Some(obj) = profile
                    .device_states
                    .get_mut(&device_id)
                    .and_then(|v| v.as_object_mut())
                {
                    obj.remove(&state_key);
                    obj.is_empty()
                } else {
                    false
                };
                if empty {
                    profile.device_states.remove(&device_id);
                }
            }
            OverrideTarget::Canvas => {
                profile.lighting.canvas = None;
            }
        }
    }
    app.request_config_save();
    super::profiles::load_active_profile(app.clone()).await;
    crate::ipc::broadcast_state(&app).await;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::profiles::config::Profile;
    use serde_json::json;

    fn app_with_override() -> Arc<AppState> {
        let mut cfg = Config::default();
        let mut gaming = Profile::default();
        gaming.device_states.insert(
            "dev1".into(),
            json!({ "fan_curve": {"a": 2}, "rgb": {"m": "x"} }),
        );
        cfg.profiles.insert("Gaming".into(), gaming);
        cfg.active_profile = "Gaming".into();
        Arc::new(AppState::new(cfg))
    }

    #[tokio::test]
    async fn removes_one_capability_keeps_others() {
        let app = app_with_override();
        remove_profile_override(
            OverrideTarget::DeviceCapability {
                device_id: "dev1".into(),
                state_key: "fan_curve".into(),
            },
            app.clone(),
        )
        .await
        .unwrap();
        let cfg = app.config.read().await;
        let g = cfg.profiles.get("Gaming").unwrap();
        let dev = g.device_states.get("dev1").unwrap();
        assert!(dev.get("fan_curve").is_none());
        assert!(dev.get("rgb").is_some());
    }

    #[tokio::test]
    async fn removing_last_capability_prunes_device_entry() {
        let app = app_with_override();
        for key in ["fan_curve", "rgb"] {
            remove_profile_override(
                OverrideTarget::DeviceCapability {
                    device_id: "dev1".into(),
                    state_key: key.into(),
                },
                app.clone(),
            )
            .await
            .unwrap();
        }
        let cfg = app.config.read().await;
        assert!(!cfg
            .profiles
            .get("Gaming")
            .unwrap()
            .device_states
            .contains_key("dev1"));
    }

    #[tokio::test]
    async fn removes_canvas_override() {
        let mut cfg = Config::default();
        let gaming = Profile {
            lighting: crate::lighting::config::ProfileLighting {
                canvas: Some(Default::default()),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.profiles.insert("Gaming".into(), gaming);
        cfg.active_profile = "Gaming".into();
        let app = Arc::new(AppState::new(cfg));

        remove_profile_override(OverrideTarget::Canvas, app.clone())
            .await
            .unwrap();

        let cfg = app.config.read().await;
        let g = cfg.profiles.get("Gaming").unwrap();
        assert!(g.lighting.canvas.is_none());
    }

    #[tokio::test]
    async fn absent_device_is_graceful_noop() {
        let app = app_with_override();
        remove_profile_override(
            OverrideTarget::DeviceCapability {
                device_id: "nonexistent".into(),
                state_key: "fan_curve".into(),
            },
            app.clone(),
        )
        .await
        .unwrap();
        // The pre-existing override is untouched.
        let cfg = app.config.read().await;
        assert!(cfg
            .profiles
            .get("Gaming")
            .unwrap()
            .device_states
            .contains_key("dev1"));
    }

    #[tokio::test]
    async fn rejects_default_profile() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = remove_profile_override(OverrideTarget::Canvas, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("default"));
    }
}
