// SPDX-License-Identifier: GPL-3.0-or-later
use crate::domain::events::ChangeSink as _;

use anyhow::{bail, Result};
use std::sync::Arc;

use crate::application::state::AppState;
use halod_shared::commands::OverrideTarget;
use halod_shared::types::DEFAULT_PROFILE_NAME;

/// Drop a tracked override from the named profile so the unit reverts to the
/// default profile, then re-apply effective state to the live devices when
/// that profile is active.
pub async fn remove_profile_override(
    profile_name: String,
    target: OverrideTarget,
    app: Arc<AppState>,
) -> Result<()> {
    let is_active = {
        let mut cfg = app.config.write().await;
        if profile_name == DEFAULT_PROFILE_NAME {
            bail!("cannot remove overrides from the default profile");
        }
        log::info!("[Profile] Removing override {target:?} from '{profile_name}'");
        let is_active = cfg.active_profile == profile_name;
        let Some(profile) = cfg.profiles.get_mut(&profile_name) else {
            bail!("unknown profile '{profile_name}'");
        };
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
        is_active
    };
    app.request_config_save();
    if is_active {
        super::lifecycle::load_active_profile(app.clone()).await;
        app.record_change(crate::domain::events::Change::ProfileSwitch)
            .await;
    } else {
        app.record_change(crate::domain::events::Change::Profiles)
            .await;
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::config::Config;
    use crate::domain::profiles::model::Profile;
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
            "Gaming".into(),
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
                "Gaming".into(),
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
            lighting: crate::domain::lighting::model::ProfileLighting {
                canvas: Some(Default::default()),
                ..Default::default()
            },
            ..Default::default()
        };
        cfg.profiles.insert("Gaming".into(), gaming);
        cfg.active_profile = "Gaming".into();
        let app = Arc::new(AppState::new(cfg));

        remove_profile_override("Gaming".into(), OverrideTarget::Canvas, app.clone())
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
            "Gaming".into(),
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
        let err = remove_profile_override(DEFAULT_PROFILE_NAME.into(), OverrideTarget::Canvas, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("default"));
    }

    #[tokio::test]
    async fn rejects_unknown_profile() {
        let app = Arc::new(AppState::new(Config::default()));
        let err = remove_profile_override("Ghost".into(), OverrideTarget::Canvas, app)
            .await
            .unwrap_err();
        assert!(err.to_string().contains("unknown profile"));
    }

    #[tokio::test]
    async fn removes_override_from_inactive_profile() {
        let app = app_with_override();
        {
            let mut cfg = app.config.write().await;
            cfg.active_profile = DEFAULT_PROFILE_NAME.into();
        }
        remove_profile_override(
            "Gaming".into(),
            OverrideTarget::DeviceCapability {
                device_id: "dev1".into(),
                state_key: "fan_curve".into(),
            },
            app.clone(),
        )
        .await
        .unwrap();
        let cfg = app.config.read().await;
        let dev = cfg
            .profiles
            .get("Gaming")
            .unwrap()
            .device_states
            .get("dev1")
            .unwrap();
        assert!(dev.get("fan_curve").is_none());
        assert!(dev.get("rgb").is_some());
    }
}
