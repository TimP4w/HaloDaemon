// SPDX-License-Identifier: GPL-3.0-or-later
//! Capability traits a `Device` can expose, plus the `Controller`,
//! `TransportSwitchable`, and chain-related traits. Re-exported from
//! `drivers/mod.rs` so call sites keep using `crate::drivers::*`.

use super::*;
use anyhow::Result;
use async_trait::async_trait;
use halod_shared::keyboard::{
    is_iso_language, KeyVariant, KeyboardLayoutSelection, KeyboardLayoutStatus,
};
use halod_shared::types::{
    Battery, Boolean, ButtonMapping, ConnectionStatus, CoolingChannel, CoolingStatus,
    DeviceCapability, DpiStatus, EffectParamValue, Equalizer, KeyRemapStatus, KeyboardLayout,
    LcdDescriptor, LcdStatus, LightingDescriptor, LightingState, LightingStatus, Sensor,
    SensorType, SensorUnit, VisibilityState, ZoneTopology,
};
use halod_shared::zone_transform::ZoneContentTransform;
use std::collections::HashMap;
use std::sync::Arc;

#[async_trait]
pub trait Controller: Send + Sync {
    async fn discover_children(&self) -> Vec<Arc<dyn Device>> {
        vec![]
    }

    /// Re-probe paired slots for a device dropped by its wired sibling. Default no-op.
    async fn rescan_children(&self) -> Vec<Arc<dyn Device>> {
        vec![]
    }

    /// Re-enumerate children and diff against `existing`, returning
    /// `(added, gone)`. `Err` means the backing connection dropped. Default no-op.
    async fn resync_children(
        &self,
        _existing: &std::collections::HashSet<String>,
    ) -> Result<(Vec<Arc<dyn Device>>, Vec<String>)> {
        Ok((vec![], vec![]))
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        None
    }
}

/// Pair/unpair wireless devices on a receiver and surface pairing state to the UI.
#[async_trait]
pub trait PairingCapability: Send + Sync {
    /// Open the pairing lock for `timeout_secs` so a new device can be paired.
    async fn start_pairing(&self, timeout_secs: u8) -> Result<()>;
    /// Close the pairing lock, cancelling an in-progress pairing window.
    async fn stop_pairing(&self) -> Result<()>;
    /// Unpair the device occupying `slot` (1-based) and drop it from the
    /// receiver's own registries, returning the removed device so the caller can
    /// remove it from the app registry and close it. `None` if no device was
    /// tracked at that slot.
    async fn unpair(&self, slot: u8) -> Result<Option<Arc<dyn Device>>>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        None
    }
}

// Chain support — chainable ARGB channels with daisy-chained child devices.

#[derive(Debug, Clone)]
pub struct ChainLinkSpec {
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
}

/// Cooling surface for chain accessories. Chain RGB writes remain the concern
/// of [`chain::LightingDivisionHub`]; this trait routes a child's cooling channel back to
/// its parent controller.
#[async_trait]
pub trait CoolingHub: Send + Sync + 'static {
    async fn get_cooling_status(&self, channel: u8) -> Result<CoolingChannel>;
    async fn set_cooling_duty(&self, channel: u8, duty: u8) -> Result<()>;
}

/// Unified cooling surface for one or more independently addressable outputs.
#[async_trait]
pub trait CoolingCapability: Send + Sync {
    fn cooling_channels(&self) -> Vec<CoolingChannel>;
    async fn get_cooling_status(&self, channel_id: &str) -> Result<CoolingChannel>;
    async fn set_cooling_duty(&self, channel_id: &str, duty: u8) -> Result<()>;
    fn cooling_state(&self) -> &CoolingStateSlot;

    fn curves(&self) -> HashMap<String, crate::cooling::config::FanCurveRecord> {
        self.cooling_state().curves()
    }
    fn curve(&self, channel_id: &str) -> Option<crate::cooling::config::FanCurveRecord> {
        self.cooling_state().curve(channel_id)
    }
    fn set_curve(&self, channel_id: String, curve: crate::cooling::config::FanCurveRecord) {
        self.cooling_state().set_curve(channel_id, curve);
    }
    fn clear_curve(&self, channel_id: &str) {
        self.cooling_state().clear_curve(channel_id);
    }
    fn clear_curves(&self) {
        self.cooling_state().clear_curves();
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let mut channels = self.cooling_channels();
        for channel in &mut channels {
            if let Ok(status) = self.get_cooling_status(&channel.id).await {
                *channel = status;
            }
        }
        Some(DeviceCapability::Cooling(CoolingStatus { channels }))
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::FAN_CURVE
    }
    fn save_state(&self) -> serde_json::Value {
        self.cooling_state().save()
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        self.cooling_state().load_legacy(v);
    }
}

#[async_trait]
pub trait LightingCapability: Send + Sync {
    fn descriptor(&self) -> &LightingDescriptor;

    /// Apply a new RGB state to the device, e.g. set static color, start an effect, etc.
    async fn apply(&self, state: LightingState) -> Result<()>;

    /// Writes a raw color frame, bypassing state management. Only for driving the
    /// LEDs in "engine" mode — never call it to set persisted state.
    async fn write_frame(&self, channel_id: &str, bytes: &[u8]) -> Result<()>;

    /// Write all channels belonging to one device frame. Drivers that can commit a
    /// controller atomically should override this; the safe default preserves
    /// the existing per-zone behavior.
    async fn write_frame_batch(&self, channels: &[(String, Vec<u8>)]) -> Result<()> {
        for (channel_id, bytes) in channels {
            self.write_frame(channel_id, bytes).await?;
        }
        Ok(())
    }

    /// Backing slot — required so all state sub-operations have defaults.
    fn lighting_state(&self) -> &LightingStateSlot;

    fn current_state(&self) -> Option<LightingState> {
        self.lighting_state().current_state()
    }

    fn placed_channels(&self) -> Vec<crate::config::PlacedZone> {
        self.lighting_state().placed_channels()
    }
    fn set_canvas_zones(&self, channels: Vec<crate::config::PlacedZone>) {
        self.lighting_state().set_canvas_zones(channels);
    }

    fn channel_transforms(&self) -> HashMap<String, ZoneContentTransform> {
        self.lighting_state().channel_transforms()
    }
    fn transform_for(&self, channel_id: &str) -> ZoneContentTransform {
        self.lighting_state().transform_for(channel_id)
    }
    fn set_channel_transform(&self, channel_id: String, transform: ZoneContentTransform) {
        self.lighting_state()
            .set_zone_transform(channel_id, transform);
    }
    fn set_zone_transforms(&self, transforms: HashMap<String, ZoneContentTransform>) {
        self.lighting_state().set_zone_transforms(transforms);
    }

    /// Produces the wire snapshot for the IPC serializer.
    fn serialize_lighting(&self) -> LightingStatus {
        LightingStatus {
            descriptor: self.descriptor().clone(),
            state: self.current_state(),
            channel_transforms: self.channel_transforms(),
        }
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Lighting(self.serialize_lighting()))
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::RGB
    }
    fn save_state(&self) -> serde_json::Value {
        let slot = self.lighting_state();
        let canvas = slot.placed_channels();
        let transforms = self.channel_transforms();
        let mut obj = serde_json::Map::new();
        if let Some(s) = slot.current_state() {
            obj.insert("state".into(), serde_json::to_value(s).unwrap_or_default());
        }
        if !canvas.is_empty() {
            obj.insert(
                "placed_channels".into(),
                serde_json::to_value(canvas).unwrap_or_default(),
            );
        }
        if !transforms.is_empty() {
            obj.insert(
                "channel_transforms".into(),
                serde_json::to_value(transforms).unwrap_or_default(),
            );
        }
        if obj.is_empty() {
            serde_json::Value::Null
        } else {
            obj.into()
        }
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(z) = v.get("placed_channels") {
            if let Ok(channels) = serde_json::from_value(z.clone()) {
                self.lighting_state().set_canvas_zones(channels);
            }
        }
        if let Some(t) = v.get("channel_transforms") {
            if let Ok(transforms) = serde_json::from_value(t.clone()) {
                self.lighting_state().set_zone_transforms(transforms);
            }
        }
        if let Some(s) = v.get("state") {
            if let Ok(state) = serde_json::from_value(s.clone()) {
                if let Err(e) = self.apply(state).await {
                    log::warn!("[rgb restore_state] apply failed: {e:#}");
                }
            }
        }
    }
}

#[async_trait]
pub trait RangeCapability: Send + Sync {
    async fn set_range(&self, key: &str, value: i32) -> Result<()>;
    fn range_cache(&self) -> &RangeStateCache;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        None
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::RANGE
    }
    fn save_state(&self) -> serde_json::Value {
        self.range_cache().save()
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        for (key, value) in self.range_cache().load_pairs(v) {
            if let Err(e) = self.set_range(&key, value).await {
                log::warn!("[range restore_state] set_range({key}) failed: {e:#}");
            }
        }
    }
}

#[async_trait]
pub trait ChoiceCapability: Send + Sync {
    async fn set_choice(&self, key: &str, selected: usize) -> Result<()>;
    fn choice_cache(&self) -> &ChoiceStateCache;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        None
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::CHOICE
    }
    fn save_state(&self) -> serde_json::Value {
        self.choice_cache().save()
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        for (key, selected) in self.choice_cache().load_pairs(v) {
            if let Err(e) = self.set_choice(&key, selected).await {
                log::warn!("[choice restore_state] set_choice({key}) failed: {e:#}");
            }
        }
    }
}

#[async_trait]
pub trait BooleanCapability: Send + Sync {
    async fn get_booleans(&self) -> Result<Vec<Boolean>>;
    /// Only callable when the specific Boolean has `read_only = false`.
    async fn set_boolean(&self, key: &str, value: bool) -> Result<()>;
    fn bool_cache(&self) -> Option<&BoolStateCache> {
        None
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let booleans = self.get_booleans().await.unwrap_or_else(|e| {
            log::trace!("[BooleanCapability::to_wire] {e}");
            Default::default()
        });
        if booleans.is_empty() {
            None
        } else {
            Some(DeviceCapability::Boolean(booleans))
        }
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::BOOLEAN
    }
    fn save_state(&self) -> serde_json::Value {
        self.bool_cache()
            .map(|c| c.save())
            .unwrap_or(serde_json::Value::Null)
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(cache) = self.bool_cache() {
            for (key, value) in cache.load_pairs(v) {
                if let Err(e) = self.set_boolean(&key, value).await {
                    log::warn!("[boolean restore_state] set_boolean({key}) failed: {e:#}");
                }
            }
        }
    }
}

#[async_trait]
pub trait ActionCapability: Send + Sync {
    async fn trigger_action(&self, key: &str) -> Result<()>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        None
    }
}

#[async_trait]
pub trait BatteryCapability: Send + Sync {
    async fn get_batteries(&self) -> Result<Vec<Battery>>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let batteries = self.get_batteries().await.unwrap_or_else(|e| {
            log::debug!("[BatteryCapability::to_wire] {e}");
            Default::default()
        });
        if batteries.is_empty() {
            None
        } else {
            Some(DeviceCapability::Battery(batteries))
        }
    }
}

#[async_trait]
pub trait ConnectionCapability: Send + Sync {
    /// The current wired/wireless link state, or `None` for a wired-only device
    /// (which then exposes no connection indicator).
    async fn connection_status(&self) -> Option<ConnectionStatus>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.connection_status()
            .await
            .map(DeviceCapability::Connection)
    }
}

#[async_trait]
pub trait EqualizerCapability: Send + Sync {
    async fn get_equalizer(&self) -> Result<Equalizer>;
    async fn set_eq_preset(&self, preset_index: usize) -> Result<()>;
    async fn set_eq_bands(&self, values: &[f32]) -> Result<()>;

    fn current_state(&self) -> Option<Equalizer> {
        None
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.get_equalizer()
            .await
            .ok()
            .map(DeviceCapability::Equalizer)
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::EQUALIZER
    }
    fn save_state(&self) -> serde_json::Value {
        match self.current_state() {
            None => serde_json::Value::Null,
            Some(eq) => serde_json::json!({
                "preset": eq.selected_preset,
                "bands": eq.bands.iter().map(|b| b.value).collect::<Vec<_>>(),
            }),
        }
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(preset) = v["preset"].as_u64() {
            if let Err(e) = self.set_eq_preset(preset as usize).await {
                log::warn!("[eq restore_state] set_eq_preset failed: {e:#}");
            }
        }
        if let Some(arr) = v["bands"].as_array() {
            let values: Vec<f32> = arr
                .iter()
                .map(|b| b.as_f64().unwrap_or(0.0) as f32)
                .collect();
            if values.len() == 10 {
                if let Err(e) = self.set_eq_bands(&values).await {
                    log::warn!("[eq restore_state] set_eq_bands failed: {e:#}");
                }
            }
        }
    }
}

#[async_trait]
pub trait SensorCapability: Send + Sync {
    async fn get_sensors(&self) -> Result<Vec<Sensor>>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Sensors(
            self.get_sensors().await.unwrap_or_else(|e| {
                log::debug!("[SensorCapability::to_wire] {e}");
                Default::default()
            }),
        ))
    }
}

/// Synthesizes duty/RPM sensor readings for cooling channels.
pub async fn fan_sensors(device: &dyn Device) -> Vec<Sensor> {
    if let Some(cooling) = device.as_cooling() {
        let mut out = Vec::new();
        for channel in cooling.cooling_channels() {
            let Ok(status) = cooling.get_cooling_status(&channel.id).await else {
                continue;
            };
            let prefix = format!("cooling_{}_{}", device.id(), channel.id);
            if let Some(duty) = status.duty {
                out.push(Sensor {
                    id: format!("{prefix}_duty"),
                    name: format!("{} {} Duty", device.name(), status.name),
                    value: duty as f64,
                    unit: SensorUnit::Percent,
                    sensor_type: SensorType::FanDuty,
                    visibility: VisibilityState::Visible,
                });
            }
            if let Some(rpm) = status.rpm {
                out.push(Sensor {
                    id: format!("{prefix}_rpm"),
                    name: format!("{} {} RPM", device.name(), status.name),
                    value: rpm as f64,
                    unit: SensorUnit::Rpm,
                    sensor_type: SensorType::FanSpeed,
                    visibility: VisibilityState::Visible,
                });
            }
        }
        return out;
    }
    Vec::new()
}

/// Unified DPI control for pointing devices. Covers both onboard-profile DPI
/// (steps stored in device flash) and host-managed software DPI; callers read
/// [`DpiStatus::mode`] to know which layer is currently active.
#[async_trait]
pub trait DpiCapability: Send + Sync {
    /// Current DPI status (steps, active index, current value, mode).
    async fn dpi_status(&self) -> DpiStatus;
    /// Replace the DPI step list. In onboard mode this flashes the active
    /// profile; in host mode it replaces the software step list.
    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()>;
    /// Activate a specific step by index (host mode only).
    async fn set_dpi_index(&self, index: usize) -> Result<()>;
    /// Apply a DPI value directly without changing the step list or index
    /// (host mode only).
    async fn set_dpi_direct(&self, dpi: u16) -> Result<()>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Dpi(self.dpi_status().await))
    }

    fn state_key(&self) -> &'static str {
        ""
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn restore_state(&self, _: &serde_json::Value) {}
}

#[async_trait]
pub trait OnboardProfilesCapability: Send + Sync {
    /// Make slot `slot` (1-based) the device's active profile.
    async fn switch_profile(&self, slot: u8) -> Result<()>;
    /// Overwrite slot `slot` with the device's built-in ROM factory defaults.
    async fn restore_profile(&self, slot: u8) -> Result<()>;
    /// Enable (add) or disable (remove) slot `slot` in the profile directory.
    async fn set_profile_enabled(&self, slot: u8, enabled: bool) -> Result<()>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        None
    }

    fn state_key(&self) -> &'static str {
        ""
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn restore_state(&self, _: &serde_json::Value) {}
}

#[async_trait]
#[expect(
    dead_code,
    reason = "raw-streaming getter is part of the LCD driver protocol"
)]
pub trait LcdCapability: Send + Sync {
    fn lcd_descriptor(&self) -> LcdDescriptor;

    /// Backing slot — required so all state sub-operations have defaults.
    fn lcd_state(&self) -> &LcdStateSlot;

    /// `health` starts `Stable`; `registry::snapshot` overlays the real value from `VideoEngine` async.
    fn current_state(&self) -> LcdStatus {
        let slot = self.lcd_state();
        LcdStatus {
            descriptor: self.lcd_descriptor(),
            brightness: slot.brightness(),
            rotation: slot.rotation(),
            mode: slot.mode(),
            active_image: slot.active_image(),
            raw_streaming: slot.raw_streaming(),
            video_path: slot.video_path(),
            health: slot.health(),
        }
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Lcd(self.current_state()))
    }

    /// Whether uploading an image resets the device's RGB LEDs, requiring the
    /// saved RGB state to be re-applied afterward (NZXT Kraken panels do this).
    fn needs_rgb_restore_after_upload(&self) -> bool {
        false
    }

    /// Upload raw image bytes (PNG/JPEG/GIF). Caller saves to disk; device renders it.
    async fn set_image(&self, data: &[u8]) -> Result<()>;
    /// Push one rendered RGBA8 frame (`width*height*4` bytes at native resolution) to the panel.
    async fn stream_frame(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        let _ = (rgba, width, height);
        anyhow::bail!("device does not support LCD frame streaming")
    }
    async fn set_rotation(&self, degrees: u32) -> Result<()>;
    async fn set_brightness(&self, brightness: u8) -> Result<()>;
    async fn reset_to_default(&self) -> Result<()>;
    /// Update the tracked active image filename after it has been persisted to disk.
    async fn set_active_image_filename(&self, filename: Option<String>) {
        self.lcd_state().set_active_image(filename);
    }

    fn lcd_template_id(&self) -> Option<String> {
        self.lcd_state().lcd_template_id()
    }
    fn set_lcd_template_id(&self, id: Option<String>) {
        self.lcd_state().set_lcd_template_id(id);
    }
    fn lcd_template_params(&self) -> HashMap<String, EffectParamValue> {
        self.lcd_state().lcd_template_params()
    }
    fn set_lcd_template_params(&self, params: HashMap<String, EffectParamValue>) {
        self.lcd_state().set_lcd_template_params(params);
    }

    fn raw_streaming(&self) -> bool {
        self.lcd_state().raw_streaming()
    }
    fn set_raw_streaming(&self, v: bool) {
        self.lcd_state().set_raw_streaming(v);
    }
    fn video_path(&self) -> Option<String> {
        self.lcd_state().video_path()
    }
    fn set_video_path(&self, v: Option<String>) {
        self.lcd_state().set_video_path(v);
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::LCD
    }
    fn save_state(&self) -> serde_json::Value {
        let slot = self.lcd_state();
        serde_json::json!({
            "template_id":  slot.lcd_template_id(),
            "params":       slot.lcd_template_params(),
            "brightness":   slot.brightness(),
            "rotation":     slot.rotation(),
            "mode":         slot.persistent_mode(),
            "active_image": slot.active_image(),
            "raw_streaming": slot.raw_streaming(),
            "video_path": slot.video_path(),
        })
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        use halod_shared::types::LcdHealth;
        self.lcd_state().set_health(LcdHealth::Starting);
        let mut failure = None;
        if let Some(b) = v["brightness"].as_u64() {
            let b = u8::try_from(b).unwrap_or_else(|_| {
                log::warn!("[lcd restore_state] brightness {b} out of range, clamping to 0");
                0
            });
            if let Err(e) = self.set_brightness(b).await {
                log::warn!("[lcd restore_state] set_brightness failed: {e:#}");
                failure = Some(format!("restoring brightness failed: {e}"));
            }
        }
        if let Some(r) = v["rotation"].as_str() {
            let degrees = match r.parse::<u32>() {
                // Legacy numeric format (0/90/180/270).
                Ok(0) => Some(0u32),
                Ok(d @ (90 | 180 | 270)) => Some(d),
                _ => {
                    // New `ScreenRotation` enum format (serde snake_case).
                    match serde_json::from_value::<halod_shared::types::ScreenRotation>(
                        v["rotation"].clone(),
                    ) {
                        Ok(halod_shared::types::ScreenRotation::R90) => Some(90),
                        Ok(halod_shared::types::ScreenRotation::R180) => Some(180),
                        Ok(halod_shared::types::ScreenRotation::R270) => Some(270),
                        Ok(halod_shared::types::ScreenRotation::R0) => Some(0),
                        Err(_) => {
                            log::warn!(
                                "[lcd restore_state] rotation \"{r}\" unrecognised, skipping"
                            );
                            None
                        }
                    }
                }
            };
            if let Some(d) = degrees {
                if let Err(e) = self.set_rotation(d).await {
                    log::warn!("[lcd restore_state] set_rotation failed: {e:#}");
                    failure.get_or_insert_with(|| format!("restoring rotation failed: {e}"));
                }
            }
        }
        if let Some(raw) = v["raw_streaming"].as_bool() {
            self.set_raw_streaming(raw);
        }

        let has = |key: &str| v.get(key).is_some_and(|x| !x.is_null());

        // Content fields are mutually exclusive; `mode` (or, absent that, whichever content field is set) picks the one to restore so a stale sibling field can't clobber it.
        use halod_shared::types::LcdMode;
        let mode = v
            .get("mode")
            .and_then(|m| serde_json::from_value::<LcdMode>(m.clone()).ok())
            .or_else(|| {
                if has("template_id") {
                    Some(LcdMode::Engine)
                } else if has("active_image") {
                    Some(LcdMode::Image)
                } else if has("video_path") {
                    Some(LcdMode::Video)
                } else {
                    None
                }
            });
        match mode {
            Some(LcdMode::Image) | Some(LcdMode::Gif) => {
                if let Some(img) = v.get("active_image").and_then(|i| {
                    serde_json::from_value::<Option<String>>(i.clone())
                        .ok()
                        .flatten()
                }) {
                    self.lcd_state().set_active_image(Some(img));
                }
            }
            Some(LcdMode::Engine) => {
                if let Some(id) = v.get("template_id").and_then(|i| {
                    serde_json::from_value::<Option<String>>(i.clone())
                        .ok()
                        .flatten()
                }) {
                    self.set_lcd_template_id(Some(id));
                    if let Some(params) = v
                        .get("params")
                        .and_then(|p| serde_json::from_value(p.clone()).ok())
                    {
                        self.set_lcd_template_params(params);
                    }
                }
            }
            Some(LcdMode::Video) => {
                if let Some(path) = v.get("video_path").and_then(|p| {
                    serde_json::from_value::<Option<String>>(p.clone())
                        .ok()
                        .flatten()
                }) {
                    self.set_video_path(Some(path));
                }
            }
            _ => self.lcd_state().set_mode(LcdMode::Default),
        }
        self.lcd_state().set_health(match failure {
            Some(error) => LcdHealth::Failed(error),
            None => LcdHealth::Stable,
        });
    }
}

#[async_trait]
pub trait KeyRemapCapability: Send + Sync {
    async fn get_key_remap_status(&self) -> KeyRemapStatus;
    async fn set_button_mapping(&self, mapping: ButtonMapping) -> Result<()>;
    async fn reset_button_mapping(&self, cid: u16) -> Result<()>;
    async fn reset_all_button_mappings(&self) -> Result<()>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let status = self.get_key_remap_status().await;
        if status.buttons.is_empty() {
            None
        } else {
            Some(DeviceCapability::KeyRemap(status))
        }
    }

    fn state_key(&self) -> &'static str {
        ""
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn restore_state(&self, _: &serde_json::Value) {}
}

/// Resolve a user selection, firmware-detected language, and the profile's
/// static default language into the effective `(variant, language)` a keyboard
/// should render and address LEDs with.
///
/// - Language: explicit override wins; else the firmware-detected language when
///   known; else the profile default.
/// - Variant: explicit override always wins; on Auto it derives from whether
///   the effective language is an ISO language, gated by device ISO support.
pub fn effective_layout(
    sel: KeyboardLayoutSelection,
    detected: KeyboardLayout,
    default_lang: KeyboardLayout,
    iso_supported: bool,
) -> (KeyVariant, KeyboardLayout) {
    let language = sel.language.unwrap_or(match detected {
        KeyboardLayout::Unknown => default_lang,
        d => d,
    });
    let variant = sel.variant.unwrap_or({
        if iso_supported && is_iso_language(language) {
            KeyVariant::Iso
        } else {
            KeyVariant::Ansi
        }
    });
    (variant, language)
}

/// Keyboard layout (variant + language) selection, surfaced to both GUI tabs.
/// Wire-only: the selection itself is persisted in `Config::keyboard_layouts`,
/// not in the per-capability device state blob.
#[async_trait]
pub trait KeyboardLayoutCapability: Send + Sync {
    async fn keyboard_layout_status(&self) -> KeyboardLayoutStatus;

    /// Re-apply any hardware state that depends on the layout after the slot's
    /// selection changed (e.g. a physical-layout setup burst), without a full
    /// device re-registration. Default no-op for devices whose layout is purely
    /// a rendering hint. `keyboard_layout_status` already reflects the new slot.
    async fn apply_layout(&self) -> Result<()> {
        Ok(())
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::KeyboardLayout(
            self.keyboard_layout_status().await,
        ))
    }
}

#[cfg(test)]
mod effective_layout_tests {
    use super::*;
    use proptest::prelude::*;

    fn any_language() -> impl Strategy<Value = KeyboardLayout> {
        prop_oneof![
            Just(KeyboardLayout::US),
            Just(KeyboardLayout::CH),
            Just(KeyboardLayout::IT),
            Just(KeyboardLayout::DE),
            Just(KeyboardLayout::FR),
            Just(KeyboardLayout::UK),
            Just(KeyboardLayout::Unknown),
        ]
    }

    #[test]
    fn auto_with_unknown_detected_falls_back_to_default() {
        let (variant, language) = effective_layout(
            KeyboardLayoutSelection::default(),
            KeyboardLayout::Unknown,
            KeyboardLayout::CH,
            true,
        );
        assert_eq!(language, KeyboardLayout::CH);
        assert_eq!(variant, KeyVariant::Iso, "CH is an ISO language");
    }

    #[test]
    fn auto_prefers_detected_over_default() {
        let (variant, language) = effective_layout(
            KeyboardLayoutSelection::default(),
            KeyboardLayout::US,
            KeyboardLayout::CH,
            true,
        );
        assert_eq!(language, KeyboardLayout::US);
        assert_eq!(variant, KeyVariant::Ansi, "US is an ANSI language");
    }

    #[test]
    fn auto_variant_stays_ansi_when_iso_unsupported() {
        let (variant, _) = effective_layout(
            KeyboardLayoutSelection::default(),
            KeyboardLayout::CH,
            KeyboardLayout::US,
            false,
        );
        assert_eq!(variant, KeyVariant::Ansi);
    }

    proptest! {
        /// An explicit selection on an axis always wins over detection/default.
        #[test]
        fn override_always_wins(
            detected in any_language(),
            default_lang in any_language(),
            iso_supported in any::<bool>(),
        ) {
            let sel = KeyboardLayoutSelection {
                variant: Some(KeyVariant::Iso),
                language: Some(KeyboardLayout::DE),
            };
            let (variant, language) = effective_layout(sel, detected, default_lang, iso_supported);
            prop_assert_eq!(variant, KeyVariant::Iso);
            prop_assert_eq!(language, KeyboardLayout::DE);
        }
    }
}

#[cfg(test)]
mod fan_sensor_tests {
    use super::*;
    use crate::test_support::MockDevice;

    #[tokio::test]
    async fn fan_sensors_reports_duty_and_rpm_when_available() {
        let dev = MockDevice::new("fan0").with_fan_rpm(1234);
        let sensors = fan_sensors(&dev).await;
        assert_eq!(sensors.len(), 2);
        assert!(sensors
            .iter()
            .any(|s| s.id == "cooling_fan0_default_duty" && s.sensor_type == SensorType::FanDuty));
        assert!(sensors.iter().any(|s| s.id == "cooling_fan0_default_rpm"
            && s.value == 1234.0
            && s.unit == SensorUnit::Rpm));
    }

    #[tokio::test]
    async fn fan_sensors_omits_rpm_when_fan_reports_none() {
        let dev = MockDevice::new("fan0").with_fan();
        let sensors = fan_sensors(&dev).await;
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].id, "cooling_fan0_default_duty");
    }

    #[tokio::test]
    async fn fan_sensors_empty_for_non_fan_device() {
        let dev = MockDevice::new("rgb0").with_rgb();
        assert!(fan_sensors(&dev).await.is_empty());
    }
}
