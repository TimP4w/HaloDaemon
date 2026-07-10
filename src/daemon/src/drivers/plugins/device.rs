// SPDX-License-Identifier: GPL-3.0-or-later
//! The generic device a plugin instantiates. It sits behind the same `Device`
//! seam as every native driver and forwards capability calls into the per-device
//! Lua worker. Which capabilities it advertises is decided entirely by the
//! manifest — Halo owns the capability taxonomy; the script only fills it in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock, Weak};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{
    Choice, DeviceCapability, DeviceType, DpiMode, DpiStatus, LcdDescriptor, NativeEffect,
    RgbColor, RgbDescriptor, RgbState, RgbZone, ScreenRotation, ScreenShape, Sensor,
    WriteRateStatus,
};

use crate::drivers::chain::{ChainAdapter, ChainHost, ChainHub, ChannelDescriptor};
use crate::drivers::{
    CapabilityRef, ChainCapability, ChoiceCapability, ChoiceStateCache, Controller, Device,
    DpiCapability, FanCapability, FanHub, FanStateSlot, LcdCapability, LcdStateSlot, RgbCapability,
    RgbStateSlot, SensorCapability,
};

use super::chain_leaf::ChainLeaf;
use super::manifest::{
    topology_from, AccessoryManifest, ChoiceDef, DpiManifest, MatchSpec, PluginManifest,
};
use super::transport::PluginIo;
use super::worker::{DevMatch, InitLcd, InitZone, PluginHandle};

/// Host-side DPI step-cycle state (the plugin only writes the chosen value).
struct DpiState {
    steps: Vec<u16>,
    index: usize,
    current: u16,
}
use crate::drivers::vendors::generic::devices::common::{linear_rgb_zone, ring_led_positions};
use halod_shared::types::ZoneTopology;

/// A device whose behaviour is defined by a plugin script rather than native
/// Rust.
pub struct LuaDevice {
    id: String,
    name: String,
    vendor: String,
    model: String,
    plugin_id: String,
    device_type: DeviceType,
    /// A short label for the Info UI's transport line ("hid", "smbus", …).
    transport_kind: &'static str,

    /// Firmware/model discovered at `initialize()` time, overriding the static
    /// manifest `model` when present (e.g. an SMBus controller's version string).
    dynamic_model: OnceLock<String>,

    /// Present when the plugin declares a capability (RGB/fan/sensor). Absent
    /// for a device-only plugin.
    worker: Option<PluginHandle>,
    /// Clone of the (metered) transport, kept so the device can report
    /// write-rate/throughput to the Info UI. `None` for device-only plugins.
    transport: Option<PluginIo>,

    has_rgb: bool,
    has_fan: bool,
    has_sensor: bool,
    has_lcd: bool,
    has_dpi: bool,
    has_choice: bool,

    /// Host-owned DPI step-cycle state + bounds/mode (present iff `has_dpi`).
    dpi_state: Mutex<DpiState>,
    dpi_min: u16,
    dpi_max: u16,
    dpi_mode: DpiMode,

    /// Declared choice controls + the current selection cache.
    choices: Vec<ChoiceDef>,
    choice_cache: ChoiceStateCache,

    /// LCD panel descriptor, reported by `initialize` (resolution can vary by
    /// device variant). Absent until initialized.
    lcd_descriptor: OnceLock<LcdDescriptor>,
    lcd_slot: LcdStateSlot,
    /// Re-apply RGB after an LCD image upload (some panels reset their LEDs).
    lcd_needs_rgb_restore: bool,

    rgb_descriptor: RgbDescriptor,
    /// RGB zones discovered at `initialize()` (dynamic LED counts). Overrides
    /// `rgb_descriptor` when set.
    dynamic_rgb_descriptor: OnceLock<RgbDescriptor>,
    rgb_slot: RgbStateSlot,
    fan_slot: FanStateSlot,
    fan_channel: u8,

    /// Host-run status poll: aborted on drop. `poll_paused` lets a future LCD
    /// path silence polling during a bulk transfer without tearing it down.
    poll_task: Option<tokio::task::JoinHandle<()>>,
    poll_paused: Arc<AtomicBool>,

    // ── chain / children (present only when the manifest declares `chain`) ──
    has_chain: bool,
    /// Set after construction (needs the `Arc<Self>`); `None` for non-chain devices.
    chain_host: OnceLock<Arc<ChainHost>>,
    /// Weak back-reference so `discover_children` can hand children a `FanHub`.
    self_ref: Weak<LuaDevice>,
    chain_channels: Vec<ChannelDescriptor>,
    accessories: Vec<AccessoryManifest>,
}

impl Drop for LuaDevice {
    fn drop(&mut self) {
        if let Some(task) = self.poll_task.take() {
            task.abort();
        }
    }
}

impl LuaDevice {
    /// A plugin that declares no capability — identity + lifecycle only.
    pub fn device_only(id: String, manifest: &PluginManifest, spec: &MatchSpec) -> Self {
        Self::build(id, manifest, spec, None, None)
    }

    /// A plugin with capabilities, backed by a worker over `transport`.
    pub fn with_transport(
        id: String,
        manifest: &PluginManifest,
        spec: &MatchSpec,
        dev_match: DevMatch,
        transport: PluginIo,
        handle: tokio::runtime::Handle,
    ) -> Self {
        // Keep a handle to the (metered) transport so the device can report
        // write-rate/throughput; the worker owns the one it does I/O through.
        let rate_transport = transport.clone();
        let worker = PluginHandle::spawn(
            manifest.script_source.clone(),
            transport,
            dev_match,
            handle.clone(),
        );
        let mut dev = Self::build(
            id,
            manifest,
            spec,
            Some(worker.clone()),
            Some(rate_transport),
        );

        // The status poll loop stays host-side (not in the single-threaded VM):
        // a ticker enqueues one poll per interval, run serially by the worker.
        if let Some(poll) = &manifest.poll {
            let interval = Duration::from_millis(poll.interval_ms.max(1));
            let paused = dev.poll_paused.clone();
            dev.poll_task = Some(handle.spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                loop {
                    ticker.tick().await;
                    if paused.load(Ordering::Relaxed) {
                        continue;
                    }
                    if worker.poll().await.is_err() {
                        break; // worker gone
                    }
                }
            }));
        }
        dev
    }

    fn build(
        id: String,
        manifest: &PluginManifest,
        spec: &MatchSpec,
        worker: Option<PluginHandle>,
        transport: Option<PluginIo>,
    ) -> Self {
        Self {
            id,
            name: manifest.display_name_for(spec),
            vendor: manifest.identity.vendor.clone(),
            model: manifest.identity.model.clone(),
            plugin_id: manifest.plugin_id.clone(),
            device_type: spec.device_type.unwrap_or_default(),
            transport_kind: super::transport::descriptor_for(&spec.transport)
                .map(|d| d.kind)
                .unwrap_or("unknown"),
            dynamic_model: OnceLock::new(),
            worker,
            transport,
            has_rgb: manifest.rgb.is_some(),
            has_fan: manifest.fan.is_some(),
            has_sensor: manifest.sensor.is_some(),
            has_lcd: manifest.lcd.is_some(),
            lcd_descriptor: OnceLock::new(),
            lcd_slot: LcdStateSlot::default(),
            lcd_needs_rgb_restore: manifest
                .lcd
                .as_ref()
                .map(|l| l.needs_rgb_restore)
                .unwrap_or(false),
            has_dpi: manifest.dpi.is_some(),
            has_choice: manifest.choice.is_some(),
            dpi_state: Mutex::new(build_dpi_state(manifest.dpi.as_ref())),
            dpi_min: manifest.dpi.as_ref().map(|d| d.min).unwrap_or(0),
            dpi_max: manifest.dpi.as_ref().map(|d| d.max).unwrap_or(0),
            dpi_mode: match manifest.dpi.as_ref().map(|d| d.onboard) {
                Some(true) => DpiMode::Onboard,
                _ => DpiMode::Host,
            },
            choices: manifest
                .choice
                .as_ref()
                .map(|c| c.choices.clone())
                .unwrap_or_default(),
            choice_cache: ChoiceStateCache::default(),
            rgb_descriptor: manifest.rgb_descriptor().unwrap_or(RgbDescriptor {
                zones: Vec::new(),
                native_effects: Vec::new(),
            }),
            dynamic_rgb_descriptor: OnceLock::new(),
            rgb_slot: RgbStateSlot::default(),
            fan_slot: FanStateSlot::default(),
            fan_channel: manifest.fan.as_ref().map(|f| f.channel).unwrap_or(0),
            poll_task: None,
            poll_paused: Arc::new(AtomicBool::new(false)),
            has_chain: manifest.chain.is_some(),
            chain_host: OnceLock::new(),
            self_ref: Weak::new(),
            chain_channels: manifest
                .chain
                .as_ref()
                .map(|c| {
                    c.channels
                        .iter()
                        .map(|ch| ChannelDescriptor {
                            channel_id: ch.id.clone(),
                            display_name: ch.name.clone(),
                            max_leds: ch.max_leds,
                        })
                        .collect()
                })
                .unwrap_or_default(),
            accessories: manifest
                .chain
                .as_ref()
                .map(|c| c.accessories.clone())
                .unwrap_or_default(),
        }
    }

    /// Set the weak self-reference (children need the parent as a `FanHub`).
    /// Called from `build_device` inside `Arc::new_cyclic`.
    pub(super) fn set_self_ref(&mut self, weak: Weak<LuaDevice>) {
        self.self_ref = weak;
    }

    /// Install the chain host (built from `Arc<Self>` as the adapter).
    pub(super) fn install_chain_host(&self, host: Arc<ChainHost>) {
        let _ = self.chain_host.set(host);
    }

    /// Pause/resume the background status poll (used when an exclusive transfer
    /// must own the transport, e.g. an LCD bulk upload).
    pub fn set_polling_paused(&self, paused: bool) {
        self.poll_paused.store(paused, Ordering::Relaxed);
    }

    /// Trigger one status poll synchronously (used by tests; production relies on
    /// the ticker).
    #[cfg(test)]
    pub async fn poll_once(&self) -> Result<()> {
        self.worker()?.poll().await
    }

    pub fn plugin_id(&self) -> &str {
        &self.plugin_id
    }

    fn worker(&self) -> Result<&PluginHandle> {
        self.worker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plugin '{}' has no worker", self.plugin_id))
    }
}

/// Build an `RgbDescriptor` from `initialize`-reported zones, computing LED
/// positions from the declared topology + count (as static accessory zones do).
/// Native effects carry over from the static manifest descriptor.
fn build_dynamic_descriptor(
    zones: Vec<InitZone>,
    native_effects: &[NativeEffect],
) -> RgbDescriptor {
    let zones = zones
        .into_iter()
        .map(|z| {
            let topology = topology_from(&z.topology, z.rings);
            // `ring_led_positions` only lays out ring topologies; linear zones use
            // the evenly-spaced strip layout (as the native drivers did).
            if matches!(topology, ZoneTopology::Linear) {
                linear_rgb_zone(&z.id, &z.name, z.led_count as usize)
            } else {
                RgbZone {
                    leds: ring_led_positions(&topology, z.led_count),
                    id: z.id,
                    name: z.name,
                    topology,
                }
            }
        })
        .collect();
    RgbDescriptor {
        zones,
        native_effects: native_effects.to_vec(),
    }
}

#[async_trait]
impl Device for LuaDevice {
    fn id(&self) -> &str {
        &self.id
    }

    fn name(&self) -> &str {
        &self.name
    }

    fn vendor(&self) -> &str {
        &self.vendor
    }

    fn model(&self) -> &str {
        self.dynamic_model.get().unwrap_or(&self.model)
    }

    fn wire_device_type(&self) -> DeviceType {
        self.device_type
    }

    async fn initialize(&self) -> Result<bool> {
        let Some(w) = &self.worker else {
            return Ok(true);
        };
        let outcome = w.initialize().await?;
        if let Some(model) = outcome.model {
            let _ = self.dynamic_model.set(model);
        }
        if let Some(zones) = outcome.zones {
            let _ = self.dynamic_rgb_descriptor.set(build_dynamic_descriptor(
                zones,
                &self.rgb_descriptor.native_effects,
            ));
        }
        if let Some(lcd) = outcome.lcd {
            self.lcd_slot.set_brightness(lcd.brightness);
            self.lcd_slot
                .set_rotation(degrees_to_rotation(lcd.rotation));
            self.lcd_slot.set_raw_streaming(lcd.raw_streaming);
            self.lcd_slot.set_latches_last_frame(lcd.latches);
            let _ = self.lcd_descriptor.set(build_lcd_descriptor(&lcd));
        }
        Ok(outcome.ok)
    }

    async fn close(&self) {
        if let Some(w) = &self.worker {
            w.close().await;
        }
    }

    fn write_rate_status(&self) -> Option<WriteRateStatus> {
        self.transport.as_ref().map(|t| t.rate_status())
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some(self.transport_kind)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let mut caps = Vec::new();
        if self.has_rgb {
            caps.push(CapabilityRef::Rgb(self));
        }
        if self.has_fan {
            caps.push(CapabilityRef::Fan(self));
        }
        if self.has_sensor {
            caps.push(CapabilityRef::Sensor(self));
        }
        if self.has_lcd {
            caps.push(CapabilityRef::Lcd(self));
        }
        if self.has_dpi {
            caps.push(CapabilityRef::Dpi(self));
        }
        if self.has_choice {
            caps.push(CapabilityRef::Choice(self));
        }
        if self.has_chain {
            caps.push(CapabilityRef::Controller(self));
            caps.push(CapabilityRef::Chain(self));
        }
        caps
    }
}

#[async_trait]
impl RgbCapability for LuaDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        self.dynamic_rgb_descriptor
            .get()
            .unwrap_or(&self.rgb_descriptor)
    }

    async fn apply(&self, state: RgbState) -> Result<()> {
        self.rgb_slot.set_state(Some(state.clone()));
        self.worker()?.rgb_apply(state).await
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        self.worker()?.rgb_write_frame(zone_id, colors).await
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb_slot
    }
}

#[async_trait]
impl FanCapability for LuaDevice {
    async fn get_duty(&self) -> Result<u8> {
        self.worker()?.fan_get_duty().await
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        self.worker()?.fan_set_duty(duty).await
    }

    async fn get_rpm(&self) -> Option<u32> {
        match &self.worker {
            Some(w) => w.fan_get_rpm().await,
            None => None,
        }
    }

    fn fan_state(&self) -> &FanStateSlot {
        &self.fan_slot
    }

    fn fan_channel_id(&self) -> u8 {
        self.fan_channel
    }
}

#[async_trait]
impl SensorCapability for LuaDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        self.worker()?.get_sensors().await
    }
}

// ── Chain / children: the parent surface ────────────────────────────────────
//
// Reuses the native `ChainHost` machinery. The script supplies only the probe
// (`detect_accessories`), the per-accessory descriptor table, and the routing
// callbacks (`write_ext_frame` / fan-hub). The generic `ChainLeaf` child and the
// `ChainHost` frame composition are unchanged.

#[async_trait]
impl Controller for LuaDevice {
    async fn discover_children(&self) -> Vec<Arc<dyn Device>> {
        let (Some(worker), Some(host)) = (&self.worker, self.chain_host.get()) else {
            return Vec::new();
        };
        // Accessory detection does exclusive reads; pause the status poll so it
        // doesn't race the detect reply (mirrors the native Kraken).
        self.set_polling_paused(true);
        let detected = worker.detect_accessories().await;
        self.set_polling_paused(false);
        let detected = match detected {
            Ok(d) => d,
            Err(e) => {
                log::warn!("plugin '{}' detect_accessories: {e:#}", self.plugin_id);
                return Vec::new();
            }
        };
        let Some(parent) = self.self_ref.upgrade() else {
            return Vec::new();
        };
        let fan_hub: Arc<dyn FanHub> = parent;
        let chain_hub: Arc<dyn ChainHub> = host.clone();

        let mut out = Vec::new();
        for d in detected {
            let Some(accessory) = self.accessories.iter().find(|a| a.id == d.accessory) else {
                log::debug!(
                    "plugin '{}': unknown accessory 0x{:02x}",
                    self.plugin_id,
                    d.accessory
                );
                continue;
            };
            let channel_str = d.channel.to_string();
            let leaf: Arc<dyn Device> = Arc::new(ChainLeaf::new(
                format!("{}_acc_{}_{}", self.id, channel_str, d.accessory),
                self.vendor.clone(),
                channel_str.clone(),
                d.channel,
                accessory,
                chain_hub.clone(),
                fan_hub.clone(),
            ));
            if let Err(e) = leaf.initialize().await {
                log::warn!("plugin '{}' child init failed: {e:#}", self.plugin_id);
                continue;
            }
            host.register_auto_link(&channel_str, leaf.clone()).await;
            out.push(leaf);
        }
        out
    }
}

impl ChainCapability for LuaDevice {
    fn chain_host(&self) -> Option<&Arc<ChainHost>> {
        self.chain_host.get()
    }
}

fn degrees_to_rotation(degrees: u32) -> ScreenRotation {
    match degrees % 360 {
        90 => ScreenRotation::R90,
        180 => ScreenRotation::R180,
        270 => ScreenRotation::R270,
        _ => ScreenRotation::R0,
    }
}

fn rotation_to_degrees(rotation: ScreenRotation) -> u32 {
    match rotation {
        ScreenRotation::R0 => 0,
        ScreenRotation::R90 => 90,
        ScreenRotation::R180 => 180,
        ScreenRotation::R270 => 270,
    }
}

/// Build an `LcdDescriptor` from the panel info `initialize` reported.
fn build_lcd_descriptor(lcd: &InitLcd) -> LcdDescriptor {
    let shape = if lcd.shape.eq_ignore_ascii_case("square") {
        ScreenShape::Square
    } else {
        ScreenShape::Circle
    };
    let supported_rotations = if lcd.rotations.is_empty() {
        vec![ScreenRotation::R0]
    } else {
        lcd.rotations
            .iter()
            .map(|d| degrees_to_rotation(*d))
            .collect()
    };
    LcdDescriptor {
        shape,
        width: lcd.width,
        height: lcd.height,
        supported_rotations,
        supported_image_types: lcd.image_types.clone(),
        latches_last_frame: lcd.latches,
    }
}

#[async_trait]
impl LcdCapability for LuaDevice {
    fn lcd_descriptor(&self) -> LcdDescriptor {
        self.lcd_descriptor.get().cloned().unwrap_or(LcdDescriptor {
            shape: ScreenShape::Circle,
            width: 0,
            height: 0,
            supported_rotations: vec![ScreenRotation::R0],
            supported_image_types: Vec::new(),
            latches_last_frame: false,
        })
    }

    fn lcd_state(&self) -> &LcdStateSlot {
        &self.lcd_slot
    }

    fn needs_rgb_restore_after_upload(&self) -> bool {
        self.lcd_needs_rgb_restore
    }

    /// One rendered engine frame. Rotation/brightness/mode live in the slot and
    /// are passed to the plugin so it can pre-rotate and pick the stream path.
    async fn stream_frame(&self, rgba: &[u8], width: u32, height: u32) -> Result<()> {
        let rotation = rotation_to_degrees(self.lcd_slot.rotation());
        let raw = self.lcd_slot.raw_streaming();
        let brightness = self.lcd_slot.brightness();
        // The bulk transfer owns the transport; silence the status poll meanwhile.
        self.set_polling_paused(true);
        let result = self
            .worker()?
            .lcd_stream_frame(rgba.to_vec(), width, height, rotation, raw, brightness)
            .await;
        self.set_polling_paused(false);
        result
    }

    async fn set_image(&self, data: &[u8]) -> Result<()> {
        let rotation = rotation_to_degrees(self.lcd_slot.rotation());
        self.set_polling_paused(true);
        let result = self.worker()?.lcd_set_image(data.to_vec(), rotation).await;
        self.set_polling_paused(false);
        result
    }

    async fn set_brightness(&self, brightness: u8) -> Result<()> {
        let rotation = rotation_to_degrees(self.lcd_slot.rotation());
        self.worker()?
            .lcd_set_brightness(brightness, rotation)
            .await?;
        self.lcd_slot.set_brightness(brightness);
        Ok(())
    }

    async fn set_rotation(&self, degrees: u32) -> Result<()> {
        let brightness = self.lcd_slot.brightness();
        self.worker()?.lcd_set_rotation(brightness, degrees).await?;
        self.lcd_slot.set_rotation(degrees_to_rotation(degrees));
        Ok(())
    }

    async fn reset_to_default(&self) -> Result<()> {
        self.worker()?.lcd_reset().await
    }
}

/// Initial DPI state from the manifest: mid-step selected, like the native driver.
fn build_dpi_state(dpi: Option<&DpiManifest>) -> DpiState {
    let steps: Vec<u16> = dpi.map(|d| d.steps.clone()).unwrap_or_default();
    let index = steps.len() / 2;
    let current = steps
        .get(index)
        .copied()
        .unwrap_or_else(|| dpi.map(|d| d.min).unwrap_or(0));
    DpiState {
        steps,
        index,
        current,
    }
}

impl LuaDevice {
    fn clamp_dpi(&self, dpi: u16) -> u16 {
        dpi.clamp(self.dpi_min, self.dpi_max)
    }
}

#[async_trait]
impl DpiCapability for LuaDevice {
    async fn dpi_status(&self) -> DpiStatus {
        let dpi = self.dpi_state.lock().unwrap();
        DpiStatus {
            steps: dpi.steps.clone(),
            current_index: dpi.index,
            current_dpi: dpi.current,
            available_dpis: (self.dpi_min..=self.dpi_max).step_by(100).collect(),
            mode: self.dpi_mode,
        }
    }

    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()> {
        let apply = {
            let mut dpi = self.dpi_state.lock().unwrap();
            dpi.steps = steps.iter().map(|&s| self.clamp_dpi(s)).collect();
            if dpi.index >= dpi.steps.len() {
                dpi.index = dpi.steps.len().saturating_sub(1);
            }
            dpi.steps.get(dpi.index).copied()
        };
        if let Some(v) = apply {
            self.dpi_state.lock().unwrap().current = v;
            self.worker()?.dpi_set(v).await?;
        }
        Ok(())
    }

    async fn set_dpi_index(&self, index: usize) -> Result<()> {
        let value = {
            let mut dpi = self.dpi_state.lock().unwrap();
            let &v = dpi
                .steps
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("dpi index {index} out of range"))?;
            dpi.index = index;
            dpi.current = v;
            v
        };
        self.worker()?.dpi_set(value).await
    }

    async fn set_dpi_direct(&self, dpi: u16) -> Result<()> {
        let value = self.clamp_dpi(dpi);
        self.dpi_state.lock().unwrap().current = value;
        self.worker()?.dpi_set(value).await
    }
}

#[async_trait]
impl ChoiceCapability for LuaDevice {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.choice_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        if self.choices.is_empty() {
            return None;
        }
        let choices = self
            .choices
            .iter()
            .map(|c| Choice {
                key: c.key.clone(),
                label: c.label.clone(),
                options: c.options.clone(),
                selected: self.choice_cache.get(&c.key).unwrap_or(c.default),
                category: c.category.clone(),
                display: c.display.clone(),
                visible_when: None,
            })
            .collect();
        Some(DeviceCapability::Choice(choices))
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        let choice = self
            .choices
            .iter()
            .find(|c| c.key == key)
            .ok_or_else(|| anyhow::anyhow!("unknown choice key: {key}"))?;
        if selected >= choice.options.len() {
            anyhow::bail!("choice '{key}' selection {selected} out of range");
        }
        self.choice_cache.record(key, selected);
        self.worker()?.choice_set(key, selected).await
    }
}

#[async_trait]
impl ChainAdapter for LuaDevice {
    fn parent_id(&self) -> String {
        self.id.clone()
    }
    fn channels(&self) -> Vec<ChannelDescriptor> {
        self.chain_channels.clone()
    }
    async fn write_composed_frame(&self, channel_id: &str, composed: &[RgbColor]) -> Result<()> {
        self.worker()?.write_ext_frame(channel_id, composed).await
    }
}

#[async_trait]
impl FanHub for LuaDevice {
    fn id(&self) -> &str {
        &self.id
    }
    async fn get_fan_rpm(&self, channel: u8) -> Result<u32> {
        self.worker()?.hub_fan_rpm(channel).await
    }
    async fn get_fan_duty(&self, channel: u8) -> Result<u8> {
        self.worker()?.hub_fan_duty(channel).await
    }
    async fn get_fan_controllable(&self, channel: u8) -> Result<bool> {
        self.worker()?.hub_fan_controllable(channel).await
    }
    async fn set_fan_duty(&self, channel: u8, duty: u8) -> Result<()> {
        self.worker()?.hub_set_fan_duty(channel, duty).await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;
    use crate::drivers::transports::Transport;
    use crate::drivers::{FanCapability, RgbCapability, SensorCapability};
    use std::path::Path;

    fn hid_match() -> DevMatch {
        DevMatch {
            transport: "hid".into(),
            bus: None,
            addr: None,
            pid: Some(0x300E), // Kraken Z (320x320 LCD) for LCD-capable tests
        }
    }

    /// Build a HID plugin device over a mock byte-stream transport.
    fn hid_device(id: &str, manifest: &PluginManifest, transport: Arc<dyn Transport>) -> LuaDevice {
        let spec = &manifest.match_specs[0];
        LuaDevice::with_transport(
            id.into(),
            manifest,
            spec,
            hid_match(),
            PluginIo::Stream {
                transport,
                bulk: None,
            },
            tokio::runtime::Handle::current(),
        )
    }

    const SCRIPT: &str = r#"
        return {
          match = { transport = "hid", vid = 0x1, pid = 0x2 },
          identity = { vendor = "Test", model = "M" },
          transports = { hid = { report_size = 8 } },
          rgb = { zones = { {
              id = "z", name = "Z", topology = { type = "linear" },
              leds = { {id=0, x=0.0, y=0.0}, {id=1, x=1.0, y=0.0} },
          } } },
          fan = { channel = 3 },
          sensor = {},

          write_frame = function(dev, zone, colors)
            local bytes = { 0xAB }
            for _, c in ipairs(colors) do
              bytes[#bytes+1] = c.r
              bytes[#bytes+1] = c.g
              bytes[#bytes+1] = c.b
            end
            dev.transport:write(string.char(table.unpack(bytes)))
          end,
          apply = function(dev, state)
            dev.transport:write(string.char(0xCC))
          end,
          set_duty = function(dev, duty)
            dev.transport:write(string.char(0xFA, duty))
          end,
          get_duty = function(dev) return 42 end,
          get_rpm = function(dev) return 1200 end,
          get_sensors = function(dev)
            return { { id="t", name="Temp", value=30.5, unit="celsius", sensor_type="temperature" } }
          end,
        }
    "#;

    fn device(transport: Arc<dyn Transport>) -> LuaDevice {
        let manifest = super::super::parse_manifest(SCRIPT, Path::new("t.lua")).unwrap();
        hid_device("t-0", &manifest, transport)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advertises_declared_capabilities() {
        let dev = device(Arc::new(MockTransport::empty()));
        let kinds: Vec<_> = dev
            .capabilities()
            .iter()
            .map(std::mem::discriminant)
            .collect();
        assert_eq!(kinds.len(), 3, "rgb + fan + sensor");
        assert_eq!(dev.fan_channel_id(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_frame_encodes_colors_and_reaches_transport() {
        let mock = Arc::new(MockTransport::empty());
        let dev = device(mock.clone());
        let colors = [RgbColor { r: 1, g: 2, b: 3 }, RgbColor { r: 4, g: 5, b: 6 }];
        dev.write_frame("z", &colors).await.unwrap();
        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0xAB, 1, 2, 3, 4, 5, 6]]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn write_frame_accepts_a_halod_buffer() {
        // Same behaviour as the string path, but built with the bounds-checked
        // buffer (0-based, mutable) and passed straight to transport:write.
        const BUF_SCRIPT: &str = r#"
            return {
              match = { transport = "hid", vid = 0x1, pid = 0x2 },
              identity = { vendor = "Test", model = "M" },
              rgb = { zones = { { id="z", name="Z", topology={type="linear"}, leds={ {id=0,x=0,y=0} } } } },
              write_frame = function(dev, zone, colors)
                local b = halod.buffer(1 + 3 * #colors)
                b:set_u8(0, 0xAB)
                for i, c in ipairs(colors) do
                  local base = 1 + (i - 1) * 3
                  b:set_u8(base, c.r)
                  b:set_u8(base + 1, c.g)
                  b:set_u8(base + 2, c.b)
                end
                dev.transport:write(b)
              end,
            }
        "#;
        let manifest = super::super::parse_manifest(BUF_SCRIPT, Path::new("buf.lua")).unwrap();
        let mock = Arc::new(MockTransport::empty());
        let dev = hid_device("b-0", &manifest, mock.clone());
        dev.write_frame(
            "z",
            &[RgbColor {
                r: 10,
                g: 20,
                b: 30,
            }],
        )
        .await
        .unwrap();
        assert_eq!(*mock.written.lock().await, vec![vec![0xAB, 10, 20, 30]]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn apply_persists_state_and_calls_script() {
        let mock = Arc::new(MockTransport::empty());
        let dev = device(mock.clone());
        let state = RgbState::Static {
            color: RgbColor { r: 9, g: 9, b: 9 },
        };
        dev.apply(state).await.unwrap();
        assert_eq!(*mock.written.lock().await, vec![vec![0xCC]]);
        assert!(matches!(
            RgbCapability::current_state(&dev),
            Some(RgbState::Static { .. })
        ));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn fan_duty_and_rpm_round_trip() {
        let mock = Arc::new(MockTransport::empty());
        let dev = device(mock.clone());
        dev.set_duty(50).await.unwrap();
        assert_eq!(*mock.written.lock().await, vec![vec![0xFA, 50]]);
        assert_eq!(dev.get_duty().await.unwrap(), 42);
        assert_eq!(dev.get_rpm().await, Some(1200));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn reports_write_rate_from_transport() {
        // The Info UI's throughput meter reads Device::write_rate_status().
        let dev = device(Arc::new(MockTransport::empty()));
        assert!(dev.write_rate_status().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn get_sensors_deserializes_lua_table() {
        let dev = device(Arc::new(MockTransport::empty()));
        let sensors = dev.get_sensors().await.unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].name, "Temp");
        assert_eq!(sensors[0].value, 30.5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn poll_caches_status_read_by_sensors() {
        // read_status parses a report into dev.status; get_sensors reads the
        // cache rather than hitting hardware. Long interval => only the ticker's
        // immediate first tick plus our explicit poll_once fire (2 reads).
        const POLL_SCRIPT: &str = r#"
            return {
              match = { transport = "hid", vid = 0x1, pid = 0x2 },
              identity = { vendor = "Test", model = "M" },
              sensor = {},
              poll = { interval_ms = 3600000 },
              read_status = function(dev)
                local b = halod.buffer(dev.transport:read_nonblocking(1))
                return { temp = b:get_u8(0) }
              end,
              get_sensors = function(dev)
                local s = dev.status or {}
                return { { id="t", name="Temp", value = s.temp or -1, unit="celsius" } }
              end,
            }
        "#;
        let manifest = super::super::parse_manifest(POLL_SCRIPT, Path::new("poll.lua")).unwrap();
        let mock = Arc::new(MockTransport::new(vec![vec![55], vec![55]]));
        let dev = hid_device("poll-0", &manifest, mock.clone());
        dev.poll_once().await.unwrap();
        assert_eq!(dev.get_sensors().await.unwrap()[0].value, 55.0);
    }

    // ── Chain / children ────────────────────────────────────────────────

    const CHAIN_SCRIPT: &str = r#"
        return {
          match = { transport = "hid", vid = 0x1, pid = 0x2 },
          identity = { vendor = "NZXT", model = "Kraken" },
          chain = {
            channels = { { id = "0", name = "External", max_leds = 40 } },
            accessories = {
              { id = 0x13, name = "F120 RGB", led_count = 8, topology = "ring", fan = true },
            },
          },
          detect_accessories = function(dev)
            return { { channel = 0, accessory = 0x13 } }
          end,
          write_ext_frame = function(dev, channel, colors)
            local b = halod.buffer(1 + #colors)
            b:set_u8(0, 0xE0)
            for i, c in ipairs(colors) do b:set_u8(i, c.r) end
            dev.transport:write(b)
          end,
          fan_duty = function(dev, ch) return 60 end,
          fan_rpm = function(dev, ch) return 1400 end,
          fan_controllable = function(dev, ch) return true end,
          set_fan_duty = function(dev, ch, duty)
            dev.transport:write(string.char(0xFD, ch, duty))
          end,
        }
    "#;

    fn chain_device(transport: Arc<dyn Transport>) -> Arc<LuaDevice> {
        use crate::drivers::chain::{ChainAdapter, ChainHost};
        use crate::drivers::CHAIN_LINK_KIND_NZXT_ARGB;
        let manifest = super::super::parse_manifest(CHAIN_SCRIPT, Path::new("kraken.lua")).unwrap();
        let spec = &manifest.match_specs[0];
        let dev = Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::with_transport(
                "kraken-0".into(),
                &manifest,
                spec,
                hid_match(),
                PluginIo::Stream {
                    transport,
                    bulk: None,
                },
                tokio::runtime::Handle::current(),
            );
            d.set_self_ref(weak.clone());
            d
        });
        let adapter: Arc<dyn ChainAdapter> = dev.clone();
        dev.install_chain_host(ChainHost::new(adapter, CHAIN_LINK_KIND_NZXT_ARGB));
        dev
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn kraken_example_emits_native_wire_bytes() {
        // Conformance: the shipped Kraken plugin must encode exactly like the
        // native NzxtKrakenProtocol (Z/Elite 0x26 0x14 GRB, 0x72 duty profiles).
        use crate::drivers::chain::{ChainAdapter, ChainHost};
        use crate::drivers::CHAIN_LINK_KIND_NZXT_ARGB;
        let src = include_str!("builtins/nzxt_kraken.lua");
        let manifest = super::super::parse_manifest(src, Path::new("nzxt_kraken.lua")).unwrap();
        let mock = Arc::new(MockTransport::empty());
        let spec = &manifest.match_specs[0];
        let dev = Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::with_transport(
                "k".into(),
                &manifest,
                spec,
                hid_match(),
                PluginIo::Stream {
                    transport: mock.clone(),
                    bulk: None,
                },
                tokio::runtime::Handle::current(),
            );
            d.set_self_ref(weak.clone());
            d
        });
        let adapter: Arc<dyn ChainAdapter> = dev.clone();
        dev.install_chain_host(ChainHost::new(adapter, CHAIN_LINK_KIND_NZXT_ARGB));

        // Pump duty: [0x72,0x01,0x00,0x00] + 40 x clamp(duty,20,100).
        dev.set_duty(60).await.unwrap();
        {
            let w = mock.written.lock().await;
            let pkt = w.last().unwrap();
            assert_eq!(&pkt[0..4], &[0x72, 0x01, 0x00, 0x00]);
            assert_eq!(pkt.len(), 4 + 40);
            assert!(pkt[4..].iter().all(|&d| d == 60));
        }

        // Ring RGB: [0x26,0x14,0x01,0x01] + 120 GRB bytes; LED0 = g,r,b.
        let mut colors = vec![RgbColor { r: 0, g: 0, b: 0 }; 24];
        colors[0] = RgbColor {
            r: 10,
            g: 20,
            b: 30,
        };
        dev.write_frame("ring", &colors).await.unwrap();
        {
            let w = mock.written.lock().await;
            let pkt = w.last().unwrap();
            assert_eq!(&pkt[0..4], &[0x26, 0x14, 0x01, 0x01]);
            assert_eq!(pkt.len(), 4 + 120);
            assert_eq!(&pkt[4..7], &[20, 10, 30]); // GRB order
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advertises_controller_and_chain() {
        use crate::drivers::Device;
        let dev = chain_device(Arc::new(MockTransport::empty()));
        assert!(dev.as_controller().is_some());
        assert!(dev.as_chain().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn discover_children_builds_and_wires_the_fan_child() {
        use crate::drivers::Controller;
        let mock = Arc::new(MockTransport::empty());
        let dev = chain_device(mock.clone());

        let children = dev.discover_children().await;
        assert_eq!(children.len(), 1);
        let child = &children[0];
        assert_eq!(child.id(), "kraken-0_acc_0_19"); // 0x13 == 19
        assert_eq!(child.name(), "F120 RGB");

        // Fan reads/writes route back through the parent's FanHub into the script.
        let fan = child.as_fan().expect("child has a fan");
        assert_eq!(fan.get_duty().await.unwrap(), 60);
        assert_eq!(fan.get_rpm().await, Some(1400));
        fan.set_duty(77).await.unwrap();
        assert_eq!(
            *mock.written.lock().await.last().unwrap(),
            vec![0xFD, 0, 77]
        );

        // An RGB frame composes through ChainHost and reaches write_ext_frame.
        let rgb = child.as_rgb().expect("child has rgb");
        rgb.write_frame("ring", &[RgbColor { r: 5, g: 0, b: 0 }; 8])
            .await
            .unwrap();
        assert_eq!(
            *mock.written.lock().await.last().unwrap(),
            vec![0xE0, 5, 5, 5, 5, 5, 5, 5, 5]
        );
    }
}
