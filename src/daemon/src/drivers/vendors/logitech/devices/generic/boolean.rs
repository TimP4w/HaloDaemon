// SPDX-License-Identifier: GPL-3.0-or-later

use anyhow::{bail, Result};
use async_trait::async_trait;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::is_host_mode;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::settings::{MODE_HOST, MODE_ONBOARD};
use crate::drivers::BooleanCapability;
use halod_shared::types::Boolean;

#[async_trait]
impl BooleanCapability for LogitechDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        let mut booleans = Vec::new();
        let state = self.state.lock().await;

        if let Some(mode) = state.profile.onboard_mode {
            booleans.push(Boolean {
                key: "host_mode".to_string(),
                label: "Host Mode".to_string(),
                value: is_host_mode(mode),
                read_only: false,
                category: "Profiles".to_string(),
                visible_when: None,
            });
        }

        if state.hires_wheel.present {
            let hw = &state.hires_wheel;
            if hw.has_invert {
                booleans.push(Boolean {
                    key: "hires_invert".to_string(),
                    label: "Scroll Wheel Direction".to_string(),
                    value: hw.invert,
                    read_only: false,
                    category: "Scroll Wheel".to_string(),
                    visible_when: None,
                });
            }
            booleans.push(Boolean {
                key: "hires_resolution".to_string(),
                label: "Scroll Wheel Resolution".to_string(),
                value: hw.hi_res,
                read_only: false,
                category: "Scroll Wheel".to_string(),
                visible_when: None,
            });
            booleans.push(Boolean {
                key: "hires_diversion".to_string(),
                label: "Scroll Wheel Diversion".to_string(),
                value: hw.hidpp_target,
                read_only: false,
                category: "Scroll Wheel".to_string(),
                visible_when: None,
            });
        }

        if state.fn_inversion.present {
            booleans.push(Boolean {
                key: "fn_inversion".to_string(),
                label: "Swap Fx Function".to_string(),
                value: state.fn_inversion.inverted,
                read_only: !state.fn_inversion.writeable,
                category: "Keyboard".to_string(),
                visible_when: None,
            });
        }

        if let Some(ref info) = state.brightness.info {
            if info.has_on_off {
                booleans.push(Boolean {
                    key: "brightness_on".to_string(),
                    label: "Keyboard Brightness".to_string(),
                    value: state.brightness.on,
                    read_only: false,
                    category: "Keyboard".to_string(),
                    visible_when: None,
                });
            }
        }

        Ok(booleans)
    }

    fn state_key(&self) -> &'static str {
        "boolean"
    }

    fn save_state(&self) -> serde_json::Value {
        let host_mode = self
            .state
            .try_lock()
            .ok()
            .and_then(|s| s.profile.onboard_mode.map(is_host_mode));
        let mut v = serde_json::Map::new();
        if let Some(hm) = host_mode {
            v.insert("host_mode".into(), serde_json::json!(hm));
        }
        if let Ok(state) = self.state.try_lock() {
            if state.hires_wheel.present {
                let hw = &state.hires_wheel;
                v.insert("hires_invert".into(), serde_json::json!(hw.invert));
                v.insert("hires_resolution".into(), serde_json::json!(hw.hi_res));
                v.insert("hires_diversion".into(), serde_json::json!(hw.hidpp_target));
            }
            if state.fn_inversion.present {
                v.insert(
                    "fn_inversion".into(),
                    serde_json::json!(state.fn_inversion.inverted),
                );
            }
            if state.brightness.info.as_ref().is_some_and(|i| i.has_on_off) {
                v.insert(
                    "brightness_on".into(),
                    serde_json::json!(state.brightness.on),
                );
            }
        }
        if v.is_empty() {
            serde_json::Value::Null
        } else {
            serde_json::Value::Object(v)
        }
    }

    async fn restore_state(&self, v: &serde_json::Value) {
        let obj = match v.as_object() {
            Some(o) => o,
            None => return,
        };
        for (key, val) in obj {
            let Some(want) = val.as_bool() else { continue };

            if key == "host_mode" {
                let current = self
                    .state
                    .lock()
                    .await
                    .profile
                    .onboard_mode
                    .map(is_host_mode);
                if current != Some(want) {
                    if let Err(e) = self.set_boolean(key, want).await {
                        log::warn!("[{}] restoring {key}={want} failed: {e}", self.id);
                    }
                }
                continue;
            }

            if let Err(e) = self.set_boolean(key, want).await {
                log::warn!("[{}] restoring {key}={want} failed: {e}", self.id);
            }
        }
    }

    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        match key {
            "host_mode" => {
                if self.state.lock().await.profile.onboard_mode.is_none() {
                    bail!("onboard profiles not available");
                }
                self.hidpp2().await.set_onboard_mode(value).await?;
                let mode_byte = if value { MODE_HOST } else { MODE_ONBOARD };
                self.state.lock().await.profile.onboard_mode = Some(mode_byte);

                {
                    let mut state = self.state.lock().await;
                    let features = state.features.clone();
                    self.init_onboard(&features, &mut state).await;
                    self.init_dpi(&features, &mut state).await;
                }

                if value {
                    self.restore_rgb_control().await;
                }
            }
            "hires_invert" => {
                if !self.state.lock().await.hires_wheel.present {
                    bail!("scroll wheel not available");
                }
                self.hidpp2().await.set_hires_invert(value).await?;
                self.state.lock().await.hires_wheel.invert = value;
            }
            "hires_resolution" => {
                if !self.state.lock().await.hires_wheel.present {
                    bail!("scroll wheel not available");
                }
                self.hidpp2().await.set_hires_resolution(value).await?;
                self.state.lock().await.hires_wheel.hi_res = value;
            }
            "hires_diversion" => {
                if !self.state.lock().await.hires_wheel.present {
                    bail!("scroll wheel not available");
                }
                self.hidpp2().await.set_hires_diversion(value).await?;
                self.state.lock().await.hires_wheel.hidpp_target = value;
            }
            "fn_inversion" => {
                if !self.state.lock().await.fn_inversion.writeable {
                    bail!("Fn inversion not available");
                }
                self.hidpp2().await.set_fn_inversion(!value).await?;
                self.state.lock().await.fn_inversion.inverted = value;
            }
            "brightness_on" => {
                if !self
                    .state
                    .lock()
                    .await
                    .brightness
                    .info
                    .as_ref()
                    .is_some_and(|i| i.has_on_off)
                {
                    bail!("brightness on/off not available");
                }
                self.hidpp2().await.set_brightness_on_off(value).await?;
                self.state.lock().await.brightness.on = value;
            }
            other => bail!("unknown boolean key: {other}"),
        }
        Ok(())
    }
}
