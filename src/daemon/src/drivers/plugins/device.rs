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
    Action, Battery, Boolean, ButtonAction, ButtonDescriptor, ButtonMapping, Choice,
    ConnectionStatus, DeviceCapability, DeviceType, DpiMode, DpiStatus, Equalizer, KeyRemapStatus,
    LcdDescriptor, NativeEffect, PluginKind, Range, RgbColor, RgbDescriptor, RgbState, RgbZone,
    ScreenRotation, ScreenShape, Sensor, WriteRateStatus,
};

use crate::drivers::chain::{ChainAdapter, ChainHost, ChainHub, ChannelDescriptor};
use crate::drivers::{
    ActionCapability, BatteryCapability, BoolStateCache, BooleanCapability, CapabilityRef,
    ChainCapability, ChoiceCapability, ChoiceStateCache, ConnectionCapability, Controller, Device,
    DpiCapability, EqualizerCapability, FanCapability, FanHub, FanStateSlot, KeyRemapCapability,
    LcdCapability, LcdStateSlot, OnboardProfilesCapability, PairingCapability, RangeCapability,
    RangeStateCache, RgbCapability, RgbStateSlot, SensorCapability,
};

use super::chain_leaf::ChainLeaf;
use super::manifest::{
    topology_from, AccessoryManifest, ActionDef, BooleanDef, ChoiceDef, DeviceSpec, DpiManifest,
    PluginManifest, RangeDef, RgbManifest,
};
use super::transport::PluginIo;
use super::worker::{DetectedController, DevMatch, InitLcd, InitZone, PluginHandle};
use std::collections::HashMap;

/// Host-side DPI step-cycle state (the plugin only writes the chosen value).
struct DpiState {
    steps: Vec<u16>,
    index: usize,
    current: u16,
}
use crate::drivers::vendors::generic::devices::common::{
    linear_rgb_zone, ring_led_positions, transformed_zone_frame,
};
use halod_shared::types::ZoneTopology;

/// Yields the transport one integration child drives its worker over. The `u32`
/// is the controller index. Injectable so tests can back it with a mock
/// transport instead of a real socket. Every controller shares the root's single
/// connection, so their frame writes serialise behind one socket lock and stay
/// in phase (see `integration_scan`).
pub(super) type ChildWorkerFactory = Arc<dyn Fn(u32) -> Result<PluginIo> + Send + Sync>;

/// The capability sections a manifest can declare. Stored as `caps` on the
/// device so `capabilities()` reads a single list instead of one boolean per
/// kind; the mapping to `CapabilityRef` lives in `capabilities()`.
#[derive(Clone, Copy, PartialEq, Eq)]
enum Cap {
    Rgb,
    Fan,
    Sensor,
    Lcd,
    Dpi,
    Choice,
    Range,
    Boolean,
    Action,
    Battery,
    Connection,
    Equalizer,
    Pairing,
    OnboardProfiles,
    KeyRemap,
    Chain,
}

/// Which capability sections the manifest declares, in advertised order. Mirrors
/// `PluginManifest::needs_worker` / `capability_labels`, but yields the typed
/// `Cap` list the device stores.
fn declared_caps(manifest: &PluginManifest) -> Vec<Cap> {
    let mut caps = Vec::new();
    let mut push = |present: bool, cap: Cap| {
        if present {
            caps.push(cap);
        }
    };
    push(manifest.rgb.is_some(), Cap::Rgb);
    push(manifest.fan.is_some(), Cap::Fan);
    push(manifest.sensor.is_some(), Cap::Sensor);
    push(manifest.lcd.is_some(), Cap::Lcd);
    push(manifest.dpi.is_some(), Cap::Dpi);
    push(manifest.choice.is_some(), Cap::Choice);
    push(manifest.range.is_some(), Cap::Range);
    push(manifest.boolean.is_some(), Cap::Boolean);
    push(manifest.action.is_some(), Cap::Action);
    push(manifest.battery.is_some(), Cap::Battery);
    push(manifest.connection.is_some(), Cap::Connection);
    push(manifest.equalizer.is_some(), Cap::Equalizer);
    push(manifest.pairing.is_some(), Cap::Pairing);
    push(manifest.onboard_profiles.is_some(), Cap::OnboardProfiles);
    push(manifest.key_remap.is_some(), Cap::KeyRemap);
    push(manifest.chain.is_some(), Cap::Chain);
    caps
}

/// The four "control" capability groups (choice/range/boolean/action) share the
/// same shape — a `Vec<Def>` of declared controls plus a value cache. Grouping
/// them slims `LuaDevice` and hosts the repeated wire/lookup logic in one place.
#[derive(Default)]
struct Controls {
    choices: Vec<ChoiceDef>,
    choice_cache: ChoiceStateCache,
    ranges: Vec<RangeDef>,
    range_cache: RangeStateCache,
    /// Boolean values are read live from `get_booleans`; the cache only backs
    /// save/restore of the last-known write.
    booleans: Vec<BooleanDef>,
    bool_cache: BoolStateCache,
    /// Fire-and-forget; no cached state.
    actions: Vec<ActionDef>,
}

impl Controls {
    fn from_manifest(manifest: &PluginManifest) -> Self {
        Self {
            choices: manifest
                .choice
                .as_ref()
                .map(|c| c.choices.clone())
                .unwrap_or_default(),
            ranges: manifest
                .range
                .as_ref()
                .map(|r| r.ranges.clone())
                .unwrap_or_default(),
            booleans: manifest
                .boolean
                .as_ref()
                .map(|b| b.booleans.clone())
                .unwrap_or_default(),
            actions: manifest
                .action
                .as_ref()
                .map(|a| a.actions.clone())
                .unwrap_or_default(),
            ..Default::default()
        }
    }

    /// Wire snapshot of the choice controls (cache overrides each default), or
    /// `None` when none are declared.
    fn choices_wire(&self) -> Option<DeviceCapability> {
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

    /// Wire snapshot of the range controls (cache overrides each default), or
    /// `None` when none are declared.
    fn ranges_wire(&self) -> Option<DeviceCapability> {
        if self.ranges.is_empty() {
            return None;
        }
        let ranges = self
            .ranges
            .iter()
            .map(|r| Range {
                key: r.key.clone(),
                label: r.label.clone(),
                min: r.min,
                max: r.max,
                step: r.step,
                value: self.range_cache.get(&r.key).unwrap_or(r.default),
                read_only: r.read_only,
                category: r.category.clone(),
                start_label: r.start_label.clone(),
                end_label: r.end_label.clone(),
                display: r.display.clone(),
                visible_when: None,
            })
            .collect();
        Some(DeviceCapability::Range(ranges))
    }

    /// Wire snapshot of the action controls, or `None` when none are declared.
    fn actions_wire(&self) -> Option<DeviceCapability> {
        if self.actions.is_empty() {
            return None;
        }
        let actions = self
            .actions
            .iter()
            .map(|a| Action {
                key: a.key.clone(),
                label: a.label.clone(),
                category: a.category.clone(),
                visible_when: None,
            })
            .collect();
        Some(DeviceCapability::Action(actions))
    }

    /// Validate a choice selection against the declared options and cache it.
    /// Errors if the key is unknown or the index is out of range.
    fn record_choice(&self, key: &str, selected: usize) -> Result<()> {
        let choice = self
            .choices
            .iter()
            .find(|c| c.key == key)
            .ok_or_else(|| anyhow::anyhow!("unknown choice key: {key}"))?;
        if selected >= choice.options.len() {
            anyhow::bail!("choice '{key}' selection {selected} out of range");
        }
        self.choice_cache.record(key, selected);
        Ok(())
    }

    /// Clamp a range value to its declared bounds and cache it, returning the
    /// clamped value to forward to the worker. Errors if the key is unknown.
    fn record_range(&self, key: &str, value: i32) -> Result<i32> {
        let range = self
            .ranges
            .iter()
            .find(|r| r.key == key)
            .ok_or_else(|| anyhow::anyhow!("unknown range key: {key}"))?;
        let value = value.clamp(range.min, range.max);
        self.range_cache.record(key, value);
        Ok(value)
    }

    /// Backfill empty `label`/`category` on live boolean reads from the manifest
    /// decls (scripts may report only `{key, value}` pairs).
    fn backfill_booleans(&self, live: &mut [Boolean]) {
        for b in live {
            if let Some(decl) = self.booleans.iter().find(|d| d.key == b.key) {
                if b.label.is_empty() {
                    b.label = decl.label.clone();
                }
                if b.category.is_empty() {
                    b.category = decl.category.clone();
                }
            }
        }
    }

    /// True if an action with `key` is declared.
    fn has_action(&self, key: &str) -> bool {
        self.actions.iter().any(|a| a.key == key)
    }
}

/// A device whose behaviour is defined by a plugin script rather than native
/// Rust.
pub struct LuaDevice {
    id: String,
    name: String,
    vendor: String,
    model: String,
    plugin_id: String,
    plugin_type: PluginKind,
    device_type: DeviceType,
    transport_kind: &'static str,
    dynamic_model: OnceLock<String>,
    worker: Option<PluginHandle>,
    transport: Option<PluginIo>,
    /// For integration roots only: opens a fresh transport per controller.
    integration_child_worker: Option<ChildWorkerFactory>,
    /// For integration roots only: the manifest each controller child is
    /// synthesized from (`child_manifest_for`).
    root_manifest: Option<Arc<PluginManifest>>,

    /// Capability sections the manifest declared, in advertised order. Drives
    /// `capabilities()` (chain also implies `Controller`; integration adds one).
    caps: Vec<Cap>,

    dpi_state: Mutex<DpiState>,
    dpi_min: u16,
    dpi_max: u16,
    dpi_mode: DpiMode,

    /// Declared choice/range/boolean/action controls + their value caches.
    controls: Controls,

    /// Last equalizer snapshot read, backing `EqualizerCapability::current_state`
    /// (and therefore save/restore) between explicit `get_equalizer` calls.
    eq_cache: Mutex<Option<Equalizer>>,

    key_remap_buttons: Vec<ButtonDescriptor>,
    key_remap_requires_host_mode: bool,
    key_remap_default_mappings: Vec<ButtonMapping>,
    /// Host-cached mappings that differ from `ButtonAction::Native`.
    key_remap_mappings: Mutex<HashMap<u16, ButtonMapping>>,

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
    pub fn device_only(id: String, manifest: &PluginManifest, spec: &DeviceSpec) -> Self {
        Self::build(id, manifest, Some(spec), None, None)
    }

    /// A plugin with capabilities, backed by a worker over `transport`.
    pub fn with_transport(
        id: String,
        manifest: &PluginManifest,
        spec: &DeviceSpec,
        dev_match: DevMatch,
        transport: PluginIo,
        handle: tokio::runtime::Handle,
    ) -> Self {
        Self::with_worker(id, manifest, Some(spec), dev_match, transport, handle)
    }

    /// The headless root of a config-instantiated integration plugin
    pub fn integration_root(
        id: String,
        manifest: &PluginManifest,
        transport: PluginIo,
        child_worker: ChildWorkerFactory,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let dev_match = DevMatch {
            transport: "tcp".to_owned(),
            ..Default::default()
        };
        let mut dev = Self::with_worker(id, manifest, None, dev_match, transport, handle);
        dev.integration_child_worker = Some(child_worker);
        dev.root_manifest = Some(Arc::new(manifest.clone()));
        dev
    }

    /// One integration controller as a full `LuaDevice`: its capability set
    /// comes from the enumerated controller (`child_manifest_for`), and its
    /// worker VM is seeded with the controller `index` in `dev.match.index`, so
    /// the shared script routes each capability call to the right remote
    /// controller.
    pub fn integration_child(
        id: String,
        name: String,
        vendor: String,
        manifest: &PluginManifest,
        controller_index: u32,
        transport: PluginIo,
        handle: tokio::runtime::Handle,
    ) -> Self {
        let dev_match = DevMatch {
            transport: "tcp".to_owned(),
            index: Some(controller_index),
            ..Default::default()
        };
        let mut dev = Self::with_worker(id, manifest, None, dev_match, transport, handle);
        dev.name = name;
        dev.vendor = vendor;
        dev
    }

    fn with_worker(
        id: String,
        manifest: &PluginManifest,
        spec: Option<&DeviceSpec>,
        dev_match: DevMatch,
        transport: PluginIo,
        handle: tokio::runtime::Handle,
    ) -> Self {
        // Keep a handle to the (metered) transport so the device can report
        // write-rate/throughput; the worker owns the one it does I/O through.
        let rate_transport = transport.clone();
        let granted = super::granted_for(&manifest.plugin_id);
        let config = super::resolved_config_for(&manifest.plugin_id, &granted);
        let zones: Vec<RgbZone> = manifest
            .rgb
            .as_ref()
            .map(|r| r.zones.clone())
            .unwrap_or_default();
        let worker = PluginHandle::spawn(
            manifest.script_source.clone(),
            transport,
            dev_match,
            granted,
            config,
            handle.clone(),
            zones,
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
        spec: Option<&DeviceSpec>,
        worker: Option<PluginHandle>,
        transport: Option<PluginIo>,
    ) -> Self {
        Self {
            id,
            name: spec
                .map(|s| s.display_name().to_owned())
                .unwrap_or_else(|| manifest.display_name()),
            vendor: spec.map(|s| s.vendor.clone()).unwrap_or_default(),
            model: spec.map(|s| s.model.clone()).unwrap_or_default(),
            plugin_id: manifest.plugin_id.clone(),
            plugin_type: manifest.plugin_type,
            device_type: spec.and_then(|s| s.device_type).unwrap_or_default(),
            transport_kind: spec
                .and_then(|s| super::transport::descriptor_for(&s.transport))
                .map(|d| d.kind)
                .unwrap_or("tcp"),
            dynamic_model: OnceLock::new(),
            worker,
            transport,
            integration_child_worker: None,
            root_manifest: None,
            caps: declared_caps(manifest),
            lcd_descriptor: OnceLock::new(),
            lcd_slot: LcdStateSlot::default(),
            lcd_needs_rgb_restore: manifest
                .lcd
                .as_ref()
                .map(|l| l.needs_rgb_restore)
                .unwrap_or(false),
            dpi_state: Mutex::new(build_dpi_state(manifest.dpi.as_ref())),
            dpi_min: manifest.dpi.as_ref().map(|d| d.min).unwrap_or(0),
            dpi_max: manifest.dpi.as_ref().map(|d| d.max).unwrap_or(0),
            dpi_mode: match manifest.dpi.as_ref().map(|d| d.onboard) {
                Some(true) => DpiMode::Onboard,
                _ => DpiMode::Host,
            },
            controls: Controls::from_manifest(manifest),
            eq_cache: Mutex::new(None),
            key_remap_buttons: manifest
                .key_remap
                .as_ref()
                .map(|k| k.buttons.clone())
                .unwrap_or_default(),
            key_remap_requires_host_mode: manifest
                .key_remap
                .as_ref()
                .map(|k| k.requires_host_mode)
                .unwrap_or(false),
            key_remap_default_mappings: manifest
                .key_remap
                .as_ref()
                .map(|k| k.default_mappings.clone())
                .unwrap_or_default(),
            key_remap_mappings: Mutex::new(HashMap::new()),
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

    /// Surface a worker call's Lua runtime error to the user (deduplicated),
    /// clearing the dedup flag on success. Background/engine paths (RGB apply,
    /// fan duty, sensor poll) otherwise only log the failure, so a broken plugin
    /// is invisible — this turns the first failure of an episode into one toast.
    async fn track<T>(&self, result: Result<T>) -> Result<T> {
        match &result {
            Ok(_) => super::clear_runtime_error(&self.id),
            Err(e) => super::report_runtime_error(&self.id, &self.name, format!("{e:#}")).await,
        }
        result
    }

    /// Trigger one status poll synchronously (used by tests; production relies on
    /// the ticker).
    #[cfg(test)]
    pub async fn poll_once(&self) -> Result<()> {
        self.worker()?.poll().await
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

    fn integration_id(&self) -> Option<String> {
        (self.plugin_type == PluginKind::Integration).then(|| self.plugin_id.clone())
    }

    fn owning_plugin_id(&self) -> Option<String> {
        Some(self.plugin_id.clone())
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
        // Seed the host range/choice caches with the values the device reported,
        // so the UI shows live hardware state rather than manifest defaults.
        if let Some(ranges) = outcome.ranges {
            for (key, value) in ranges {
                self.controls.range_cache.record(&key, value);
            }
        }
        if let Some(choices) = outcome.choices {
            for (key, selected) in choices {
                self.controls.choice_cache.record(&key, selected);
            }
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
        for cap in &self.caps {
            match cap {
                Cap::Rgb => caps.push(CapabilityRef::Rgb(self)),
                Cap::Fan => caps.push(CapabilityRef::Fan(self)),
                Cap::Sensor => caps.push(CapabilityRef::Sensor(self)),
                Cap::Lcd => caps.push(CapabilityRef::Lcd(self)),
                Cap::Dpi => caps.push(CapabilityRef::Dpi(self)),
                Cap::Choice => caps.push(CapabilityRef::Choice(self)),
                Cap::Range => caps.push(CapabilityRef::Range(self)),
                Cap::Boolean => caps.push(CapabilityRef::Boolean(self)),
                Cap::Action => caps.push(CapabilityRef::Action(self)),
                Cap::Battery => caps.push(CapabilityRef::Battery(self)),
                Cap::Connection => caps.push(CapabilityRef::Connection(self)),
                Cap::Equalizer => caps.push(CapabilityRef::Equalizer(self)),
                Cap::Pairing => caps.push(CapabilityRef::Pairing(self)),
                Cap::OnboardProfiles => caps.push(CapabilityRef::OnboardProfiles(self)),
                Cap::KeyRemap => caps.push(CapabilityRef::KeyRemap(self)),
                Cap::Chain => {
                    caps.push(CapabilityRef::Controller(self));
                    caps.push(CapabilityRef::Chain(self));
                }
            }
        }
        if self.plugin_type == PluginKind::Integration {
            caps.push(CapabilityRef::Controller(self));
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
        let state = apply_per_led_transforms(self.descriptor(), &self.rgb_slot, state);
        self.rgb_slot.set_state(Some(state.clone()));
        let r = self.worker()?.rgb_apply(state).await;
        self.track(r).await
    }

    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()> {
        if !self.descriptor().zones.iter().any(|z| z.id == zone_id) {
            anyhow::bail!("unknown zone: {zone_id}");
        }
        let r = self.worker()?.rgb_write_frame(zone_id, colors).await;
        self.track(r).await
    }

    fn rgb_state(&self) -> &RgbStateSlot {
        &self.rgb_slot
    }
}

/// Apply each zone's content transform to PerLed colour maps before handing the
/// state to the plugin, so the plugin never needs to understand transforms.
fn apply_per_led_transforms(
    descriptor: &RgbDescriptor,
    slot: &RgbStateSlot,
    state: RgbState,
) -> RgbState {
    let RgbState::PerLed { zones } = state else {
        return state;
    };
    let mut transformed = HashMap::new();
    for (zone_id, led_map) in &zones {
        let Some(zone) = descriptor.zones.iter().find(|z| &z.id == zone_id) else {
            transformed.insert(zone_id.clone(), led_map.clone());
            continue;
        };
        let colors = transformed_zone_frame(zone, slot, led_map);
        let new_map: HashMap<String, RgbColor> = colors
            .iter()
            .enumerate()
            .map(|(i, c)| (i.to_string(), *c))
            .collect();
        transformed.insert(zone_id.clone(), new_map);
    }
    RgbState::PerLed { zones: transformed }
}

#[async_trait]
impl FanCapability for LuaDevice {
    async fn get_duty(&self) -> Result<u8> {
        self.worker()?.fan_get_duty().await
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        let r = self.worker()?.fan_set_duty(duty).await;
        self.track(r).await
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
        let r = self.worker()?.get_sensors().await;
        self.track(r).await
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
        if self.plugin_type == PluginKind::Integration {
            return self.discover_controllers().await;
        }
        self.discover_chain_accessories().await
    }
}

impl LuaDevice {
    async fn discover_chain_accessories(&self) -> Vec<Arc<dyn Device>> {
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

    /// Integration path: one full `LuaDevice` per controller the plugin's
    /// `enumerate_controllers` reports. Each controller advertises its own
    /// capabilities (synthesized into a child manifest) and gets its own Lua VM,
    /// seeded with the controller index so the shared script routes each call to
    /// the right remote controller. All children share the root's single
    /// connection, so their frame writes serialise behind one socket lock and
    /// sibling controllers stay in phase.
    async fn discover_controllers(&self) -> Vec<Arc<dyn Device>> {
        let Some(worker) = &self.worker else {
            return Vec::new();
        };
        let detected = match worker.enumerate_controllers().await {
            Ok(d) => d,
            Err(e) => {
                log::warn!("plugin '{}' enumerate_controllers: {e:#}", self.plugin_id);
                return Vec::new();
            }
        };
        let (Some(factory), Some(root_manifest)) = (
            self.integration_child_worker.clone(),
            self.root_manifest.clone(),
        ) else {
            log::error!(
                "plugin '{}': integration root missing child-worker factory or manifest — this is a daemon bug",
                self.plugin_id
            );
            return Vec::new();
        };
        let handle = tokio::runtime::Handle::current();

        let mut out = Vec::new();
        for controller in detected {
            // Open a fresh connection for this controller. The connect can block
            // for the transport's timeout, so run it off the runtime.
            let factory = factory.clone();
            let index = controller.index;
            let transport = match tokio::task::spawn_blocking(move || factory(index)).await {
                Ok(Ok(t)) => t,
                Ok(Err(e)) => {
                    log::warn!(
                        "plugin '{}' controller {} connect failed: {e:#}",
                        self.plugin_id,
                        controller.index
                    );
                    continue;
                }
                Err(e) => {
                    log::warn!(
                        "plugin '{}' controller {} connect task panicked: {e}",
                        self.plugin_id,
                        controller.index
                    );
                    continue;
                }
            };
            let child_manifest = child_manifest_for(&root_manifest, &controller);
            // `new_cyclic` so a controller that itself declares `chain` can hand
            // its accessories a `FanHub` back-reference (nested chain).
            let child = Arc::new_cyclic(|weak| {
                let mut d = LuaDevice::integration_child(
                    format!("{}_ctrl_{}", self.id, controller.index),
                    controller.name.clone(),
                    self.vendor.clone(),
                    &child_manifest,
                    controller.index,
                    transport,
                    handle.clone(),
                );
                d.set_self_ref(weak.clone());
                d
            });
            if child_manifest.chain.is_some() {
                let adapter: Arc<dyn ChainAdapter> = child.clone();
                let host = ChainHost::new(adapter, crate::drivers::CHAIN_LINK_KIND_NZXT_ARGB);
                child.install_chain_host(host);
            }
            if let Err(e) = child.initialize().await {
                log::warn!(
                    "plugin '{}' controller {} init failed: {e:#}",
                    self.plugin_id,
                    controller.index
                );
                child.close().await;
                continue;
            }
            out.push(child as Arc<dyn Device>);
        }
        out
    }
}

/// Synthesize the per-controller manifest an integration builds one child
/// `LuaDevice` from: a `Device`-kind clone of the (headless, capability-less)
/// root whose capability sections come from the enumerated controller. The
/// `zones` shorthand is promoted to an `rgb` section when no explicit one is
/// given.
fn child_manifest_for(root: &PluginManifest, ctrl: &DetectedController) -> PluginManifest {
    let mut m = root.clone();
    m.plugin_type = PluginKind::Device;
    m.rgb = ctrl.rgb.clone().or_else(|| {
        (!ctrl.zones.is_empty()).then(|| RgbManifest {
            zones: ctrl.rgb_descriptor().zones,
            native_effects: Vec::new(),
        })
    });
    m.fan = ctrl.fan.clone();
    m.sensor = ctrl.sensor.clone();
    m.lcd = ctrl.lcd.clone();
    m.dpi = ctrl.dpi.clone();
    m.choice = ctrl.choice.clone();
    m.range = ctrl.range.clone();
    m.boolean = ctrl.boolean.clone();
    m.action = ctrl.action.clone();
    m.battery = ctrl.battery.clone();
    m.connection = ctrl.connection.clone();
    m.equalizer = ctrl.equalizer.clone();
    m.pairing = ctrl.pairing.clone();
    m.onboard_profiles = ctrl.onboard_profiles.clone();
    m.key_remap = ctrl.key_remap.clone();
    m.chain = ctrl.chain.clone();
    m
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
        &self.controls.choice_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.controls.choices_wire()
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        self.controls.record_choice(key, selected)?;
        self.worker()?.choice_set(key, selected).await
    }
}

#[async_trait]
impl RangeCapability for LuaDevice {
    fn range_cache(&self) -> &RangeStateCache {
        &self.controls.range_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.controls.ranges_wire()
    }

    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        let value = self.controls.record_range(key, value)?;
        self.worker()?.range_set(key, value).await
    }
}

#[async_trait]
impl BooleanCapability for LuaDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        let mut live = self.worker()?.boolean_get().await?;
        self.controls.backfill_booleans(&mut live);
        Ok(live)
    }

    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        self.controls.bool_cache.record(key, value);
        self.worker()?.boolean_set(key, value).await
    }

    fn bool_cache(&self) -> Option<&BoolStateCache> {
        Some(&self.controls.bool_cache)
    }
}

#[async_trait]
impl ActionCapability for LuaDevice {
    async fn trigger_action(&self, key: &str) -> Result<()> {
        if !self.controls.has_action(key) {
            anyhow::bail!("unknown action key: {key}");
        }
        self.worker()?.action_trigger(key).await
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.controls.actions_wire()
    }
}

#[async_trait]
impl BatteryCapability for LuaDevice {
    async fn get_batteries(&self) -> Result<Vec<Battery>> {
        self.worker()?.battery_get().await
    }
}

#[async_trait]
impl ConnectionCapability for LuaDevice {
    async fn connection_status(&self) -> Option<ConnectionStatus> {
        self.worker().ok()?.connection_get().await.ok().flatten()
    }
}

#[async_trait]
impl EqualizerCapability for LuaDevice {
    async fn get_equalizer(&self) -> Result<Equalizer> {
        let eq = self.worker()?.equalizer_get().await?;
        *self.eq_cache.lock().unwrap() = Some(eq.clone());
        Ok(eq)
    }

    async fn set_eq_preset(&self, preset_index: usize) -> Result<()> {
        self.worker()?.equalizer_set_preset(preset_index).await
    }

    async fn set_eq_bands(&self, values: &[f32]) -> Result<()> {
        self.worker()?.equalizer_set_bands(values).await
    }

    fn current_state(&self) -> Option<Equalizer> {
        self.eq_cache.lock().unwrap().clone()
    }
}

#[async_trait]
impl PairingCapability for LuaDevice {
    async fn start_pairing(&self, timeout_secs: u8) -> Result<()> {
        self.worker()?.pairing_start(timeout_secs).await
    }

    async fn stop_pairing(&self) -> Result<()> {
        self.worker()?.pairing_stop().await
    }

    /// Runs the plugin's hardware-side unpair, but does not remove a live
    /// child `Device` from the registry — `LuaDevice` has no owned-child model
    /// for paired wireless slots (unlike the ARGB chain/accessory path). A
    /// plugin driving a receiver with real paired child devices needs that
    /// wired up as a follow-up; today `unpair` only clears the hardware slot.
    async fn unpair(&self, slot: u8) -> Result<Option<Arc<dyn Device>>> {
        self.worker()?.pairing_unpair(slot).await?;
        Ok(None)
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let status = self.worker().ok()?.pairing_status().await.ok()?;
        Some(DeviceCapability::Pairing(status))
    }
}

#[async_trait]
impl OnboardProfilesCapability for LuaDevice {
    async fn switch_profile(&self, slot: u8) -> Result<()> {
        self.worker()?.onboard_switch_profile(slot).await
    }

    async fn restore_profile(&self, slot: u8) -> Result<()> {
        self.worker()?.onboard_restore_profile(slot).await
    }

    async fn set_profile_enabled(&self, slot: u8, enabled: bool) -> Result<()> {
        self.worker()?
            .onboard_set_profile_enabled(slot, enabled)
            .await
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let profiles = self.worker().ok()?.onboard_profiles_get().await.ok()?;
        Some(DeviceCapability::OnboardProfiles(profiles))
    }
}

#[async_trait]
impl KeyRemapCapability for LuaDevice {
    async fn get_key_remap_status(&self) -> KeyRemapStatus {
        let mappings: Vec<ButtonMapping> = self
            .key_remap_mappings
            .lock()
            .unwrap()
            .values()
            .cloned()
            .collect();
        let host_mode_active = match &self.worker {
            Some(w) => w.key_remap_host_mode_active().await,
            None => false,
        };
        KeyRemapStatus {
            buttons: self.key_remap_buttons.clone(),
            mappings,
            requires_host_mode: self.key_remap_requires_host_mode,
            host_mode_active,
        }
    }

    async fn set_button_mapping(&self, mapping: ButtonMapping) -> Result<()> {
        self.worker()?
            .key_remap_set_mapping(mapping.clone())
            .await?;
        let mut cache = self.key_remap_mappings.lock().unwrap();
        if mapping.base == ButtonAction::Native && mapping.shifted == ButtonAction::Native {
            cache.remove(&mapping.cid);
        } else {
            cache.insert(mapping.cid, mapping);
        }
        Ok(())
    }

    async fn reset_button_mapping(&self, cid: u16) -> Result<()> {
        self.worker()?.key_remap_reset(cid).await?;
        self.key_remap_mappings.lock().unwrap().remove(&cid);
        Ok(())
    }

    async fn reset_all_button_mappings(&self) -> Result<()> {
        self.worker()?.key_remap_reset_all().await?;
        self.key_remap_mappings.lock().unwrap().clear();
        Ok(())
    }

    async fn default_mappings(&self) -> Vec<ButtonMapping> {
        self.key_remap_default_mappings.clone()
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
            index: None,
        }
    }

    /// Build a HID plugin device over a mock byte-stream transport.
    fn hid_device(id: &str, manifest: &PluginManifest, transport: Arc<dyn Transport>) -> LuaDevice {
        let spec = &manifest.devices[0];
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
          devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "M" } },
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
    async fn chain_advertises_controller_and_chain() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            transports = { hid = { report_size = 8 } },
            chain = { channels = { { id = "c1", name = "Port 1", max_leds = 30 } } },
        }"#;
        let manifest = super::super::parse_manifest(src, Path::new("chain.lua")).unwrap();
        let dev = hid_device("c-0", &manifest, Arc::new(MockTransport::empty()));
        let caps = dev.capabilities();
        assert!(caps
            .iter()
            .any(|c| matches!(c, CapabilityRef::Controller(_))));
        assert!(caps.iter().any(|c| matches!(c, CapabilityRef::Chain(_))));
    }

    #[test]
    fn device_only_advertises_no_capabilities() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
        }"#;
        let manifest = super::super::parse_manifest(src, Path::new("bare.lua")).unwrap();
        let dev = LuaDevice::device_only("d-0".into(), &manifest, &manifest.devices[0]);
        assert!(dev.capabilities().is_empty());
        // A device-type plugin reports its owner for scoped teardown, even
        // though it is not an integration (so `integration_id` stays `None`).
        assert_eq!(dev.owning_plugin_id(), Some(manifest.plugin_id.clone()));
        assert_eq!(dev.integration_id(), None);
    }

    #[test]
    fn controls_wire_and_validate_round_trip() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            choice = { choices = { { key = "mode", label = "Mode",
                options = { { id = "a", label = "A" }, { id = "b", label = "B" } }, default = 0 } } },
            range = { ranges = { { key = "hz", label = "Hz", min = 100, max = 1000, default = 500 } } },
            boolean = { booleans = { { key = "snap", label = "Angle Snap", category = "Mouse" } } },
            action = { actions = { { key = "cal", label = "Calibrate" } } },
        }"#;
        let m = super::super::parse_manifest(src, Path::new("controls.lua")).unwrap();
        let controls = Controls::from_manifest(&m);

        // Empty groups yield no wire capability.
        assert!(Controls::default().choices_wire().is_none());
        assert!(Controls::default().ranges_wire().is_none());
        assert!(Controls::default().actions_wire().is_none());

        // Choice: cache override shows through; bad key/index rejected.
        controls.record_choice("mode", 1).unwrap();
        let Some(DeviceCapability::Choice(choices)) = controls.choices_wire() else {
            panic!("expected choice capability");
        };
        assert_eq!(choices[0].selected, 1);
        assert!(controls.record_choice("mode", 2).is_err());
        assert!(controls.record_choice("nope", 0).is_err());

        // Range: value clamps to declared bounds; bad key rejected.
        assert_eq!(controls.record_range("hz", 5000).unwrap(), 1000);
        assert_eq!(controls.record_range("hz", 0).unwrap(), 100);
        assert!(controls.record_range("nope", 0).is_err());
        let Some(DeviceCapability::Range(ranges)) = controls.ranges_wire() else {
            panic!("expected range capability");
        };
        assert_eq!(ranges[0].value, 100, "last recorded value wins");

        // Boolean backfill fills empty label/category from the manifest decl.
        let mut live = vec![Boolean {
            key: "snap".into(),
            value: true,
            label: String::new(),
            read_only: false,
            category: String::new(),
            visible_when: None,
        }];
        controls.backfill_booleans(&mut live);
        assert_eq!(live[0].label, "Angle Snap");
        assert_eq!(live[0].category, "Mouse");

        // Action existence gate.
        assert!(controls.has_action("cal"));
        assert!(!controls.has_action("nope"));
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
              devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "M" } },
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
              devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "M" } },
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

    // ── Range / Boolean / Action / Battery / Connection / Equalizer ────────

    const CONTROLS_SCRIPT: &str = r#"
        return {
          devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "M" } },
          range = { ranges = { { key = "poll_hz", label = "Poll Rate", min = 125, max = 1000, default = 500 } } },
          boolean = { booleans = { { key = "sniper", label = "Sniper" } } },
          action = { actions = { { key = "calibrate", label = "Calibrate" } } },
          battery = {},
          connection = {},
          equalizer = {},

          set_range = function(dev, key, value)
            dev.transport:write(string.char(0xA0, value & 0xFF))
          end,
          get_booleans = function(dev)
            return { { key = "sniper", value = true } }
          end,
          set_boolean = function(dev, key, value)
            dev.transport:write(string.char(0xB0, value and 1 or 0))
          end,
          trigger_action = function(dev, key)
            dev.transport:write(string.char(0xC0))
          end,
          get_batteries = function(dev)
            return { { key = "main", label = "Battery", level = 77, status = "discharging" } }
          end,
          connection_status = function(dev)
            return { connection_type = "wireless" }
          end,
          get_equalizer = function(dev)
            return { presets = {}, selected_preset = 0, bands = {}, bands_editable = false }
          end,
          set_eq_preset = function(dev, preset)
            dev.transport:write(string.char(0xD0, preset))
          end,
          set_eq_bands = function(dev, values)
            dev.transport:write(string.char(0xD1, #values))
          end,
        }
    "#;

    fn controls_device(transport: Arc<dyn Transport>) -> LuaDevice {
        let manifest =
            super::super::parse_manifest(CONTROLS_SCRIPT, Path::new("controls.lua")).unwrap();
        hid_device("controls-0", &manifest, transport)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advertises_all_declared_controls() {
        use crate::drivers::Device;
        let dev = controls_device(Arc::new(MockTransport::empty()));
        assert!(dev.as_range().is_some());
        assert!(dev.as_boolean().is_some());
        assert!(dev.as_action().is_some());
        assert!(dev.as_battery().is_some());
        assert!(dev.as_equalizer().is_some());
        assert!(dev
            .capabilities()
            .iter()
            .any(|c| matches!(c, CapabilityRef::Connection(_))));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn range_set_clamps_and_caches_and_reaches_script() {
        use crate::drivers::RangeCapability;
        let mock = Arc::new(MockTransport::empty());
        let dev = controls_device(mock.clone());
        dev.set_range("poll_hz", 5000).await.unwrap(); // above max, clamped
        assert_eq!(*mock.written.lock().await, vec![vec![0xA0, 232]]); // 1000 & 0xFF
        assert_eq!(dev.range_cache().get("poll_hz"), Some(1000));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_seeds_range_and_choice_caches_from_reported_values() {
        // A plugin whose `initialize` reads live hardware values reports them
        // back; the host seeds its range/choice caches so the UI shows the
        // device state instead of the manifest defaults.
        use crate::drivers::{ChoiceCapability, RangeCapability};
        const SEED_SCRIPT: &str = r#"
            return {
              devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "M" } },
              range = { ranges = { { key = "hz", label = "Hz", min = 0, max = 1000, default = 500 } } },
              choice = { choices = { { key = "mode", label = "Mode", default = 0,
                options = { { id = "0", label = "A" }, { id = "1", label = "B" } } } } },
              initialize = function(dev)
                return { ok = true, ranges = { hz = 250 }, choices = { mode = 1 } }
              end,
              set_range = function(dev, key, value) end,
              set_choice = function(dev, key, selected) end,
            }
        "#;
        let manifest = super::super::parse_manifest(SEED_SCRIPT, Path::new("seed.lua")).unwrap();
        let dev = hid_device("seed-0", &manifest, Arc::new(MockTransport::empty()));
        // Before initialize: caches empty, so the wire value falls back to default.
        assert_eq!(dev.range_cache().get("hz"), None);
        dev.initialize().await.unwrap();
        assert_eq!(dev.range_cache().get("hz"), Some(250));
        assert_eq!(dev.choice_cache().get("mode"), Some(1));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn boolean_round_trips_value_and_backfills_label() {
        use crate::drivers::BooleanCapability;
        let mock = Arc::new(MockTransport::empty());
        let dev = controls_device(mock.clone());
        let booleans = dev.get_booleans().await.unwrap();
        assert_eq!(booleans.len(), 1);
        assert!(booleans[0].value);
        assert_eq!(booleans[0].label, "Sniper");
        dev.set_boolean("sniper", false).await.unwrap();
        assert_eq!(*mock.written.lock().await, vec![vec![0xB0, 0]]);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn action_trigger_reaches_script_and_rejects_unknown_key() {
        use crate::drivers::ActionCapability;
        let mock = Arc::new(MockTransport::empty());
        let dev = controls_device(mock.clone());
        dev.trigger_action("calibrate").await.unwrap();
        assert_eq!(*mock.written.lock().await, vec![vec![0xC0]]);
        assert!(dev.trigger_action("unknown").await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn battery_and_connection_deserialize_from_lua() {
        use crate::drivers::{BatteryCapability, ConnectionCapability};
        let dev = controls_device(Arc::new(MockTransport::empty()));
        let batteries = dev.get_batteries().await.unwrap();
        assert_eq!(batteries[0].level, 77);
        let status = dev.connection_status().await.unwrap();
        assert_eq!(
            status.connection_type,
            halod_shared::types::ConnectionType::Wireless
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn equalizer_caches_last_read_for_current_state() {
        use crate::drivers::EqualizerCapability;
        let mock = Arc::new(MockTransport::empty());
        let dev = controls_device(mock.clone());
        assert!(
            EqualizerCapability::current_state(&dev).is_none(),
            "nothing read yet"
        );
        let eq = dev.get_equalizer().await.unwrap();
        assert_eq!(eq.selected_preset, 0);
        assert!(
            EqualizerCapability::current_state(&dev).is_some(),
            "cached after get_equalizer"
        );
        dev.set_eq_preset(2).await.unwrap();
        dev.set_eq_bands(&[1.0, 2.0, 3.0]).await.unwrap();
        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0xD0, 2], vec![0xD1, 3]]
        );
    }

    // ── Pairing / OnboardProfiles / KeyRemap ────────────────────────────

    const RECEIVER_SCRIPT: &str = r#"
        return {
          devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "Receiver" } },
          pairing = {},
          onboard_profiles = {},
          key_remap = {
            buttons = { { cid = 1, label = "Left", divertable = true, group = 0 } },
            requires_host_mode = true,
          },

          start_pairing = function(dev, timeout_secs)
            dev.transport:write(string.char(0xE0, timeout_secs))
          end,
          stop_pairing = function(dev)
            dev.transport:write(string.char(0xE1))
          end,
          unpair = function(dev, slot)
            dev.transport:write(string.char(0xE2, slot))
          end,
          pairing_status = function(dev)
            return { state = "idle", max_slots = 1, slots = {} }
          end,
          switch_profile = function(dev, slot)
            dev.transport:write(string.char(0xF0, slot))
          end,
          restore_profile = function(dev, slot)
            dev.transport:write(string.char(0xF1, slot))
          end,
          set_profile_enabled = function(dev, slot, enabled)
            dev.transport:write(string.char(0xF2, slot, enabled and 1 or 0))
          end,
          onboard_profiles_status = function(dev)
            return { active_slot = 1, slots = { { index = 1, enabled = true, active = true, has_rom_default = true } } }
          end,
          set_button_mapping = function(dev, mapping)
            dev.transport:write(string.char(0x90, mapping.cid))
          end,
          reset_button_mapping = function(dev, cid)
            dev.transport:write(string.char(0x91, cid))
          end,
          reset_all_button_mappings = function(dev)
            dev.transport:write(string.char(0x92))
          end,
          key_remap_host_mode = function(dev)
            return true
          end,
        }
    "#;

    fn receiver_device(transport: Arc<dyn Transport>) -> LuaDevice {
        let manifest =
            super::super::parse_manifest(RECEIVER_SCRIPT, Path::new("receiver.lua")).unwrap();
        hid_device("receiver-0", &manifest, transport)
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn advertises_pairing_onboard_and_key_remap() {
        let dev = receiver_device(Arc::new(MockTransport::empty()));
        assert!(dev.as_pairing().is_some());
        assert!(dev.as_onboard_profiles().is_some());
        assert!(dev.as_key_remap().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn pairing_start_stop_unpair_reach_script() {
        use crate::drivers::PairingCapability;
        let mock = Arc::new(MockTransport::empty());
        let dev = receiver_device(mock.clone());
        dev.start_pairing(30).await.unwrap();
        dev.stop_pairing().await.unwrap();
        let removed = dev.unpair(1).await.unwrap();
        assert!(removed.is_none(), "no owned child to remove");
        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0xE0, 30], vec![0xE1], vec![0xE2, 1]]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn onboard_profiles_switch_restore_enable_reach_script() {
        use crate::drivers::OnboardProfilesCapability;
        let mock = Arc::new(MockTransport::empty());
        let dev = receiver_device(mock.clone());
        dev.switch_profile(2).await.unwrap();
        dev.restore_profile(3).await.unwrap();
        dev.set_profile_enabled(4, true).await.unwrap();
        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0xF0, 2], vec![0xF1, 3], vec![0xF2, 4, 1]]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn key_remap_round_trips_mapping_and_reports_status() {
        use crate::drivers::KeyRemapCapability;
        use halod_shared::types::{ButtonAction, ButtonMapping, MouseBtn};
        let mock = Arc::new(MockTransport::empty());
        let dev = receiver_device(mock.clone());

        let status = dev.get_key_remap_status().await;
        assert_eq!(status.buttons.len(), 1);
        assert!(status.requires_host_mode);
        assert!(status.host_mode_active);
        assert!(status.mappings.is_empty());

        dev.set_button_mapping(ButtonMapping {
            cid: 1,
            base: ButtonAction::MouseButton {
                btn: MouseBtn::Right,
            },
            shifted: ButtonAction::Native,
        })
        .await
        .unwrap();
        let status = dev.get_key_remap_status().await;
        assert_eq!(status.mappings.len(), 1);
        assert_eq!(status.mappings[0].cid, 1);

        dev.reset_button_mapping(1).await.unwrap();
        assert!(dev.get_key_remap_status().await.mappings.is_empty());

        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0x90, 1], vec![0x91, 1]]
        );
    }

    // ── Chain / children ────────────────────────────────────────────────

    const CHAIN_SCRIPT: &str = r#"
        return {
          devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "NZXT", model = "Kraken" } },
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
        let spec = &manifest.devices[0];
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

    const INTEGRATION_SCRIPT: &str = r#"
        return {
          type = "integration",
          identity = { name = "Test Hub" },
          config = { fields = { { key = "host", label = "Host" }, { key = "port", label = "Port" } } },
          transports = { tcp = {} },

          enumerate_controllers = function(dev)
            return {
              { index = 0, name = "Keyboard", zones = {
                  { id = "main", name = "Main", topology = "linear", led_count = 4 },
              } },
              { index = 1, name = "Mobo", zones = {
                  { id = "aux", name = "Aux", topology = "linear", led_count = 2 },
                  { id = "z2", name = "Zone 2", topology = "linear", led_count = 3 },
              } },
            }
          end,

          -------------------------------
          -- Per-controller callbacks; dev.match.index routes to the right
          -- controller. The same script source is shared by every child worker.

          write_frame = function(dev, zone_id, colors)
            local bytes = { dev.match.index, string.byte(zone_id, 1) }
            for _, c in ipairs(colors) do
              bytes[#bytes+1] = c.r
              bytes[#bytes+1] = c.g
              bytes[#bytes+1] = c.b
            end
            dev.transport:write(string.char(table.unpack(bytes)))
          end,

          apply = function(dev, state)
            local color = (state.mode == "static") and state.color
            if not color then return end
            local idx = dev.match.index
            -- Zone layout per controller (mirrors enumerate_controllers).
            local zones = idx == 0 and {
              { id = "main", n = 4 },
            } or {
              { id = "aux", n = 2 },
              { id = "z2", n = 3 },
            }
            for _, z in ipairs(zones) do
              local cs = {}
              for i = 1, z.n do cs[i] = color end
              -- Reuse the write_frame wire format so tests see the same bytes.
              local bytes = { idx, string.byte(z.id, 1) }
              for _, c in ipairs(cs) do
                bytes[#bytes+1] = c.r
                bytes[#bytes+1] = c.g
                bytes[#bytes+1] = c.b
              end
              dev.transport:write(string.char(table.unpack(bytes)))
            end
          end,
        }
    "#;

    fn integration_device(transport: Arc<dyn Transport>) -> Arc<LuaDevice> {
        let manifest =
            super::super::parse_manifest(INTEGRATION_SCRIPT, Path::new("integ.lua")).unwrap();
        // Every controller child spawns its own worker over the same mock
        // transport (a real integration opens a fresh socket per controller).
        let child_transport = transport.clone();
        let child_worker: ChildWorkerFactory = Arc::new(move |_index| {
            Ok(PluginIo::Stream {
                transport: child_transport.clone(),
                bulk: None,
            })
        });
        Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::integration_root(
                "integ-0".into(),
                &manifest,
                PluginIo::Stream {
                    transport,
                    bulk: None,
                },
                child_worker,
                tokio::runtime::Handle::current(),
            );
            d.set_self_ref(weak.clone());
            d
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_root_advertises_controller_capability() {
        // Regression: `register_device_and_children` (the real registration
        // path) finds children via `device.as_controller()`, not by calling
        // `Controller::discover_children` directly on the concrete type — a
        // plugin whose `capabilities()` forgets to advertise `Controller`
        // silently never gets its children discovered at all, even though
        // the trait impl itself is correct. Guard the capability list, not
        // just the trait method.
        let dev = integration_device(Arc::new(MockTransport::empty()));
        assert!(dev.as_controller().is_some());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_root_marks_itself_on_the_wire_but_not_its_children() {
        // The GUI hides an integration's root from Home/sidebar via this
        // marker (see `Device::integration_id`) — a device plugin and every
        // `IntegrationLeaf` child must never set it, or they'd disappear too.
        let dev = integration_device(Arc::new(MockTransport::empty()));
        assert_eq!(dev.integration_id(), Some("integ".to_string()));
        // `owning_plugin_id` is the *scoped teardown* selector and is broader
        // than `integration_id`: it is set on every plugin device, so the
        // generic plugin toggle can tear an integration plugin down too.
        assert_eq!(dev.owning_plugin_id(), Some("integ".to_string()));
        let wire = dev.serialize().await;
        assert_eq!(wire.integration_id, Some("integ".to_string()));

        let children = dev.as_controller().unwrap().discover_children().await;
        for child in &children {
            assert_eq!(child.integration_id(), None);
            assert_eq!(child.serialize().await.integration_id, None);
        }
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_root_discovers_one_child_per_controller() {
        let dev = integration_device(Arc::new(MockTransport::empty()));

        let children = dev.as_controller().unwrap().discover_children().await;
        assert_eq!(children.len(), 2);
        assert_eq!(children[0].id(), "integ-0_ctrl_0");
        assert_eq!(children[0].name(), "Keyboard");
        assert_eq!(children[1].id(), "integ-0_ctrl_1");
        assert_eq!(children[1].name(), "Mobo");

        let mobo_rgb = children[1].as_rgb().expect("has rgb");
        assert_eq!(mobo_rgb.descriptor().zones.len(), 2);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_leaf_write_frame_routes_to_the_parent_with_its_own_index() {
        let mock = Arc::new(MockTransport::empty());
        let dev = integration_device(mock.clone());
        let children = dev.as_controller().unwrap().discover_children().await;

        let keyboard_rgb = children[0].as_rgb().expect("has rgb");
        keyboard_rgb
            .write_frame("main", &[RgbColor { r: 9, g: 8, b: 7 }; 4])
            .await
            .unwrap();
        // index 0, zone 'main' -> byte('m') = 109, then 4x(9,8,7).
        assert_eq!(
            *mock.written.lock().await.last().unwrap(),
            vec![0, 109, 9, 8, 7, 9, 8, 7, 9, 8, 7, 9, 8, 7]
        );

        let mobo_rgb = children[1].as_rgb().expect("has rgb");
        mobo_rgb
            .write_frame("z2", &[RgbColor { r: 1, g: 1, b: 1 }; 2])
            .await
            .unwrap();
        // index 1, zone 'z2' -> byte('z') = 122, then 2x(1,1,1).
        assert_eq!(
            *mock.written.lock().await.last().unwrap(),
            vec![1, 122, 1, 1, 1, 1, 1, 1]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_leaf_write_frame_rejects_an_unknown_zone() {
        let dev = integration_device(Arc::new(MockTransport::empty()));
        let children = dev.as_controller().unwrap().discover_children().await;
        let rgb = children[0].as_rgb().expect("has rgb");
        assert!(rgb.write_frame("nope", &[]).await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_leaf_apply_static_broadcasts_to_every_zone() {
        let mock = Arc::new(MockTransport::empty());
        let dev = integration_device(mock.clone());
        let children = dev.as_controller().unwrap().discover_children().await;
        let mobo_rgb = children[1].as_rgb().expect("has rgb");

        mobo_rgb
            .apply(RgbState::Static {
                color: RgbColor { r: 3, g: 3, b: 3 },
            })
            .await
            .unwrap();

        let written = mock.written.lock().await;
        assert_eq!(written.len(), 2, "one write per zone");
        // 'aux' has 2 LEDs; 'z2' has 3. index 1, byte('a')=97 / byte('z')=122.
        assert!(written.iter().any(|w| *w == vec![1, 97, 3, 3, 3, 3, 3, 3]));
        assert!(written
            .iter()
            .any(|w| *w == vec![1, 122, 3, 3, 3, 3, 3, 3, 3, 3, 3]));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_leaf_close_shuts_down_only_its_own_worker() {
        // Each controller owns its own worker VM (they only share the root
        // connection), so closing one leaf must not disturb its siblings.
        let dev = integration_device(Arc::new(MockTransport::empty()));
        let children = dev.as_controller().unwrap().discover_children().await;

        children[0].close().await;
        // The closed child's worker is gone: a write now errors.
        let keyboard = children[0].as_rgb().expect("has rgb");
        assert!(keyboard
            .write_frame("main", &[RgbColor { r: 1, g: 1, b: 1 }; 4])
            .await
            .is_err());

        // A sibling still has a live worker of its own.
        let mobo = children[1].as_rgb().expect("has rgb");
        assert!(mobo
            .write_frame("z2", &[RgbColor { r: 2, g: 2, b: 2 }; 3])
            .await
            .is_ok());
    }

    // ---- multi-capability integration fixture ----------------------------------------

    const INTEGRATION_MULTI_SCRIPT: &str = r#"
        return {
          type = "integration",
          identity = { name = "Multi Hub" },
          config = { fields = { { key = "host", label = "Host" } } },
          transports = { tcp = {} },

          enumerate_controllers = function(dev)
            return {
              { index = 0, name = "Ctrl0",
                zones = { { id = "leds", name = "LEDs", topology = "linear", led_count = 2 } },
                fan = { channel = 0 },
                sensor = {},
              },
              { index = 1, name = "Ctrl1",
                zones = { { id = "ring", name = "Ring", topology = "linear", led_count = 4 } },
                battery = {},
              },
            }
          end,

          write_frame = function(dev, zone_id, colors)
            local bytes = { dev.match.index, string.byte(zone_id, 1) }
            for _, c in ipairs(colors) do
              bytes[#bytes+1] = c.r; bytes[#bytes+1] = c.g; bytes[#bytes+1] = c.b
            end
            dev.transport:write(string.char(table.unpack(bytes)))
          end,

          set_duty = function(dev, duty)
            dev.transport:write(string.char(dev.match.index, duty))
          end,

          get_duty = function(dev)
            return dev.match.index == 0 and 60 or 0
          end,

          get_sensors = function(dev)
            return { { id = "temp", name = "Temperature", value = 42 + dev.match.index, unit = "celsius" } }
          end,

          get_batteries = function(dev)
            return { { id = "bat0", level = 80 } }
          end,
        }
    "#;

    fn integration_multi_device() -> Arc<LuaDevice> {
        let manifest =
            super::super::parse_manifest(INTEGRATION_MULTI_SCRIPT, Path::new("multi.lua")).unwrap();
        let transport = Arc::new(MockTransport::empty());
        let child_transport = transport.clone();
        let child_worker: ChildWorkerFactory = Arc::new(move |_index| {
            Ok(PluginIo::Stream {
                transport: child_transport.clone(),
                bulk: None,
            })
        });
        Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::integration_root(
                "multi-0".into(),
                &manifest,
                PluginIo::Stream {
                    transport,
                    bulk: None,
                },
                child_worker,
                tokio::runtime::Handle::current(),
            );
            d.set_self_ref(weak.clone());
            d
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_child_reports_all_declared_capabilities() {
        let dev = integration_multi_device();
        let children = dev.as_controller().unwrap().discover_children().await;
        assert_eq!(children.len(), 2);

        // Ctrl0: rgb (zones shorthand), fan, sensor
        let c0 = &children[0];
        assert!(c0.as_rgb().is_some());
        assert!(c0.as_fan().is_some());
        assert!(c0.as_sensor_capability().is_some());
        assert!(c0.as_battery().is_none());
        assert_eq!(c0.integration_id(), None);

        // Ctrl1: rgb (zones shorthand), battery
        let c1 = &children[1];
        assert!(c1.as_rgb().is_some());
        assert!(c1.as_battery().is_some());
        assert!(c1.as_fan().is_none());
        assert!(c1.as_sensor_capability().is_none());
        assert_eq!(c1.integration_id(), None);

        // Root stays hidden.
        assert_eq!(dev.integration_id(), Some("multi".to_string()));
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn integration_child_fan_and_sensor_route_with_correct_index() {
        let mock = Arc::new(MockTransport::empty());
        let transport = mock.clone();
        let manifest =
            super::super::parse_manifest(INTEGRATION_MULTI_SCRIPT, Path::new("multi.lua")).unwrap();
        let child_transport = transport.clone();
        let child_worker: ChildWorkerFactory = Arc::new(move |_index| {
            Ok(PluginIo::Stream {
                transport: child_transport.clone(),
                bulk: None,
            })
        });
        let dev = Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::integration_root(
                "multi-0".into(),
                &manifest,
                PluginIo::Stream {
                    transport,
                    bulk: None,
                },
                child_worker,
                tokio::runtime::Handle::current(),
            );
            d.set_self_ref(weak.clone());
            d
        });
        let children = dev.as_controller().unwrap().discover_children().await;

        // Ctrl0 (index 0) has a fan: set_duty writes [0, duty].
        let fan = children[0].as_fan().expect("ctrl0 has fan");
        fan.set_duty(60).await.unwrap();
        assert_eq!(*mock.written.lock().await.last().unwrap(), vec![0, 60]);
        assert_eq!(fan.get_duty().await.unwrap(), 60);

        // Ctrl1 (index 1) sensors return 42 + 1 = 43.
        let sensor = children[0]
            .as_sensor_capability()
            .expect("ctrl0 has sensor");
        let sensors: Vec<Sensor> = sensor.get_sensors().await.unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].value, 42.0);
    }

    // Holds the process-wide test lock across `.await` to serialize the shared
    // notification sink against other plugin-device tests; nothing awaited here
    // re-takes that lock, so it can't deadlock.
    #[allow(clippy::await_holding_lock)]
    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_errors_surface_once_per_failure_episode() {
        use crate::config::Config;
        use crate::ipc::ClientHandle;
        use crate::state::AppState;

        // The runtime-error sink and dedup set are process-wide globals.
        let _guard = super::super::TEST_GLOBALS_LOCK
            .lock()
            .unwrap_or_else(|e| e.into_inner());

        // `apply` fails unless the requested colour is black — a per-call toggle
        // so one device can walk fail → fail → recover → fail.
        let src = r#"return {
            devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "T", model = "M" } },
            transports = { hid = { report_size = 8 } },
            rgb = { zones = { { id = "z", name = "Z", topology = { type = "linear" },
                leds = { { id = 0, x = 0.0, y = 0.0 } } } } },
            apply = function(dev, state)
                if state.color.r == 0 then return end
                error("boom")
            end,
        }"#;
        let manifest = super::super::parse_manifest(src, Path::new("err.lua")).unwrap();
        let dev = hid_device("err-dev", &manifest, Arc::new(MockTransport::empty()));

        let app = Arc::new(AppState::new(Config::default()));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<Vec<u8>>>(16);
        app.clients.lock().await.push(ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        });
        super::super::set_notification_sink(&app);

        let fail = || RgbState::Static {
            color: RgbColor { r: 1, g: 0, b: 0 },
        };
        let ok = || RgbState::Static {
            color: RgbColor { r: 0, g: 0, b: 0 },
        };
        let code_of = |frame: &[u8]| -> serde_json::Value {
            serde_json::from_slice::<serde_json::Value>(&frame[5..]).unwrap()["data"]["code"]
                .clone()
        };

        // First failure surfaces exactly one plugin_runtime_error toast.
        assert!(dev.apply(fail()).await.is_err());
        let frame = rx.try_recv().expect("first failure surfaced");
        assert_eq!(code_of(&frame), "plugin_runtime_error");

        // A second consecutive failure is deduped — no new toast.
        assert!(dev.apply(fail()).await.is_err());
        assert!(
            rx.try_recv().is_err(),
            "consecutive failure must not re-toast"
        );

        // A success clears the dedup flag (and never toasts)...
        dev.apply(ok()).await.unwrap();
        assert!(rx.try_recv().is_err(), "success must not toast");

        // ...so the next failure alerts again.
        assert!(dev.apply(fail()).await.is_err());
        assert!(
            rx.try_recv().is_ok(),
            "a failure after recovery must re-toast"
        );
    }
}
