// SPDX-License-Identifier: GPL-3.0-or-later
//! The generic device a plugin instantiates. It sits behind the same `Device`
//! seam as every native driver and forwards capability calls into the per-device
//! Lua worker. Which capabilities it advertises is decided entirely by the
//! manifest — Halo owns the capability taxonomy; the script only fills it in.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock, Weak};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{RgbColor, RgbDescriptor, RgbState, Sensor, WriteRateStatus};

use crate::drivers::chain::{ChainAdapter, ChainHost, ChainHub, ChannelDescriptor};
use crate::drivers::transports::Transport;
use crate::drivers::{
    CapabilityRef, ChainCapability, Controller, Device, FanCapability, FanHub, FanStateSlot,
    RgbCapability, RgbStateSlot, SensorCapability,
};

use super::chain_leaf::ChainLeaf;
use super::manifest::{AccessoryManifest, PluginManifest};
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
    /// Clone of the (metered) transport, kept so the device can report
    /// write-rate/throughput to the Info UI. `None` for device-only plugins.
    transport: Option<Arc<dyn Transport>>,

    has_rgb: bool,
    has_fan: bool,
    has_sensor: bool,

    rgb_descriptor: RgbDescriptor,
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
    pub fn device_only(id: String, manifest: &PluginManifest) -> Self {
        Self::build(id, manifest, None, None)
    }

    /// A plugin with capabilities, backed by a worker over `transport`.
    pub fn with_transport(
        id: String,
        manifest: &PluginManifest,
        transport: Arc<dyn Transport>,
        handle: tokio::runtime::Handle,
    ) -> Self {
        // Keep a handle to the (metered) transport so the device can report
        // write-rate/throughput; the worker owns the one it does I/O through.
        let rate_transport = transport.clone();
        let worker = PluginHandle::spawn(manifest.script_source.clone(), transport, handle.clone());
        let mut dev = Self::build(id, manifest, Some(worker.clone()), Some(rate_transport));

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
        worker: Option<PluginHandle>,
        transport: Option<Arc<dyn Transport>>,
    ) -> Self {
        Self {
            id,
            name: manifest.display_name().to_owned(),
            vendor: manifest.identity.vendor.clone(),
            model: manifest.identity.model.clone(),
            plugin_id: manifest.plugin_id.clone(),
            worker,
            transport,
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

    fn write_rate_status(&self) -> Option<WriteRateStatus> {
        self.transport.as_ref().map(|t| t.rate_status())
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
        let dev = LuaDevice::with_transport(
            "b-0".into(),
            &manifest,
            mock.clone(),
            tokio::runtime::Handle::current(),
        );
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
        let dev = LuaDevice::with_transport(
            "poll-0".into(),
            &manifest,
            mock.clone(),
            tokio::runtime::Handle::current(),
        );
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
        let dev = Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::with_transport(
                "kraken-0".into(),
                &manifest,
                transport,
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
        let src = include_str!("../../../../../plugins/examples/nzxt_kraken.lua");
        let manifest = super::super::parse_manifest(src, Path::new("nzxt_kraken.lua")).unwrap();
        let mock = Arc::new(MockTransport::empty());
        let dev = Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::with_transport(
                "k".into(),
                &manifest,
                mock.clone(),
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
