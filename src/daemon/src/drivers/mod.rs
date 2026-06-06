pub mod chain;
pub mod transports;
pub mod vendors;

use anyhow::Result;
use async_trait::async_trait;
use crate::drivers::vendors::generic::devices::common::WireDeviceBuilder;
use halod_protocol::types::{
    Battery, Boolean, ButtonMapping, ChainableChannelInfo, ConnectionType, DeviceCapability,
    DeviceType, DpiStatus, EffectParamValue, Equalizer, FanStatus, KeyRemapStatus, LcdDescriptor,
    LcdStatus, RgbColor, RgbDescriptor, RgbState, RgbStatus, Sensor, VisibilityState, WireDevice,
    ZoneTopology,
};
use halod_protocol::zone_transform::ZoneContentTransform;
use serde::{de::DeserializeOwned, Serialize};
use std::{collections::HashMap, sync::Arc};

/// Shared slot for user-controlled device visibility. Embed in device structs and return
/// a reference from `visibility_slot()` to opt in to the enable/disable feature.
#[derive(Default)]
pub struct VisibilitySlot(std::sync::Mutex<VisibilityState>);

impl VisibilitySlot {
    pub fn get(&self) -> VisibilityState {
        self.0.lock().unwrap().clone()
    }
    pub fn set(&self, state: VisibilityState) {
        *self.0.lock().unwrap() = state;
    }
}

pub struct KvStateCache<V>(std::sync::Mutex<std::collections::HashMap<String, V>>);

impl<V> Default for KvStateCache<V> {
    fn default() -> Self {
        KvStateCache(std::sync::Mutex::new(std::collections::HashMap::new()))
    }
}

impl<V: Clone + Serialize + DeserializeOwned> KvStateCache<V> {
    pub fn record(&self, key: &str, value: V) {
        self.0.lock().unwrap().insert(key.to_string(), value);
    }

    pub fn get(&self, key: &str) -> Option<V>
    where
        V: Copy,
    {
        self.0.lock().unwrap().get(key).copied()
    }

    pub fn save(&self) -> serde_json::Value {
        let map = self.0.lock().unwrap();
        if map.is_empty() {
            return serde_json::Value::Null;
        }
        serde_json::to_value(&*map).unwrap_or(serde_json::Value::Null)
    }

    pub fn load_pairs(&self, v: &serde_json::Value) -> Vec<(String, V)> {
        let map: std::collections::HashMap<String, V> =
            serde_json::from_value(v.clone()).unwrap_or_default();
        map.into_iter().collect()
    }
}

pub type RangeStateCache = KvStateCache<i32>;
pub type ChoiceStateCache = KvStateCache<usize>;
pub type BoolStateCache = KvStateCache<bool>;

// ---------------------------------------------------------------------------
// New unified slot types
// ---------------------------------------------------------------------------

pub struct RgbStateSlot(std::sync::Mutex<RgbStateInner>);

#[derive(Default)]
struct RgbStateInner {
    current_state: Option<RgbState>,
    canvas_zones: Vec<crate::config::PlacedZone>,
    zone_transforms: HashMap<String, ZoneContentTransform>,
}

impl Default for RgbStateSlot {
    fn default() -> Self {
        Self(std::sync::Mutex::new(RgbStateInner::default()))
    }
}

impl RgbStateSlot {
    pub fn current_state(&self) -> Option<RgbState> {
        self.0.lock().unwrap().current_state.clone()
    }
    pub fn set_state(&self, s: Option<RgbState>) {
        self.0.lock().unwrap().current_state = s;
    }
    pub fn canvas_zones(&self) -> Vec<crate::config::PlacedZone> {
        self.0.lock().unwrap().canvas_zones.clone()
    }
    pub fn set_canvas_zones(&self, z: Vec<crate::config::PlacedZone>) {
        self.0.lock().unwrap().canvas_zones = z;
    }
    pub fn zone_transforms(&self) -> HashMap<String, ZoneContentTransform> {
        self.0.lock().unwrap().zone_transforms.clone()
    }
    pub fn transform_for(&self, id: &str) -> ZoneContentTransform {
        self.0
            .lock()
            .unwrap()
            .zone_transforms
            .get(id)
            .copied()
            .unwrap_or_default()
    }
    pub fn set_zone_transform(&self, id: String, t: ZoneContentTransform) {
        self.0.lock().unwrap().zone_transforms.insert(id, t);
    }
    pub fn set_zone_transforms(&self, m: HashMap<String, ZoneContentTransform>) {
        self.0.lock().unwrap().zone_transforms = m;
    }
}

pub struct FanStateSlot(std::sync::Mutex<Option<crate::config::FanCurveRecord>>);

impl Default for FanStateSlot {
    fn default() -> Self {
        Self(std::sync::Mutex::new(None))
    }
}

impl FanStateSlot {
    pub fn fan_curve(&self) -> Option<crate::config::FanCurveRecord> {
        self.0.lock().unwrap().clone()
    }
    pub fn set_fan_curve(&self, c: crate::config::FanCurveRecord) {
        *self.0.lock().unwrap() = Some(c);
    }
    pub fn clear_fan_curve(&self) {
        *self.0.lock().unwrap() = None;
    }
}

pub struct LcdStateSlot(std::sync::Mutex<LcdStateInner>);

#[derive(Default)]
struct LcdStateInner {
    template_id: Option<String>,
    params: HashMap<String, halod_protocol::types::EffectParamValue>,
    brightness: u8,
    rotation: u32,
    mode: halod_protocol::types::LcdMode,
    active_image: Option<String>,
}

impl Default for LcdStateSlot {
    fn default() -> Self {
        Self(std::sync::Mutex::new(LcdStateInner::default()))
    }
}

impl LcdStateSlot {
    pub fn lcd_template_id(&self) -> Option<String> {
        self.0.lock().unwrap().template_id.clone()
    }
    pub fn set_lcd_template_id(&self, id: Option<String>) {
        self.0.lock().unwrap().template_id = id;
    }
    pub fn lcd_template_params(
        &self,
    ) -> HashMap<String, halod_protocol::types::EffectParamValue> {
        self.0.lock().unwrap().params.clone()
    }
    pub fn set_lcd_template_params(
        &self,
        p: HashMap<String, halod_protocol::types::EffectParamValue>,
    ) {
        self.0.lock().unwrap().params = p;
    }
    pub fn brightness(&self) -> u8 {
        self.0.lock().unwrap().brightness
    }
    pub fn set_brightness(&self, v: u8) {
        self.0.lock().unwrap().brightness = v;
    }
    pub fn rotation(&self) -> u32 {
        self.0.lock().unwrap().rotation
    }
    pub fn set_rotation(&self, v: u32) {
        self.0.lock().unwrap().rotation = v;
    }
    pub fn mode(&self) -> halod_protocol::types::LcdMode {
        self.0.lock().unwrap().mode.clone()
    }
    pub fn set_mode(&self, v: halod_protocol::types::LcdMode) {
        self.0.lock().unwrap().mode = v;
    }
    pub fn active_image(&self) -> Option<String> {
        self.0.lock().unwrap().active_image.clone()
    }
    pub fn set_active_image(&self, v: Option<String>) {
        self.0.lock().unwrap().active_image = v;
    }
}

/// All capabilities a `Device` can expose. Add a variant here when introducing a new
/// capability type; `Device::capabilities()` never grows for existing ones.
pub enum CapabilityRef<'a> {
    Fan(&'a dyn FanCapability),
    Rgb(&'a dyn RgbCapability),
    Sensor(&'a dyn SensorCapability),
    Range(&'a dyn RangeCapability),
    Choice(&'a dyn ChoiceCapability),
    Boolean(&'a dyn BooleanCapability),
    Action(&'a dyn ActionCapability),
    Battery(&'a dyn BatteryCapability),
    Equalizer(&'a dyn EqualizerCapability),
    Dpi(&'a dyn DpiCapability),
    OnboardProfiles(&'a dyn OnboardProfilesCapability),
    Lcd(&'a dyn LcdCapability),
    KeyRemap(&'a dyn KeyRemapCapability),
    Chain(&'a dyn ChainCapability),
    Controller(&'a dyn Controller),
    TransportSwitchable(&'a dyn TransportSwitchable),
}

macro_rules! capability_dispatch {
    (
        persisting: [$($P:ident),* $(,)?],
        wire_only:  [$($W:ident),* $(,)?] $(,)?
    ) => {
        impl CapabilityRef<'_> {
            pub fn state_key(&self) -> &'static str {
                match self {
                    $( CapabilityRef::$P(c) => c.state_key(), )*
                    _ => "",
                }
            }

            pub fn save_state(&self) -> serde_json::Value {
                match self {
                    $( CapabilityRef::$P(c) => c.save_state(), )*
                    _ => serde_json::Value::Null,
                }
            }

            pub async fn restore_state(&self, v: &serde_json::Value) {
                match self {
                    $( CapabilityRef::$P(c) => c.restore_state(v).await, )*
                    _ => {}
                }
            }

            pub async fn to_wire(&self) -> Option<DeviceCapability> {
                match self {
                    $( CapabilityRef::$P(c) => c.to_wire().await, )*
                    $( CapabilityRef::$W(c) => c.to_wire().await, )*
                }
            }
        }
    };
}

capability_dispatch!(
    persisting: [Fan, Rgb, Range, Choice, Boolean, Equalizer, Dpi, Lcd, KeyRemap, OnboardProfiles],
    wire_only:  [Sensor, Action, Battery, Chain, Controller, TransportSwitchable],
);

macro_rules! as_capability {
    ($method:ident, $variant:ident, $trait:path) => {
        fn $method(&self) -> Option<&dyn $trait> {
            self.capabilities().into_iter().find_map(|c| match c {
                CapabilityRef::$variant(x) => Some(x),
                _ => None,
            })
        }
    };
}

#[async_trait]
pub trait Device: Send + Sync {
    /// Unique identifier for the device. This must be stable across runs and uniquely identify the same physical device.
    fn id(&self) -> String;

    /// Human-readable name for the device, e.g. "NVIDIA GeForce RTX 5080"
    fn name(&self) -> &str;

    /// True if the device's display name is owned by a parent (e.g. a
    /// `ChainHost` for user-added ARGB strips) rather than by the descriptor /
    /// `DeviceRecord`. The unified `set_device_name` usecase routes these
    /// through the parent's `ChainCapability::rename_chain_link`; the
    /// serializer's name-patch skips them so the parent's name wins.
    fn has_external_name(&self) -> bool {
        false
    }

    /// Vendor string, e.g. "NVIDIA"
    fn vendor(&self) -> &str;

    /// Model string, e.g. "GeForce RTX 5080"
    fn model(&self) -> &str;

    /// Initializes the device, e.g. opens connections, starts polling tasks, etc. Returns whether the device is currently connected.
    async fn initialize(&self) -> Result<bool>;
    async fn close(&self);

    /// Serializes the device into a format that can be sent to the client.
    async fn serialize(&self) -> WireDevice {
        let mut caps = Vec::new();
        for cap_ref in self.capabilities() {
            if let Some(w) = cap_ref.to_wire().await {
                caps.push(w);
            }
        }
        // Inject chainable_channels from Chain into existing Rgb, or synthesize a
        // minimal Rgb carrier for hub devices that expose chainable channels but own
        // no RGB zones of their own.
        if let Some(chain) = self.as_chain() {
            let channels = chain.chainable_channels();
            if !channels.is_empty() {
                let has_rgb = caps.iter().any(|c| matches!(c, DeviceCapability::Rgb(_)));
                if has_rgb {
                    for cap in &mut caps {
                        if let DeviceCapability::Rgb(rgb) = cap {
                            rgb.chainable_channels = channels;
                            break;
                        }
                    }
                } else {
                    caps.insert(0, DeviceCapability::Rgb(RgbStatus {
                        descriptor: RgbDescriptor { zones: vec![], native_effects: vec![] },
                        state: None,
                        zone_transforms: HashMap::new(),
                        chainable_channels: channels,
                    }));
                }
            }
        }
        WireDeviceBuilder::from_parts(
            self.id(),
            self.wire_device_name().await,
            self.vendor().to_string(),
            self.model().to_string(),
        )
        .device_type(self.wire_device_type())
        .connection_type(self.wire_connection_type().await)
        .serial_number(self.wire_serial_number())
        .connected(self.wire_device_connected().await)
        .capabilities(caps)
        .build()
    }

    fn wire_device_type(&self) -> DeviceType {
        DeviceType::Other
    }

    async fn wire_connection_type(&self) -> Option<ConnectionType> {
        None
    }

    fn wire_serial_number(&self) -> Option<String> {
        None
    }

    async fn wire_device_connected(&self) -> bool {
        true
    }

    async fn wire_device_name(&self) -> String {
        self.name().to_string()
    }

    /// Transport-independent hardware serial (e.g. Logitech unit ID).
    /// Used to detect the same physical device appearing on a different transport.
    fn hardware_serial(&self) -> Option<String> {
        None
    }

    /// All capabilities this device exposes.  Add a `CapabilityRef` variant when
    /// a new capability is introduced; this method never grows for existing ones.
    fn capabilities(&self) -> Vec<CapabilityRef<'_>>;

    as_capability!(as_fan,                Fan,                FanCapability);
    as_capability!(as_rgb,                Rgb,                RgbCapability);
    as_capability!(as_sensor_capability,  Sensor,             SensorCapability);
    as_capability!(as_range,              Range,              RangeCapability);
    as_capability!(as_choice,             Choice,             ChoiceCapability);
    as_capability!(as_boolean,            Boolean,            BooleanCapability);
    as_capability!(as_action,             Action,             ActionCapability);
    as_capability!(as_battery,            Battery,            BatteryCapability);
    as_capability!(as_equalizer,          Equalizer,          EqualizerCapability);
    as_capability!(as_dpi,                Dpi,                DpiCapability);
    as_capability!(as_onboard_profiles,   OnboardProfiles,    OnboardProfilesCapability);
    as_capability!(as_lcd,                Lcd,                LcdCapability);
    as_capability!(as_key_remap,          KeyRemap,           KeyRemapCapability);
    as_capability!(as_chain,              Chain,              ChainCapability);
    as_capability!(as_controller,         Controller,         Controller);
    as_capability!(as_transport_switchable, TransportSwitchable, TransportSwitchable);

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        None
    }

    fn active_state(&self) -> VisibilityState {
        self.visibility_slot().map(|s| s.get()).unwrap_or_default()
    }

    fn set_active_state(&self, state: VisibilityState) {
        if let Some(slot) = self.visibility_slot() {
            slot.set(state);
        }
    }

    fn set_sensor_visibility(&self, _sensor_id: &str, _state: VisibilityState) {}

    async fn save_state(&self) -> serde_json::Value {
        let mut obj = serde_json::Map::new();
        for cap in self.capabilities() {
            let key = cap.state_key();
            if key.is_empty() {
                continue;
            }
            let v = cap.save_state();
            if !v.is_null() {
                obj.insert(key.to_string(), v);
            }
        }
        if obj.is_empty() {
            serde_json::Value::Null
        } else {
            obj.into()
        }
    }

    async fn load_state(&self, state: &serde_json::Value) {
        for cap in self.capabilities() {
            let key = cap.state_key();
            if key.is_empty() {
                continue;
            }
            if let Some(v) = state.get(key) {
                cap.restore_state(v).await;
            }
        }
    }

    /// Driver-specific diagnostic key/value pairs surfaced to the debug UI.
    /// Default is empty; the generic transport info (vid/pid/path/interface)
    /// is added by the daemon-side debug usecase, not by the device itself.
    fn debug_info_extra(&self) -> Vec<(String, String)> {
        Vec::new()
    }

    /// Transport label for the debug UI. `None` lets the daemon fall back to
    /// HID-tracking + id-prefix heuristics; drivers whose transport can't be
    /// inferred from those (e.g. ENE GPU lives on a `SmbusBusKind::Gpu` bus
    /// served by NvAPI, not the chipset SMBus) should override this.
    fn debug_transport(&self) -> Option<&'static str> {
        None
    }

    /// Called once after the device is fully registered in AppState (after initialize()
    /// and discover_children()). Override to start internal notification watchers that
    /// require Arc<AppState> for broadcasting state changes.
    async fn after_register(&self, _app: Arc<crate::state::AppState>) {}
}

#[async_trait]
pub trait Controller: Send + Sync {
    async fn discover_children(&self, _app: Arc<crate::state::AppState>) -> Vec<Arc<dyn Device>> {
        vec![]
    }

    /// Re-probe paired slots for devices not yet registered. Called when a wired
    /// sibling of a paired device is removed so the device can be picked up wirelessly.
    /// Default no-op; listeners are not restarted.
    async fn rescan_children(&self, _app: Arc<crate::state::AppState>) {}

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

/// Optional capability for devices that can hot-swap their transport layer.
/// Implemented by LogitechDevice to switch between wireless (receiver) and
/// wired (direct USB) without changing the Arc identity in app.devices.
#[async_trait]
pub trait TransportSwitchable: Send + Sync {
    /// Switch to a new wired HID transport at `path`.
    /// Re-initializes the device through the new transport.
    async fn adopt_wired_transport(&self, path: &str, pid: u16) -> Result<()>;
    /// Revert to the saved wireless transport.
    /// Returns true if a wireless fallback existed (caller spawns reinit retry).
    /// Returns false if there was no fallback (caller should close + remove the device).
    async fn revert_to_wireless(&self) -> bool;
    /// True while the device is actively using its wired transport.
    async fn is_using_wired_transport(&self) -> bool;

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

/**
 * Capabilities represent specific capabilities of a device that can be interacted with.
 */

// ---------------------------------------------------------------------------
// Chain support — chainable ARGB channels with daisy-chained child devices.
// The parent owns one wire output per channel; the chain is the ordered list
// of children that share its frame buffer. The parent composes the channel
// frame from each child's last-known colours and writes it to hardware.
// ---------------------------------------------------------------------------

#[derive(Debug, Clone)]
pub struct ChainLinkSpec {
    pub kind: ChainLinkKind,
    pub name: String,
    pub topology: ZoneTopology,
    pub led_count: u32,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ChainLinkKind {
    GenericAuraArgb,
    GenericNzxtArgb,
}

impl ChainLinkKind {
    pub fn as_str(&self) -> &'static str {
        match self {
            ChainLinkKind::GenericAuraArgb => "generic_aura_argb",
            ChainLinkKind::GenericNzxtArgb => "generic_nzxt_argb",
        }
    }

    pub fn from_str(s: &str) -> Option<Self> {
        match s {
            "generic_aura_argb" => Some(ChainLinkKind::GenericAuraArgb),
            "generic_nzxt_argb" => Some(ChainLinkKind::GenericNzxtArgb),
            _ => None,
        }
    }
}

/// Parent-side capability for managing chainable channels.
///
/// The whole CRUD surface delegates to a shared [`chain::ChainHost`] — drivers
/// implement [`chain::ChainAdapter`], wrap themselves in a `ChainHost`, and
/// expose it through [`ChainCapability::chain_host`]. The default forwarding
/// methods below pick it up from there; adding a new chainable vendor never
/// needs to override anything else.
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
        app: Arc<crate::state::AppState>,
    ) -> Result<String> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.add_link(channel_id, spec, app).await
    }

    async fn remove_chain_link(
        &self,
        channel_id: &str,
        child_id: &str,
        app: Arc<crate::state::AppState>,
    ) -> Result<()> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.remove_link(channel_id, child_id, app).await
    }

    async fn rename_chain_link(
        &self,
        channel_id: &str,
        child_id: &str,
        new_name: &str,
        _app: Arc<crate::state::AppState>,
    ) -> Result<()> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.rename_link(channel_id, child_id, new_name).await
    }

    async fn reorder_chain_link(
        &self,
        channel_id: &str,
        child_id: &str,
        new_index: usize,
        _app: Arc<crate::state::AppState>,
    ) -> Result<()> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.reorder_link(channel_id, child_id, new_index).await
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
        record: &crate::config::ChainLinkRecord,
        app: Arc<crate::state::AppState>,
    ) -> Result<()> {
        let host = self
            .chain_host()
            .ok_or_else(|| anyhow::anyhow!("chain host not initialized"))?;
        host.restore_link(channel_id, record, app).await
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

    fn state_key(&self) -> &'static str {
        ""
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn restore_state(&self, _: &serde_json::Value) {}
}

/// NZXT-specific parent trait carrying fan status accessors. Used by F-Fan
/// children alongside their generic [`chain::ChainHub`] reference.
///
/// Chain writes go through [`chain::ChainHub`] — this trait is purely about
/// the fan-status surface, which has no analogue on non-NZXT vendors.
#[async_trait]
pub trait NzxtFanHub: Send + Sync + 'static {
    fn id(&self) -> String;
    async fn get_fan_rpm(&self, channel: &u8) -> Result<u32>;
    async fn get_fan_duty(&self, channel: &u8) -> Result<u8>;
    async fn get_fan_controllable(&self, channel: &u8) -> Result<bool>;
    async fn set_fan_duty(&self, channel: &u8, duty: u8) -> Result<()>;
}

#[async_trait]
pub trait RgbCapability: Send + Sync {
    /// Describe zones and native effects supported by the device.
    fn descriptor(&self) -> &RgbDescriptor;

    /// Apply a new RGB state to the device, e.g. set static color, start an effect, etc.
    async fn apply(&self, state: RgbState) -> Result<()>;

    /// Writes a raw color frame without affecting saved state or mode
    /// Don't use directly; it will bypass state management and should only be used to change the colors of the LEDs in "engine" mode
    async fn write_frame(&self, zone_id: &str, colors: &[RgbColor]) -> Result<()>;

    /// Backing slot — required so all state sub-operations have defaults.
    fn rgb_state(&self) -> &RgbStateSlot;

    /// Returns a clone of the current state
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
        "rgb"
    }
    fn save_state(&self) -> serde_json::Value {
        let slot = self.rgb_state();
        let canvas = slot.canvas_zones();
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
        if let Some(s) = v.get("state") {
            if let Ok(state) = serde_json::from_value(s.clone()) {
                let _ = self.apply(state).await;
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
        "range"
    }
    fn save_state(&self) -> serde_json::Value {
        self.range_cache().save()
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        for (key, value) in self.range_cache().load_pairs(v) {
            let _ = self.set_range(&key, value).await;
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
        "choice"
    }
    fn save_state(&self) -> serde_json::Value {
        self.choice_cache().save()
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        for (key, selected) in self.choice_cache().load_pairs(v) {
            let _ = self.set_choice(&key, selected).await;
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
        let booleans = self.get_booleans().await.unwrap_or_default();
        if booleans.is_empty() {
            None
        } else {
            Some(DeviceCapability::Boolean(booleans))
        }
    }

    fn state_key(&self) -> &'static str {
        "boolean"
    }
    fn save_state(&self) -> serde_json::Value {
        self.bool_cache()
            .map(|c| c.save())
            .unwrap_or(serde_json::Value::Null)
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(cache) = self.bool_cache() {
            for (key, value) in cache.load_pairs(v) {
                let _ = self.set_boolean(&key, value).await;
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

    fn state_key(&self) -> &'static str {
        ""
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn restore_state(&self, _: &serde_json::Value) {}
}

#[async_trait]
pub trait BatteryCapability: Send + Sync {
    async fn get_batteries(&self) -> Result<Vec<Battery>>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let batteries = self.get_batteries().await.unwrap_or_default();
        if batteries.is_empty() {
            None
        } else {
            Some(DeviceCapability::Battery(batteries))
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

#[async_trait]
pub trait EqualizerCapability: Send + Sync {
    async fn get_equalizer(&self) -> Result<Equalizer>;
    async fn set_eq_preset(&self, preset_index: usize) -> Result<()>;
    async fn set_eq_bands(&self, values: &[f32]) -> Result<()>;

    fn current_state(&self) -> Option<Equalizer> {
        None
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        self.get_equalizer().await.ok().map(DeviceCapability::Equalizer)
    }

    fn state_key(&self) -> &'static str {
        "equalizer"
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
            let _ = self.set_eq_preset(preset as usize).await;
        }
        if let Some(arr) = v["bands"].as_array() {
            let values: Vec<f32> = arr
                .iter()
                .map(|b| b.as_f64().unwrap_or(0.0) as f32)
                .collect();
            if values.len() == 10 {
                let _ = self.set_eq_bands(&values).await;
            }
        }
    }
}

#[async_trait]
pub trait FanCapability: Send + Sync {
    /// Returns current duty %.
    async fn get_duty(&self) -> Result<u8>;

    /// Sets one fan duty %.
    async fn set_duty(&self, duty: u8) -> Result<()>;

    /// Returns current fan speed in RPM. Returns `None` for devices (e.g. pumps) that
    /// report duty but not RPM.
    async fn get_rpm(&self) -> Option<u32>;

    /// Backing slot — required so all state sub-operations have defaults.
    fn fan_state(&self) -> &FanStateSlot;

    fn fan_curve(&self) -> Option<crate::config::FanCurveRecord> {
        self.fan_state().fan_curve()
    }
    fn set_fan_curve(&self, curve: crate::config::FanCurveRecord) {
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
            duty: self.get_duty().await.unwrap_or(0),
            controllable: self.fan_controllable().await,
        }))
    }

    fn state_key(&self) -> &'static str {
        "fan_curve"
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::to_value(self.fan_curve()).unwrap_or(serde_json::Value::Null)
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        match serde_json::from_value::<Option<crate::config::FanCurveRecord>>(v.clone()) {
            Ok(Some(c)) => self.set_fan_curve(c),
            Ok(None) => self.clear_fan_curve(),
            Err(_) => {}
        }
    }
}

#[async_trait]
pub trait SensorCapability: Send + Sync {
    /// Return sensors vector
    async fn get_sensors(&self) -> Result<Vec<Sensor>>;

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Sensors(
            self.get_sensors().await.unwrap_or_default(),
        ))
    }

    fn state_key(&self) -> &'static str {
        ""
    }
    fn save_state(&self) -> serde_json::Value {
        serde_json::Value::Null
    }
    async fn restore_state(&self, _: &serde_json::Value) {}
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
pub trait LcdCapability: Send + Sync {
    fn lcd_descriptor(&self) -> LcdDescriptor;

    /// Backing slot — required so all state sub-operations have defaults.
    fn lcd_state(&self) -> &LcdStateSlot;

    fn current_state(&self) -> LcdStatus {
        let slot = self.lcd_state();
        LcdStatus {
            descriptor: self.lcd_descriptor(),
            brightness: slot.brightness(),
            rotation: slot.rotation(),
            mode: slot.mode(),
            active_image: slot.active_image(),
        }
    }
    fn lcd_status(&self) -> LcdStatus {
        self.current_state()
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        Some(DeviceCapability::Lcd(self.lcd_status()))
    }
    /// Upload raw image bytes (PNG/JPEG/GIF). Caller saves to disk; device renders it.
    async fn set_image(&self, data: &[u8]) -> Result<()>;
    /// Stream one raw RGBA8 frame straight to the panel — no encoding, no
    /// stored-image handshake. `rgba` is `width*height*4` bytes at the panel's
    /// native resolution. Used by the LCD engine for live animation. Devices
    /// without a streaming path return an error.
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

    fn state_key(&self) -> &'static str {
        "lcd"
    }
    fn save_state(&self) -> serde_json::Value {
        let slot = self.lcd_state();
        serde_json::json!({
            "template_id":  slot.lcd_template_id(),
            "params":       slot.lcd_template_params(),
            "brightness":   slot.brightness(),
            "rotation":     slot.rotation(),
            "mode":         slot.mode(),
            "active_image": slot.active_image(),
        })
    }
    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(id) = v.get("template_id") {
            self.set_lcd_template_id(serde_json::from_value(id.clone()).ok().flatten());
        }
        if let Some(p) = v.get("params") {
            if let Ok(params) = serde_json::from_value(p.clone()) {
                self.set_lcd_template_params(params);
            }
        }
        if let Some(b) = v["brightness"].as_u64() {
            let _ = self.set_brightness(b as u8).await;
        }
        if let Some(r) = v["rotation"].as_u64() {
            let _ = self.set_rotation(r as u32).await;
        }
        if let Some(m) = v.get("mode") {
            if let Ok(mode) =
                serde_json::from_value::<halod_protocol::types::LcdMode>(m.clone())
            {
                self.lcd_state().set_mode(mode);
            }
        }
        if let Some(img) = v.get("active_image") {
            self.lcd_state()
                .set_active_image(serde_json::from_value(img.clone()).ok().flatten());
        }
    }
}

// ---------------------------------------------------------------------------
// Key remapper capability traits
// ---------------------------------------------------------------------------

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

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_state_slot_round_trip() {
        use crate::config::FanCurveRecord;

        let slot = FanStateSlot::default();
        assert!(slot.fan_curve().is_none(), "starts None");

        let curve = FanCurveRecord {
            sensor_id: Some("cpu".to_string()),
            points: vec![(0.0, 30.0), (100.0, 100.0)],
        };
        slot.set_fan_curve(curve.clone());

        let got = slot.fan_curve().expect("should have a curve");
        assert_eq!(got.sensor_id, curve.sensor_id);
        assert_eq!(got.points, curve.points);

        slot.clear_fan_curve();
        assert!(slot.fan_curve().is_none(), "cleared to None");
    }

    #[test]
    fn rgb_state_slot_canvas_zones_round_trip() {
        use crate::config::PlacedZone;

        let slot = RgbStateSlot::default();
        assert!(slot.canvas_zones().is_empty(), "starts empty");

        let zones = vec![PlacedZone {
            device_id: "dev1".to_string(),
            zone_id: "z1".to_string(),
            x: 0.0,
            y: 0.0,
            w: 1.0,
            h: 1.0,
            rotation: 0.0,
        }];
        slot.set_canvas_zones(zones.clone());

        let got = slot.canvas_zones();
        assert_eq!(got.len(), 1);
        assert_eq!(got[0].device_id, zones[0].device_id);
        assert_eq!(got[0].zone_id, zones[0].zone_id);

        slot.set_canvas_zones(vec![]);
        assert!(slot.canvas_zones().is_empty(), "cleared to empty");
    }

    #[test]
    fn lcd_state_slot_round_trip() {
        let slot = LcdStateSlot::default();
        assert!(slot.lcd_template_id().is_none(), "starts None");

        slot.set_lcd_template_id(Some("my-template".to_string()));
        assert_eq!(slot.lcd_template_id().as_deref(), Some("my-template"));

        slot.set_brightness(75);
        assert_eq!(slot.brightness(), 75);

        slot.set_rotation(90);
        assert_eq!(slot.rotation(), 90);

        slot.set_active_image(Some("test.gif".to_string()));
        assert_eq!(slot.active_image().as_deref(), Some("test.gif"));

        slot.set_lcd_template_id(None);
        assert!(slot.lcd_template_id().is_none(), "cleared to None");
    }

    #[tokio::test]
    async fn default_save_state_uses_capabilities() {
        use crate::config::FanCurveRecord;

        struct MockFanDevice {
            fan: FanStateSlot,
        }
        #[async_trait::async_trait]
        impl FanCapability for MockFanDevice {
            async fn get_duty(&self) -> anyhow::Result<u8> {
                Ok(0)
            }
            async fn set_duty(&self, _: u8) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get_rpm(&self) -> Option<u32> {
                None
            }
            fn fan_state(&self) -> &FanStateSlot {
                &self.fan
            }
        }

        struct MockDevice {
            fan: MockFanDevice,
        }
        #[async_trait::async_trait]
        impl Device for MockDevice {
            fn id(&self) -> String {
                "mock".into()
            }
            fn name(&self) -> &str {
                "mock"
            }
            fn vendor(&self) -> &str {
                "test"
            }
            fn model(&self) -> &str {
                "test"
            }
            async fn initialize(&self) -> anyhow::Result<bool> {
                Ok(true)
            }
            async fn close(&self) {}
            fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
                vec![CapabilityRef::Fan(&self.fan)]
            }
        }

        let dev = MockDevice {
            fan: MockFanDevice {
                fan: FanStateSlot::default(),
            },
        };
        dev.fan.set_fan_curve(FanCurveRecord {
            sensor_id: Some("cpu".into()),
            points: vec![],
        });

        let saved = dev.save_state().await;
        assert!(!saved.is_null());
        assert!(!saved["fan_curve"].is_null());

        let dev2 = MockDevice {
            fan: MockFanDevice {
                fan: FanStateSlot::default(),
            },
        };
        dev2.load_state(&saved).await;
        assert_eq!(
            dev2.fan.fan_curve().unwrap().sensor_id.as_deref(),
            Some("cpu")
        );
    }

    #[test]
    fn kv_state_cache_round_trip_i32() {
        let cache: KvStateCache<i32> = KvStateCache::default();
        cache.record("brightness", 80);
        cache.record("volume", 50);
        let saved = cache.save();
        assert_eq!(saved["brightness"], 80);
        assert_eq!(saved["volume"], 50);
        let pairs = cache.load_pairs(&saved);
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map["brightness"], 80);
        assert_eq!(map["volume"], 50);
    }

    #[test]
    fn kv_state_cache_round_trip_usize() {
        let cache: KvStateCache<usize> = KvStateCache::default();
        cache.record("mode", 2);
        let saved = cache.save();
        let pairs = cache.load_pairs(&saved);
        let map: std::collections::HashMap<_, _> = pairs.into_iter().collect();
        assert_eq!(map["mode"], 2usize);
    }

    #[test]
    fn kv_state_cache_empty_returns_null() {
        let cache: KvStateCache<i32> = KvStateCache::default();
        assert!(cache.save().is_null());
    }

    #[test]
    fn kv_state_cache_get() {
        let cache: KvStateCache<i32> = KvStateCache::default();
        cache.record("vol", 42);
        assert_eq!(cache.get("vol"), Some(42));
        assert_eq!(cache.get("missing"), None);
    }

    #[tokio::test]
    async fn capability_ref_dispatches_state_key() {
        struct MockFan {
            fan: FanStateSlot,
        }
        #[async_trait::async_trait]
        impl FanCapability for MockFan {
            async fn get_duty(&self) -> anyhow::Result<u8> {
                Ok(50)
            }
            async fn set_duty(&self, _: u8) -> anyhow::Result<()> {
                Ok(())
            }
            async fn get_rpm(&self) -> Option<u32> {
                None
            }
            fn fan_state(&self) -> &FanStateSlot {
                &self.fan
            }
        }
        let fan = MockFan {
            fan: FanStateSlot::default(),
        };
        let cap = CapabilityRef::Fan(&fan);
        assert_eq!(cap.state_key(), "fan_curve");
        assert!(cap.save_state().is_null()); // no curve set
        fan.set_fan_curve(crate::config::FanCurveRecord {
            sensor_id: Some("cpu".into()),
            points: vec![(30.0, 20.0), (100.0, 100.0)],
        });
        let saved = cap.save_state();
        assert!(!saved.is_null());
        fan.clear_fan_curve();
        cap.restore_state(&saved).await;
        assert!(fan.fan_curve().is_some());
    }
}
