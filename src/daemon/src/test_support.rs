#![cfg(test)]
//! Shared mock device + builders for unit tests. Test-only.

use crate::drivers::{
    ActionCapability, BooleanCapability, CapabilityRef, ChoiceCapability, ChoiceStateCache, Device,
    DpiCapability, FanCapability, FanStateSlot, LcdCapability, LcdStateSlot, RangeCapability,
    RangeStateCache, RgbCapability, RgbStateSlot, VisibilitySlot,
};
use anyhow::Result;
use async_trait::async_trait;
use halod_protocol::types::{Boolean, DpiMode, DpiStatus, LcdDescriptor, RgbColor, RgbDescriptor, RgbState, ScreenShape};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc, Mutex,
};

// ---------------------------------------------------------------------------
// InitBehavior — controls what initialize() returns
// ---------------------------------------------------------------------------

/// Controls the return value of `MockDevice::initialize()`.
pub enum InitBehavior {
    /// Returns `Ok(true)` — device is connected and ready.
    OkTrue,
    /// Returns `Ok(false)` — device exists but is not ready.
    OkFalse,
    /// Returns `Err(...)` — simulated initialization failure.
    Err,
    /// Panics — asserts that initialize() is never called (e.g. disabled devices).
    Panic,
}

// ---------------------------------------------------------------------------
// MockDevice — configurable device for use across test modules
// ---------------------------------------------------------------------------

pub struct MockDevice {
    pub id: String,
    pub name: String,
    pub vendor: String,
    pub model: String,
    pub visibility: VisibilitySlot,
    /// Controls the return value of `initialize()`. Use builder methods (`ok_false`, `fail`, `init_panics`).
    init_behavior: InitBehavior,
    /// Set to `true` by the default `load_state()` override when tracking is enabled.
    pub load_called: Arc<AtomicBool>,
    // Capability slots — `None` means the capability is absent.
    pub fan: Option<FanStateSlot>,
    pub rgb: Option<RgbStateSlot>,
    pub lcd: Option<LcdStateSlot>,
    pub choice: Option<ChoiceStateCache>,
    pub range: Option<RangeStateCache>,
    // Boolean capability
    pub booleans: Option<Vec<Boolean>>,
    pub bool_last_set: Option<Mutex<Option<(String, bool)>>>,
    // Action capability
    pub action_last_key: Option<Mutex<Option<String>>>,
    // DPI capability — present when `with_dpi()` is called; records last set steps
    pub dpi_last_steps: Option<Mutex<Option<Vec<u16>>>>,
    // Tracking: last (key, value) set via set_choice / set_range
    pub choice_last_set: Option<Mutex<Option<(String, usize)>>>,
    pub range_last_set: Option<Mutex<Option<(String, i32)>>>,
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
            init_behavior: InitBehavior::OkTrue,
            load_called: Arc::new(AtomicBool::new(false)),
            fan: None,
            rgb: None,
            lcd: None,
            choice: None,
            range: None,
            booleans: None,
            bool_last_set: None,
            action_last_key: None,
            dpi_last_steps: None,
            choice_last_set: None,
            range_last_set: None,
        }
    }

    /// Make `initialize()` return `Ok(false)`.
    pub fn ok_false(mut self) -> Self {
        self.init_behavior = InitBehavior::OkFalse;
        self
    }

    /// Make `initialize()` return `Err(...)`.
    pub fn fail(mut self) -> Self {
        self.init_behavior = InitBehavior::Err;
        self
    }

    /// Make `initialize()` panic — asserts it is never called.
    pub fn init_panics(mut self) -> Self {
        self.init_behavior = InitBehavior::Panic;
        self
    }

    /// Override the display name (default: "mock").
    pub fn with_name(mut self, name: &str) -> Self {
        self.name = name.to_string();
        self
    }

    /// Override the vendor string (default: "mock").
    pub fn with_vendor(mut self, vendor: &str) -> Self {
        self.vendor = vendor.to_string();
        self
    }

    /// Override the model string (default: "mock").
    pub fn with_model(mut self, model: &str) -> Self {
        self.model = model.to_string();
        self
    }

    /// Add a FanCapability backed by a default `FanStateSlot`.
    pub fn with_fan(mut self) -> Self {
        self.fan = Some(FanStateSlot::default());
        self
    }

    /// Add an RgbCapability backed by a default `RgbStateSlot`.
    pub fn with_rgb(mut self) -> Self {
        self.rgb = Some(RgbStateSlot::default());
        self
    }

    /// Add an LcdCapability backed by a default `LcdStateSlot`.
    pub fn with_lcd(mut self) -> Self {
        self.lcd = Some(LcdStateSlot::default());
        self
    }

    /// Add a ChoiceCapability backed by a default `ChoiceStateCache`.
    pub fn with_choice(mut self) -> Self {
        self.choice = Some(ChoiceStateCache::default());
        self.choice_last_set = Some(Mutex::new(None));
        self
    }

    /// Add a RangeCapability backed by a default `RangeStateCache`.
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

    /// Add a DpiCapability (records the last `set_dpi_steps` call).
    pub fn with_dpi(mut self) -> Self {
        self.dpi_last_steps = Some(Mutex::new(None));
        self
    }
}

// ---------------------------------------------------------------------------
// Device implementation
// ---------------------------------------------------------------------------

#[async_trait]
impl Device for MockDevice {
    fn id(&self) -> String {
        self.id.clone()
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
    async fn close(&self) {}

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
            caps.push(CapabilityRef::Fan(self));
        }
        if self.rgb.is_some() {
            caps.push(CapabilityRef::Rgb(self));
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
        caps
    }

    fn visibility_slot(&self) -> Option<&VisibilitySlot> {
        Some(&self.visibility)
    }
}

// ---------------------------------------------------------------------------
// FanCapability
// ---------------------------------------------------------------------------

#[async_trait]
impl FanCapability for MockDevice {
    async fn get_duty(&self) -> Result<u8> {
        Ok(0)
    }
    async fn set_duty(&self, _: u8) -> Result<()> {
        Ok(())
    }
    async fn get_rpm(&self) -> Option<u32> {
        None
    }
    fn fan_state(&self) -> &FanStateSlot {
        self.fan.as_ref().expect("MockDevice: FanStateSlot not present — call .with_fan()")
    }
}

// ---------------------------------------------------------------------------
// RgbCapability
// ---------------------------------------------------------------------------

static EMPTY_RGB_DESC: std::sync::OnceLock<RgbDescriptor> = std::sync::OnceLock::new();

#[async_trait]
impl RgbCapability for MockDevice {
    fn descriptor(&self) -> &RgbDescriptor {
        EMPTY_RGB_DESC.get_or_init(|| RgbDescriptor {
            zones: vec![],
            native_effects: vec![],
        })
    }
    async fn apply(&self, _: RgbState) -> Result<()> {
        Ok(())
    }
    async fn write_frame(&self, _: &str, _: &[RgbColor]) -> Result<()> {
        Ok(())
    }
    fn rgb_state(&self) -> &RgbStateSlot {
        self.rgb.as_ref().expect("MockDevice: RgbStateSlot not present — call .with_rgb()")
    }
}

// ---------------------------------------------------------------------------
// LcdCapability
// ---------------------------------------------------------------------------

static MOCK_LCD_DESC: std::sync::OnceLock<LcdDescriptor> = std::sync::OnceLock::new();

#[async_trait]
impl LcdCapability for MockDevice {
    fn lcd_descriptor(&self) -> LcdDescriptor {
        MOCK_LCD_DESC
            .get_or_init(|| LcdDescriptor {
                shape: ScreenShape::Square,
                width: 1,
                height: 1,
                supported_rotations: vec![],
                supported_image_types: vec![],
            })
            .clone()
    }
    fn lcd_state(&self) -> &LcdStateSlot {
        self.lcd.as_ref().expect("MockDevice: LcdStateSlot not present — call .with_lcd()")
    }
    async fn set_image(&self, _: &[u8]) -> Result<()> {
        Ok(())
    }
    async fn set_rotation(&self, _: u32) -> Result<()> {
        Ok(())
    }
    async fn set_brightness(&self, _: u8) -> Result<()> {
        Ok(())
    }
    async fn reset_to_default(&self) -> Result<()> {
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ChoiceCapability
// ---------------------------------------------------------------------------

#[async_trait]
impl ChoiceCapability for MockDevice {
    fn choice_cache(&self) -> &ChoiceStateCache {
        self.choice.as_ref().expect("MockDevice: ChoiceStateCache not present — call .with_choice()")
    }
    async fn set_choice(&self, key: &str, selected: usize) -> Result<()> {
        if let Some(ref last) = self.choice_last_set {
            *last.lock().unwrap() = Some((key.to_string(), selected));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// RangeCapability
// ---------------------------------------------------------------------------

#[async_trait]
impl RangeCapability for MockDevice {
    fn range_cache(&self) -> &RangeStateCache {
        self.range.as_ref().expect("MockDevice: RangeStateCache not present — call .with_range()")
    }
    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        if let Some(ref last) = self.range_last_set {
            *last.lock().unwrap() = Some((key.to_string(), value));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// BooleanCapability
// ---------------------------------------------------------------------------

#[async_trait]
impl BooleanCapability for MockDevice {
    async fn get_booleans(&self) -> Result<Vec<Boolean>> {
        Ok(self
            .booleans
            .as_ref()
            .cloned()
            .unwrap_or_default())
    }
    async fn set_boolean(&self, key: &str, value: bool) -> Result<()> {
        if let Some(ref last) = self.bool_last_set {
            *last.lock().unwrap() = Some((key.to_string(), value));
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// ActionCapability
// ---------------------------------------------------------------------------

#[async_trait]
impl ActionCapability for MockDevice {
    async fn trigger_action(&self, key: &str) -> Result<()> {
        if let Some(ref last) = self.action_last_key {
            *last.lock().unwrap() = Some(key.to_string());
        }
        Ok(())
    }
}

// ---------------------------------------------------------------------------
// DpiCapability
// ---------------------------------------------------------------------------

#[async_trait]
impl DpiCapability for MockDevice {
    async fn dpi_status(&self) -> DpiStatus {
        DpiStatus {
            steps: vec![],
            current_index: 0,
            current_dpi: 0,
            available_dpis: vec![],
            mode: DpiMode::Host,
        }
    }
    async fn set_dpi_steps(&self, steps: Vec<u16>) -> Result<()> {
        if let Some(ref last) = self.dpi_last_steps {
            *last.lock().unwrap() = Some(steps);
        }
        Ok(())
    }
    async fn set_dpi_index(&self, _index: usize) -> Result<()> {
        Ok(())
    }
    async fn set_dpi_direct(&self, _dpi: u16) -> Result<()> {
        Ok(())
    }
}
