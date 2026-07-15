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
    Battery, Boolean, ButtonMapping, ChainableChannelInfo, ConnectionStatus, DeviceCapability,
    DpiStatus, EffectParamValue, Equalizer, FanStatus, KeyRemapStatus, KeyboardLayout,
    LcdDescriptor, LcdStatus, RgbColor, RgbDescriptor, RgbState, RgbStatus, Sensor, SensorType,
    SensorUnit, VisibilityState, ZoneTopology,
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

/// Parent-side capability for managing chainable channels; the CRUD surface
/// delegates to a shared [`chain::ChainHost`] exposed via [`ChainCapability::chain_host`].
#[async_trait]
pub trait ChainCapability: Send + Sync {
    /// The driver's chain host. Returning `None` is treated as "no chainable
    /// channels yet" by every default impl below.
    fn chain_host(&self) -> Option<&Arc<chain::ChainHost>>;

    fn chainable_channels(&self) -> Vec<ChainableChannelInfo> {
        self.chain_host()
            .map(|h| h.chainable_channels())
            .unwrap_or_default()
    }

    async fn add_chain_link(
        &self,
        channel_id: &str,
        spec: ChainLinkSpec,
    ) -> Result<(String, Arc<dyn Device>)> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.add_link(channel_id, spec).await
    }

    async fn remove_chain_link(&self, channel_id: &str, child_id: &str) -> Result<String> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.remove_link(channel_id, child_id).await
    }

    async fn rename_chain_link(
        &self,
        channel_id: &str,
        child_id: &str,
        new_name: &str,
    ) -> Result<()> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.rename_link(channel_id, child_id, new_name)
    }

    async fn reorder_chain_link(
        &self,
        channel_id: &str,
        child_id: &str,
        new_index: usize,
    ) -> Result<()> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.reorder_link(channel_id, child_id, new_index)
    }

    async fn detect_channel(&self, channel_id: &str) -> Result<()> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.detect_channel(channel_id).await
    }

    /// Replays a persisted layout at startup. Skips broadcast + persist — the
    /// caller already owns both responsibilities at boot time.
    async fn restore_chain_link(
        &self,
        channel_id: &str,
        record: &crate::registry::config::ChainLinkRecord,
    ) -> Result<Arc<dyn Device>> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.restore_link(channel_id, record).await
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let host = self.chain_host()?;
        let children = host.children().await;
        if children.is_empty() {
            return None;
        }
        let mut wires = Vec::with_capacity(children.len());
        for child in &children {
            wires.push(child.serialize().await);
        }
        Some(DeviceCapability::Children(wires))
    }

    /// Merges `chainable_channels` into an existing `Rgb` wire capability, or
    /// prepends a minimal `Rgb` carrier if none exists yet.
    fn enrich_wire_capabilities(&self, caps: &mut Vec<DeviceCapability>) {
        let channels = self.chainable_channels();
        if channels.is_empty() {
            return;
        }
        let has_rgb = caps.iter().any(|c| matches!(c, DeviceCapability::Rgb(_)));
        if has_rgb {
            for cap in caps.iter_mut() {
                if let DeviceCapability::Rgb(rgb) = cap {
                    rgb.chainable_channels = channels;
                    break;
                }
            }
        } else {
            caps.insert(
                0,
                DeviceCapability::Rgb(RgbStatus {
                    descriptor: RgbDescriptor {
                        zones: vec![],
                        native_effects: vec![],
                    },
                    state: None,
                    zone_transforms: std::collections::HashMap::new(),
                    chainable_channels: channels,
                }),
            );
        }
    }
}

/// Opt-in hook for devices needing application-level setup after registration
/// in `AppState` (e.g. notification watchers, dynamic children).
/// Accessed via [`Device::as_post_register_hook`] to avoid coupling every device to `AppState`.
#[async_trait]
pub trait PostRegisterHook: Send + Sync {
    async fn on_registered(&self, app: std::sync::Arc<crate::state::AppState>);
}

/// Fan status/speed surface for chain accessory drivers with fan hardware
/// alongside their ARGB channels; chain writes go through [`chain::ChainHub`] instead.
#[async_trait]
pub trait FanHub: Send + Sync + 'static {
    async fn get_fan_rpm(&self, channel: u8) -> Result<u32>;
    async fn get_fan_duty(&self, channel: u8) -> Result<u8>;
    async fn get_fan_controllable(&self, channel: u8) -> Result<bool>;
    async fn set_fan_duty(&self, channel: u8, duty: u8) -> Result<()>;
}

#[async_trait]
pub trait RgbCapability: Send + Sync {
    /// Describe zones and native effects supported by the device.
    fn descriptor(&self) -> &RgbDescriptor;

    /// Apply a new RGB state to the device, e.g. set static color, start an effect, etc.
    async fn apply(&self, state: RgbState) -> Result<()>;

    /// Writes a raw color frame, bypassing state management. Only for driving the
    /// LEDs in "engine" mode — never call it to set persisted state.
    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()>;

    /// Write all zones belonging to one device frame. Drivers that can commit a
    /// controller atomically should override this; the safe default preserves
    /// the existing per-zone behavior.
    async fn write_frame_batch(&self, zones: &[(String, Vec<RgbColor>)]) -> Result<()> {
        for (zone_id, colors) in zones {
            self.write_frame(zone_id, colors).await?;
        }
        Ok(())
    }

    /// Backing slot — required so all state sub-operations have defaults.
    fn rgb_state(&self) -> &RgbStateSlot;

    fn current_state(&self) -> Option<RgbState> {
        self.rgb_state().current_state()
    }

    fn canvas_zones(&self) -> Vec<crate::config::PlacedZone> {
        self.rgb_state().canvas_zones()
    }
    fn set_canvas_zones(&self, zones: Vec<crate::config::PlacedZone>) {
        self.rgb_state().set_canvas_zones(zones);
    }

    fn zone_transforms(&self) -> HashMap<String, ZoneContentTransform> {
        self.rgb_state().zone_transforms()
    }
    fn transform_for(&self, zone_id: &str) -> ZoneContentTransform {
        self.rgb_state().transform_for(zone_id)
    }
    fn set_zone_transform(&self, zone_id: String, transform: ZoneContentTransform) {
        self.rgb_state().set_zone_transform(zone_id, transform);
    }
    fn set_zone_transforms(&self, transforms: HashMap<String, ZoneContentTransform>) {
        self.rgb_state().set_zone_transforms(transforms);
    }

    /// Produces the wire snapshot for the IPC serializer.
    fn serialize_rgb(&self) -> RgbStatus {
        RgbStatus {
            descriptor: self.descriptor().clone(),
            state: self.current_state(),
            zone_transforms: self.zone_transforms(),
            chainable_channels: Vec::new(),
        }
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Rgb(self.serialize_rgb()))
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::RGB
    }
    fn save_state(&self) -> serde_json::Value {
        let slot = self.rgb_state();
        let canvas = slot.canvas_zones();
        let transforms = self.zone_transforms();
        let mut obj = serde_json::Map::new();
        if let Some(s) = slot.current_state() {
            obj.insert("state".into(), serde_json::to_value(s).unwrap_or_default());
        }
        if !canvas.is_empty() {
            obj.insert(
                "canvas_zones".into(),
                serde_json::to_value(canvas).unwrap_or_default(),
            );
        }
        if !transforms.is_empty() {
            obj.insert(
                "zone_transforms".into(),
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
        if let Some(z) = v.get("canvas_zones") {
            if let Ok(zones) = serde_json::from_value(z.clone()) {
                self.rgb_state().set_canvas_zones(zones);
            }
        }
        if let Some(t) = v.get("zone_transforms") {
            if let Ok(transforms) = serde_json::from_value(t.clone()) {
                self.rgb_state().set_zone_transforms(transforms);
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
pub trait FanCapability: Send + Sync {
    async fn get_duty(&self) -> Result<u8>;

    async fn set_duty(&self, duty: u8) -> Result<()>;

    /// Returns current fan speed in RPM. Returns `None` for devices (e.g. pumps) that
    /// report duty but not RPM.
    async fn get_rpm(&self) -> Option<u32>;

    /// Backing slot — required so all state sub-operations have defaults.
    fn fan_state(&self) -> &FanStateSlot;

    fn fan_curve(&self) -> Option<crate::cooling::config::FanCurveRecord> {
        self.fan_state().fan_curve()
    }
    fn set_fan_curve(&self, curve: crate::cooling::config::FanCurveRecord) {
        self.fan_state().set_fan_curve(curve);
    }
    fn clear_fan_curve(&self) {
        self.fan_state().clear_fan_curve();
    }

    fn fan_channel_id(&self) -> u8 {
        0
    }

    async fn fan_controllable(&self) -> bool {
        true
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Fan(FanStatus {
            channel: self.fan_channel_id(),
            rpm: self.get_rpm().await.unwrap_or(0),
            duty: self.get_duty().await.unwrap_or_else(|e| {
                log::trace!("[FanCapability::to_wire] get_duty: {e}");
                0
            }),
            controllable: self.fan_controllable().await,
        }))
    }

    fn state_key(&self) -> &'static str {
        halod_shared::capability::FAN_CURVE
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::to_value(self.fan_curve()).unwrap_or(serde_json::Value::Null)
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        match serde_json::from_value::<Option<crate::cooling::config::FanCurveRecord>>(v.clone()) {
            Ok(Some(c)) => match c.validate() {
                Ok(()) => self.set_fan_curve(c),
                Err(e) => log::warn!("[fan restore_state] dropping invalid curve: {e:#}"),
            },
            Ok(None) => self.clear_fan_curve(),
            Err(e) => log::warn!("[fan restore_state] invalid curve payload: {e}"),
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

/// Deterministic id for a fan's synthesized duty-percent sensor reading.
pub fn fan_duty_sensor_id(device_id: &str) -> String {
    format!("fan_{device_id}_duty")
}

/// Deterministic id for a fan's synthesized RPM sensor reading, when it reports one.
pub fn fan_rpm_sensor_id(device_id: &str) -> String {
    format!("fan_{device_id}_rpm")
}

/// Synthesizes duty/RPM sensor readings for any device with a `FanCapability`,
/// so fan-driven effects (and the sensor dashboard) can select them like any
/// other sensor. Returns an empty `Vec` for devices without `FanCapability`.
pub async fn fan_sensors(device: &dyn Device) -> Vec<Sensor> {
    let Some(fan) = device.as_fan() else {
        return Vec::new();
    };
    let mut out = vec![Sensor {
        id: fan_duty_sensor_id(device.id()),
        name: format!("{} Duty", device.name()),
        value: fan.get_duty().await.unwrap_or_else(|e| {
            log::trace!("[fan_sensors] get_duty: {e}");
            0
        }) as f64,
        unit: SensorUnit::Percent,
        sensor_type: SensorType::FanDuty,
        visibility: VisibilityState::Visible,
    }];
    if let Some(rpm) = fan.get_rpm().await {
        out.push(Sensor {
            id: fan_rpm_sensor_id(device.id()),
            name: format!("{} RPM", device.name()),
            value: rpm as f64,
            unit: SensorUnit::Rpm,
            sensor_type: SensorType::FanSpeed,
            visibility: VisibilityState::Visible,
        });
    }
    out
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
            .any(|s| s.id == "fan_fan0_duty" && s.sensor_type == SensorType::FanDuty));
        assert!(sensors
            .iter()
            .any(|s| s.id == "fan_fan0_rpm" && s.value == 1234.0 && s.unit == SensorUnit::Rpm));
    }

    #[tokio::test]
    async fn fan_sensors_omits_rpm_when_fan_reports_none() {
        let dev = MockDevice::new("fan0").with_fan();
        let sensors = fan_sensors(&dev).await;
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].id, "fan_fan0_duty");
    }

    #[tokio::test]
    async fn fan_sensors_empty_for_non_fan_device() {
        let dev = MockDevice::new("rgb0").with_rgb();
        assert!(fan_sensors(&dev).await.is_empty());
    }
}

#[cfg(test)]
mod fan_sensor_id_prop_tests {
    use super::{fan_duty_sensor_id, fan_rpm_sensor_id};
    use proptest::prelude::*;

    proptest! {
        /// Duty and RPM ids never collide with each other or across devices.
        #[test]
        fn fan_sensor_ids_are_distinct(
            id_a in "[a-zA-Z0-9_]{1,16}",
            id_b in "[a-zA-Z0-9_]{1,16}",
        ) {
            prop_assert_ne!(fan_duty_sensor_id(&id_a), fan_rpm_sensor_id(&id_a));
            if id_a != id_b {
                prop_assert_ne!(fan_duty_sensor_id(&id_a), fan_duty_sensor_id(&id_b));
                prop_assert_ne!(fan_rpm_sensor_id(&id_a), fan_rpm_sensor_id(&id_b));
            }
        }
    }
}
