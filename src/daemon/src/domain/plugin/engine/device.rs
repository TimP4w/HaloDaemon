// SPDX-License-Identifier: GPL-3.0-or-later
//! The generic device a plugin instantiates. It sits behind the same `Device`
//! seam as every built-in host device and forwards capability calls into the per-device
//! Lua worker. The manifest defines the maximum capability set; `initialize`
//! may narrow it to the subset supported by one physical device.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, MutexGuard, OnceLock, RwLock, Weak};
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use halod_shared::keyboard::{KeyId, KeyVariant, KeyboardLayoutStatus, VisualKey};
use halod_shared::types::{
    Action, Battery, Boolean, ButtonAction, ButtonDescriptor, ButtonMapping, CategoryLayout,
    Choice, ConnectionStatus, CoolingChannel, DeviceCapability, DeviceType, DpiMode, DpiStatus,
    Equalizer, KeyRemapStatus, KeyboardFormFactor, KeyboardLayout, LcdDescriptor, LightingChannel,
    LightingDescriptor, LightingState, NativeEffect, Permission, PluginKind, Range, ScreenRotation,
    ScreenShape, Sensor, WriteRateStatus,
};
use halod_shared::zone_transform::build_permutation;

use crate::domain::device::chain::{
    ChannelDescriptor, LightingDivisionAdapter, LightingDivisionHost, LightingDivisionHub,
};
use crate::domain::device::{
    ActionCapability, BatteryCapability, BoolStateCache, BooleanCapability, CapabilityRef,
    ChoiceCapability, ChoiceStateCache, ConnectionCapability, Controller, CoolingCapability,
    CoolingHub, CoolingStateSlot, Device, DpiCapability, EqualizerCapability, KeyRemapCapability,
    KeyboardLayoutCapability, KeyboardLayoutSlot, LcdCapability, LcdStateSlot, LightingCapability,
    LightingStateSlot, OnboardProfilesCapability, PairingCapability, RangeCapability,
    RangeStateCache, SensorCapability, VisibilitySlot,
};

use super::chain_leaf::ChainLeaf;
use super::cooling_channel_leaf::CoolingChannelLeaf;
use super::manifest::{
    topology_from, AccessoryManifest, ActionDef, BooleanDef, ChoiceDef, DeviceSpec, PluginManifest,
    RangeDef,
};
use super::transport::PluginIo;
use super::worker::{
    DetectedController, DevMatch, InitControls, InitDpi, InitKeyboard, InitKeyboardVariant,
    InitLcd, InitLightingChannel, PluginHandle,
};
use std::collections::{HashMap, HashSet};

const POLL_FAILURE_DEGRADE_THRESHOLD: u8 = 3;

trait MutexRecover<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T>;
}

impl<T> MutexRecover<T> for Mutex<T> {
    fn lock_recover(&self) -> MutexGuard<'_, T> {
        self.lock().unwrap_or_else(|poisoned| poisoned.into_inner())
    }
}

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
use crate::infrastructure::drivers::vendors::generic::devices::common::{
    linear_lighting_channel, ring_led_positions,
};
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
    Lighting,
    Cooling,
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
    LightingDivision,
}

const CONTROL_CAPS: &[Cap] = &[Cap::Choice, Cap::Range, Cap::Boolean, Cap::Action];
const CAPABILITY_NAMES: &[&str] = &[
    "lighting",
    "cooling",
    "sensors",
    "lcd",
    "dpi",
    "controls",
    "battery",
    "connection",
    "equalizer",
    "pairing",
    "onboard_profiles",
    "key_remap",
    "keyboard_layout",
    "lighting_division",
];

fn cap_for(name: &str) -> &'static [Cap] {
    match name {
        "lighting" => &[Cap::Lighting],
        "cooling" => &[Cap::Cooling],
        "sensors" => &[Cap::Sensor],
        "lcd" => &[Cap::Lcd],
        "dpi" => &[Cap::Dpi],
        "controls" => CONTROL_CAPS,
        "battery" => &[Cap::Battery],
        "connection" => &[Cap::Connection],
        "equalizer" => &[Cap::Equalizer],
        "pairing" => &[Cap::Pairing],
        "onboard_profiles" => &[Cap::OnboardProfiles],
        "key_remap" => &[Cap::KeyRemap],
        "keyboard_layout" => &[Cap::KeyboardLayout],
        "lighting_division" => &[Cap::LightingDivision],
        _ => &[],
    }
}

/// Typed runtime capabilities permitted by the inert catalog. Descriptors and
/// initial values still come from `initialize`; the catalog never supplies
/// static runtime sections.
fn declared_caps(manifest: &PluginManifest) -> Vec<Cap> {
    CAPABILITY_NAMES
        .iter()
        .filter(|name| {
            manifest
                .capabilities
                .iter()
                .any(|declared| declared == **name)
        })
        .flat_map(|name| cap_for(name).iter().copied())
        .collect()
}

fn caps_named(names: &[String]) -> Vec<Cap> {
    let mut caps = Vec::new();
    let mut add = |cap| {
        if !caps.contains(&cap) {
            caps.push(cap);
        }
    };
    for name in names {
        let mapped = cap_for(name);
        if mapped.is_empty() {
            log::warn!("Lua initialize returned unknown capability '{name}'");
        } else {
            mapped.iter().copied().for_each(&mut add);
        }
    }
    caps
}

fn needs_status_poll(caps: &[Cap]) -> bool {
    StatusPollCaps::from_caps(caps).any()
}

#[derive(Clone, Copy, Default)]
struct StatusPollCaps {
    sensor: bool,
    cooling: bool,
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
            cooling: caps.contains(&Cap::Cooling),
            boolean: caps.contains(&Cap::Boolean),
            battery: caps.contains(&Cap::Battery),
            connection: caps.contains(&Cap::Connection),
            equalizer: caps.contains(&Cap::Equalizer),
            always: caps
                .iter()
                .any(|cap| matches!(cap, Cap::KeyRemap | Cap::LightingDivision)),
        }
    }

    fn any(self) -> bool {
        self.sensor
            || self.cooling
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
    cooling_channels: &OnceLock<Vec<CoolingChannel>>,
    cooling_cache: &Mutex<HashMap<String, CoolingChannel>>,
    sensor_cache: &Mutex<Vec<Sensor>>,
    boolean_cache: &Mutex<Vec<Boolean>>,
    battery_cache: &Mutex<Vec<Battery>>,
    connection_cache: &Mutex<Option<ConnectionStatus>>,
    eq_cache: &Mutex<Option<Equalizer>>,
) -> bool {
    let mut changed = false;
    if caps.cooling {
        if let Some(channels) = cooling_channels.get() {
            let mut observed = HashMap::with_capacity(channels.len());
            for channel in channels {
                if let Ok(status) = worker.cooling_status(&channel.id).await {
                    observed.insert(channel.id.clone(), status);
                }
            }
            if !observed.is_empty() {
                changed |= replace_if_changed(cooling_cache, observed);
            }
        }
    }
    if caps.sensor {
        if let Ok(sensors) = worker.get_sensors().await {
            changed |= replace_if_changed(sensor_cache, sensors);
        }
    }
    if caps.boolean {
        if let Ok(booleans) = worker.boolean_get().await {
            changed |= replace_if_changed(boolean_cache, booleans);
        }
    }
    if caps.battery {
        if let Ok(batteries) = worker.battery_get().await {
            changed |= replace_if_changed(battery_cache, batteries);
        }
    }
    if caps.connection {
        if let Ok(connection) = worker.connection_get().await {
            changed |= replace_if_changed(connection_cache, connection);
        }
    }
    if caps.equalizer {
        if let Ok(equalizer) = worker.equalizer_get().await {
            changed |= replace_if_changed(eq_cache, Some(equalizer));
        }
    }
    changed
}

fn replace_if_changed<T: serde::Serialize>(cache: &Mutex<T>, next: T) -> bool {
    let mut current = cache.lock_recover();
    let changed = match (serde_json::to_value(&*current), serde_json::to_value(&next)) {
        (Ok(current), Ok(next)) => current != next,
        _ => true,
    };
    if changed {
        *current = next;
    }
    changed
}

#[cfg(test)]
mod status_poll_tests {
    use super::{needs_status_poll, Cap};

    #[test]
    fn chain_root_polls_for_child_fan_telemetry() {
        assert!(needs_status_poll(&[Cap::LightingDivision]));
    }
}

/// The four "control" capability groups (choice/range/boolean/action) share the
/// same shape — a `Vec<Def>` of declared controls plus a value cache. Grouping
/// them slims `LuaDevice` and hosts the repeated wire/lookup logic in one place.
#[derive(Default)]
struct Controls {
    choices: Vec<ChoiceDef>,
    choice_cache: Arc<ChoiceStateCache>,
    ranges: Vec<RangeDef>,
    range_cache: Arc<RangeStateCache>,
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
            visible_when: r.visible_when.clone(),
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
pub(in crate::domain::plugin) enum DegradeReason {
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
pub(in crate::domain::plugin) enum RuntimeState {
    OpeningTransport,
    Initializing,
    Online,
    Degraded(DegradeReason),
    /// A deterministic failure (for example denied host access or a plugin
    /// escaping its declared I/O scope). Automatic calls and reconnects stop
    /// until an explicit lifecycle action constructs a fresh runtime.
    Unrecoverable,
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
    control_layout: Vec<CategoryLayout>,
    visibility: VisibilitySlot,
    transport_kind: &'static str,
    dynamic_model: OnceLock<String>,
    worker: Option<PluginHandle>,
    transport: Option<PluginIo>,
    /// The shared HTTP capability runtime (integration roots and their children
    /// hold the same one) so throughput folds HTTP bytes into `write_rate_status`.
    http: Option<crate::infrastructure::http::HttpRuntime>,
    /// Local registry lifetime. Dynamic children share their root's runtime,
    /// so closing a child must be tracked separately from closing the root.
    closed: Arc<AtomicBool>,
    /// One-shot latch for cleanup/reporting during a failing call episode.
    call_failed: Arc<AtomicBool>,
    /// Terminal transport failure for every plugin kind, including plain
    /// device plugins that do not have an integration runtime.
    unrecoverable: Arc<AtomicBool>,
    /// For integration roots only: the manifest each controller child is
    /// reported by the child's own `initialize` callback.
    root_manifest: Option<Arc<PluginManifest>>,
    /// Integration runtime lifecycle, shared by a root and all its children
    /// (one socket pool → one state). `None` for non-integration devices
    /// (always live). See [`RuntimeState`].
    runtime: Option<Arc<Mutex<RuntimeState>>>,

    /// Capability sections the manifest declared, in advertised order. Drives
    /// `capabilities()` (chain also implies `Controller`; integration adds one).
    allowed_caps: Vec<Cap>,
    caps: Arc<RwLock<Vec<Cap>>>,

    dpi_state: Mutex<DpiState>,
    dpi_config: Mutex<DpiConfig>,

    /// Declared choice/range/boolean/action controls + their value caches.
    controls: Controls,
    dynamic_controls: Arc<OnceLock<Controls>>,

    /// Last status samples. Full IPC serialization reads these caches instead
    /// of queueing callbacks on the worker used by interactive commands.
    eq_cache: Arc<Mutex<Option<Equalizer>>>,
    boolean_cache: Arc<Mutex<Vec<Boolean>>>,
    battery_cache: Arc<Mutex<Vec<Battery>>>,
    connection_cache: Arc<Mutex<Option<ConnectionStatus>>>,
    cooling_cache: Arc<Mutex<HashMap<String, CoolingChannel>>>,

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

    rgb_descriptor: LightingDescriptor,
    /// RGB channels discovered at `initialize()` (dynamic LED counts). Overrides
    /// `rgb_descriptor` when set.
    dynamic_rgb_descriptor: OnceLock<LightingDescriptor>,
    /// ISO keyboard geometry for runtime-described keyboards. The active
    /// descriptor is selected from the shared layout slot on every snapshot.
    dynamic_rgb_iso_descriptor: OnceLock<LightingDescriptor>,
    rgb_slot: LightingStateSlot,
    cooling_slot: CoolingStateSlot,
    cooling_channels: Arc<OnceLock<Vec<CoolingChannel>>>,
    cooling_as_devices: AtomicBool,

    /// Last sensor telemetry sampled by the status poll.
    sensor_cache: Arc<Mutex<Vec<Sensor>>>,

    /// Host-run status poll: aborted on drop. `poll_paused` lets a future LCD
    /// path silence polling during a bulk transfer without tearing it down.
    poll_task: Option<tokio::task::JoinHandle<()>>,
    poll_paused: Arc<AtomicBool>,
    /// Event-driven transports wake this task when their bounded host queue
    /// receives input. The task only enqueues work on the root Lua worker.
    event_task: Option<tokio::task::JoinHandle<()>>,
    event_paused: Arc<AtomicBool>,
    event_resume: Arc<tokio::sync::Notify>,
    dynamic_child_ids: Arc<RwLock<HashMap<u32, String>>>,

    /// Host-owned virtual-audio sink registry, drained on close; the guard tears
    /// remaining sinks down on drop even if the worker is dead.
    audio_registry: Option<super::audio_api::SinkRegistry>,
    audio_guard: Option<super::audio_api::AudioGuard>,

    // ── chain / children (reported by initialize) ─────────────────────────
    /// Set after construction (needs the `Arc<Self>`); `None` for non-chain devices.
    chain_host: OnceLock<Arc<LightingDivisionHost>>,
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
    notify: Weak<crate::application::state::AppState>,
}

pub(in crate::domain::plugin) struct LuaDeviceParts<'a> {
    pub id: String,
    pub manifest: &'a PluginManifest,
    pub spec: Option<&'a DeviceSpec>,
    pub notify: Weak<crate::application::state::AppState>,
    pub runtime: Option<Arc<Mutex<RuntimeState>>>,
    pub worker: LuaDeviceWorker,
}

pub(in crate::domain::plugin) enum LuaDeviceWorker {
    None,
    Spawn(Box<LuaDeviceSpawnParts>),
    Child(Box<LuaDeviceChildParts>),
}

pub(in crate::domain::plugin) struct LuaDeviceSpawnParts {
    pub dev_match: DevMatch,
    pub transport: PluginIo,
    pub handle: tokio::runtime::Handle,
    pub granted: Vec<Permission>,
    pub config: crate::domain::plugin::ResolvedConfig,
    pub data: super::data_api::DataRuntime,
}

pub(in crate::domain::plugin) struct LuaDeviceChildParts {
    pub dev_match: DevMatch,
    pub worker: PluginHandle,
    pub transport: PluginIo,
    pub http: Option<crate::infrastructure::http::HttpRuntime>,
    pub name: String,
    pub vendor: String,
    pub device_type: DeviceType,
    pub transport_kind: &'static str,
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
    fn transport_is_unrecoverable(&self) -> bool {
        self.transport
            .as_ref()
            .and_then(PluginIo::unrecoverable_error)
            .is_some()
    }

    fn set_call_failure_state(&self, reason: DegradeReason) {
        let unrecoverable = self.transport_is_unrecoverable();
        if unrecoverable {
            self.unrecoverable.store(true, Ordering::Release);
        }
        if let Some(runtime) = &self.runtime {
            let mut state = runtime.lock_recover();
            if !matches!(*state, RuntimeState::Closing | RuntimeState::Closed) {
                *state = if unrecoverable {
                    RuntimeState::Unrecoverable
                } else {
                    RuntimeState::Degraded(reason)
                };
            }
        }
    }

    pub(in crate::domain::plugin) fn new(parts: LuaDeviceParts<'_>) -> Self {
        let LuaDeviceParts {
            id,
            manifest,
            spec,
            notify,
            runtime,
            worker,
        } = parts;
        let LuaDeviceWorker::Spawn(spawn) = worker else {
            return match worker {
                LuaDeviceWorker::None => Self::build_base(id, manifest, spec, None, None, notify),
                LuaDeviceWorker::Child(child) => {
                    let LuaDeviceChildParts {
                        dev_match,
                        worker,
                        transport,
                        http,
                        name,
                        vendor,
                        device_type,
                        transport_kind,
                    } = *child;
                    let mut dev = Self::build_base(
                        id,
                        manifest,
                        spec,
                        Some(worker.child(dev_match)),
                        Some(transport),
                        notify,
                    );
                    dev.runtime = runtime;
                    dev.http = http;
                    dev.name = name;
                    dev.vendor = vendor;
                    dev.device_type = device_type;
                    dev.transport_kind = transport_kind;
                    dev
                }
                LuaDeviceWorker::Spawn(_) => unreachable!(),
            };
        };
        let LuaDeviceSpawnParts {
            dev_match,
            transport,
            handle,
            granted,
            config,
            data,
        } = *spawn;
        let transport_kind = super::transport::descriptor_for(&dev_match.transport)
            .map(|descriptor| descriptor.kind)
            .unwrap_or("tcp");
        if let Some(runtime) = &runtime {
            *runtime.lock_recover() = RuntimeState::Initializing;
        }
        let receiver_root = manifest.plugin_id == "logitech" && dev_match.pid == Some(0xc547);
        let nuvoton_sensor_root = manifest.plugin_id == "nuvoton_lpcio" && dev_match.key.is_none();
        // Keep a handle to the (metered) transport so the device can report
        // write-rate/throughput; the worker owns the one it does I/O through.
        let rate_transport = transport.clone();
        let event_receiver = match &rate_transport {
            PluginIo::Stream { transport, .. } => transport.event_receiver(),
            _ => None,
        };
        // Canonical packages report physical channels from initialize. Do not
        // seed the worker from the removed static RGB catalog section.
        let channels = Vec::new();
        let audio_registry: super::audio_api::SinkRegistry =
            std::sync::Arc::new(std::sync::Mutex::new(Vec::new()));
        let http = super::worker::http_runtime_for(manifest, &granted, &config);
        let http_meter = http.clone();
        let udp = super::worker::udp_runtime_for(manifest, &granted, &config);
        let worker = PluginHandle::spawn_with_data(
            manifest.script_source.clone(),
            manifest.module_sources.clone(),
            transport,
            dev_match,
            granted,
            config,
            handle.clone(),
            channels,
            audio_registry.clone(),
            data,
            http,
            udp,
        );
        let poll_device_id = id.clone();
        let poll_notify = notify.clone();
        let mut dev = Self::build_base(
            id,
            manifest,
            spec,
            Some(worker.clone()),
            Some(rate_transport),
            notify,
        );
        dev.runtime = runtime;
        dev.http = http_meter;
        dev.transport_kind = transport_kind;
        dev.audio_registry = Some(audio_registry.clone());
        dev.audio_guard = Some(super::audio_api::AudioGuard::new(
            audio_registry,
            handle.clone(),
        ));

        if let Some(mut events) = event_receiver {
            dev.event_paused.store(true, Ordering::Relaxed);
            let paused = dev.event_paused.clone();
            let resume = dev.event_resume.clone();
            let closed = dev.closed.clone();
            let unrecoverable = dev.unrecoverable.clone();
            let event_runtime = dev.runtime.clone();
            let event_worker = worker.clone();
            let event_notify = poll_notify.clone();
            let root_id = poll_device_id.clone();
            let child_ids = dev.dynamic_child_ids.clone();
            dev.event_task = Some(handle.spawn(async move {
                loop {
                    tokio::select! {
                        changed = events.changed() => {
                            if changed.is_err() {
                                break;
                            }
                        }
                        _ = resume.notified() => {
                            if closed.load(Ordering::Acquire) {
                                break;
                            }
                            continue;
                        }
                    }
                    if closed.load(Ordering::Acquire)
                        || unrecoverable.load(Ordering::Acquire)
                        || event_runtime.as_ref().is_some_and(|runtime| {
                            matches!(*runtime.lock_recover(), RuntimeState::Unrecoverable)
                        })
                    {
                        break;
                    }
                    while paused.load(Ordering::Acquire) {
                        resume.notified().await;
                        if closed.load(Ordering::Acquire) {
                            return;
                        }
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
                        let device_id = outcome
                            .child_index
                            .and_then(|index| child_ids.read().unwrap().get(&index).cloned())
                            .unwrap_or_else(|| root_id.clone());
                        if outcome.children_changed {
                            if let Some(root) = app.find_device_by_id(&root_id).await {
                                // Receiver firmware may announce lock closure
                                // before its pairing table exposes the new slot.
                                // Retry briefly, as the built-in receiver does.
                                for _ in 0..8 {
                                    if crate::domain::registry::usecases::receiver::reconcile_owned_children(
                                        &root, &app,
                                    )
                                    .await
                                    {
                                        break;
                                    }
                                    tokio::time::sleep(Duration::from_millis(500)).await;
                                }
                            }
                            crate::domain::plugin::usecases::runtime::topology_changed(&app).await;
                        }
                        if outcome.state_changed {
                            crate::domain::plugin::usecases::runtime::device_changed(&app, &device_id).await;
                        }
                        if outcome.button_events.pressed.is_empty()
                            && outcome.button_events.released.is_empty()
                        {
                            continue;
                        }
                        log::trace!(
                            "[plugin event] button event device={device_id} child_index={:?} pressed={:?} released={:?}",
                            outcome.child_index,
                            outcome.button_events.pressed,
                            outcome.button_events.released
                        );
                        app.input_events.publish(crate::application::state::ButtonEvent {
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
        if needs_status_poll(&dev.caps.read().unwrap())
            && !(manifest.plugin_type == PluginKind::Integration && spec.is_none())
        {
            dev.poll_paused.store(true, Ordering::Relaxed);
            let interval = Duration::from_millis(manifest.poll_interval_ms);
            let paused = dev.poll_paused.clone();
            let poll_caps = Arc::clone(&dev.caps);
            let sensor_cache = dev.sensor_cache.clone();
            let boolean_cache = dev.boolean_cache.clone();
            let battery_cache = dev.battery_cache.clone();
            let connection_cache = dev.connection_cache.clone();
            let eq_cache = dev.eq_cache.clone();
            let dynamic_controls = Arc::clone(&dev.dynamic_controls);
            let cooling_cache = Arc::clone(&dev.cooling_cache);
            let cooling_channels = Arc::clone(&dev.cooling_channels);
            let closed = dev.closed.clone();
            let unrecoverable = dev.unrecoverable.clone();
            let runtime = dev.runtime.clone();
            let poll_transport = dev.transport.clone();
            let poll_plugin_id = dev.plugin_id.clone();
            dev.poll_task = Some(handle.spawn(async move {
                // `initialize()` seeds the status caches before it releases
                // this task.  Wait a full interval before the next read rather
                // than immediately polling the transport a second time.
                let mut ticker = tokio::time::interval_at(
                    tokio::time::Instant::now() + interval,
                    interval,
                );
                ticker.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
                let mut consecutive_failures = 0u8;
                loop {
                    ticker.tick().await;
                    if let Some(detail) = poll_transport
                        .as_ref()
                        .and_then(PluginIo::unrecoverable_error)
                    {
                        unrecoverable.store(true, Ordering::Release);
                        if let Some(runtime) = &runtime {
                            *runtime.lock_recover() = RuntimeState::Unrecoverable;
                        }
                        if let Some(app) = poll_notify.upgrade() {
                            app.registry
                                .report_runtime_error(
                                    &app,
                                    &poll_plugin_id,
                                    &poll_device_id,
                                    detail,
                                )
                                .await;
                            crate::domain::plugin::usecases::runtime::device_status_changed(
                                &app,
                                &poll_device_id,
                            )
                            .await;
                        }
                        break;
                    }
                    if closed.load(Ordering::Acquire)
                        || unrecoverable.load(Ordering::Acquire)
                    {
                        break;
                    }
                    if paused.load(Ordering::Relaxed) {
                        continue;
                    }
                    let mut observed_changed = false;
                    match worker.poll().await {
                        Ok(events) => {
                            if let Some(controls) = dynamic_controls.get() {
                                for (key, value) in &events.ranges {
                                    observed_changed |= controls.range_cache.record(key, *value);
                                }
                                for (key, selected) in &events.choices {
                                    observed_changed |=
                                        controls.choice_cache.record(key, *selected);
                                }
                            }
                            if !events.cooling.is_empty() {
                                observed_changed |= replace_if_changed(
                                    &cooling_cache,
                                    events
                                        .cooling
                                        .iter()
                                        .cloned()
                                        .map(|channel| (channel.id.clone(), channel))
                                        .collect(),
                                );
                            }
                            let recovered = consecutive_failures >= POLL_FAILURE_DEGRADE_THRESHOLD;
                            consecutive_failures = 0;
                            if recovered {
                                if let Some(runtime) = &runtime {
                                    let mut state = runtime.lock_recover();
                                    if !matches!(
                                        *state,
                                        RuntimeState::Unrecoverable
                                            | RuntimeState::Closing
                                            | RuntimeState::Closed
                                    )
                                    {
                                        *state = RuntimeState::Online;
                                    }
                                }
                                if let Some(app) = poll_notify.upgrade() {
                                    crate::domain::plugin::usecases::runtime::device_changed(
                                        &app,
                                        &poll_device_id,
                                    )
                                    .await;
                                }
                            }
                            observed_changed |= events.state_changed;
                            if !events.button_events.pressed.is_empty()
                                || !events.button_events.released.is_empty()
                            {
                                if let Some(app) = poll_notify.upgrade() {
                                    app.input_events.publish(crate::application::state::ButtonEvent {
                                            device_id: poll_device_id.clone(),
                                            pressed: events.button_events.pressed,
                                            released: events.button_events.released,
                                        });
                                }
                            }
                        }
                        Err(error) => {
                            if poll_transport
                                .as_ref()
                                .and_then(PluginIo::unrecoverable_error)
                                .is_some()
                            {
                                unrecoverable.store(true, Ordering::Release);
                                if let Some(runtime) = &runtime {
                                    *runtime.lock_recover() = RuntimeState::Unrecoverable;
                                }
                                let detail = format!("{error:#}");
                                if let Some(app) = poll_notify.upgrade() {
                                    app.registry
                                        .report_runtime_error(
                                            &app,
                                            &poll_plugin_id,
                                            &poll_device_id,
                                            detail,
                                        )
                                        .await;
                                    crate::domain::plugin::usecases::runtime::device_status_changed(
                                        &app,
                                        &poll_device_id,
                                    )
                                    .await;
                                }
                                break;
                            }
                            consecutive_failures = consecutive_failures.saturating_add(1);
                            if consecutive_failures == POLL_FAILURE_DEGRADE_THRESHOLD {
                                log::warn!(
                                    "plugin read_status failed {POLL_FAILURE_DEGRADE_THRESHOLD} consecutive times for '{poll_device_id}': {error:#}"
                                );
                                if let Some(runtime) = &runtime {
                                    let mut state = runtime.lock_recover();
                                    if !matches!(*state, RuntimeState::Closing | RuntimeState::Closed)
                                    {
                                        *state = RuntimeState::Degraded(DegradeReason::CallFailed);
                                    }
                                }
                                if let Some(app) = poll_notify.upgrade() {
                                    crate::domain::plugin::usecases::runtime::device_changed(
                                        &app,
                                        &poll_device_id,
                                    )
                                    .await;
                                }
                            }
                            continue;
                        }
                    }
                    let current_poll_caps = {
                        let caps = poll_caps.read().unwrap();
                        StatusPollCaps::from_caps(&caps)
                    };
                    observed_changed |= sample_status(
                        &worker,
                        current_poll_caps,
                        &cooling_channels,
                        &cooling_cache,
                        &sensor_cache,
                        &boolean_cache,
                        &battery_cache,
                        &connection_cache,
                        &eq_cache,
                    )
                    .await;
                    if observed_changed {
                        if let Some(app) = poll_notify.upgrade() {
                            crate::domain::plugin::usecases::runtime::device_changed(
                                &app,
                                &poll_device_id,
                            )
                            .await;
                        }
                    }
                }
            }));
        }
        if manifest.dynamic_children || spec.is_none() {
            dev.root_manifest = Some(Arc::new(manifest.clone()));
        }
        if receiver_root {
            dev.caps
                .write()
                .unwrap()
                .retain(|cap| matches!(cap, Cap::Pairing));
        }
        if nuvoton_sensor_root {
            // The matched Super-I/O is the sensor controller. Its dynamic
            // children own the individual PWM channels; retaining `Cooling`
            // here makes the controller itself appear in the Cooling UI.
            dev.caps
                .write()
                .unwrap()
                .retain(|cap| !matches!(cap, Cap::Cooling));
        }
        dev
    }

    fn build_base(
        id: String,
        manifest: &PluginManifest,
        spec: Option<&DeviceSpec>,
        worker: Option<PluginHandle>,
        transport: Option<PluginIo>,
        notify: Weak<crate::application::state::AppState>,
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
            control_layout: spec.map(|s| s.control_layout.clone()).unwrap_or_default(),
            visibility: VisibilitySlot::default(),
            transport_kind: spec
                .and_then(|s| super::transport::descriptor_for(&s.transport))
                .map(|d| d.kind)
                .unwrap_or("tcp"),
            dynamic_model: OnceLock::new(),
            worker,
            transport,
            http: None,
            closed: Arc::new(AtomicBool::new(false)),
            call_failed: Arc::new(AtomicBool::new(false)),
            unrecoverable: Arc::new(AtomicBool::new(false)),
            root_manifest: None,
            runtime: None,
            allowed_caps: declared_caps(manifest),
            caps: Arc::new(RwLock::new(declared_caps(manifest))),
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
            dynamic_controls: Arc::new(OnceLock::new()),
            eq_cache: Arc::new(Mutex::new(None)),
            boolean_cache: Arc::new(Mutex::new(Vec::new())),
            battery_cache: Arc::new(Mutex::new(Vec::new())),
            connection_cache: Arc::new(Mutex::new(None)),
            cooling_cache: Arc::new(Mutex::new(HashMap::new())),
            key_remap: RwLock::new(KeyRemapDescriptor::default()),
            key_remap_mappings: Mutex::new(HashMap::new()),
            keyboard_layout: KeyboardLayoutSlot::default(),
            keyboard_descriptor: OnceLock::new(),
            rgb_descriptor: LightingDescriptor {
                channels: Vec::new(),
                native_effects: Vec::new(),
            },
            dynamic_rgb_descriptor: OnceLock::new(),
            dynamic_rgb_iso_descriptor: OnceLock::new(),
            rgb_slot: LightingStateSlot::default(),
            cooling_slot: CoolingStateSlot::default(),
            cooling_channels: Arc::new(OnceLock::new()),
            cooling_as_devices: AtomicBool::new(false),
            sensor_cache: Arc::new(Mutex::new(Vec::new())),
            poll_task: None,
            poll_paused: Arc::new(AtomicBool::new(false)),
            event_task: None,
            event_paused: Arc::new(AtomicBool::new(false)),
            event_resume: Arc::new(tokio::sync::Notify::new()),
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
    pub(in crate::domain::plugin) fn set_self_ref(&mut self, weak: Weak<LuaDevice>) {
        self.self_ref = weak;
    }

    /// Install the chain host (built from `Arc<Self>` as the adapter).
    pub(in crate::domain::plugin) fn install_chain_host(&self, host: Arc<LightingDivisionHost>) {
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
        let unrecoverable = result.is_err() && self.transport_is_unrecoverable();
        let first_failure = if result.is_err() {
            !self.call_failed.swap(true, Ordering::AcqRel)
        } else {
            self.call_failed.store(false, Ordering::Release);
            false
        };
        if unrecoverable {
            self.unrecoverable.store(true, Ordering::Release);
        }
        // Restore once when an error episode begins. Repeated engine calls can
        // race with the state broadcast, so `call_failed` is the latch.
        if let Some(r) = &self.runtime {
            let mut state = r.lock_recover();
            if !matches!(
                *state,
                RuntimeState::Unrecoverable | RuntimeState::Closing | RuntimeState::Closed
            ) {
                if result.is_ok() {
                    *state = RuntimeState::Online;
                } else {
                    *state = if unrecoverable {
                        RuntimeState::Unrecoverable
                    } else {
                        RuntimeState::Degraded(DegradeReason::CallFailed)
                    };
                }
            }
        }
        if first_failure {
            if let Some(transport) = &self.transport {
                transport.restore_safety_state();
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
                Err(crate::domain::plugin::SurfacedPluginError {
                    plugin: self.plugin_id.clone(),
                    detail,
                }
                .into())
            }
        }
    }

    /// Refresh the host-side sensor/fan caches from the worker. Driven by the
    /// status poll (and `poll_once`); serialization never calls this.
    async fn refresh_status_cache(&self) {
        let Some(worker) = &self.worker else { return };
        let caps = StatusPollCaps::from_caps(&self.caps.read().unwrap());
        sample_status(
            worker,
            caps,
            &self.cooling_channels,
            &self.cooling_cache,
            &self.sensor_cache,
            &self.boolean_cache,
            &self.battery_cache,
            &self.connection_cache,
            &self.eq_cache,
        )
        .await;
    }

    #[cfg(feature = "plugin-test")]
    pub(in crate::domain::plugin) async fn poll_once(&self) -> Result<()> {
        let outcome = self.worker()?.poll().await?;
        if let Some(controls) = self.dynamic_controls.get() {
            for (key, value) in outcome.ranges {
                controls.range_cache.record(&key, value);
            }
            for (key, selected) in outcome.choices {
                controls.choice_cache.record(&key, selected);
            }
        }
        if !outcome.cooling.is_empty() {
            replace_if_changed(
                &self.cooling_cache,
                outcome
                    .cooling
                    .into_iter()
                    .map(|channel| (channel.id.clone(), channel))
                    .collect(),
            );
        }
        self.refresh_status_cache().await;
        Ok(())
    }

    /// Drain the transport's queued events once through `event()`, returning the
    /// per-target outcomes. Production drives this from the event watcher task;
    /// the plugin-test harness calls it directly.
    #[cfg(feature = "plugin-test")]
    pub(in crate::domain::plugin) async fn pump_events(
        &self,
    ) -> Result<Vec<super::worker::PollOutcome>> {
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

/// Build an `LightingDescriptor` from `initialize`-reported channels, computing LED
/// positions from the declared topology + count (as static accessory channels do).
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
    crate::infrastructure::drivers::effective_layout(
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
    channels: Vec<InitLightingChannel>,
    native_effects: Vec<NativeEffect>,
    keyboard_keys: Option<&[VisualKey]>,
) -> LightingDescriptor {
    let channels = channels
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
            // `ring_led_positions` only lays out ring topologies; linear channels use
            // the evenly-spaced strip layout (as the former built-in drivers did).
            if !z.leds.is_empty() {
                LightingChannel {
                    leds: z.leds,
                    id: z.id,
                    name: z.name,
                    topology,
                    color_order: Default::default(),
                    division: Default::default(),
                }
            } else if matches!(topology, ZoneTopology::Keyboard { .. })
                && keyboard_keys.is_some_and(|keys| !keys.is_empty())
            {
                LightingChannel {
                    leds: keyboard_led_positions(keyboard_keys.unwrap(), &z.led_ids),
                    id: z.id,
                    name: z.name,
                    topology,
                    color_order: Default::default(),
                    division: Default::default(),
                }
            } else if !z.led_ids.is_empty() {
                let columns = if matches!(topology, ZoneTopology::Keyboard { .. }) {
                    18
                } else {
                    z.led_ids.len().max(1)
                };
                let rows = z.led_ids.len().div_ceil(columns).max(1);
                LightingChannel {
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
                    color_order: Default::default(),
                    division: Default::default(),
                }
            } else if matches!(topology, ZoneTopology::Linear) {
                linear_lighting_channel(&z.id, &z.name, led_count as usize)
            } else {
                LightingChannel {
                    leds: ring_led_positions(&topology, led_count),
                    id: z.id,
                    name: z.name,
                    topology,
                    color_order: Default::default(),
                    division: Default::default(),
                }
            }
        })
        .collect();
    LightingDescriptor {
        channels,
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
            vec![InitLightingChannel {
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

        let zone = &descriptor.channels[0];
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

    fn control_layout(&self) -> Vec<CategoryLayout> {
        self.control_layout.clone()
    }

    fn keyboard_layout_slot(&self) -> Option<&KeyboardLayoutSlot> {
        self.allowed_caps
            .contains(&Cap::KeyboardLayout)
            .then_some(&self.keyboard_layout)
    }

    fn integration_id(&self) -> Option<String> {
        let is_child = self.worker.as_ref().is_some_and(|w| w.is_child());
        integration_root_id(self.plugin_type, &self.plugin_id, is_child)
    }

    fn owning_plugin_id(&self) -> Option<String> {
        Some(self.plugin_id.clone())
    }

    fn usb_location(&self) -> Option<crate::infrastructure::drivers::transports::usb::UsbLocation> {
        self.transport.as_ref().and_then(PluginIo::usb_location)
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
        if self.closed.load(Ordering::Acquire) || self.unrecoverable.load(Ordering::Acquire) {
            return false;
        }
        self.runtime
            .as_ref()
            .is_none_or(|r| match *r.lock_recover() {
                RuntimeState::OpeningTransport
                | RuntimeState::Initializing
                | RuntimeState::Online => true,
                RuntimeState::Degraded(_) => false,
                RuntimeState::Unrecoverable => false,
                RuntimeState::Closing | RuntimeState::Closed => false,
            })
    }

    fn is_unrecoverable(&self) -> bool {
        self.unrecoverable.load(Ordering::Acquire)
            || self.runtime.as_ref().is_some_and(|runtime| {
                matches!(*runtime.lock_recover(), RuntimeState::Unrecoverable)
            })
    }

    fn wire_device_connected(&self) -> bool {
        self.is_live()
    }

    async fn initialize(&self) -> Result<bool> {
        let Some(w) = &self.worker else {
            return Ok(true);
        };
        let outcome = match w.initialize().await {
            Ok(outcome) => outcome,
            Err(error) => {
                self.set_call_failure_state(DegradeReason::CallFailed);
                if !self.call_failed.swap(true, Ordering::AcqRel) {
                    if let Some(transport) = &self.transport {
                        transport.restore_safety_state();
                    }
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
            let _ = self.keyboard_descriptor.set(keyboard);
        }
        if let Some(channels) = outcome.channels {
            let effects = outcome.native_effects.unwrap_or_default();
            let keyboard = self.keyboard_descriptor.get();
            let ansi_keys = keyboard.map(|descriptor| visual_keys(&descriptor.ansi));
            let _ = self.dynamic_rgb_descriptor.set(build_dynamic_descriptor(
                channels.clone(),
                effects.clone(),
                ansi_keys.as_deref(),
            ));
            if let Some(iso) = keyboard.and_then(|descriptor| descriptor.iso.as_ref()) {
                let iso_keys = visual_keys(iso);
                let _ = self
                    .dynamic_rgb_iso_descriptor
                    .set(build_dynamic_descriptor(channels, effects, Some(&iso_keys)));
            }
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
        if let Some(channels) = outcome.division {
            let descriptors = channels
                .into_iter()
                .map(|c| ChannelDescriptor {
                    channel_id: c.id,
                    display_name: c.name,
                    max_leds: c.max_leds,
                    color_order: c.color_order,
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
            *self.dpi_config.lock_recover() = DpiConfig {
                min: dpi.min,
                max: dpi.max,
                mode: if dpi.onboard {
                    DpiMode::Onboard
                } else {
                    DpiMode::Host
                },
                mode_control: dpi.mode_control.clone(),
            };
            *self.dpi_state.lock_recover() = dpi_state_from_runtime(&dpi);
        }
        if let Some(cooling) = outcome.cooling {
            self.cooling_as_devices
                .store(cooling.as_devices, Ordering::Release);
            let _ = self.cooling_channels.set(
                cooling
                    .channels
                    .into_iter()
                    .map(|c| CoolingChannel {
                        id: c.id,
                        name: c.name,
                        kind: c.kind,
                        controllable: c.controllable,
                        rpm: None,
                        duty: None,
                    })
                    .collect(),
            );
        }
        if let Some(key_remap) = outcome.key_remap {
            let defaults = key_remap.default_mappings.clone();
            *self.key_remap.write().unwrap() = KeyRemapDescriptor {
                buttons: key_remap.buttons,
                requires_host_mode: key_remap.requires_host_mode,
                default_mappings: key_remap.default_mappings,
            };
            let to_arm: Vec<ButtonMapping> = {
                let mut mappings = self.key_remap_mappings.lock_recover();
                if mappings.is_empty() {
                    mappings.extend(defaults.into_iter().map(|mapping| (mapping.cid, mapping)));
                }
                mappings
                    .values()
                    .filter(|m| m.base != ButtonAction::Native || m.shifted != ButtonAction::Native)
                    .cloned()
                    .collect()
            };
            if !to_arm.is_empty() {
                log::debug!(
                    "[{}] re-arming {} active key mapping(s) on connect",
                    self.id,
                    to_arm.len()
                );
            }
            for mapping in to_arm {
                let cid = mapping.cid;
                if let Err(e) = w.key_remap_set_mapping(mapping).await {
                    log::warn!("[{}] key_remap re-arm failed for cid {cid}: {e:#}", self.id);
                }
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
        if let Some(detail) = self
            .transport
            .as_ref()
            .and_then(PluginIo::unrecoverable_error)
        {
            self.set_call_failure_state(DegradeReason::CallFailed);
            if !self.call_failed.swap(true, Ordering::AcqRel) {
                if let Some(transport) = &self.transport {
                    transport.restore_safety_state();
                }
            }
            anyhow::bail!("unrecoverable plugin transport error: {detail}");
        }
        if outcome.ok {
            if let Some(runtime) = &self.runtime {
                *runtime.lock_recover() = RuntimeState::Online;
            }
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
            self.event_resume.notify_one();
        } else if let Some(runtime) = &self.runtime {
            *runtime.lock_recover() = RuntimeState::Degraded(DegradeReason::CallFailed);
        }
        Ok(outcome.ok)
    }

    async fn close(&self) {
        if self.closed.swap(true, Ordering::AcqRel) {
            return;
        }
        self.event_resume.notify_one();
        let owns_root_resources = self.worker.as_ref().is_none_or(|worker| !worker.is_child());
        if owns_root_resources {
            if let Some(r) = &self.runtime {
                *r.lock_recover() = RuntimeState::Closing;
            }
        }
        if let Some(w) = &self.worker {
            if let Err(error) = w.close().await {
                log::warn!(
                    "plugin close hook did not complete for '{}': {error:#}",
                    self.id
                );
            }
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
                *r.lock_recover() = RuntimeState::Closed;
            }
        }
    }

    fn write_rate_status(&self) -> Option<WriteRateStatus> {
        let transport = self.transport.as_ref().map(|t| t.rate_status());
        let http = self.http.as_ref().map(|h| h.rate_status());
        match (transport, http) {
            (Some(t), Some(h)) => {
                Some(crate::infrastructure::drivers::WriteRateLimiter::combine_status(t, h))
            }
            (t, h) => t.or(h),
        }
    }

    fn debug_transport(&self) -> Option<&'static str> {
        Some(self.transport_kind)
    }

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let mut caps = Vec::new();
        let active = self.caps.read().unwrap().clone();
        for cap in &active {
            match cap {
                Cap::Lighting => caps.push(CapabilityRef::Lighting(self)),
                Cap::Cooling if !self.cooling_as_devices.load(Ordering::Acquire) => {
                    caps.push(CapabilityRef::Cooling(self));
                }
                Cap::Cooling => {}
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
                Cap::LightingDivision => {
                    caps.push(CapabilityRef::Controller(self));
                }
            }
        }
        if self.plugin_type == PluginKind::Integration || self.dynamic_children {
            caps.push(CapabilityRef::Controller(self));
        }
        if self.cooling_as_devices.load(Ordering::Acquire)
            && !caps
                .iter()
                .any(|cap| matches!(cap, CapabilityRef::Controller(_)))
        {
            caps.push(CapabilityRef::Controller(self));
        }
        caps
    }

    fn chain_host(&self) -> Option<&Arc<LightingDivisionHost>> {
        self.chain_host.get()
    }
}

#[async_trait]
impl LightingCapability for LuaDevice {
    fn descriptor(&self) -> &LightingDescriptor {
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

    async fn apply(&self, state: LightingState) -> Result<()> {
        let state = apply_per_led_transforms(self.descriptor(), &self.rgb_slot, state);
        self.rgb_slot.set_state(Some(state.clone()));
        let r = self.worker()?.rgb_apply(state).await;
        self.track(r).await
    }

    async fn write_frame(&self, channel_id: &str, bytes: &[u8]) -> Result<()> {
        let r = self.worker()?.write_lighting_frame(channel_id, bytes).await;
        self.track(r).await
    }

    async fn write_frame_batch(&self, channels: &[(String, Vec<u8>)]) -> Result<()> {
        for (channel_id, bytes) in channels {
            self.write_frame(channel_id, bytes).await?;
        }
        Ok(())
    }

    fn lighting_state(&self) -> &LightingStateSlot {
        &self.rgb_slot
    }
}

/// Apply each zone's content transform to PerLed colour maps before handing the
/// state to the plugin, so the plugin never needs to understand transforms.
fn apply_per_led_transforms(
    descriptor: &LightingDescriptor,
    slot: &LightingStateSlot,
    state: LightingState,
) -> LightingState {
    let LightingState::PerLed { channels } = state else {
        return state;
    };
    let mut transformed = HashMap::new();
    for (channel_id, led_map) in &channels {
        let Some(zone) = descriptor.channels.iter().find(|z| &z.id == channel_id) else {
            transformed.insert(channel_id.clone(), led_map.clone());
            continue;
        };
        let transform = slot.transform_for(channel_id);
        if transform.is_identity() {
            transformed.insert(channel_id.clone(), led_map.clone());
            continue;
        }
        let permutation = build_permutation(zone, &transform);
        let new_map = (0..zone.leds.len())
            .filter_map(|i| {
                let source = permutation
                    .get(i)
                    .and_then(|&source| zone.leds.get(source))?;
                let target = zone.leds.get(i)?;
                let source_id = source.id.to_string();
                led_map
                    .get(&source_id)
                    .map(|color| (target.id.to_string(), *color))
            })
            .collect();
        transformed.insert(channel_id.clone(), new_map);
    }
    LightingState::PerLed {
        channels: transformed,
    }
}

#[async_trait]
impl CoolingCapability for LuaDevice {
    fn cooling_channels(&self) -> Vec<CoolingChannel> {
        let mut channels = self.cooling_channels.get().cloned().unwrap_or_default();
        let observed = self.cooling_cache.lock_recover();
        for channel in &mut channels {
            if let Some(current) = observed.get(&channel.id) {
                *channel = current.clone();
            }
        }
        channels
    }

    async fn get_cooling_status(&self, channel_id: &str) -> Result<CoolingChannel> {
        if let Some(channel) = self.cooling_cache.lock_recover().get(channel_id).cloned() {
            return Ok(channel);
        }
        self.track(self.worker()?.cooling_status(channel_id).await)
            .await
    }

    async fn set_cooling_duty(&self, channel_id: &str, duty: u8) -> Result<()> {
        self.track(self.worker()?.cooling_set_duty(channel_id, duty).await)
            .await
    }

    fn cooling_state(&self) -> &CoolingStateSlot {
        &self.cooling_slot
    }

    fn cached_cooling_status(&self) -> Vec<CoolingChannel> {
        self.cooling_cache
            .lock_recover()
            .values()
            .cloned()
            .collect()
    }
}

#[async_trait]
impl SensorCapability for LuaDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self.sensor_cache.lock_recover().clone())
    }
}

// ── Chain / children: the parent surface ────────────────────────────────────
//
// Reuses the native `LightingDivisionHost` machinery. The script supplies only the probe
// (`detect_accessories`), the per-accessory descriptor table, and the routing
// callbacks (`write_frame` / cooling hub). The generic segment child and the
// `LightingDivisionHost` frame composition are unchanged.

#[async_trait]
impl Controller for LuaDevice {
    async fn discover_children(&self) -> Vec<Arc<dyn Device>> {
        if self.plugin_type == PluginKind::Integration || self.dynamic_children {
            return self.discover_controllers().await;
        }
        let mut children = self.discover_cooling_channel_devices().await;
        children.extend(self.discover_chain_accessories().await);
        children
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
                self.set_call_failure_state(DegradeReason::EnumerateFailed);
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
    identity_scope: crate::domain::registry::identity::IdentityScope,
}

fn child_device_id(root: &str, controller: &DetectedController) -> String {
    controller
        .id
        .clone()
        .filter(|id| !id.is_empty())
        .unwrap_or_else(|| format!("{root}_ctrl_{}", controller.index))
}

impl LuaDevice {
    async fn discover_cooling_channel_devices(&self) -> Vec<Arc<dyn Device>> {
        if !self.cooling_as_devices.load(Ordering::Acquire) {
            return Vec::new();
        }
        let Some(parent) = self.self_ref.upgrade() else {
            return Vec::new();
        };
        let hub: Arc<dyn CoolingHub> = parent;
        self.cooling_channels
            .get()
            .into_iter()
            .flatten()
            .cloned()
            .map(|channel| {
                Arc::new(CoolingChannelLeaf::new(
                    format!("{}_cooling_{}", self.id, channel.id),
                    self.id.clone(),
                    self.vendor.clone(),
                    channel,
                    hub.clone(),
                )) as Arc<dyn Device>
            })
            .collect()
    }

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
        let cooling_hub: Arc<dyn CoolingHub> = parent;
        let chain_hub: Arc<dyn LightingDivisionHub> = host.clone();

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
                self.id.clone(),
                self.vendor.clone(),
                channel_str.clone(),
                d.channel,
                accessory,
                chain_hub.clone(),
                cooling_hub.clone(),
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
        let identity_scope = if self.plugin_type == PluginKind::Integration
            && root_manifest.transports.tcp.is_some()
        {
            let tcp = root_manifest.transports.tcp.clone().unwrap_or_default();
            crate::domain::registry::identity::integration_scope(
                config
                    .get(&tcp.host_key)
                    .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
                    .as_deref(),
                config
                    .get(&tcp.port_key)
                    .map(crate::domain::plugin::ResolvedConfigValue::to_config_string)
                    .as_deref(),
            )
        } else {
            crate::domain::registry::identity::IdentityScope::Local
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
        self.dynamic_child_ids
            .write()
            .unwrap()
            .insert(controller.index, child_device_id(&self.id, controller));
        // `new_cyclic` so a controller that itself declares `chain` can hand its
        // accessories a `FanHub` back-reference (nested chain).
        let child = Arc::new_cyclic(|weak| {
            let name = controller.name.clone();
            let dev_match = DevMatch {
                transport: self.transport_kind.to_owned(),
                index: Some(controller.index),
                key: controller.key.clone(),
                name: Some(name.clone()),
                extra: controller.extra.clone(),
                ..Default::default()
            };
            let mut d = LuaDevice::new(LuaDeviceParts {
                id: child_device_id(&self.id, controller),
                manifest: &ctx.root_manifest,
                spec: None,
                notify: self.notify.clone(),
                runtime: self.runtime.clone(),
                worker: LuaDeviceWorker::Child(Box::new(LuaDeviceChildParts {
                    dev_match,
                    worker: ctx.worker.clone(),
                    transport: ctx.transport.clone(),
                    http: self.http.clone(),
                    name,
                    vendor: self.vendor.clone(),
                    device_type: controller.device_type,
                    transport_kind: self.transport_kind,
                })),
            });
            d.set_self_ref(weak.clone());
            d
        });
        let identity = crate::domain::registry::identity::DeviceIdentity {
            scope: Some(ctx.identity_scope.clone()),
            serial: crate::domain::registry::identity::normalize_serial(
                controller.serial.as_deref(),
            ),
            location: crate::domain::registry::identity::location_from_openrgb(
                controller.location.as_deref(),
            ),
            usb: None,
            usb_address: None,
        };
        Some(Arc::new(
            crate::domain::registry::identity::IdentifiedDevice::new(
                child as Arc<dyn Device>,
                identity,
                if self.plugin_type == PluginKind::Integration {
                    crate::domain::registry::identity::DeviceOrigin::Integration(
                        self.plugin_id.clone(),
                    )
                } else {
                    crate::domain::registry::identity::DeviceOrigin::Plugin(self.plugin_id.clone())
                },
            ),
        ))
    }
}

/// The `integration_id` a plugin device advertises: `Some(plugin_id)` only for
/// the integration *root* (the SDK client the GUI hides under Integrations).
/// The controllers it exposes as children report `None` so they stay listable
/// and remain eligible for duplicate-device conflict detection against the
/// native driver for the same hardware.
fn integration_root_id(plugin_type: PluginKind, plugin_id: &str, is_child: bool) -> Option<String> {
    (plugin_type == PluginKind::Integration && !is_child).then(|| plugin_id.to_owned())
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
        let config = self.dpi_config.lock_recover();
        dpi.clamp(config.min, config.max)
    }
}

#[async_trait]
impl DpiCapability for LuaDevice {
    async fn dpi_status(&self) -> DpiStatus {
        let dpi = self.dpi_state.lock_recover();
        let config = self.dpi_config.lock_recover().clone();
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
            let config = self.dpi_config.lock_recover();
            (config.min, config.max)
        };
        let available = self.dpi_state.lock_recover().available_dpis.clone();
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
        let mut dpi = self.dpi_state.lock_recover();
        dpi.steps = steps;
        if dpi.index >= dpi.steps.len() {
            dpi.index = dpi.steps.len() - 1;
        }
        dpi.current = dpi.steps[dpi.index];
        Ok(())
    }

    async fn set_dpi_index(&self, index: usize) -> Result<()> {
        let value = {
            let dpi = self.dpi_state.lock_recover();
            let &v = dpi
                .steps
                .get(index)
                .ok_or_else(|| anyhow::anyhow!("dpi index {index} out of range"))?;
            v
        };
        self.worker()?.dpi_set(value).await?;
        let mut dpi = self.dpi_state.lock_recover();
        dpi.index = index;
        dpi.current = value;
        Ok(())
    }

    async fn set_dpi_direct(&self, dpi: u16) -> Result<()> {
        let value = self.clamp_dpi(dpi);
        self.worker()?.dpi_set(value).await?;
        self.dpi_state.lock_recover().current = value;
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
        let mut live = self.boolean_cache.lock_recover().clone();
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
        let mut config = self.dpi_config.lock_recover();
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
        Ok(self.battery_cache.lock_recover().clone())
    }
}

#[async_trait]
impl ConnectionCapability for LuaDevice {
    async fn connection_status(&self) -> Option<ConnectionStatus> {
        self.connection_cache.lock_recover().clone()
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
        let worker = self.worker()?;
        worker.equalizer_set_preset(preset_index).await?;
        match worker.equalizer_get().await {
            Ok(equalizer) => *self.eq_cache.lock_recover() = Some(equalizer),
            Err(_) => {
                if let Some(equalizer) = self.eq_cache.lock_recover().as_mut() {
                    equalizer.selected_preset = preset_index;
                }
            }
        }
        Ok(())
    }

    async fn set_eq_bands(&self, values: &[f32]) -> Result<()> {
        let worker = self.worker()?;
        worker.equalizer_set_bands(values).await?;
        if let Ok(equalizer) = worker.equalizer_get().await {
            *self.eq_cache.lock_recover() = Some(equalizer);
        }
        Ok(())
    }

    fn current_state(&self) -> Option<Equalizer> {
        self.eq_cache.lock_recover().clone()
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
        crate::domain::input::validate::validate_cid(
            &self.key_remap.read().unwrap().buttons,
            &mapping,
        )?;
        self.worker()?
            .key_remap_set_mapping(mapping.clone())
            .await?;
        let mut cache = self.key_remap_mappings.lock_recover();
        if mapping.base == ButtonAction::Native && mapping.shifted == ButtonAction::Native {
            cache.remove(&mapping.cid);
        } else {
            cache.insert(mapping.cid, mapping);
        }
        Ok(())
    }

    async fn reset_button_mapping(&self, cid: u16) -> Result<()> {
        self.worker()?.key_remap_reset(cid).await?;
        let default = self
            .key_remap
            .read()
            .unwrap()
            .default_mappings
            .iter()
            .find(|mapping| mapping.cid == cid)
            .cloned();
        let mut mappings = self.key_remap_mappings.lock_recover();
        match default {
            Some(mapping)
                if mapping.base != ButtonAction::Native
                    || mapping.shifted != ButtonAction::Native =>
            {
                mappings.insert(cid, mapping);
            }
            _ => {
                mappings.remove(&cid);
            }
        }
        Ok(())
    }

    async fn reset_all_button_mappings(&self) -> Result<()> {
        self.worker()?.key_remap_reset_all().await?;
        let defaults = self.key_remap.read().unwrap().default_mappings.clone();
        let mut mappings = self.key_remap_mappings.lock_recover();
        mappings.clear();
        mappings.extend(defaults.into_iter().filter_map(|mapping| {
            (mapping.base != ButtonAction::Native || mapping.shifted != ButtonAction::Native)
                .then_some((mapping.cid, mapping))
        }));
        Ok(())
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
impl LightingDivisionAdapter for LuaDevice {
    fn parent_id(&self) -> String {
        self.id.clone()
    }
    fn channels(&self) -> Vec<ChannelDescriptor> {
        // Runtime-reported channels (from `initialize`) win over the static
        // manifest ones. `LightingDivisionHost` reads this live, so channels discovered
        // during init appear even though the host was built before it.
        self.dynamic_chain_channels
            .get()
            .cloned()
            .unwrap_or_else(|| self.chain_channels.clone())
    }
    async fn write_divided_frame(&self, channel_id: &str, bytes: &[u8]) -> Result<()> {
        self.worker()?.write_lighting_frame(channel_id, bytes).await
    }
}

#[async_trait]
impl CoolingHub for LuaDevice {
    async fn get_cooling_status(&self, channel: &str) -> Result<CoolingChannel> {
        if let Some(status) = self.cooling_cache.lock_recover().get(channel).cloned() {
            return Ok(status);
        }
        self.worker()?.cooling_status(channel).await
    }
    async fn set_cooling_duty(&self, channel: &str, duty: u8) -> Result<()> {
        self.worker()?.cooling_set_duty(channel, duty).await
    }
    fn cached_cooling_status(&self, channel: &str) -> Option<CoolingChannel> {
        self.cooling_cache.lock_recover().get(channel).cloned()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::domain::device::LightingCapability;
    use crate::infrastructure::drivers::transports::mock::test_transport::MockTransport;
    use crate::infrastructure::drivers::transports::Transport;

    fn test_manifest(
        id: &str,
        capabilities: &[&str],
        script: &str,
    ) -> (tempfile::TempDir, PluginManifest) {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join(id);
        std::fs::create_dir_all(&dir).unwrap();
        let capabilities = capabilities.join(", ");
        std::fs::write(
            dir.join("plugin.yaml"),
            format!(
                "id: {id}\nversion: 1.0.0\npermissions: [hid]\ncapabilities: [{capabilities}]\ntransports:\n  hid:\n    report_size: 8\ndevices:\n  - vendor: Test\n    model: Device\n    match:\n      hid: {{ vid: 1, pid: 2 }}\n"
            ),
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), script).unwrap();
        let manifest = crate::domain::plugin::parse_manifest_from_dir(&dir).unwrap();
        (tmp, manifest)
    }

    fn hid_match() -> DevMatch {
        DevMatch {
            transport: "hid".into(),
            pid: Some(2),
            ..Default::default()
        }
    }

    fn hid_device(id: &str, manifest: &PluginManifest, transport: Arc<dyn Transport>) -> LuaDevice {
        LuaDevice::new(LuaDeviceParts {
            id: id.into(),
            manifest,
            spec: Some(&manifest.devices[0]),
            notify: Weak::new(),
            runtime: Some(Arc::new(Mutex::new(RuntimeState::OpeningTransport))),
            worker: LuaDeviceWorker::Spawn(Box::new(LuaDeviceSpawnParts {
                dev_match: hid_match(),
                transport: PluginIo::Stream {
                    transport,
                    usb: None,
                },
                handle: tokio::runtime::Handle::current(),
                granted: Vec::new(),
                config: HashMap::new(),
                data: Default::default(),
            })),
        })
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn manifest_control_layout_reaches_the_wire_device() {
        let tmp = tempfile::tempdir().unwrap();
        let dir = tmp.path().join("layout_plug");
        std::fs::create_dir_all(&dir).unwrap();
        std::fs::write(
            dir.join("plugin.yaml"),
            "id: layout_plug\nversion: 1.0.0\npermissions: [hid]\ncapabilities: [controls]\ntransports:\n  hid:\n    report_size: 8\ndevices:\n  - vendor: Test\n    model: Device\n    match:\n      hid: { vid: 1, pid: 2 }\n    control_layout:\n      - { category: Microphone, order: 0, column: 0 }\n      - { category: Noise Cancelling, order: 1, column: 1 }\n",
        )
        .unwrap();
        std::fs::write(dir.join("main.lua"), "return {}").unwrap();
        let manifest = crate::domain::plugin::parse_manifest_from_dir(&dir).unwrap();

        let device = LuaDevice::new(LuaDeviceParts {
            id: "layout_plug_0".into(),
            manifest: &manifest,
            spec: Some(&manifest.devices[0]),
            notify: Weak::new(),
            runtime: None,
            worker: LuaDeviceWorker::None,
        });

        let wire = device.serialize().await;
        assert_eq!(
            wire.control_layout
                .iter()
                .map(|l| (l.category.as_str(), l.column, l.span))
                .collect::<Vec<_>>(),
            vec![("Microphone", 0, 1), ("Noise Cancelling", 1, 1)]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn wire_device_control_layout_is_empty_without_a_manifest_layout() {
        let (_tmp, manifest) = test_manifest("no_layout", &["controls"], "return {}");
        let device = LuaDevice::new(LuaDeviceParts {
            id: "no_layout_0".into(),
            manifest: &manifest,
            spec: Some(&manifest.devices[0]),
            notify: Weak::new(),
            runtime: None,
            worker: LuaDeviceWorker::None,
        });

        assert!(device.serialize().await.control_layout.is_empty());
    }

    #[test]
    fn declared_capabilities_expand_controls_and_preserve_order() {
        let (_tmp, manifest) = test_manifest(
            "declared_caps",
            &["lighting", "controls", "cooling", "lighting_division"],
            "return {}",
        );
        assert_eq!(
            declared_caps(&manifest),
            vec![
                Cap::Lighting,
                Cap::Cooling,
                Cap::Choice,
                Cap::Range,
                Cap::Boolean,
                Cap::Action,
                Cap::LightingDivision,
            ]
        );
    }

    #[test]
    fn runtime_capability_names_are_deduplicated_and_unknown_names_ignored() {
        let caps = caps_named(&[
            "controls".into(),
            "lighting".into(),
            "controls".into(),
            "unknown".into(),
        ]);
        assert_eq!(
            caps,
            vec![
                Cap::Choice,
                Cap::Range,
                Cap::Boolean,
                Cap::Action,
                Cap::Lighting,
            ]
        );
    }

    #[test]
    fn every_manifest_capability_maps_to_a_runtime_capability() {
        for name in crate::domain::plugin::manifest::SUPPORTED_CAPABILITIES {
            assert!(
                !cap_for(name).is_empty(),
                "manifest capability '{name}' has no runtime mapping"
            );
        }
    }

    #[test]
    fn every_runtime_capability_name_is_in_the_manifest_vocabulary() {
        for name in CAPABILITY_NAMES {
            assert!(
                crate::domain::plugin::manifest::SUPPORTED_CAPABILITIES.contains(name),
                "runtime capability '{name}' is not accepted by manifests"
            );
        }
    }

    #[test]
    fn integration_id_names_only_the_root_not_child_controllers() {
        // The root (non-child worker) is hidden under Integrations; the
        // controllers it exposes as children stay listable and conflict-eligible.
        assert_eq!(
            integration_root_id(PluginKind::Integration, "openrgb", false).as_deref(),
            Some("openrgb")
        );
        assert_eq!(
            integration_root_id(PluginKind::Integration, "openrgb", true),
            None
        );
        // A plain device plugin never claims an integration id.
        assert_eq!(integration_root_id(PluginKind::Device, "acme", false), None);
    }

    #[test]
    fn device_only_keeps_plugin_identity_without_capabilities() {
        let (_tmp, manifest) = test_manifest("identity_only", &[], "return {}");
        let dev = LuaDevice::new(LuaDeviceParts {
            id: "identity_only-0".into(),
            manifest: &manifest,
            spec: Some(&manifest.devices[0]),
            notify: Weak::new(),
            runtime: None,
            worker: LuaDeviceWorker::None,
        });
        assert!(dev.capabilities().is_empty());
        assert_eq!(dev.owning_plugin_id().as_deref(), Some("identity_only"));
        assert!(dev.is_live());
    }

    #[test]
    fn runtime_controls_validate_cache_and_render_wire_state() {
        let runtime: InitControls = serde_yaml::from_str(
            "choices:\n  - key: mode\n    label: Mode\n    options:\n      - { id: a, label: A }\n      - { id: b, label: B }\n    default: 0\nranges:\n  - key: hz\n    label: Hz\n    min: 100\n    max: 1000\n    default: 500\n    visible_when: { key: mode, equals: [1] }\nbooleans:\n  - key: snap\n    label: Angle Snap\n    category: Mouse\nactions:\n  - key: calibrate\n    label: Calibrate\n",
        )
        .unwrap();
        let controls = Controls::from_runtime(runtime);

        controls.record_choice("mode", 1).unwrap();
        assert!(controls.record_choice("mode", 2).is_err());
        let Some(DeviceCapability::Choice(choices)) = controls.choices_wire() else {
            panic!("choice capability missing");
        };
        assert_eq!(choices[0].selected, 1);

        assert_eq!(controls.record_range("hz", 5000).unwrap(), 1000);
        assert!(controls.record_range("missing", 10).is_err());
        let Some(DeviceCapability::Range(ranges)) = controls.ranges_wire() else {
            panic!("range capability missing");
        };
        assert_eq!(ranges[0].value, 1000);
        assert_eq!(ranges[0].visible_when.as_ref().unwrap().key, "mode");
        assert_eq!(ranges[0].visible_when.as_ref().unwrap().equals, vec![1]);

        let mut booleans = vec![Boolean {
            key: "snap".into(),
            value: true,
            label: String::new(),
            read_only: false,
            category: String::new(),
            visible_when: None,
        }];
        controls.backfill_booleans(&mut booleans);
        assert_eq!(booleans[0].label, "Angle Snap");
        assert_eq!(booleans[0].category, "Mouse");
        assert!(controls.has_action("calibrate"));
        assert!(!controls.has_action("missing"));
    }

    #[test]
    fn dynamic_rgb_descriptor_uses_reported_led_ids() {
        let descriptor = build_dynamic_descriptor(
            vec![InitLightingChannel {
                id: "ring".into(),
                name: "Ring".into(),
                topology: "linear".into(),
                led_count: 2,
                led_ids: vec![10, 20],
                leds: Vec::new(),
                rings: 0,
                keyboard_form_factor: None,
                keyboard_layout: None,
            }],
            Vec::new(),
            None,
        );
        assert_eq!(descriptor.channels.len(), 1);
        assert_eq!(
            descriptor.channels[0]
                .leds
                .iter()
                .map(|led| led.id)
                .collect::<Vec<_>>(),
            vec![10, 20]
        );
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_narrows_capabilities_and_installs_runtime_descriptors() {
        let script = r#"
            return {
              initialize = function(dev)
                return {
                  capabilities = { "lighting", "cooling" },
                  channels = { { id = "ring", name = "Ring", led_count = 2,
                              led_ids = { 7, 9 } } },
                  cooling = { channels = {
                    { id = "fan1", name = "Fan", kind = "fan", controllable = true },
                  } },
                }
              end,
            }
        "#;
        let (_tmp, manifest) = test_manifest(
            "runtime_descriptors",
            &["lighting", "cooling", "sensors"],
            script,
        );
        let dev = hid_device(
            "runtime_descriptors-0",
            &manifest,
            Arc::new(MockTransport::empty()),
        );
        assert!(dev.initialize().await.unwrap());
        let caps = dev.capabilities();
        assert!(caps
            .iter()
            .any(|cap| matches!(cap, CapabilityRef::Lighting(_))));
        assert!(caps
            .iter()
            .any(|cap| matches!(cap, CapabilityRef::Cooling(_))));
        assert!(!caps
            .iter()
            .any(|cap| matches!(cap, CapabilityRef::Sensor(_))));
        assert_eq!(
            LightingCapability::descriptor(&dev).channels[0].leds[0].id,
            7
        );
        dev.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_rearms_active_key_mappings_on_connect() {
        let script = r#"
            return {
              initialize = function(dev)
                return {
                  key_remap = {
                    buttons = { { cid = 5, label = "B5", divertable = true, group = 0 } },
                    default_mappings = {
                      { cid = 5, base = { type = "dpi_cycle", direction = "up" },
                        shifted = { type = "native" } },
                    },
                  },
                }
              end,
              set_button_mapping = function(dev, mapping)
                dev.transport:write(string.char(0xEE, mapping.cid))
              end,
            }
        "#;
        let (_tmp, manifest) = test_manifest("rearm", &["key_remap"], script);
        let mock = Arc::new(MockTransport::empty());
        let dev = hid_device("rearm-0", &manifest, mock.clone());
        assert!(dev.initialize().await.unwrap());
        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0xEE, 5]],
            "the seeded active default must be re-armed on the hardware at connect"
        );
        dev.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn rgb_frames_reach_the_transport_through_the_current_worker() {
        let script = r#"
            return {
              initialize = function(dev)
                return { channels = { { id = "ring", name = "Ring", led_count = 2 } } }
              end,
              write_frame = function(dev, channel, bytes)
                assert(channel == "ring")
                dev.transport:write(string.char(0xAB, table.unpack(bytes)))
              end,
            }
        "#;
        let (_tmp, manifest) = test_manifest("rgb_write", &["lighting"], script);
        let mock = Arc::new(MockTransport::empty());
        let dev = hid_device("rgb_write-0", &manifest, mock.clone());
        assert!(dev.initialize().await.unwrap());
        dev.write_frame("ring", &[1, 2, 3, 4, 5, 6]).await.unwrap();
        assert_eq!(
            *mock.written.lock().await,
            vec![vec![0xAB, 1, 2, 3, 4, 5, 6]]
        );
        dev.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn failed_initialize_degrades_and_close_is_terminal() {
        let (_tmp, manifest) = test_manifest(
            "lifecycle",
            &["lighting"],
            "return { initialize = function(dev) error('broken') end }",
        );
        let runtime = Arc::new(Mutex::new(RuntimeState::OpeningTransport));
        let mut dev = hid_device("lifecycle-0", &manifest, Arc::new(MockTransport::empty()));
        dev.runtime = Some(runtime.clone());
        assert!(dev.initialize().await.is_err());
        assert_eq!(
            *runtime.lock_recover(),
            RuntimeState::Degraded(DegradeReason::CallFailed)
        );
        dev.close().await;
        assert_eq!(*runtime.lock_recover(), RuntimeState::Closed);
        assert!(!dev.is_live());
        assert!(dev.initialize().await.is_err());
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn deterministic_transport_error_makes_initialize_unrecoverable() {
        let (_tmp, manifest) = test_manifest(
            "fatal_init",
            &[],
            r#"return {
                initialize = function(dev)
                    command.run("sh", {})
                    return true
                end,
            }"#,
        );
        let runtime = Arc::new(Mutex::new(RuntimeState::OpeningTransport));
        let command = super::super::transport::CommandExecutor::new(["nvidia-smi".to_owned()]);
        let dev = LuaDevice::new(LuaDeviceParts {
            id: "fatal_init-0".into(),
            manifest: &manifest,
            spec: Some(&manifest.devices[0]),
            notify: Weak::new(),
            runtime: Some(runtime.clone()),
            worker: LuaDeviceWorker::Spawn(Box::new(LuaDeviceSpawnParts {
                dev_match: DevMatch {
                    transport: "command".into(),
                    ..Default::default()
                },
                transport: PluginIo::Command(command),
                handle: tokio::runtime::Handle::current(),
                granted: Vec::new(),
                config: HashMap::new(),
                data: Default::default(),
            })),
        });

        assert!(dev.initialize().await.is_err());
        assert!(dev.is_unrecoverable());
        assert_eq!(*runtime.lock_recover(), RuntimeState::Unrecoverable);
        dev.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn initialize_false_stays_degraded_and_not_live() {
        let (_tmp, manifest) = test_manifest(
            "not_ready",
            &["lighting"],
            "return { initialize = function() return { ok = false } end }",
        );
        let runtime = Arc::new(Mutex::new(RuntimeState::OpeningTransport));
        let mut dev = hid_device("not_ready-0", &manifest, Arc::new(MockTransport::empty()));
        dev.runtime = Some(runtime.clone());

        assert!(!dev.initialize().await.unwrap());
        assert_eq!(
            *runtime.lock_recover(),
            RuntimeState::Degraded(DegradeReason::CallFailed)
        );
        assert!(!dev.is_live());
        dev.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_eq_preset_refreshes_cached_bands_to_the_new_preset() {
        // Each preset carries a distinct curve; get_equalizer derives bands from the
        // selected preset. Switching preset must leave the cache self-consistent —
        // bands matching selected_preset, not the previously cached curve.
        let script = r#"
            local sel = 0
            return {
              initialize = function() return true end,
              get_equalizer = function()
                return {
                  presets = { { id = "a", label = "A" }, { id = "b", label = "B" } },
                  selected_preset = sel,
                  editable = false,
                  bands = { {
                    index = 0, label = "60", min = -10, max = 10, step = 1,
                    value = (sel == 0) and 0.0 or 5.0,
                  } },
                }
              end,
              set_eq_preset = function(dev, p) sel = p end,
            }
        "#;
        let (_tmp, manifest) = test_manifest("eq_preset", &["equalizer"], script);
        let dev = hid_device("eq_preset-0", &manifest, Arc::new(MockTransport::empty()));
        assert!(dev.initialize().await.unwrap());

        dev.refresh_status_cache().await;
        assert_eq!(
            dev.eq_cache.lock_recover().as_ref().unwrap().bands[0].value,
            0.0
        );

        dev.set_eq_preset(1).await.unwrap();
        let eq = dev.eq_cache.lock_recover().clone().unwrap();
        assert_eq!(eq.selected_preset, 1);
        assert_eq!(eq.bands[0].value, 5.0);
        dev.close().await;
    }

    #[tokio::test(flavor = "multi_thread", worker_threads = 2)]
    async fn set_eq_bands_caches_the_value_the_worker_applied_not_the_request() {
        // The worker clamps to the device's dB range; the host cache must reflect
        // what was applied, not the raw request. A request of -20 lands as -10.
        let script = r#"
            local applied = 0.0
            return {
              initialize = function() return true end,
              get_equalizer = function()
                return {
                  presets = { { id = "custom", label = "Custom", is_custom = true } },
                  selected_preset = 0, editable = true,
                  bands = { {
                    index = 0, label = "60", min = -10, max = 10, step = 1,
                    value = applied,
                  } },
                }
              end,
              set_eq_bands = function(dev, values)
                applied = math.max(-10, math.min(10, values[1]))
              end,
            }
        "#;
        let (_tmp, manifest) = test_manifest("eq_bands", &["equalizer"], script);
        let dev = hid_device("eq_bands-0", &manifest, Arc::new(MockTransport::empty()));
        assert!(dev.initialize().await.unwrap());
        dev.refresh_status_cache().await;

        dev.set_eq_bands(&[-20.0]).await.unwrap();
        assert_eq!(
            dev.eq_cache.lock_recover().as_ref().unwrap().bands[0].value,
            -10.0
        );
        dev.close().await;
    }
}
