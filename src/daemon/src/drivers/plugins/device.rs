// SPDX-License-Identifier: GPL-3.0-or-later
//! The generic device a plugin instantiates. It sits behind the same `Device`
//! seam as every native driver and forwards capability calls into the per-device
//! Lua worker. Which capabilities it advertises is decided entirely by the
//! manifest — Halo owns the capability taxonomy; the script only fills it in.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{RgbColor, RgbDescriptor, RgbState, Sensor};

use crate::drivers::transports::Transport;
use crate::drivers::{
    CapabilityRef, Device, FanCapability, FanStateSlot, RgbCapability, RgbStateSlot,
    SensorCapability,
};

use super::manifest::PluginManifest;
use super::worker::PluginHandle;

/// A device whose behaviour is defined by a plugin script rather than native
/// Rust.
pub struct LuaDevice {
    id: String,
    name: String,
    vendor: String,
    model: String,
    plugin_id: String,

    /// Present when the plugin declares a capability (RGB/fan/sensor). Absent
    /// for a device-only plugin.
    worker: Option<PluginHandle>,

    has_rgb: bool,
    has_fan: bool,
    has_sensor: bool,

    rgb_descriptor: RgbDescriptor,
    rgb_slot: RgbStateSlot,
    fan_slot: FanStateSlot,
    fan_channel: u8,
}

impl LuaDevice {
    /// A plugin that declares no capability — identity + lifecycle only.
    pub fn device_only(id: String, manifest: &PluginManifest) -> Self {
        Self::build(id, manifest, None)
    }

    /// A plugin with capabilities, backed by a worker over `transport`.
    pub fn with_transport(
        id: String,
        manifest: &PluginManifest,
        transport: Arc<dyn Transport>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let worker = PluginHandle::spawn(manifest.script_source.clone(), transport, handle);
        Self::build(id, manifest, Some(worker))
    }

    fn build(id: String, manifest: &PluginManifest, worker: Option<PluginHandle>) -> Self {
        Self {
            id,
            name: manifest.display_name().to_owned(),
            vendor: manifest.identity.vendor.clone(),
            model: manifest.identity.model.clone(),
            plugin_id: manifest.plugin_id.clone(),
            worker,
            has_rgb: manifest.rgb.is_some(),
            has_fan: manifest.fan.is_some(),
            has_sensor: manifest.sensor.is_some(),
            rgb_descriptor: manifest.rgb_descriptor().unwrap_or(RgbDescriptor {
                zones: Vec::new(),
                native_effects: Vec::new(),
            }),
            rgb_slot: RgbStateSlot::default(),
            fan_slot: FanStateSlot::default(),
            fan_channel: manifest.fan.as_ref().map(|f| f.channel).unwrap_or(0),
        }
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
        &self.model
    }

    async fn initialize(&self) -> Result<bool> {
        match &self.worker {
            Some(w) => w.initialize().await,
            None => Ok(true),
        }
    }

    async fn close(&self) {
        if let Some(w) = &self.worker {
            w.close().await;
        }
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
        caps
    }
}

#[async_trait]
impl RgbCapability for LuaDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        &self.rgb_descriptor
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

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;
    use crate::drivers::{FanCapability, RgbCapability, SensorCapability};
    use std::path::Path;

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
        LuaDevice::with_transport(
            "t-0".into(),
            &manifest,
            transport,
            tokio::runtime::Handle::current(),
        )
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
    async fn apply_persists_state_and_calls_script() {
        let mock = Arc::new(MockTransport::empty());
        let dev = device(mock.clone());
        let state = RgbState::Static {
            color: RgbColor { r: 9, g: 9, b: 9 },
        };
        dev.apply(state).await.unwrap();
        assert_eq!(*mock.written.lock().await, vec![vec![0xCC]]);
        assert!(matches!(dev.current_state(), Some(RgbState::Static { .. })));
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
    async fn get_sensors_deserializes_lua_table() {
        let dev = device(Arc::new(MockTransport::empty()));
        let sensors = dev.get_sensors().await.unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].name, "Temp");
        assert_eq!(sensors[0].value, 30.5);
    }
}
