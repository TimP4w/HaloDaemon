// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(test)]
//! Shared mock device + builders for unit tests. Test-only.

use crate::domain::device::{
    ActionCapability, BooleanCapability, CapabilityRef, ChoiceCapability, ChoiceStateCache,
    CoolingCapability, CoolingStateSlot, Device, DpiCapability, EqualizerCapability,
    KeyRemapCapability, KeyboardLayoutCapability, KeyboardLayoutSlot, LcdCapability, LcdStateSlot,
    LightingCapability, LightingStateSlot, RangeCapability, RangeStateCache, SensorCapability,
    VisibilitySlot,
};
use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{
    Boolean, ButtonDescriptor, ButtonMapping, CoolingChannel, CoolingChannelKind, DpiMode,
    DpiStatus, EqBand, Equalizer, KeyRemapStatus, LcdDescriptor, LightingDescriptor, LightingState,
    ScreenShape, Sensor,
};
use std::sync::{
    atomic::{AtomicBool, AtomicUsize, Ordering},
    Arc, Mutex,
};

/// Process-wide mutex for tests that mutate `HALOD_CONFIG_DIR`.
/// All such tests MUST acquire this before setting or removing the variable;
/// they are spread across multiple modules so a single shared lock is required.
pub static HALOD_CONFIG_DIR_LOCK: Mutex<()> = Mutex::new(());

/// Run an async test closure with a temporary `HALOD_CONFIG_DIR`. Holds the
/// process-wide lock across the awaited body so no other test can overwrite the
/// env var concurrently.
#[allow(clippy::await_holding_lock)]
pub async fn with_tmp_config<F, Fut>(f: F)
where
    F: FnOnce(Arc<crate::application::state::AppState>) -> Fut,
    Fut: std::future::Future<Output = ()>,
{
    let _guard = HALOD_CONFIG_DIR_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let tmp = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HALOD_CONFIG_DIR", tmp.path()) };
    f(Arc::new(crate::application::state::AppState::new(
        crate::config::Config::default(),
    )))
    .await;
    unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
}

/// RAII form of [`with_tmp_config`] for tests that build their own `AppState`
/// (custom `Config`): points `HALOD_CONFIG_DIR` at a fresh tempdir under the
/// shared lock and restores it on drop, so a `request_config_save()` inside the
/// test never touches the real `~/.config/halod`. Bind it for the whole test.
pub struct TmpConfigDir {
    _dir: tempfile::TempDir,
    _guard: std::sync::MutexGuard<'static, ()>,
}

pub fn tmp_config_dir() -> TmpConfigDir {
    let guard = HALOD_CONFIG_DIR_LOCK
        .lock()
        .unwrap_or_else(|e| e.into_inner());
    let dir = tempfile::tempdir().unwrap();
    unsafe { std::env::set_var("HALOD_CONFIG_DIR", dir.path()) };
    TmpConfigDir {
        _dir: dir,
        _guard: guard,
    }
}

impl Drop for TmpConfigDir {
    fn drop(&mut self) {
        unsafe { std::env::remove_var("HALOD_CONFIG_DIR") };
    }
}

#[derive(Debug)]
pub enum InitBehavior {
    OkTrue,
    OkFalse,
    Err,
    /// Panics — asserts that initialize() is never called (e.g. disabled devices).
    Panic,
}

pub struct MockDevice {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub visibility: VisibilitySlot,
    /// `Some` when the device opts into runtime keyboard-layout selection.
    pub keyboard_layout: Option<KeyboardLayoutSlot>,
    /// Use builder methods (`ok_false`, `fail`, `init_panics`).
    init_behavior: InitBehavior,
    /// Set to `true` by the default `load_state()` override when tracking is enabled.
    pub load_called: Arc<AtomicBool>,
    /// Set to `true` by `close()`, for tests asserting a device was torn down.
    pub closed: Arc<AtomicBool>,
    /// Backs `Device::is_live`. Defaults to live; flip via `.offline()` or the
    /// shared `Arc` to exercise engine liveness gating.
    pub live: Arc<AtomicBool>,
    /// Owning plugin id when this device stands in for an integration root
    /// (see `Device::integration_id`). `None` for a normal device.
    pub integration_id: Option<String>,
    /// Owning plugin id for scoped teardown (see `Device::owning_plugin_id`).
    /// `None` for a normal built-in host device.
    pub owning_plugin_id: Option<String>,
    // Capability slots — `None` means the capability is absent.
    pub fan: Option<CoolingStateSlot>,
    /// RPM reported by the cooling channel; `None` (the default) means "no tachometer".
    pub fan_rpm: Option<u32>,
    pub rgb: Option<LightingStateSlot>,
    /// Number of `LightingCapability::apply` calls, for tests asserting a specific
    /// call count (e.g. the leaving-engine-mode drain re-apply).
    pub rgb_apply_count: Arc<AtomicUsize>,
    pub lcd: Option<LcdStateSlot>,
    pub lcd_stream_ok: bool,
    pub choice: Option<ChoiceStateCache>,
    pub range: Option<RangeStateCache>,
    pub booleans: Option<Vec<Boolean>>,
    pub bool_last_set: Option<Mutex<Option<(String, bool)>>>,
    pub action_last_key: Option<Mutex<Option<String>>>,
    pub dpi_last_steps: Option<Mutex<Option<Vec<u16>>>>,
    /// Tracks the last value passed to `set_dpi_direct`. Also used as the live
    /// `current_dpi` returned by `dpi_status()` once set.
    pub dpi_direct_last: Option<Mutex<Option<u16>>>,
    /// Initial `current_dpi` returned by `dpi_status()` before any `set_dpi_direct` call.
    pub dpi_initial: Option<u16>,
    pub choice_last_set: Option<Mutex<Option<(String, usize)>>>,
    pub range_last_set: Option<Mutex<Option<(String, i32)>>>,
    /// Tracks the last `set_button_mapping` call when key_remap is enabled.
    pub key_remap_last_mapping: Option<Mutex<Option<ButtonMapping>>>,
    /// Mappings returned by `get_key_remap_status`. Set via `with_key_remap_mappings`.
    pub key_remap_mappings: Vec<ButtonMapping>,
    // Sensor capability
    pub sensors: Option<Mutex<Vec<Sensor>>>,
    // Equalizer capability
    pub eq_state: Option<Mutex<Equalizer>>,
    pub eq_last_preset: Option<Mutex<Option<usize>>>,
    pub eq_last_bands: Option<Mutex<Option<Vec<f32>>>>,
}

impl MockDevice {
    /// Minimal device with no capabilities. `id` is the device id; sensible
    /// string defaults fill the rest.
    pub fn new(id: &str) -> Self {
        Self {
            id: id.to_string(),
            name: "mock".to_string(),
            vendor: "mock".to_string(),
            model: "mock".to_string(),
            visibility: VisibilitySlot::default(),
            keyboard_layout: None,
            init_behavior: InitBehavior::OkTrue,
            load_called: Arc::new(AtomicBool::new(false)),
            closed: Arc::new(AtomicBool::new(false)),
            live: Arc::new(AtomicBool::new(true)),
            integration_id: None,
            owning_plugin_id: None,
            fan: None,
            fan_rpm: None,
            rgb: None,
            rgb_apply_count: Arc::new(AtomicUsize::new(0)),
            lcd: None,
            lcd_stream_ok: false,
            choice: None,
            range: None,
            booleans: None,
            bool_last_set: None,
            action_last_key: None,
            dpi_last_steps: None,
            dpi_direct_last: None,
            dpi_initial: None,
            choice_last_set: None,
            range_last_set: None,
            key_remap_last_mapping: None,
            key_remap_mappings: Vec::new(),
            sensors: None,
            eq_state: None,
            eq_last_preset: None,
            eq_last_bands: None,
        }
    }

    pub fn ok_false(mut self) -> Self {
        self.init_behavior = InitBehavior::OkFalse;
        self
    }

    pub fn fail(mut self) -> Self {
        self.init_behavior = InitBehavior::Err;
        self
    }

    /// Make `initialize()` panic — asserts it is never called.
    pub fn init_panics(mut self) -> Self {
        self.init_behavior = InitBehavior::Panic;
        self
    }

    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    pub fn with_vendor(mut self, vendor: &str) -> Self {
        self.vendor = vendor.to_string();
        self
    }

    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    /// Stand in for an integration root owned by plugin `id` (see
    /// `Device::integration_id`).
    pub fn with_integration_id(mut self, id: &str) -> Self {
        self.integration_id = Some(id.to_string());
        self
    }

    /// Stand in for a device owned by plugin `id` for scoped teardown
    /// (see `Device::owning_plugin_id`).
    pub fn with_owning_plugin_id(mut self, id: &str) -> Self {
        self.owning_plugin_id = Some(id.to_string());
        self
    }

    /// Start the device offline (`is_live() == false`), for engine liveness-gating tests.
    #[allow(dead_code)] // shared test builder option; only some feature/target suites use it
    pub fn offline(self) -> Self {
        self.live.store(false, Ordering::SeqCst);
        self
    }

    pub fn with_fan(mut self) -> Self {
        self.fan = Some(CoolingStateSlot::default());
        self
    }

    /// Make the cooling channel report a tachometer reading. Implies `with_fan()`.
    pub fn with_fan_rpm(mut self, rpm: u32) -> Self {
        self = self.with_fan();
        self.fan_rpm = Some(rpm);
        self
    }

    pub fn with_rgb(mut self) -> Self {
        self.rgb = Some(LightingStateSlot::default());
        self
    }

    pub fn with_lcd(mut self) -> Self {
        self.lcd = Some(LcdStateSlot::default());
        self
    }

    pub fn with_lcd_latches_last_frame(mut self) -> Self {
        self = self.with_lcd();
        self.lcd.as_ref().unwrap().set_latches_last_frame(true);
        self
    }

    pub fn with_lcd_stream_ok(mut self) -> Self {
        self.lcd_stream_ok = true;
        self
    }

    pub fn with_choice(mut self) -> Self {
        self.choice = Some(ChoiceStateCache::default());
        self.choice_last_set = Some(Mutex::new(None));
        self
    }

    pub fn with_range(mut self) -> Self {
        self.range = Some(RangeStateCache::default());
        self.range_last_set = Some(Mutex::new(None));
        self
    }

    /// Add a BooleanCapability with the provided boolean definitions.
    pub fn with_booleans(mut self, booleans: Vec<Boolean>) -> Self {
        self.booleans = Some(booleans);
        self.bool_last_set = Some(Mutex::new(None));
        self
    }

    /// Add an ActionCapability (records the last triggered key).
    pub fn with_action(mut self) -> Self {
        self.action_last_key = Some(Mutex::new(None));
        self
    }

    /// Add a DpiCapability (records the last `set_dpi_steps` and `set_dpi_direct` calls).
    pub fn with_dpi(mut self) -> Self {
        self.dpi_last_steps = Some(Mutex::new(None));
        self.dpi_direct_last = Some(Mutex::new(None));
        self
    }

    /// Set the initial `current_dpi` returned by `dpi_status()`. Implies `with_dpi()`.
    pub fn with_dpi_initial(mut self, dpi: u16) -> Self {
        self = self.with_dpi();
        self.dpi_initial = Some(dpi);
        self
    }

    /// Add a KeyRemapCapability (records the last `set_button_mapping` call).
    pub fn with_key_remap(mut self) -> Self {
        self.key_remap_last_mapping = Some(Mutex::new(None));
        self
    }

    /// Add a KeyRemapCapability whose `get_key_remap_status` reports `mappings`.
    pub fn with_key_remap_mappings(mut self, mappings: Vec<ButtonMapping>) -> Self {
        self.key_remap_last_mapping = Some(Mutex::new(None));
        self.key_remap_mappings = mappings;
        self
    }

    /// Add a KeyboardLayoutCapability backed by a seedable slot.
    pub fn with_keyboard_layout(mut self) -> Self {
        self.keyboard_layout = Some(KeyboardLayoutSlot::default());
        self
    }

    /// Add a SensorCapability with the given sensor values.
    pub fn with_sensor(mut self, sensors: Vec<Sensor>) -> Self {
        self.sensors = Some(Mutex::new(sensors));
        self
    }

    /// Add an EqualizerCapability with the given initial state.
    pub fn with_equalizer(mut self, eq: Equalizer) -> Self {
        self.eq_state = Some(Mutex::new(eq));
        self.eq_last_preset = Some(Mutex::new(None));
        self.eq_last_bands = Some(Mutex::new(None));
        self
    }
}

#[async_trait]
impl Device for MockDevice {
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
        match self.init_behavior {
            InitBehavior::OkTrue => Ok(true),
            InitBehavior::OkFalse => Ok(false),
            InitBehavior::Err => Err(anyhow::anyhow!("simulated failure")),
            InitBehavior::Panic => panic!("initialize must not be called"),
        }
    }
    async fn close(&self) {
        self.closed.store(true, Ordering::SeqCst);
    }

    fn integration_id(&self) -> Option<String> {
        self.integration_id.clone()
    }

    fn owning_plugin_id(&self) -> Option<String> {
        self.owning_plugin_id.clone()
    }

    fn is_live(&self) -> bool {
        self.live.load(Ordering::SeqCst)
    }

    async fn load_state(&self, state: &serde_json::Value) {
        self.load_called.store(true, Ordering::SeqCst);
        // Delegate to the default capability-based restore for any real slots.
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

    fn capabilities(&self) -> Vec<CapabilityRef<'_>> {
        let mut caps: Vec<CapabilityRef<'_>> = Vec::new();
        if self.fan.is_some() {
            caps.push(CapabilityRef::Cooling(self));
        }
        if self.rgb.is_some() {
            caps.push(CapabilityRef::Lighting(self));
        }
        if self.lcd.is_some() {
            caps.push(CapabilityRef::Lcd(self));
        }
        if self.choice.is_some() {
            caps.push(CapabilityRef::Choice(self));
        }
        if self.range.is_some() {
            caps.push(CapabilityRef::Range(self));
        }
        if self.booleans.is_some() {
            caps.push(CapabilityRef::Boolean(self));
        }
        if self.action_last_key.is_some() {
            caps.push(CapabilityRef::Action(self));
        }
        if self.dpi_last_steps.is_some() {
            caps.push(CapabilityRef::Dpi(self));
        }
        if self.key_remap_last_mapping.is_some() {
            caps.push(CapabilityRef::KeyRemap(self));
        }
        if self.sensors.is_some() {
            caps.push(CapabilityRef::Sensor(self));
        }
        if self.eq_state.is_some() {
            caps.push(CapabilityRef::Equalizer(self));
        }
        if self.keyboard_layout.is_some() {
            caps.push(CapabilityRef::KeyboardLayout(self));
        }
        caps
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }

    fn keyboard_layout_slot(&self) -> Option<&KeyboardLayoutSlot> {
        self.keyboard_layout.as_ref()
    }
}

#[async_trait]
impl KeyboardLayoutCapability for MockDevice {
    async fn keyboard_layout_status(&self) -> halod_shared::keyboard::KeyboardLayoutStatus {
        use halod_shared::keyboard::{KeyVariant, KeyboardLayoutStatus};
        use halod_shared::types::KeyboardLayout;
        let slot = self
            .keyboard_layout
            .as_ref()
            .expect("MockDevice: keyboard_layout slot not present — call .with_keyboard_layout()");
        KeyboardLayoutStatus {
            keys: vec![],
            variant: KeyVariant::Ansi,
            language: KeyboardLayout::US,
            detected_language: KeyboardLayout::US,
            selection: slot.selection(),
            iso_supported: true,
            languages: vec![KeyboardLayout::US, KeyboardLayout::CH],
        }
    }
}

#[async_trait]
impl CoolingCapability for MockDevice {
    fn cooling_channels(&self) -> Vec<CoolingChannel> {
        vec![CoolingChannel {
            id: "default".into(),
            name: "Fan".into(),
            kind: CoolingChannelKind::Fan,
            controllable: true,
            rpm: self.fan_rpm,
            duty: Some(0),
        }]
    }
    async fn get_cooling_status(&self, channel_id: &str) -> Result<CoolingChannel> {
        self.cooling_channels()
            .into_iter()
            .find(|channel| channel.id == channel_id)
            .ok_or_else(|| anyhow::anyhow!("unknown cooling channel '{channel_id}'"))
    }
    async fn set_cooling_duty(&self, _channel_id: &str, _duty: u8) -> Result<()> {
        Ok(())
    }
    fn cooling_state(&self) -> &CoolingStateSlot {
        self.fan
            .as_ref()
            .expect("MockDevice: CoolingStateSlot not present — call .with_fan()")
    }
}

static MOCK_RGB_DESC: std::sync::OnceLock<LightingDescriptor> = std::sync::OnceLock::new();

#[async_trait]
impl LightingCapability for MockDevice {
    fn descriptor(&self) -> &LightingDescriptor {
        MOCK_RGB_DESC.get_or_init(|| {
            let zone = |id: &str, topology| halod_shared::types::LightingChannel {
                id: id.to_string(),
                name: id.to_string(),
                topology,
                leds: vec![],
                color_order: Default::default(),
                division: Default::default(),
            };
            LightingDescriptor {
                channels: vec![
                    zone("ring", halod_shared::types::ZoneTopology::Ring),
                    zone("strip", halod_shared::types::ZoneTopology::Linear),
                ],
                native_effects: vec![],
            }
        })
    }
    async fn apply(&self, state: LightingState) -> Result<()> {
        self.rgb_apply_count.fetch_add(1, Ordering::SeqCst);
        self.lighting_state().set_state(Some(state));
        Ok(())
    }
    async fn write_frame(&self, _: &str, _: &[u8]) -> Result<()> {
        Ok(())
    }
    fn lighting_state(&self) -> &LightingStateSlot {
        self.rgb
            .as_ref()
            .expect("MockDevice: LightingStateSlot not present — call .with_rgb()")
    }
}

#[async_trait]
impl LcdCapability for MockDevice {
    fn lcd_descriptor(&self) -> LcdDescriptor {
        LcdDescriptor {
            shape: ScreenShape::Square,
            width: 1,
            height: 1,
            supported_rotations: vec![],
            supported_image_types: vec![],
            latches_last_frame: self.lcd.as_ref().is_some_and(|l| l.latches_last_frame()),
        }
    }
    fn lcd_state(&self) -> &LcdStateSlot {
        self.lcd
            .as_ref()
            .expect("MockDevice: LcdStateSlot not present — call .with_lcd()")
    }
    async fn set_image(&self, _: &[u8]) -> Result<()> {
        Ok(())
    }
    async fn stream_frame(&self, _rgba: &[u8], _width: u32, _height: u32) -> Result<()> {
        if self.lcd_stream_ok {
            Ok(())
        } else {
            anyhow::bail!("MockDevice: stream_frame not enabled — call .with_lcd_stream_ok()")
        }
    }
    async fn set_rotation(&self, degrees: u32) -> Result<()> {
        let rotation = match degrees {
            90 => halod_shared::types::ScreenRotation::R90,
            180 => halod_shared::types::ScreenRotation::R180,
            270 => halod_shared::types::ScreenRotation::R270,
            _ => halod_shared::types::ScreenRotation::R0,
        };
        self.lcd_state().set_rotation(rotation);
        Ok(())
    }
    async fn set_brightness(&self, brightness: u8) -> Result<()> {
        self.lcd_state().set_brightness(brightness);
        Ok(())
    }
    async fn reset_to_default(&self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl ChoiceCapability for MockDevice {
    fn choice_cache(&self) -> &ChoiceStateCache {
        self.choice
            .as_ref()
            .expect("MockDevice: ChoiceStateCache not present — call .with_choice()")
    }
    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        self.choice_cache().record(key, selected);
        *self
            .choice_last_set
            .as_ref()
            .expect("MockDevice: choice_last_set not present — call .with_choice()")
            .lock()
            .unwrap() = Some((key.to_string(), selected));
        Ok(())
    }
}

#[async_trait]
impl RangeCapability for MockDevice {
    fn range_cache(&self) -> &RangeStateCache {
        self.range
            .as_ref()
            .expect("MockDevice: RangeStateCache not present — call .with_range()")
    }
    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        self.range_cache().record(key, value);
        *self
            .range_last_set
            .as_ref()
            .expect("MockDevice: range_last_set not present — call .with_range()")
            .lock()
            .unwrap() = Some((key.to_string(), value));
        Ok(())
    }
}

#[async_trait]
impl BooleanCapability for MockDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        Ok(self.booleans.as_ref().cloned().unwrap_or_default())
    }
    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        *self
            .bool_last_set
            .as_ref()
            .expect("MockDevice: bool_last_set not present — call .with_booleans()")
            .lock()
            .unwrap() = Some((key.to_string(), value));
        Ok(())
    }
}

#[async_trait]
impl ActionCapability for MockDevice {
    async fn trigger_action(&self, key: &str) -> Result<()> {
        *self
            .action_last_key
            .as_ref()
            .expect("MockDevice: action_last_key not present — call .with_action()")
            .lock()
            .unwrap() = Some(key.to_string());
        Ok(())
    }
}

#[async_trait]
impl DpiCapability for MockDevice {
    async fn dpi_status(&self) -> DpiStatus {
        let current_dpi = self
            .dpi_direct_last
            .as_ref()
            .and_then(|m| *m.lock().unwrap())
            .or(self.dpi_initial)
            .unwrap_or(0);
        DpiStatus {
            steps: vec![],
            current_index: 0,
            current_dpi,
            available_dpis: vec![],
            mode: DpiMode::Host,
        }
    }
    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()> {
        *self
            .dpi_last_steps
            .as_ref()
            .expect("MockDevice: dpi_last_steps not present — call .with_dpi()")
            .lock()
            .unwrap() = Some(steps);
        Ok(())
    }
    async fn set_dpi_index(&self, _index: usize) -> Result<()> {
        Ok(())
    }
    async fn set_dpi_direct(&self, dpi: u16) -> Result<()> {
        if let Some(m) = &self.dpi_direct_last {
            *m.lock().unwrap() = Some(dpi);
        }
        Ok(())
    }
}

#[async_trait]
impl KeyRemapCapability for MockDevice {
    async fn get_key_remap_status(&self) -> KeyRemapStatus {
        KeyRemapStatus {
            buttons: vec![ButtonDescriptor {
                cid: 1,
                label: "Button".into(),
                divertable: true,
                group: 0,
            }],
            mappings: self.key_remap_mappings.clone(),
            requires_host_mode: false,
            host_mode_active: false,
        }
    }
    async fn set_button_mapping(&self, mapping: ButtonMapping) -> Result<()> {
        *self
            .key_remap_last_mapping
            .as_ref()
            .expect("MockDevice: key_remap_last_mapping not present — call .with_key_remap()")
            .lock()
            .unwrap() = Some(mapping);
        Ok(())
    }
    async fn reset_button_mapping(&self, _cid: u16) -> Result<()> {
        Ok(())
    }
    async fn reset_all_button_mappings(&self) -> Result<()> {
        Ok(())
    }
}

#[async_trait]
impl SensorCapability for MockDevice {
    async fn get_sensors(&self) -> Result<Vec<Sensor>> {
        Ok(self
            .sensors
            .as_ref()
            .expect("MockDevice: sensors not present — call .with_sensor()")
            .lock()
            .unwrap()
            .clone())
    }
}

#[async_trait]
impl EqualizerCapability for MockDevice {
    async fn get_equalizer(&self) -> Result<Equalizer> {
        Ok(self
            .eq_state
            .as_ref()
            .expect("MockDevice: eq_state not present — call .with_equalizer()")
            .lock()
            .unwrap()
            .clone())
    }

    async fn set_eq_preset(&self, preset_index: usize) -> Result<()> {
        self.eq_state
            .as_ref()
            .expect("MockDevice: eq_state not present")
            .lock()
            .unwrap()
            .selected_preset = preset_index;
        *self
            .eq_last_preset
            .as_ref()
            .expect("MockDevice: eq_last_preset not present")
            .lock()
            .unwrap() = Some(preset_index);
        Ok(())
    }

    async fn set_eq_bands(&self, values: &[f32]) -> Result<()> {
        let mut eq = self
            .eq_state
            .as_ref()
            .expect("MockDevice: eq_state not present")
            .lock()
            .unwrap();
        for (band, &v) in eq.bands.iter_mut().zip(values) {
            band.value = v;
        }
        drop(eq);
        *self
            .eq_last_bands
            .as_ref()
            .expect("MockDevice: eq_last_bands not present")
            .lock()
            .unwrap() = Some(values.to_vec());
        Ok(())
    }

    fn current_state(&self) -> Option<Equalizer> {
        Some(
            self.eq_state
                .as_ref()
                .expect("MockDevice: eq_state not present")
                .lock()
                .unwrap()
                .clone(),
        )
    }
}

#[cfg(test)]
mod capability_tests {
    use super::*;
    use crate::domain::device::EqualizerCapability as EqCap;
    use crate::domain::device::LcdCapability as LcdCap;

    #[tokio::test]
    async fn equalizer_save_restore_round_trip() {
        let bands: Vec<EqBand> = (0..10)
            .map(|i| EqBand {
                index: i,
                label: format!("band{i}"),
                min: -10.0,
                max: 10.0,
                step: 0.5,
                value: 2.0,
            })
            .collect();

        let dev = MockDevice::new("eq_dev").with_equalizer(Equalizer {
            presets: vec![],
            selected_preset: 1,
            bands: bands.clone(),
            editable: true,
        });

        let saved = EqCap::save_state(&dev);
        assert!(!saved.is_null());
        assert_eq!(saved["preset"], 1);

        // Create a fresh device and restore
        let dev2 = MockDevice::new("eq_dev").with_equalizer(Equalizer {
            presets: vec![],
            selected_preset: 0,
            bands: vec![],
            editable: false,
        });
        EqCap::restore_state(&dev2, &saved).await;

        let last_preset = dev2.eq_last_preset.as_ref().unwrap().lock().unwrap();
        assert_eq!(*last_preset, Some(1));

        let last_bands = dev2.eq_last_bands.as_ref().unwrap().lock().unwrap();
        assert!(last_bands.is_some());
        assert_eq!(last_bands.as_ref().unwrap().len(), 10);
    }

    #[tokio::test]
    async fn equalizer_restore_wrong_band_count_silently_skipped() {
        let dev = MockDevice::new("eq_dev").with_equalizer(Equalizer {
            presets: vec![],
            selected_preset: 0,
            bands: vec![],
            editable: false,
        });

        // Save state with 6 bands (not 10) — should be silently skipped by the
        // hardcoded `values.len() == 10` guard in restore_state.
        let bad_state = serde_json::json!({
            "preset": 1,
            "bands": [0.0, 1.0, 2.0, 3.0, 4.0, 5.0],
        });
        EqCap::restore_state(&dev, &bad_state).await;

        // The preset was set but bands were skipped (len != 10)
        let last_preset = dev.eq_last_preset.as_ref().unwrap().lock().unwrap();
        assert_eq!(*last_preset, Some(1));
        // Bands should not have been called
        let last_bands = dev.eq_last_bands.as_ref().unwrap().lock().unwrap();
        assert!(last_bands.is_none());
    }

    #[tokio::test]
    async fn lcd_save_restore_round_trip() {
        let dev = MockDevice::new("lcd_dev").with_lcd();
        let slot = dev.lcd.as_ref().unwrap();

        slot.set_brightness(80);
        slot.set_rotation(halod_shared::types::ScreenRotation::R90);
        slot.set_active_image(Some("test.png".into()));
        slot.set_raw_streaming(true);

        let saved = LcdCap::save_state(&dev);
        assert_eq!(saved["brightness"], 80);
        assert_eq!(saved["rotation"], "r90");
        assert_eq!(saved["active_image"], "test.png");
        assert_eq!(saved["raw_streaming"], true);
        assert_eq!(saved["video_path"], serde_json::Value::Null);

        let dev2 = MockDevice::new("lcd_dev2").with_lcd();
        LcdCap::restore_state(&dev2, &saved).await;
        let slot2 = dev2.lcd.as_ref().unwrap();

        assert_eq!(slot2.brightness(), 80);
        assert_eq!(slot2.rotation(), halod_shared::types::ScreenRotation::R90);
        assert!(matches!(slot2.mode(), halod_shared::types::LcdMode::Image));
        assert_eq!(slot2.active_image(), Some("test.png".into()));
        assert!(slot2.raw_streaming());
        assert_eq!(slot2.video_path(), None);
    }

    #[tokio::test]
    async fn lcd_restore_video_mode_is_not_clobbered_by_the_null_sibling_fields() {
        let dev = MockDevice::new("lcd_dev").with_lcd();
        let slot = dev.lcd.as_ref().unwrap();
        slot.set_video_path(Some("/tmp/v.mp4".into()));
        let saved = LcdCap::save_state(&dev);

        let dev2 = MockDevice::new("lcd_dev2").with_lcd();
        LcdCap::restore_state(&dev2, &saved).await;
        let slot2 = dev2.lcd.as_ref().unwrap();

        assert_eq!(slot2.video_path(), Some("/tmp/v.mp4".into()));
        assert!(matches!(slot2.mode(), halod_shared::types::LcdMode::Video));
    }
}
