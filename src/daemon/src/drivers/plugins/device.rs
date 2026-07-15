// SPDX-License-Identifier: GPL-3.0-or-later
//! The generic device a plugin instantiates. It sits behind the same `Device`
//! seam as every native driver and forwards capability calls into the per-device
//! Lua worker. The manifest defines the maximum capability set; `initialize`
//! may narrow it to the subset supported by one physical device.

use std::sync::atomic::{AtomicBool, AtomicU8, Ordering};
use std::sync::{Arc, Mutex, OnceLock, RwLock, Weak};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use halod_shared::keyboard::{KeyId, KeyVariant, KeyboardLayoutStatus, VisualKey};
use halod_shared::types::{
    Action, Battery, Boolean, ButtonAction, ButtonDescriptor, ButtonMapping, Choice,
    ConnectionStatus, DeviceCapability, DeviceType, DpiMode, DpiStatus, Equalizer, KeyRemapStatus,
    KeyboardFormFactor, KeyboardLayout, LcdDescriptor, NativeEffect, Permission, PluginKind, Range,
    RgbColor, RgbDescriptor, RgbState, RgbZone, ScreenRotation, ScreenShape, Sensor,
    WriteRateStatus,
};
use halod_shared::zone_transform::build_permutation;

use crate::drivers::chain::{ChainAdapter, ChainHost, ChainHub, ChannelDescriptor};
use crate::drivers::{
    ActionCapability, BatteryCapability, BoolStateCache, BooleanCapability, CapabilityRef,
    ChainCapability, ChoiceCapability, ChoiceStateCache, ConnectionCapability, Controller, Device,
    DpiCapability, EqualizerCapability, FanCapability, FanHub, FanStateSlot, KeyRemapCapability,
    KeyboardLayoutCapability, KeyboardLayoutSlot, LcdCapability, LcdStateSlot,
    OnboardProfilesCapability, PairingCapability, RangeCapability, RangeStateCache, RgbCapability,
    RgbStateSlot, SensorCapability, VisibilitySlot,
};

use super::chain_leaf::ChainLeaf;
use super::manifest::{
    topology_from, AccessoryManifest, ActionDef, BooleanDef, ChoiceDef, DeviceSpec, PluginManifest,
    RangeDef, RgbManifest,
};
use super::transport::PluginIo;
use super::worker::{
    DetectedController, DevMatch, InitControls, InitDpi, InitKeyboard, InitKeyboardVariant,
    InitLcd, InitZone, PluginHandle,
};
use std::collections::{HashMap, HashSet};

/// Host-side DPI step-cycle state (the plugin only writes the chosen value).
struct DpiState {
    steps: Vec<u16>,
    available_dpis: Vec<u16>,
    index: usize,
    current: u16,
}

#[derive(Clone)]
struct DpiConfig {
    min: u16,
    max: u16,
    mode: DpiMode,
    mode_control: Option<String>,
}

#[derive(Clone, Default)]
struct KeyRemapDescriptor {
    buttons: Vec<ButtonDescriptor>,
    requires_host_mode: bool,
    default_mappings: Vec<ButtonMapping>,
}
use crate::drivers::vendors::generic::devices::common::{linear_rgb_zone, ring_led_positions};
use halod_shared::types::ZoneTopology;

/// Yields the transport one integration child drives its worker over. The `u32`
/// is the controller index. Injectable so tests can back it with a mock
/// transport instead of a real socket. Every controller shares the root's single
/// connection, so their frame writes serialise behind one socket lock and stay
/// in phase (see `integration_scan`).
/// The capability sections a manifest can declare. Stored as `caps` on the
/// device so `capabilities()` reads a single list instead of one boolean per
/// kind; the mapping to `CapabilityRef` lives in `capabilities()`.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
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
    KeyboardLayout,
    Chain,
}

/// Typed runtime capabilities permitted by the inert catalog. Descriptors and
/// initial values still come from `initialize`; the catalog never supplies
/// static runtime sections.
fn declared_caps(manifest: &PluginManifest) -> Vec<Cap> {
    let mut caps = Vec::new();
    let mut push = |name: &str, cap: Cap| {
        if manifest
            .capabilities
            .iter()
            .any(|declared| declared == name)
        {
            caps.push(cap);
        }
    };
    push("rgb", Cap::Rgb);
    push("fan", Cap::Fan);
    push("sensors", Cap::Sensor);
    push("lcd", Cap::Lcd);
    push("dpi", Cap::Dpi);
    push("controls", Cap::Choice);
    push("controls", Cap::Range);
    push("controls", Cap::Boolean);
    push("controls", Cap::Action);
    push("battery", Cap::Battery);
    push("connection", Cap::Connection);
    push("equalizer", Cap::Equalizer);
    push("pairing", Cap::Pairing);
    push("onboard_profiles", Cap::OnboardProfiles);
    push("key_remap", Cap::KeyRemap);
    push("keyboard_layout", Cap::KeyboardLayout);
    push("chain", Cap::Chain);
    caps
}

fn caps_named(names: &[String]) -> Vec<Cap> {
    let mut caps = Vec::new();
    let mut add = |cap| {
        if !caps.contains(&cap) {
            caps.push(cap);
        }
    };
    for name in names {
        match name.as_str() {
            "rgb" => add(Cap::Rgb),
            "fan" => add(Cap::Fan),
            "sensors" => add(Cap::Sensor),
            "lcd" => add(Cap::Lcd),
            "dpi" => add(Cap::Dpi),
            "controls" => {
                add(Cap::Choice);
                add(Cap::Range);
                add(Cap::Boolean);
                add(Cap::Action);
            }
            "battery" => add(Cap::Battery),
            "connection" => add(Cap::Connection),
            "equalizer" => add(Cap::Equalizer),
            "pairing" => add(Cap::Pairing),
            "onboard_profiles" => add(Cap::OnboardProfiles),
            "key_remap" => add(Cap::KeyRemap),
            "keyboard_layout" => add(Cap::KeyboardLayout),
            "chain" => add(Cap::Chain),
            other => log::warn!("Lua initialize returned unknown capability '{other}'"),
        }
    }
    caps
}

fn needs_status_poll(caps: &[Cap]) -> bool {
    StatusPollCaps::from_caps(caps).any()
}

#[derive(Default, Clone)]
struct FanSample {
    duty: Option<u8>,
    rpm: Option<u32>,
}

#[derive(Clone, Copy, Default)]
struct StatusPollCaps {
    sensor: bool,
    fan: bool,
    boolean: bool,
    battery: bool,
    connection: bool,
    equalizer: bool,
    always: bool,
}

impl StatusPollCaps {
    fn from_caps(caps: &[Cap]) -> Self {
        Self {
            sensor: caps.contains(&Cap::Sensor),
            fan: caps.contains(&Cap::Fan),
            boolean: caps.contains(&Cap::Boolean),
            battery: caps.contains(&Cap::Battery),
            connection: caps.contains(&Cap::Connection),
            equalizer: caps.contains(&Cap::Equalizer),
            always: caps
                .iter()
                .any(|cap| matches!(cap, Cap::KeyRemap | Cap::Chain)),
        }
    }

    fn any(self) -> bool {
        self.sensor
            || self.fan
            || self.boolean
            || self.battery
            || self.connection
            || self.equalizer
            || self.always
    }
}

/// Sample dynamic status through the worker into the host caches that
/// serialization reads, keeping fresh hardware I/O off the serialize path.
/// Runs only on the status-poll cadence; a failed read keeps the last value.
async fn sample_status(
    worker: &PluginHandle,
    caps: StatusPollCaps,
    sensor_cache: &Mutex<Vec<Sensor>>,
    fan_cache: &Mutex<FanSample>,
    boolean_cache: &Mutex<Vec<Boolean>>,
    battery_cache: &Mutex<Vec<Battery>>,
    connection_cache: &Mutex<Option<ConnectionStatus>>,
    eq_cache: &Mutex<Option<Equalizer>>,
) {
    if caps.sensor {
        if let Ok(sensors) = worker.get_sensors().await {
            *sensor_cache.lock().unwrap() = sensors;
        }
    }
    if caps.fan {
        if let Ok(duty) = worker.fan_get_duty().await {
            fan_cache.lock().unwrap().duty = Some(duty);
        }
        fan_cache.lock().unwrap().rpm = worker.fan_get_rpm().await;
    }
    if caps.boolean {
        if let Ok(booleans) = worker.boolean_get().await {
            *boolean_cache.lock().unwrap() = booleans;
        }
    }
    if caps.battery {
        if let Ok(batteries) = worker.battery_get().await {
            *battery_cache.lock().unwrap() = batteries;
        }
    }
    if caps.connection {
        if let Ok(connection) = worker.connection_get().await {
            *connection_cache.lock().unwrap() = connection;
        }
    }
    if caps.equalizer {
        if let Ok(equalizer) = worker.equalizer_get().await {
            *eq_cache.lock().unwrap() = Some(equalizer);
        }
    }
}

#[cfg(test)]
mod status_poll_tests {
    use super::{needs_status_poll, Cap};

    #[test]
    fn chain_root_polls_for_child_fan_telemetry() {
        assert!(needs_status_poll(&[Cap::Chain]));
    }
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

/// Shared shape of the `*_wire` snapshots: `None` when no controls are declared,
/// else map each declared control to its wire type and wrap the batch in the
/// matching [`DeviceCapability`] variant.
fn wire_group<T, U>(
    items: &[T],
    wrap: impl Fn(Vec<U>) -> DeviceCapability,
    f: impl Fn(&T) -> U,
) -> Option<DeviceCapability> {
    if items.is_empty() {
        return None;
    }
    Some(wrap(items.iter().map(f).collect()))
}

impl Controls {
    /// Legacy fixture adapter. Production package loading never calls this:
    /// canonical controls are supplied by `initialize`.
    #[cfg(test)]
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

    fn from_runtime(runtime: InitControls) -> Self {
        Self {
            choices: runtime.choices,
            ranges: runtime.ranges,
            booleans: runtime.booleans,
            actions: runtime.actions,
            ..Default::default()
        }
    }

    /// Wire snapshot of the choice controls (cache overrides each default), or
    /// `None` when none are declared.
    fn choices_wire(&self) -> Option<DeviceCapability> {
        wire_group(&self.choices, DeviceCapability::Choice, |c| Choice {
            key: c.key.clone(),
            label: c.label.clone(),
            options: c.options.clone(),
            selected: self.choice_cache.get(&c.key).unwrap_or(c.default),
            category: c.category.clone(),
            display: c.display.clone(),
            visible_when: None,
        })
    }

    /// Wire snapshot of the range controls (cache overrides each default), or
    /// `None` when none are declared.
    fn ranges_wire(&self) -> Option<DeviceCapability> {
        wire_group(&self.ranges, DeviceCapability::Range, |r| Range {
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
    }

    /// Wire snapshot of the action controls, or `None` when none are declared.
    fn actions_wire(&self) -> Option<DeviceCapability> {
        wire_group(&self.actions, DeviceCapability::Action, |a| Action {
            key: a.key.clone(),
            label: a.label.clone(),
            category: a.category.clone(),
            visible_when: None,
        })
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
                b.read_only = decl.read_only;
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

/// Why a [`LuaDevice`] is [`RuntimeState::Degraded`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum DegradeReason {
    /// A tracked capability call (`track`) returned `Err`.
    CallFailed,
    /// `resync_children` couldn't enumerate the remote's controllers.
    EnumerateFailed,
}

/// Runtime lifecycle of an integration [`LuaDevice`] (root or child), shared
/// by a root and all its children — one socket pool, one state. Plain device
/// plugins have none (`LuaDevice::runtime` is `None`) and are always live.
///
/// Transitions: `OpeningTransport -> Initializing -> Online <-> Degraded`
/// (`track`/`resync_children` mirror the last call, self-healing on success)
/// `-> Closing -> Closed` (terminal — a call landing after close can't
/// resurrect the device).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum RuntimeState {
    OpeningTransport,
    Initializing,
    Online,
    Degraded(DegradeReason),
    Closing,
    Closed,
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
    dynamic_children: bool,
    device_type: DeviceType,
    visibility: VisibilitySlot,
    transport_kind: &'static str,
    dynamic_model: OnceLock<String>,
    worker: Option<PluginHandle>,
    transport: Option<PluginIo>,
    /// Local registry lifetime. Dynamic children share their root's runtime,
    /// so closing a child must be tracked separately from closing the root.
    closed: AtomicBool,
    /// For integration roots only: the manifest each controller child is
    /// synthesized from (`child_manifest_for`).
    root_manifest: Option<Arc<PluginManifest>>,
    /// Integration runtime lifecycle, shared by a root and all its children
    /// (one socket pool → one state). `None` for non-integration devices
    /// (always live). See [`RuntimeState`].
    runtime: Option<Arc<Mutex<RuntimeState>>>,

    /// Capability sections the manifest declared, in advertised order. Drives
    /// `capabilities()` (chain also implies `Controller`; integration adds one).
    allowed_caps: Vec<Cap>,
    caps: RwLock<Vec<Cap>>,

    dpi_state: Mutex<DpiState>,
    dpi_config: Mutex<DpiConfig>,

    /// Declared choice/range/boolean/action controls + their value caches.
    controls: Controls,
    dynamic_controls: OnceLock<Controls>,

    /// Last status samples. Full IPC serialization reads these caches instead
    /// of queueing callbacks on the worker used by interactive commands.
    eq_cache: Arc<Mutex<Option<Equalizer>>>,
    boolean_cache: Arc<Mutex<Vec<Boolean>>>,
    battery_cache: Arc<Mutex<Vec<Battery>>>,
    connection_cache: Arc<Mutex<Option<ConnectionStatus>>>,

    key_remap: RwLock<KeyRemapDescriptor>,
    /// Host-cached mappings that differ from `ButtonAction::Native`.
    key_remap_mappings: Mutex<HashMap<u16, ButtonMapping>>,
    keyboard_layout: KeyboardLayoutSlot,
    keyboard_descriptor: OnceLock<InitKeyboard>,

    /// LCD panel descriptor, reported by `initialize` (resolution can vary by
    /// device variant). Absent until initialized.
    lcd_descriptor: OnceLock<LcdDescriptor>,
    lcd_slot: LcdStateSlot,
    /// Re-apply RGB after an LCD image upload (some panels reset their LEDs).
    lcd_needs_rgb_restore: AtomicBool,

    rgb_descriptor: RgbDescriptor,
    /// RGB zones discovered at `initialize()` (dynamic LED counts). Overrides
    /// `rgb_descriptor` when set.
    dynamic_rgb_descriptor: OnceLock<RgbDescriptor>,
    /// ISO keyboard geometry for runtime-described keyboards. The active
    /// descriptor is selected from the shared layout slot on every snapshot.
    dynamic_rgb_iso_descriptor: OnceLock<RgbDescriptor>,
    rgb_slot: RgbStateSlot,
    fan_slot: FanStateSlot,
    fan_channel: AtomicU8,

    /// Last sensor/fan telemetry sampled by the status poll.
    sensor_cache: Arc<Mutex<Vec<Sensor>>>,
    fan_cache: Arc<Mutex<FanSample>>,

    /// Host-run status poll: aborted on drop. `poll_paused` lets a future LCD
    /// path silence polling during a bulk transfer without tearing it down.
    poll_task: Option<tokio::task::JoinHandle<()>>,
    poll_paused: Arc<AtomicBool>,
    /// Event-driven transports wake this task when their bounded host queue
    /// receives input. The task only enqueues work on the root Lua worker.
    event_task: Option<tokio::task::JoinHandle<()>>,
    event_paused: Arc<AtomicBool>,
    dynamic_child_ids: Arc<RwLock<HashMap<u32, String>>>,

    /// Host-owned virtual-audio sink registry, drained on close; the guard tears
    /// remaining sinks down on drop even if the worker is dead.
    audio_registry: Option<super::audio_api::SinkRegistry>,
    audio_guard: Option<super::audio_api::AudioGuard>,

    // ── chain / children (reported by initialize) ─────────────────────────
    /// Set after construction (needs the `Arc<Self>`); `None` for non-chain devices.
    chain_host: OnceLock<Arc<ChainHost>>,
    /// Weak back-reference so `discover_children` can hand children a `FanHub`.
    self_ref: Weak<LuaDevice>,
    chain_channels: Vec<ChannelDescriptor>,
    /// Channels reported dynamically by `initialize` (capacity known only at
    /// runtime, e.g. ARGB headers read from a config table). Takes precedence
    /// over `chain_channels` once set.
    dynamic_chain_channels: OnceLock<Vec<ChannelDescriptor>>,
    accessories: Vec<AccessoryManifest>,
    dynamic_accessories: OnceLock<Vec<AccessoryManifest>>,

    /// Weak handle to `AppState` so a failed runtime callback can push a toast
    /// (via `app.registry`) without the device owning `AppState`.
    notify: Weak<crate::state::AppState>,
}

impl Drop for LuaDevice {
    fn drop(&mut self) {
        if let Some(task) = self.poll_task.take() {
            task.abort();
        }
        if let Some(task) = self.event_task.take() {
            task.abort();
        }
    }
}

impl LuaDevice {
    /// A plugin that declares no capability — identity + lifecycle only.
    pub fn device_only(
        id: String,
        manifest: &PluginManifest,
        spec: &DeviceSpec,
        notify: Weak<crate::state::AppState>,
    ) -> Self {
        Self::build(id, manifest, Some(spec), None, None, notify)
    }

    /// A plugin with capabilities, backed by a worker over `transport`.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn with_transport(
        id: String,
        manifest: &PluginManifest,
        spec: &DeviceSpec,
        dev_match: DevMatch,
        transport: PluginIo,
        handle: tokio::runtime::Handle,
        granted: Vec<Permission>,
        config: HashMap<String, String>,
        notify: Weak<crate::state::AppState>,
        runtime: Arc<Mutex<RuntimeState>>,
    ) -> Self {
        *runtime.lock().unwrap() = RuntimeState::Initializing;
        let receiver_root = manifest.plugin_id == "logitech" && dev_match.pid == Some(0xc547);
        let nuvoton_sensor_root = manifest.plugin_id == "nuvoton_lpcio" && dev_match.key.is_none();
        let mut dev = Self::with_worker(
            id,
            manifest,
            Some(spec),
            dev_match,
            transport,
            handle,
            granted,
            config,
            notify,
            Some(runtime),
        );
        if manifest.dynamic_children {
            dev.root_manifest = Some(Arc::new(manifest.clone()));
        }
        if receiver_root {
            dev.caps
                .get_mut()
                .unwrap()
                .retain(|cap| matches!(cap, Cap::Pairing));
        }
        if nuvoton_sensor_root {
            // The matched Super-I/O is the sensor controller. Its dynamic
            // children own the individual PWM channels; retaining `Fan` here
            // makes the controller itself appear in the Cooling UI.
            dev.caps
                .get_mut()
                .unwrap()
                .retain(|cap| !matches!(cap, Cap::Fan));
        }
        dev
    }

    /// The headless root of a config-instantiated integration plugin. `runtime`
    /// is handled the same way as [`Self::with_transport`]; children share it.
    #[allow(clippy::too_many_arguments)]
    pub(super) fn integration_root(
        id: String,
        manifest: &PluginManifest,
        transport: PluginIo,
        handle: tokio::runtime::Handle,
        granted: Vec<Permission>,
        config: HashMap<String, String>,
        notify: Weak<crate::state::AppState>,
        runtime: Arc<Mutex<RuntimeState>>,
    ) -> Self {
        let dev_match = DevMatch {
            transport: "tcp".to_owned(),
            ..Default::default()
        };
        let mut dev = Self::with_worker(
            id,
            manifest,
            None,
            dev_match,
            transport,
            handle,
            granted,
            config,
            notify,
            Some(runtime.clone()),
        );
        dev.root_manifest = Some(Arc::new(manifest.clone()));
        *runtime.lock().unwrap() = RuntimeState::Initializing;
        dev
    }

    /// One integration controller as a full `LuaDevice`: its capability set
    /// comes from the enumerated controller (`child_manifest_for`), and its
    /// worker VM is seeded with the controller `index` in `dev.match.index`, so
    /// the shared script routes each capability call to the right remote
    /// controller.
    #[allow(clippy::too_many_arguments)]
    fn integration_child(
        id: String,
        name: String,
        vendor: String,
        device_type: DeviceType,
        manifest: &PluginManifest,
        controller_index: u32,
        controller_key: Option<String>,
        controller_extra: HashMap<String, u64>,
        transport_kind: String,
        worker: PluginHandle,
        transport: PluginIo,
        notify: Weak<crate::state::AppState>,
        runtime: Arc<Mutex<RuntimeState>>,
    ) -> Self {
        let dev_match = DevMatch {
            transport: transport_kind,
            index: Some(controller_index),
            key: controller_key,
            name: Some(name.clone()),
            extra: controller_extra,
            ..Default::default()
        };
        let mut dev = Self::build(
            id,
            manifest,
            None,
            Some(worker.child(dev_match)),
            Some(transport),
            notify,
        );
        dev.runtime = Some(runtime);
        dev.name = name;
        dev.vendor = vendor;
        dev.device_type = device_type;
        dev
    }

    /// `runtime` is stored as-is (already `OpeningTransport`, or a shared
    /// root state for a child) — callers own the `OpeningTransport ->
    /// Initializing` transition, since only they know whether they're
    /// building the owner or sharing an already-running root's state. `None`
    /// for plain device plugins (see [`Self::with_transport`]).
    #[allow(clippy::too_many_arguments)]
    fn with_worker(
        id: String,
        manifest: &PluginManifest,
        spec: Option<&DeviceSpec>,
        dev_match: DevMatch,
        transport: PluginIo,
        handle: tokio::runtime::Handle,
        granted: Vec<Permission>,
        config: HashMap<String, String>,
        notify: Weak<crate::state::AppState>,
        runtime: Option<Arc<Mutex<RuntimeState>>>,
    ) -> Self {
        // Keep a handle to the (metered) transport so the device can report
        // write-rate/throughput; the worker owns the one it does I/O through.
        let rate_transport = transport.clone();
        let event_receiver = match &rate_transport {
            PluginIo::Stream { transport, .. } => transport.event_receiver(),
            _ => None,
        };
        // Canonical packages report physical zones from initialize. Do not
        // seed the worker from the removed static RGB catalog section.
        let zones = Vec::new();
        let audio_registry: super::audio_api::SinkRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let worker = PluginHandle::spawn(
            manifest.script_source.clone(),
            manifest.module_sources.clone(),
            transport,
            dev_match,
            granted,
            config,
            handle.clone(),
            zones,
            audio_registry.clone(),
        );
        let poll_device_id = id.clone();
        let poll_notify = notify.clone();
        let mut dev = Self::build(
            id,
            manifest,
            spec,
            Some(worker.clone()),
            Some(rate_transport),
            notify,
        );
        dev.runtime = runtime;
        dev.audio_registry = Some(audio_registry.clone());
        dev.audio_guard = Some(super::audio_api::AudioGuard::new(
            audio_registry,
            handle.clone(),
        ));

        if let Some(mut events) = event_receiver {
            dev.event_paused.store(true, Ordering::Relaxed);
            let paused = dev.event_paused.clone();
            let event_worker = worker.clone();
            let event_notify = poll_notify.clone();
            let root_id = poll_device_id.clone();
            let child_ids = dev.dynamic_child_ids.clone();
            dev.event_task = Some(handle.spawn(async move {
                while events.changed().await.is_ok() {
                    while paused.load(Ordering::Relaxed) {
                        tokio::time::sleep(Duration::from_millis(10)).await;
                    }
                    let outcomes = match event_worker.on_transport_events().await {
                        Ok(outcomes) => outcomes,
                        Err(error) => {
                            log::warn!(
                                "plugin HID event dispatch failed for '{root_id}': {error:#}"
                            );
                            continue;
                        }
                    };
                    let Some(app) = event_notify.upgrade() else {
                        break;
                    };
                    for outcome in outcomes {
                        if outcome.children_changed {
                            if let Some(root) = app.find_device_by_id(&root_id).await {
                                // Receiver firmware may announce lock closure
                                // before its pairing table exposes the new slot.
                                // Retry briefly, as the native receiver does.
                                for _ in 0..8 {
                                    if crate::registry::usecases::receiver::reconcile_owned_children(
                                        &root, &app,
                                    )
                                    .await
                                    {
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                }
                            }
                        }
                        if outcome.state_changed {
                            app.broadcast_state().await;
                        }
                        if outcome.button_events.pressed.is_empty()
                            && outcome.button_events.released.is_empty()
                        {
                            continue;
                        }
                        let device_id = outcome
                            .child_index
                            .and_then(|index| child_ids.read().unwrap().get(&index).cloned())
                            .unwrap_or_else(|| root_id.clone());
                        let _ = app.input.button_event_tx.send(crate::state::ButtonEvent {
                            device_id,
                            pressed: outcome.button_events.pressed,
                            released: outcome.button_events.released,
                        });
                    }
                }
            }));
        }

        // Sensor/fan refresh stays host-side (not in the serialized VM): a
        // daemon-cadence ticker enqueues one status read at a time. Chain roots
        // also need this: fan telemetry can belong exclusively to chained
        // accessories, while read_status() and its cache live on the root.
        // Start paused and let initialize() release it. This matters because a
        // plugin device is constructed before central registration checks the
        // persisted Disabled state.
        if needs_status_poll(&dev.caps.read().unwrap()) {
            dev.poll_paused.store(true, Ordering::Relaxed);
            let interval = Duration::from_secs(1);
            let paused = dev.poll_paused.clone();
            let poll_caps = StatusPollCaps::from_caps(&dev.caps.read().unwrap());
            let sensor_cache = dev.sensor_cache.clone();
            let fan_cache = dev.fan_cache.clone();
            let boolean_cache = dev.boolean_cache.clone();
            let battery_cache = dev.battery_cache.clone();
            let connection_cache = dev.connection_cache.clone();
            let eq_cache = dev.eq_cache.clone();
            dev.poll_task = Some(handle.spawn(async move {
                let mut ticker = tokio::time::interval(interval);
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                loop {
                    ticker.tick().await;
                    if paused.load(Ordering::Relaxed) {
                        continue;
                    }
                    match worker.poll().await {
                        Ok(events) => {
                            if events.state_changed {
                                if let Some(app) = poll_notify.upgrade() {
                                    app.broadcast_state().await;
                                }
                            }
                            if !events.button_events.pressed.is_empty()
                                || !events.button_events.released.is_empty()
                            {
                                if let Some(app) = poll_notify.upgrade() {
                                    let _ =
                                        app.input.button_event_tx.send(crate::state::ButtonEvent {
                                            device_id: poll_device_id.clone(),
                                            pressed: events.button_events.pressed,
                                            released: events.button_events.released,
                                        });
                                }
                            }
                        }
                        Err(_) => break, // worker gone
                    }
                    sample_status(
                        &worker,
                        poll_caps,
                        &sensor_cache,
                        &fan_cache,
                        &boolean_cache,
                        &battery_cache,
                        &connection_cache,
                        &eq_cache,
                    )
                    .await;
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
        notify: Weak<crate::state::AppState>,
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
            dynamic_children: manifest.dynamic_children,
            device_type: spec.and_then(|s| s.device_type).unwrap_or_default(),
            visibility: VisibilitySlot::default(),
            transport_kind: spec
                .and_then(|s| super::transport::descriptor_for(&s.transport))
                .map(|d| d.kind)
                .unwrap_or("tcp"),
            dynamic_model: OnceLock::new(),
            worker,
            transport,
            closed: AtomicBool::new(false),
            root_manifest: None,
            runtime: None,
            allowed_caps: declared_caps(manifest),
            caps: RwLock::new(declared_caps(manifest)),
            lcd_descriptor: OnceLock::new(),
            lcd_slot: LcdStateSlot::default(),
            lcd_needs_rgb_restore: AtomicBool::new(false),
            dpi_state: Mutex::new(DpiState {
                steps: Vec::new(),
                available_dpis: Vec::new(),
                index: 0,
                current: 0,
            }),
            dpi_config: Mutex::new(DpiConfig {
                min: 0,
                max: 0,
                mode: DpiMode::Host,
                mode_control: None,
            }),
            // Canonical package controls are runtime descriptors returned by
            // initialize. Keep the pre-initialize surface empty.
            controls: Controls::default(),
            dynamic_controls: OnceLock::new(),
            eq_cache: Arc::new(Mutex::new(None)),
            boolean_cache: Arc::new(Mutex::new(Vec::new())),
            battery_cache: Arc::new(Mutex::new(Vec::new())),
            connection_cache: Arc::new(Mutex::new(None)),
            key_remap: RwLock::new(KeyRemapDescriptor::default()),
            key_remap_mappings: Mutex::new(HashMap::new()),
            keyboard_layout: KeyboardLayoutSlot::default(),
            keyboard_descriptor: OnceLock::new(),
            rgb_descriptor: RgbDescriptor {
                zones: Vec::new(),
                native_effects: Vec::new(),
            },
            dynamic_rgb_descriptor: OnceLock::new(),
            dynamic_rgb_iso_descriptor: OnceLock::new(),
            rgb_slot: RgbStateSlot::default(),
            fan_slot: FanStateSlot::default(),
            fan_channel: AtomicU8::new(0),
            sensor_cache: Arc::new(Mutex::new(Vec::new())),
            fan_cache: Arc::new(Mutex::new(FanSample::default())),
            poll_task: None,
            poll_paused: Arc::new(AtomicBool::new(false)),
            event_task: None,
            event_paused: Arc::new(AtomicBool::new(false)),
            dynamic_child_ids: Arc::new(RwLock::new(HashMap::new())),
            audio_registry: None,
            audio_guard: None,
            chain_host: OnceLock::new(),
            self_ref: Weak::new(),
            chain_channels: Vec::new(),
            dynamic_chain_channels: OnceLock::new(),
            accessories: Vec::new(),
            dynamic_accessories: OnceLock::new(),
            notify,
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
        if result.is_err() {
            if let Some(transport) = &self.transport {
                transport.restore_safety_state();
            }
        }
        // A failing call greys the integration; a success self-heals it —
        // unless the device already closed, which is terminal.
        if let Some(r) = &self.runtime {
            let mut state = r.lock().unwrap();
            if !matches!(*state, RuntimeState::Closing | RuntimeState::Closed) {
                *state = if result.is_ok() {
                    RuntimeState::Online
                } else {
                    RuntimeState::Degraded(DegradeReason::CallFailed)
                };
            }
        }
        let Some(app) = self.notify.upgrade() else {
            return result;
        };
        match result {
            Ok(value) => {
                app.registry.clear_runtime_error(&self.plugin_id, &self.id);
                Ok(value)
            }
            Err(e) => {
                let detail = format!("{e:#}");
                app.registry
                    .report_runtime_error(&app, &self.plugin_id, &self.id, detail.clone())
                    .await;
                Err(super::SurfacedPluginError {
                    plugin: self.plugin_id.clone(),
                    detail,
                }
                .into())
            }
        }
    }

    /// Trigger one status poll synchronously (used by tests; production relies on
    /// the ticker).
    #[cfg(test)]
    pub async fn poll_once(&self) -> Result<()> {
        self.worker()?.poll().await?;
        self.refresh_status_cache().await;
        Ok(())
    }

    /// Refresh the host-side sensor/fan caches from the worker. Driven by the
    /// status poll (and `poll_once`); serialization never calls this.
    async fn refresh_status_cache(&self) {
        let Some(worker) = &self.worker else { return };
        let caps = StatusPollCaps::from_caps(&self.caps.read().unwrap());
        sample_status(
            worker,
            caps,
            &self.sensor_cache,
            &self.fan_cache,
            &self.boolean_cache,
            &self.battery_cache,
            &self.connection_cache,
            &self.eq_cache,
        )
        .await;
    }

    /// Drain the transport's queued events once through `event()`, returning the
    /// per-target outcomes. Production drives this from the event watcher task;
    /// the plugin-test harness calls it directly.
    pub(super) async fn pump_events(&self) -> Result<Vec<super::worker::PollOutcome>> {
        self.worker()?.on_transport_events().await
    }

    fn worker(&self) -> Result<&PluginHandle> {
        anyhow::ensure!(
            !self.closed.load(Ordering::Acquire),
            "plugin device '{}' is closed",
            self.id
        );
        self.worker
            .as_ref()
            .ok_or_else(|| anyhow::anyhow!("plugin '{}' has no worker", self.plugin_id))
    }

    fn active_controls(&self) -> &Controls {
        self.dynamic_controls.get().unwrap_or(&self.controls)
    }
}

/// Build an `RgbDescriptor` from `initialize`-reported zones, computing LED
/// positions from the declared topology + count (as static accessory zones do).
fn visual_keys(variant: &InitKeyboardVariant) -> Vec<VisualKey> {
    let cells = variant.base.cells();
    let mut keys = Vec::new();
    // Preserve the standard layout's physical order, matching the native
    // KeyLayoutSpec resolver rather than the firmware/CID table order.
    for cell in cells {
        if let Some(mapping) = variant
            .keys
            .iter()
            .find(|mapping| mapping.key == Some(cell.id))
        {
            keys.push(VisualKey {
                led_id: mapping.led_id,
                remap_cid: mapping.remap_cid,
                cell: mapping
                    .cell
                    .as_ref()
                    .map(|geometry| halod_shared::keyboard::KeyCell {
                        id: cell.id,
                        col: geometry.col,
                        row: geometry.row,
                        w: geometry.w,
                        h: geometry.h,
                    })
                    .unwrap_or(cell),
            });
        }
    }
    // Model-specific media/Fn/etc. keys have explicit geometry and use their
    // firmware LED id as the stable custom-key identity.
    keys.extend(variant.keys.iter().filter_map(|mapping| {
        let geometry = mapping.cell.as_ref()?;
        Some(VisualKey {
            led_id: mapping.led_id,
            remap_cid: mapping.remap_cid,
            cell: halod_shared::keyboard::KeyCell {
                id: mapping
                    .key
                    .unwrap_or(KeyId::Custom(mapping.led_id.min(u16::MAX as u32) as u16)),
                col: geometry.col,
                row: geometry.row,
                w: geometry.w,
                h: geometry.h,
            },
        })
    }));
    keys
}

fn keyboard_led_positions(
    keys: &[VisualKey],
    advertised: &[u32],
) -> Vec<halod_shared::types::LedPosition> {
    keys.iter()
        .filter(|key| advertised.is_empty() || advertised.contains(&key.led_id))
        .map(|key| halod_shared::types::LedPosition {
            id: key.led_id,
            // Exact projection used by the former native TKL descriptor.
            x: (key.cell.col + key.cell.w / 2.0) / 18.0,
            y: (key.cell.row + 1.5) / 7.0,
        })
        .collect()
}

fn effective_keyboard_layout(
    descriptor: &InitKeyboard,
    slot: &KeyboardLayoutSlot,
) -> (KeyVariant, KeyboardLayout) {
    crate::drivers::effective_layout(
        slot.selection(),
        descriptor.detected_language,
        descriptor
            .languages
            .first()
            .copied()
            .unwrap_or(KeyboardLayout::US),
        descriptor.iso.is_some(),
    )
}

fn selected_visual_keys(descriptor: &InitKeyboard, slot: &KeyboardLayoutSlot) -> Vec<VisualKey> {
    let (variant, _) = effective_keyboard_layout(descriptor, slot);
    let layout = match variant {
        KeyVariant::Iso => descriptor.iso.as_ref().unwrap_or(&descriptor.ansi),
        KeyVariant::Ansi => &descriptor.ansi,
    };
    visual_keys(layout)
}

fn build_dynamic_descriptor(
    zones: Vec<InitZone>,
    native_effects: Vec<NativeEffect>,
    keyboard_keys: Option<&[VisualKey]>,
) -> RgbDescriptor {
    let zones = zones
        .into_iter()
        .map(|z| {
            let topology = if z.topology == "keyboard" {
                ZoneTopology::Keyboard {
                    form_factor: z.keyboard_form_factor.unwrap_or(KeyboardFormFactor::TKL),
                    layout: z.keyboard_layout.unwrap_or(KeyboardLayout::Unknown),
                }
            } else {
                topology_from(&z.topology, z.rings)
            };
            let led_count = if z.led_ids.is_empty() {
                z.led_count
            } else {
                z.led_ids.len() as u32
            };
            // `ring_led_positions` only lays out ring topologies; linear zones use
            // the evenly-spaced strip layout (as the native drivers did).
            if !z.leds.is_empty() {
                RgbZone {
                    leds: z.leds,
                    id: z.id,
                    name: z.name,
                    topology,
                }
            } else if matches!(topology, ZoneTopology::Keyboard { .. })
                && keyboard_keys.is_some_and(|keys| !keys.is_empty())
            {
                RgbZone {
                    leds: keyboard_led_positions(keyboard_keys.unwrap(), &z.led_ids),
                    id: z.id,
                    name: z.name,
                    topology,
                }
            } else if !z.led_ids.is_empty() {
                let columns = if matches!(topology, ZoneTopology::Keyboard { .. }) {
                    18
                } else {
                    z.led_ids.len().max(1)
                };
                let rows = z.led_ids.len().div_ceil(columns).max(1);
                RgbZone {
                    leds: z
                        .led_ids
                        .iter()
                        .enumerate()
                        .map(|(i, id)| halod_shared::types::LedPosition {
                            id: *id,
                            x: if columns <= 1 {
                                0.0
                            } else {
                                (i % columns) as f32 / (columns - 1) as f32
                            },
                            y: if rows <= 1 {
                                0.5
                            } else {
                                (i / columns) as f32 / (rows - 1) as f32
                            },
                        })
                        .collect(),
                    id: z.id,
                    name: z.name,
                    topology,
                }
            } else if matches!(topology, ZoneTopology::Linear) {
                linear_rgb_zone(&z.id, &z.name, led_count as usize)
            } else {
                RgbZone {
                    leds: ring_led_positions(&topology, led_count),
                    id: z.id,
                    name: z.name,
                    topology,
                }
            }
        })
        .collect();
    RgbDescriptor {
        zones,
        native_effects,
    }
}

#[cfg(test)]
mod dynamic_rgb_descriptor_tests {
    use super::*;
    use halod_shared::types::LedPosition;

    #[test]
    fn runtime_zone_preserves_explicit_led_geometry() {
        let leds = vec![
            LedPosition {
                id: 7,
                x: 0.98,
                y: 1.0,
            },
            LedPosition {
                id: 8,
                x: 0.5,
                y: 0.05,
            },
        ];
        let descriptor = build_dynamic_descriptor(
            vec![InitZone {
                id: "ambiglow".into(),
                name: "Ambiglow".into(),
                topology: "grid".into(),
                led_count: 2,
                led_ids: Vec::new(),
                leds: leds.clone(),
                rings: 0,
                keyboard_form_factor: None,
                keyboard_layout: None,
            }],
            Vec::new(),
            None,
        );

        let zone = &descriptor.zones[0];
        assert!(matches!(zone.topology, ZoneTopology::Grid));
        assert_eq!(zone.leds.len(), 2);
        for (actual, expected) in zone.leds.iter().zip(leds) {
            assert_eq!(actual.id, expected.id);
            assert_eq!(actual.x, expected.x);
            assert_eq!(actual.y, expected.y);
        }
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

    fn keyboard_layout_slot(&self) -> Option<&KeyboardLayoutSlot> {
        self.allowed_caps
            .contains(&Cap::KeyboardLayout)
            .then_some(&self.keyboard_layout)
    }

    fn integration_id(&self) -> Option<String> {
        (self.plugin_type == PluginKind::Integration).then(|| self.plugin_id.clone())
    }

    fn owning_plugin_id(&self) -> Option<String> {
        Some(self.plugin_id.clone())
    }

    async fn wire_connection_type(&self) -> Option<halod_shared::types::ConnectionType> {
        ConnectionCapability::connection_status(self)
            .await
            .map(|status| status.connection_type)
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn is_live(&self) -> bool {
        if self.closed.load(Ordering::Acquire) {
            return false;
        }
        self.runtime
            .as_ref()
            .is_none_or(|r| match *r.lock().unwrap() {
                RuntimeState::OpeningTransport
                | RuntimeState::Initializing
                | RuntimeState::Online => true,
                RuntimeState::Degraded(_) => self.plugin_type != PluginKind::Integration,
                RuntimeState::Closing | RuntimeState::Closed => false,
            })
    }

    async fn wire_device_connected(&self) -> bool {
        self.is_live()
    }

    async fn initialize(&self) -> Result<bool> {
        let Some(w) = &self.worker else {
            return Ok(true);
        };
        let outcome = match w.initialize().await {
            Ok(outcome) => outcome,
            Err(error) => {
                if let Some(runtime) = &self.runtime {
                    *runtime.lock().unwrap() = RuntimeState::Degraded(DegradeReason::CallFailed);
                }
                return Err(error);
            }
        };
        if let Some(names) = &outcome.capabilities {
            let requested = caps_named(names);
            let accepted: Vec<_> = requested
                .into_iter()
                .filter(|cap| {
                    let declared = self.allowed_caps.contains(cap);
                    if !declared {
                        log::warn!(
                            "plugin '{}' returned undeclared runtime capability {:?} for '{}'; ignoring it",
                            self.plugin_id,
                            cap,
                            self.id
                        );
                    }
                    declared
                })
                .collect();
            *self.caps.write().unwrap() = accepted;
        }
        if let Some(model) = outcome.model {
            let _ = self.dynamic_model.set(model);
        }
        if let Some(keyboard) = outcome.keyboard {
            self.keyboard_layout
                .set_detected(keyboard.detected_language);
            let _ = self.keyboard_descriptor.set(keyboard);
        }
        if let Some(zones) = outcome.zones {
            let effects = outcome.native_effects.unwrap_or_default();
            let keyboard = self.keyboard_descriptor.get();
            let ansi_keys = keyboard.map(|descriptor| visual_keys(&descriptor.ansi));
            let _ = self.dynamic_rgb_descriptor.set(build_dynamic_descriptor(
                zones.clone(),
                effects.clone(),
                ansi_keys.as_deref(),
            ));
            if let Some(iso) = keyboard.and_then(|descriptor| descriptor.iso.as_ref()) {
                let iso_keys = visual_keys(iso);
                let _ = self
                    .dynamic_rgb_iso_descriptor
                    .set(build_dynamic_descriptor(zones, effects, Some(&iso_keys)));
            }
        }
        if let Some(runtime) = &self.runtime {
            *runtime.lock().unwrap() = RuntimeState::Online;
        }
        if let Some(lcd) = outcome.lcd {
            self.lcd_slot.set_brightness(lcd.brightness);
            self.lcd_slot
                .set_rotation(degrees_to_rotation(lcd.rotation));
            self.lcd_slot.set_raw_streaming(lcd.raw_streaming);
            self.lcd_slot.set_latches_last_frame(lcd.latches);
            self.lcd_needs_rgb_restore
                .store(lcd.needs_rgb_restore, Ordering::Relaxed);
            let _ = self.lcd_descriptor.set(build_lcd_descriptor(&lcd));
        }
        if let Some(channels) = outcome.chain {
            let descriptors = channels
                .into_iter()
                .map(|c| ChannelDescriptor {
                    channel_id: c.id,
                    display_name: c.name,
                    max_leds: c.max_leds,
                })
                .collect();
            let _ = self.dynamic_chain_channels.set(descriptors);
        }
        if let Some(accessories) = outcome.accessories {
            let _ = self.dynamic_accessories.set(accessories);
        }
        if let Some(controls) = outcome.controls {
            let _ = self.dynamic_controls.set(Controls::from_runtime(controls));
        }
        if let Some(dpi) = outcome.dpi {
            *self.dpi_config.lock().unwrap() = DpiConfig {
                min: dpi.min,
                max: dpi.max,
                mode: if dpi.onboard {
                    DpiMode::Onboard
                } else {
                    DpiMode::Host
                },
                mode_control: dpi.mode_control.clone(),
            };
            *self.dpi_state.lock().unwrap() = dpi_state_from_runtime(&dpi);
        }
        if let Some(fan) = outcome.fan {
            self.fan_channel.store(fan.channel, Ordering::Relaxed);
        }
        if let Some(key_remap) = outcome.key_remap {
            let defaults = key_remap.default_mappings.clone();
            *self.key_remap.write().unwrap() = KeyRemapDescriptor {
                buttons: key_remap.buttons,
                requires_host_mode: key_remap.requires_host_mode,
                default_mappings: key_remap.default_mappings,
            };
            let mut mappings = self.key_remap_mappings.lock().unwrap();
            if mappings.is_empty() {
                mappings.extend(defaults.into_iter().map(|mapping| (mapping.cid, mapping)));
            }
        }
        // Seed the host range/choice caches with the values the device reported,
        // so the UI shows live hardware state rather than manifest defaults.
        if let Some(ranges) = outcome.ranges {
            for (key, value) in ranges {
                self.active_controls().range_cache.record(&key, value);
            }
        }
        if let Some(choices) = outcome.choices {
            for (key, selected) in choices {
                self.active_controls().choice_cache.record(&key, selected);
            }
        }
        if outcome.ok {
            // Seed every wire-status cache before registration publishes the
            // first snapshot (and before transport-conflict checks consult the
            // cached connection type). Later refreshes run on the poll task.
            self.refresh_status_cache().await;
            if w.supports_events().await? {
                if let Some(PluginIo::Stream { transport, .. }) = &self.transport {
                    transport.enable_event_listener()?;
                }
            }
            self.poll_paused.store(false, Ordering::Relaxed);
            self.event_paused.store(false, Ordering::Relaxed);
        }
        Ok(outcome.ok)
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        let owns_root_resources = self.worker.as_ref().is_none_or(|worker| !worker.is_child());
        if owns_root_resources {
            if let Some(r) = &self.runtime {
                *r.lock().unwrap() = RuntimeState::Closing;
            }
        }
        if let Some(w) = &self.worker {
            w.close().await;
        }
        if owns_root_resources {
            if let Some(transport) = &self.transport {
                transport.restore_safety_state();
            }
        }
        // Drain audio sinks on the device side so cleanup runs even if the
        // worker's close request failed (wedged/dead worker).
        if owns_root_resources {
            if let Some(reg) = &self.audio_registry {
                super::audio_api::drain_and_remove(reg).await;
            }
        }
        if owns_root_resources {
            if let Some(r) = &self.runtime {
                *r.lock().unwrap() = RuntimeState::Closed;
            }
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
        let active = self.caps.read().unwrap().clone();
        for cap in &active {
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
                Cap::KeyboardLayout => caps.push(CapabilityRef::KeyboardLayout(self)),
                Cap::Chain => {
                    caps.push(CapabilityRef::Controller(self));
                    caps.push(CapabilityRef::Chain(self));
                }
            }
        }
        if self.plugin_type == PluginKind::Integration || self.dynamic_children {
            caps.push(CapabilityRef::Controller(self));
        }
        caps
    }
}

#[async_trait]
impl RgbCapability for LuaDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        if let Some(keyboard) = self.keyboard_descriptor.get() {
            let (variant, _) = effective_keyboard_layout(keyboard, &self.keyboard_layout);
            if variant == KeyVariant::Iso {
                if let Some(descriptor) = self.dynamic_rgb_iso_descriptor.get() {
                    return descriptor;
                }
            }
        }
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

    async fn write_frame_batch(&self, zones: &[(String, Vec<RgbColor>)]) -> Result<()> {
        for (zone_id, _) in zones {
            if !self.descriptor().zones.iter().any(|z| z.id == *zone_id) {
                anyhow::bail!("unknown zone: {zone_id}");
            }
        }
        let r = self.worker()?.rgb_write_frame_batch(zones).await;
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
        let transform = slot.transform_for(zone_id);
        if transform.is_identity() {
            transformed.insert(zone_id.clone(), led_map.clone());
            continue;
        }
        let permutation = build_permutation(zone, &transform);
        let new_map = (0..zone.leds.len())
            .filter_map(|i| {
                let source_id = zone.leds[permutation[i]].id.to_string();
                led_map
                    .get(&source_id)
                    .map(|color| (zone.leds[i].id.to_string(), *color))
            })
            .collect();
        transformed.insert(zone_id.clone(), new_map);
    }
    RgbState::PerLed { zones: transformed }
}

#[async_trait]
impl FanCapability for LuaDevice {
    async fn get_duty(&self) -> Result<u8> {
        self.fan_cache
            .lock()
            .unwrap()
            .duty
            .ok_or_else(|| anyhow::anyhow!("fan duty not sampled yet"))
    }

    async fn set_duty(&self, duty: u8) -> Result<()> {
        let r = self.worker()?.fan_set_duty(duty).await;
        self.track(r).await?;
        self.fan_cache.lock().unwrap().duty = Some(duty);
        Ok(())
    }

    async fn get_rpm(&self) -> Option<u32> {
        self.fan_cache.lock().unwrap().rpm
    }

    fn fan_state(&self) -> &FanStateSlot {
        &self.fan_slot
    }

    fn fan_channel_id(&self) -> u8 {
        self.fan_channel.load(Ordering::Relaxed)
    }
}

#[async_trait]
impl SensorCapability for LuaDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.sensor_cache.lock().unwrap().clone())
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
        if self.plugin_type == PluginKind::Integration || self.dynamic_children {
            return self.discover_controllers().await;
        }
        self.discover_chain_accessories().await
    }

    /// Diff the live server's controllers against `existing`: build only new
    /// ones, report departed ids, leave survivors untouched. `Err` greys the
    /// integration (server dropped mid-enumerate).
    async fn resync_children(
        &self,
        existing: &HashSet<String>,
    ) -> Result<(Vec<Arc<dyn Device>>, Vec<String>)> {
        if self.plugin_type != PluginKind::Integration && !self.dynamic_children {
            return Ok((vec![], vec![]));
        }
        let detected = match self.worker()?.enumerate_controllers().await {
            Ok(d) => d,
            Err(e) => {
                if let Some(r) = &self.runtime {
                    let mut state = r.lock().unwrap();
                    if !matches!(*state, RuntimeState::Closing | RuntimeState::Closed) {
                        *state = RuntimeState::Degraded(DegradeReason::EnumerateFailed);
                    }
                }
                return Err(e);
            }
        };
        let Some(ctx) = self.child_build_ctx() else {
            anyhow::bail!("integration root missing child-build context");
        };
        let live_ids: HashSet<String> = detected
            .iter()
            .map(|c| child_device_id(&self.id, c))
            .collect();
        {
            let mut routes = self.dynamic_child_ids.write().unwrap();
            routes.retain(|_, id| live_ids.contains(id));
            for controller in &detected {
                routes.insert(controller.index, child_device_id(&self.id, controller));
            }
        }
        let mut added = Vec::new();
        for controller in &detected {
            let child_id = child_device_id(&self.id, controller);
            if existing.contains(&child_id) {
                continue;
            }
            if let Some(child) = self.build_child(controller, &ctx).await {
                added.push(child);
            }
        }
        let gone: Vec<String> = existing
            .iter()
            .filter(|id| !live_ids.contains(*id))
            .cloned()
            .collect();
        Ok((added, gone))
    }
}

/// Shared inputs `build_child` needs, gathered once from the root.
struct ChildBuildCtx {
    worker: PluginHandle,
    transport: PluginIo,
    root_manifest: Arc<PluginManifest>,
    identity_scope: crate::registry::identity::IdentityScope,
}

fn child_device_id(root: &str, controller: &DetectedController) -> String {
    controller
        .id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| format!("{root}_ctrl_{}", controller.index))
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
            let Some(accessory) = self
                .dynamic_accessories
                .get()
                .unwrap_or(&self.accessories)
                .iter()
                .find(|a| a.id == d.accessory)
            else {
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
        let Some(ctx) = self.child_build_ctx() else {
            return Vec::new();
        };
        let mut out = Vec::new();
        for controller in &detected {
            if let Some(child) = self.build_child(controller, &ctx).await {
                out.push(child);
            }
        }
        out
    }

    /// `None` when the integration root is missing its factory/manifest (a bug).
    fn child_build_ctx(&self) -> Option<ChildBuildCtx> {
        let (Some(worker), Some(transport), Some(root_manifest)) = (
            self.worker.clone(),
            self.transport.clone(),
            self.root_manifest.clone(),
        ) else {
            log::error!(
                "plugin '{}': integration root missing child-worker factory or manifest — this is a daemon bug",
                self.plugin_id
            );
            return None;
        };
        let config = match self.notify.upgrade() {
            Some(app) => {
                let granted = app.registry.granted_for(&root_manifest.plugin_id);
                app.registry.resolved_config_for(
                    app.secret_store.as_ref(),
                    &root_manifest.plugin_id,
                    &granted,
                )
            }
            None => HashMap::new(),
        };
        let identity_scope = if self.plugin_type == PluginKind::Integration {
            let tcp = root_manifest.transports.tcp.clone().unwrap_or_default();
            crate::registry::identity::integration_scope(
                config.get(&tcp.host_key).map(String::as_str),
                config.get(&tcp.port_key).map(String::as_str),
            )
        } else {
            crate::registry::identity::IdentityScope::Local
        };
        Some(ChildBuildCtx {
            worker,
            transport,
            root_manifest,
            identity_scope,
        })
    }

    /// Build one integration controller as a child `LuaDevice` sharing the
    /// root's [`RuntimeState`]. Initialization is deliberately deferred to the
    /// central registration lifecycle, which first checks whether this child is
    /// disabled. `None` on connect/build failure. Shared by first-time discovery
    /// and `resync_children`.
    async fn build_child(
        &self,
        controller: &DetectedController,
        ctx: &ChildBuildCtx,
    ) -> Option<Arc<dyn Device>> {
        let child_manifest = child_manifest_for(&ctx.root_manifest, controller);
        self.dynamic_child_ids
            .write()
            .unwrap()
            .insert(controller.index, child_device_id(&self.id, controller));
        // `new_cyclic` so a controller that itself declares `chain` can hand its
        // accessories a `FanHub` back-reference (nested chain).
        let child = Arc::new_cyclic(|weak| {
            let mut d = LuaDevice::integration_child(
                child_device_id(&self.id, controller),
                controller.name.clone(),
                self.vendor.clone(),
                controller.device_type,
                &child_manifest,
                controller.index,
                controller.key.clone(),
                controller.extra.clone(),
                self.transport_kind.to_owned(),
                ctx.worker.clone(),
                ctx.transport.clone(),
                self.notify.clone(),
                self.runtime
                    .clone()
                    .expect("integration root always has runtime state"),
            );
            d.set_self_ref(weak.clone());
            d
        });
        if child_manifest.chain.is_some() {
            let adapter: Arc<dyn ChainAdapter> = child.clone();
            let host = ChainHost::new(adapter);
            child.install_chain_host(host);
        }
        let identity = crate::registry::identity::DeviceIdentity {
            scope: Some(ctx.identity_scope.clone()),
            serial: crate::registry::identity::normalize_serial(controller.serial.as_deref()),
            location: crate::registry::identity::location_from_openrgb(
                controller.location.as_deref(),
            ),
            usb: None,
        };
        Some(Arc::new(crate::registry::identity::IdentifiedDevice::new(
            child as Arc<dyn Device>,
            identity,
            if self.plugin_type == PluginKind::Integration {
                crate::registry::identity::DeviceOrigin::Integration(self.plugin_id.clone())
            } else {
                crate::registry::identity::DeviceOrigin::Plugin(self.plugin_id.clone())
            },
        )))
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
    // Only the physical root enumerates children. A child inherits the root
    // package's callbacks but must never recursively enumerate itself.
    m.dynamic_children = false;
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
        self.lcd_needs_rgb_restore.load(Ordering::Relaxed)
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

/// Install a runtime DPI descriptor, selecting its midpoint like the native
/// driver. This is called after the worker has validated `initialize` output.
fn dpi_state_from_runtime(dpi: &InitDpi) -> DpiState {
    let steps = dpi.steps.clone();
    let current = dpi
        .current
        .unwrap_or_else(|| steps.get(steps.len() / 2).copied().unwrap_or(dpi.min));
    let index = steps
        .iter()
        .position(|&step| step == current)
        .unwrap_or(steps.len() / 2);
    DpiState {
        steps,
        available_dpis: dpi.available_dpis.clone(),
        index,
        current,
    }
}

impl LuaDevice {
    fn clamp_dpi(&self, dpi: u16) -> u16 {
        let config = self.dpi_config.lock().unwrap();
        dpi.clamp(config.min, config.max)
    }
}

#[async_trait]
impl DpiCapability for LuaDevice {
    async fn dpi_status(&self) -> DpiStatus {
        let dpi = self.dpi_state.lock().unwrap();
        let config = self.dpi_config.lock().unwrap().clone();
        DpiStatus {
            steps: dpi.steps.clone(),
            current_index: dpi.index,
            current_dpi: dpi.current,
            available_dpis: if dpi.available_dpis.is_empty() {
                (config.min..=config.max).step_by(100).collect()
            } else {
                dpi.available_dpis.clone()
            },
            mode: config.mode,
        }
    }

    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()> {
        if steps.is_empty() {
            anyhow::bail!("DPI steps list cannot be empty");
        }
        let (min, max) = {
            let config = self.dpi_config.lock().unwrap();
            (config.min, config.max)
        };
        let available = self.dpi_state.lock().unwrap().available_dpis.clone();
        for &step in &steps {
            anyhow::ensure!(
                step >= min && step <= max,
                "DPI {step} is outside {min}..={max}"
            );
            if !available.is_empty() {
                anyhow::ensure!(
                    available.contains(&step),
                    "DPI {step} is not supported by this device"
                );
            }
        }
        self.worker()?.dpi_set_steps(&steps).await?;
        let mut dpi = self.dpi_state.lock().unwrap();
        dpi.steps = steps;
        if dpi.index >= dpi.steps.len() {
            dpi.index = dpi.steps.len() - 1;
        }
        dpi.current = dpi.steps[dpi.index];
        Ok(())
    }

    async fn set_dpi_index(&self, index: usize) -> Result<()> {
        let value = {
            let dpi = self.dpi_state.lock().unwrap();
            let &v = dpi
                .steps
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("dpi index {index} out of range"))?;
            v
        };
        self.worker()?.dpi_set(value).await?;
        let mut dpi = self.dpi_state.lock().unwrap();
        dpi.index = index;
        dpi.current = value;
        Ok(())
    }

    async fn set_dpi_direct(&self, dpi: u16) -> Result<()> {
        let value = self.clamp_dpi(dpi);
        self.worker()?.dpi_set(value).await?;
        self.dpi_state.lock().unwrap().current = value;
        Ok(())
    }
}

#[async_trait]
impl ChoiceCapability for LuaDevice {
    fn choice_cache(&self) -> &ChoiceStateCache {
        &self.active_controls().choice_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.active_controls().choices_wire()
    }

    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        self.active_controls().record_choice(key, selected)?;
        self.worker()?.choice_set(key, selected).await
    }
}

#[async_trait]
impl RangeCapability for LuaDevice {
    fn range_cache(&self) -> &RangeStateCache {
        &self.active_controls().range_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.active_controls().ranges_wire()
    }

    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        let value = self.active_controls().record_range(key, value)?;
        self.worker()?.range_set(key, value).await
    }
}

#[async_trait]
impl BooleanCapability for LuaDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        let mut live = self.boolean_cache.lock().unwrap().clone();
        self.active_controls().backfill_booleans(&mut live);
        Ok(live)
    }

    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        self.worker()?.boolean_set(key, value).await?;
        self.active_controls().bool_cache.record(key, value);
        if let Some(current) = self
            .boolean_cache
            .lock()
            .unwrap()
            .iter_mut()
            .find(|boolean| boolean.key == key)
        {
            current.value = value;
        }
        let mut config = self.dpi_config.lock().unwrap();
        if config.mode_control.as_deref() == Some(key) {
            config.mode = if value {
                DpiMode::Host
            } else {
                DpiMode::Onboard
            };
        }
        Ok(())
    }

    fn bool_cache(&self) -> Option<&BoolStateCache> {
        Some(&self.active_controls().bool_cache)
    }
}

#[async_trait]
impl ActionCapability for LuaDevice {
    async fn trigger_action(&self, key: &str) -> Result<()> {
        if !self.active_controls().has_action(key) {
            anyhow::bail!("unknown action key: {key}");
        }
        self.worker()?.action_trigger(key).await
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.active_controls().actions_wire()
    }
}

#[async_trait]
impl BatteryCapability for LuaDevice {
    async fn get_batteries(&self) -> Result<Vec<Battery>> {
        Ok(self.battery_cache.lock().unwrap().clone())
    }
}

#[async_trait]
impl ConnectionCapability for LuaDevice {
    async fn connection_status(&self) -> Option<ConnectionStatus> {
        self.connection_cache.lock().unwrap().clone()
    }
}

#[async_trait]
impl EqualizerCapability for LuaDevice {
    async fn get_equalizer(&self) -> Result<Equalizer> {
        self.eq_cache
            .lock()
            .unwrap()
            .clone()
            .ok_or_else(|| anyhow::anyhow!("equalizer not sampled yet"))
    }

    async fn set_eq_preset(&self, preset_index: usize) -> Result<()> {
        self.worker()?.equalizer_set_preset(preset_index).await?;
        if let Some(equalizer) = self.eq_cache.lock().unwrap().as_mut() {
            equalizer.selected_preset = preset_index;
        }
        Ok(())
    }

    async fn set_eq_bands(&self, values: &[f32]) -> Result<()> {
        self.worker()?.equalizer_set_bands(values).await?;
        if let Some(equalizer) = self.eq_cache.lock().unwrap().as_mut() {
            for (band, value) in equalizer.bands.iter_mut().zip(values) {
                band.value = *value;
            }
        }
        Ok(())
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

    /// Runs the plugin's hardware-side unpair. The receiver use case follows
    /// this by reconciling the controller's explicitly owned children, which
    /// removes the departed paired slot from the registry.
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
        let descriptor = self.key_remap.read().unwrap().clone();
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
            buttons: descriptor.buttons,
            mappings,
            requires_host_mode: descriptor.requires_host_mode,
            host_mode_active,
        }
    }

    async fn set_button_mapping(&self, mapping: ButtonMapping) -> Result<()> {
        crate::input::validate::validate_cid(&self.key_remap.read().unwrap().buttons, &mapping)?;
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
        self.key_remap.read().unwrap().default_mappings.clone()
    }
}

#[async_trait]
impl KeyboardLayoutCapability for LuaDevice {
    async fn keyboard_layout_status(&self) -> KeyboardLayoutStatus {
        let selection = self.keyboard_layout.selection();
        let Some(descriptor) = self.keyboard_descriptor.get() else {
            return KeyboardLayoutStatus {
                keys: Vec::new(),
                variant: selection.variant.unwrap_or(KeyVariant::Ansi),
                language: selection.language.unwrap_or(KeyboardLayout::Unknown),
                detected_language: KeyboardLayout::Unknown,
                selection,
                iso_supported: false,
                languages: Vec::new(),
            };
        };
        let (variant, language) = effective_keyboard_layout(descriptor, &self.keyboard_layout);
        KeyboardLayoutStatus {
            keys: selected_visual_keys(descriptor, &self.keyboard_layout),
            variant,
            language,
            detected_language: descriptor.detected_language,
            selection,
            iso_supported: descriptor.iso.is_some(),
            languages: descriptor.languages.clone(),
        }
    }
}

#[async_trait]
impl ChainAdapter for LuaDevice {
    fn parent_id(&self) -> String {
        self.id.clone()
    }
    fn channels(&self) -> Vec<ChannelDescriptor> {
        // Runtime-reported channels (from `initialize`) win over the static
        // manifest ones. `ChainHost` reads this live, so channels discovered
        // during init appear even though the host was built before it.
        self.dynamic_chain_channels
            .get()
            .cloned()
            .unwrap_or_else(|| self.chain_channels.clone())
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

#[cfg(all(test, any()))]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;
    use crate::drivers::transports::Transport;
    use crate::drivers::{FanCapability, RgbCapability, SensorCapability};
    use halod_shared::types::VisibilityState;
    use std::path::Path;

    fn hid_match() -> DevMatch {
        DevMatch {
            transport: "hid".into(),
            bus: None,
            addr: None,
            vid: None,
            pid: Some(0x300E), // Kraken Z (320x320 LCD) for LCD-capable tests
            index: None,
            extra: Default::default(),
        }
    }

    /// A fresh `OpeningTransport` handle, as a caller would create before
    /// opening a transport, for constructors under test.
    fn opening_runtime() -> Arc<Mutex<RuntimeState>> {
        Arc::new(Mutex::new(RuntimeState::OpeningTransport))
    }

    /// Build a HID plugin device over a mock byte-stream transport.
    fn hid_device(id: &str, manifest: &PluginManifest, transport: Arc<dyn Transport>) -> LuaDevice {
        let spec = &manifest.devices[0];
        let notify = Weak::new();
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
            Vec::<Permission>::new(),
            HashMap::new(),
            notify,
            opening_runtime(),
        )
    }

    const SCRIPT: &str = r#"
        return {
          devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "M" } },
          permissions = { "hid" },
          capabilities = { "rgb", "fan", "sensors" },
          transports = { hid = { report_size = 8 } },
          rgb = { zones = { {
              id = "z", name = "Z", topology = { type = "linear" },
              leds = { {id=0, x=0.0, y=0.0}, {id=1, x=1.0, y=0.0} },
          } } },
          fan = { channel = 3 },
          sensor = {},

          initialize = function(dev) return { fan = { channel = 3 } } end,

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
        dev.initialize().await.unwrap();
        assert_eq!(dev.fan_channel_id(), 3);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn chain_advertises_controller_and_chain() {
        let src = r#"return {
            devices = { { transport = "hid", vid = 1, pid = 2, vendor = "x", model = "y" } },
            permissions = { "hid" },
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
        let notify = Weak::new();
        let dev = LuaDevice::device_only("d-0".into(), &manifest, &manifest.devices[0], notify);
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
            permissions = { "hid" },
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
        assert_eq!(dev.get_duty().await.unwrap(), 50, "write updates cache");
        // Duty/rpm are read from the poll-populated cache, not live hardware.
        dev.poll_once().await.unwrap();
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
        dev.poll_once().await.unwrap();
        let sensors = dev.get_sensors().await.unwrap();
        assert_eq!(sensors.len(), 1);
        assert_eq!(sensors[0].name, "Temp");
        assert_eq!(sensors[0].value, 30.5);
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn poll_caches_status_read_by_sensors() {
        // read_status parses a report into dev.status; get_sensors reads the
        // cache rather than hitting hardware. This device is not registered or
        // initialized, so its background ticker remains paused; poll_once is
        // the only read.
        const POLL_SCRIPT: &str = r#"
            return {
              devices = { { transport = "hid", vid = 0x1, pid = 0x2, vendor = "Test", model = "M" } },
              permissions = { "hid" },
              capabilities = { "sensors" },
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
        dev.poll_once().await.unwrap();
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
        use crate::drivers::{BatteryCapability, ConnectionCapability, Device};
        let dev = controls_device(Arc::new(MockTransport::empty()));
        dev.poll_once().await.unwrap();
        let batteries = dev.get_batteries().await.unwrap();
        assert_eq!(batteries[0].level, 77);
        let status = dev.connection_status().await.unwrap();
        assert_eq!(
            status.connection_type,
            halod_shared::types::ConnectionType::Wireless
        );
        assert_eq!(
            dev.wire_connection_type().await,
            Some(halod_shared::types::ConnectionType::Wireless)
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn equalizer_status_poll_populates_current_state() {
        use crate::drivers::EqualizerCapability;
        let mock = Arc::new(MockTransport::empty());
        let dev = controls_device(mock.clone());
        assert!(
            EqualizerCapability::current_state(&dev).is_none(),
            "nothing read yet"
        );
        dev.poll_once().await.unwrap();
        let eq = dev.get_equalizer().await.unwrap();
        assert_eq!(eq.selected_preset, 0);
        assert!(
            EqualizerCapability::current_state(&dev).is_some(),
            "cached after status poll"
        );
        dev.set_eq_preset(2).await.unwrap();
        dev.set_eq_bands(&[1.0, 2.0, 3.0]).await.unwrap();
        assert_eq!(
            EqualizerCapability::current_state(&dev)
                .unwrap()
                .selected_preset,
            2,
            "write updates cache"
        );
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
        let manifest = super::super::parse_manifest(CHAIN_SCRIPT, Path::new("kraken.lua")).unwrap();
        let spec = &manifest.devices[0];
        let notify = Weak::new();
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
                Vec::<Permission>::new(),
                HashMap::new(),
                notify,
                opening_runtime(),
            );
            d.set_self_ref(weak.clone());
            d
        });
        let adapter: Arc<dyn ChainAdapter> = dev.clone();
        dev.install_chain_host(ChainHost::new(adapter));
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
          permissions = {"network"},
          identity = { name = "Test Hub" },
          config = { fields = { { key = "host", label = "Host" }, { key = "port", label = "Port" } } },
          transports = { tcp = {} },

          initialize = function(dev)
            if dev.match.index ~= nil then
              dev.transport:write(string.char(0xFE, dev.match.index))
            end
            return true
          end,

          enumerate_controllers = function(dev)
            return {
              { index = 0, name = "Keyboard", device_type = "keyboard", zones = {
                  { id = "main", name = "Main", topology = "linear", led_count = 4 },
              } },
              { index = 1, name = "Mobo", zones = {
                  { id = "aux", name = "Aux", topology = "linear", led_count = 2 },
                  { id = "z2", name = "Zone 2", topology = "linear", led_count = 3 },
              } },
            }
          end,

          -------------------------------
          -- Standard callbacks route through the persistent child match table.

          write_frame = function(dev, zone_id, colors)
            local index = assert(dev.match.index)
            local bytes = { index, string.byte(zone_id, 1) }
            for _, c in ipairs(colors) do
              bytes[#bytes+1] = c.r
              bytes[#bytes+1] = c.g
              bytes[#bytes+1] = c.b
            end
            dev.transport:write(string.char(table.unpack(bytes)))
          end,

          apply = function(dev, state)
            local index = assert(dev.match.index)
            local color = (state.mode == "static") and state.color
            if not color then return end
            -- Zone layout per controller (mirrors enumerate_controllers).
            local zones = index == 0 and {
              { id = "main", n = 4 },
            } or {
              { id = "aux", n = 2 },
              { id = "z2", n = 3 },
            }
            for _, z in ipairs(zones) do
              local cs = {}
              for i = 1, z.n do cs[i] = color end
              -- Reuse the write_frame wire format so tests see the same bytes.
              local bytes = { index, string.byte(z.id, 1) }
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
        let notify = Weak::new();
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
                Vec::<Permission>::new(),
                HashMap::new(),
                notify,
                opening_runtime(),
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
    async fn track_flips_integration_liveness_to_last_result() {
        // The shared root/child runtime state greys the integration on a failed
        // call and self-heals on a success — `is_live()` always mirrors the
        // last result.
        let dev = integration_device(Arc::new(MockTransport::empty()));
        assert!(dev.is_live(), "starts live");
        let _ = dev.track::<()>(Err(anyhow::anyhow!("boom"))).await;
        assert!(!dev.is_live(), "an error greys the integration");
        let _ = dev.track::<()>(Ok(())).await;
        assert!(dev.is_live(), "a success self-heals a transient blip");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn close_is_terminal_and_a_later_success_does_not_resurrect_it() {
        // Closing -> Closed is terminal: a capability call that completes
        // after close() must not flip the device back to live.
        let dev = integration_device(Arc::new(MockTransport::empty()));
        dev.close().await;
        assert!(!dev.is_live(), "closed devices are not live");
        let _ = dev.track::<()>(Ok(())).await;
        assert!(
            !dev.is_live(),
            "a tracked success after close must not resurrect the device"
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn non_integration_device_is_always_live() {
        // A device with no remote backend has no runtime state; `track` never
        // greys it, so engines always drive it.
        let dev = device(Arc::new(MockTransport::empty()));
        assert!(dev.is_live());
        let _ = dev.track::<()>(Err(anyhow::anyhow!("boom"))).await;
        assert!(dev.is_live(), "a backing-less device stays live");
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn resync_children_builds_only_new_and_reports_gone() {
        // enumerate_controllers reports ctrl_0 and ctrl_1. With ctrl_0 already
        // registered and a stale ctrl_9 in `existing`, resync builds only the
        // new ctrl_1 and reports ctrl_9 gone — the survivor is left untouched.
        let dev = integration_device(Arc::new(MockTransport::empty()));
        let existing: std::collections::HashSet<String> = ["integ-0_ctrl_0", "integ-0_ctrl_9"]
            .into_iter()
            .map(String::from)
            .collect();

        let (added, gone) = dev
            .as_controller()
            .unwrap()
            .resync_children(&existing)
            .await
            .unwrap();

        assert_eq!(added.len(), 1, "only the un-registered controller is built");
        assert_eq!(added[0].id(), "integ-0_ctrl_1");
        assert_eq!(gone, vec!["integ-0_ctrl_9".to_string()]);
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
    async fn disabled_integration_child_is_not_initialized() {
        let mock = Arc::new(MockTransport::empty());
        let dev = integration_device(mock.clone());
        let app = Arc::new(crate::state::AppState::new(crate::config::Config::default()));
        app.config.write().await.known_devices.insert(
            "integ-0_ctrl_0".into(),
            crate::registry::config::DeviceRecord {
                name: String::new(),
                vendor: String::new(),
                model: String::new(),
                active_state: VisibilityState::Disabled,
            },
        );

        crate::registry::usecases::registration::register_device_and_children(
            &app,
            dev as Arc<dyn Device>,
        )
        .await;

        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0xFE, 1]],
            "only the enabled controller may run initialize()"
        );
        let devices = app.devices.read().await;
        let disabled = devices.iter().find(|d| d.id() == "integ-0_ctrl_0").unwrap();
        assert_eq!(disabled.active_state(), VisibilityState::Disabled);
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
    async fn dynamic_child_close_keeps_shared_root_and_siblings_live() {
        // Dynamic children own only a route/table in the root VM. Removing a
        // receiver slot or integration child must not close the root runtime
        // or restore the shared transport out from under its siblings.
        let dev = integration_device(Arc::new(MockTransport::empty()));
        let children = dev.as_controller().unwrap().discover_children().await;

        children[0].close().await;
        assert!(dev.is_live(), "closing a child must leave its root live");
        // The closed child's local surface is terminal.
        let keyboard = children[0].as_rgb().expect("has rgb");
        assert!(keyboard
            .write_frame("main", &[RgbColor { r: 1, g: 1, b: 1 }; 4])
            .await
            .is_err());

        // A sibling still routes through the shared worker.
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
          permissions = {"network"},
          identity = { name = "Multi Hub" },
          config = { fields = {
            { key = "host", label = "Host" },
            { key = "port", label = "Port", default = "12345" },
          } },
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
        let notify = Weak::new();
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
                Vec::<Permission>::new(),
                HashMap::new(),
                notify,
                opening_runtime(),
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
        let notify = Weak::new();
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
                Vec::<Permission>::new(),
                HashMap::new(),
                notify,
                opening_runtime(),
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

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn runtime_errors_surface_once_per_failure_episode() {
        use crate::config::Config;
        use crate::ipc::ClientHandle;
        use crate::state::AppState;

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

        let app = Arc::new(AppState::new(Config::default()));
        let (tx, mut rx) = tokio::sync::mpsc::channel::<Arc<Vec<u8>>>(16);
        app.clients.lock().await.push(ClientHandle {
            id: 0,
            tx,
            subs: Arc::default(),
        });

        let dev = LuaDevice::with_transport(
            "err-dev".into(),
            &manifest,
            &manifest.devices[0],
            hid_match(),
            PluginIo::Stream {
                transport: Arc::new(MockTransport::empty()),
                bulk: None,
            },
            tokio::runtime::Handle::current(),
            Vec::<Permission>::new(),
            HashMap::new(),
            Arc::downgrade(&app),
            opening_runtime(),
        );

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
        let err = dev.apply(fail()).await.unwrap_err();
        let surfaced = err
            .downcast_ref::<crate::drivers::plugins::SurfacedPluginError>()
            .expect("foreground error is marked as already surfaced");
        assert_eq!(surfaced.plugin, manifest.plugin_id);
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
