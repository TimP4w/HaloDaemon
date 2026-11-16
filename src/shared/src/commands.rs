// SPDX-License-Identifier: GPL-3.0-or-later
//
// Typed representation of every IPC command the daemon understands.
//
// All payloads use typed protocol types from the crate; serde tags produce
// the same JSON wire format as the previous opaque Value fields.

use std::collections::HashMap;

use crate::types::{
    ButtonMapping, ChannelKind, EffectDef, EffectParamValue, IntegrationSetupMode, LightingState,
    MacroStep, PluginRepoLocation, SamplingMode, VisibilityState, ZoneTopology,
};
use crate::zone_transform::ZoneContentTransform;
use serde::{Deserialize, Serialize};

/// The engines that `SetEngineConfig` can target. Typed on the wire so adding an
/// engine is a compile-time change in both the daemon and the UI.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum EngineKind {
    FanCurve,
    Canvas,
    Lcd,
}

/// The trackable unit that a `RemoveProfileOverride` command targets.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum OverrideTarget {
    DeviceCapability {
        device_id: String,
        state_key: String,
    },
    Canvas,
}

/// Typed LCD screen rotation values. Only 90° multiples are physically supported.
#[derive(Debug, Default, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ScreenRotation {
    #[default]
    R0,
    R90,
    R180,
    R270,
}

impl ScreenRotation {
    pub fn degrees(self) -> u32 {
        match self {
            Self::R0 => 0,
            Self::R90 => 90,
            Self::R180 => 180,
            Self::R270 => 270,
        }
    }

    pub fn from_degrees(degrees: u32) -> Self {
        match degrees % 360 {
            90 => Self::R90,
            180 => Self::R180,
            270 => Self::R270,
            _ => Self::R0,
        }
    }
}

/// Typed IPC commands shared between the daemon and the UI.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonCommand {
    /// A command scoped to one device; it runs under that device's lock.
    Device {
        device_id: String,
        op: DeviceOp,
    },
    /// No-op keepalive
    Ping,

    // Device capability settings

    // Profile management
    AddProfile {
        name: String,
    },
    RenameProfile {
        old_name: String,
        new_name: String,
    },
    RemoveProfile {
        name: String,
    },
    SwitchProfile {
        name: String,
    },
    RemoveProfileOverride {
        profile: String,
        target: OverrideTarget,
    },
    /// Persist the RGB Lighting view's device/zone selection in the active profile.
    SetLightingTargets {
        device_ids: Vec<String>,
        #[serde(default)]
        channels: HashMap<String, Vec<String>>,
    },

    // App rules
    AddAppRule {
        process_names: Vec<String>,
        profile: String,
        enabled: bool,
    },
    UpdateAppRule {
        index: usize,
        process_names: Vec<String>,
        profile: String,
        enabled: bool,
    },
    RemoveAppRule {
        index: usize,
    },

    // Misc / global
    Rediscover,
    /// Report whether the effective Linux udev rules match the currently
    /// installed plugin catalog.
    GetUdevRulesStatus,
    /// Enable or disable a device plugin by id. Applies immediately and only
    /// reconciles devices owned by that plugin.
    SetPluginEnabled {
        id: String,
        enabled: bool,
    },
    /// Replace a plugin's user-editable config values (see `ConfigFieldDef`).
    /// A `secure` field's key is included only when the user typed a new
    /// value; an absent secure key leaves the previously stored secret
    /// unchanged, so secrets are never round-tripped through the GUI or
    /// cleared by accident. Applies immediately.
    SetPluginConfig {
        id: String,
        values: HashMap<String, String>,
    },
    AddPluginRepo {
        source: PluginRepoLocation,
    },
    /// Unregister a repository and uninstall every plugin it provided.
    RemovePluginRepo {
        slug: String,
    },
    /// Fetch every remote catalog; never installs anything.
    RefreshPluginRepos,
    /// Install or update one plugin from a repository's catalog.
    InstallPlugin {
        repo: String,
        id: String,
    },
    UninstallPlugin {
        id: String,
    },
    /// Update every installed plugin whose catalog has a newer compatible version.
    UpdateAllPlugins,
    /// Enable or disable a single integration, independent of the generic
    /// plugin toggle (which only governs whether its Lua may run at all —
    /// see `SetPluginEnabled`). Applies immediately, scoped to just this
    /// integration's root device and the devices it exposes.
    SetIntegrationEnabled {
        id: String,
        enabled: bool,
    },
    /// Replace a single integration's user-editable config values and
    /// reconnect just that integration. Applies immediately.
    SetIntegrationConfig {
        id: String,
        values: HashMap<String, String>,
    },
    SetLogLevel {
        level: String,
    },
    SetLanguage {
        lang: String,
    },
    /// Replace the Home dashboard's widget row. Adds, removals and reorders
    /// are all whole-list mutations, so one command covers them.
    SetHomeWidgets {
        widgets: Vec<crate::types::HomeWidget>,
    },
    /// Replace the Cooling page's widget row. Mirrors [`SetHomeWidgets`] but
    /// persists to a separate config key so the two pages don't share a row.
    SetCoolingWidgets {
        widgets: Vec<crate::types::HomeWidget>,
    },
    SetUiConfig {
        close_to_tray: bool,
        suppress_dependency_warning: bool,
        hide_window_controls: bool,
        low_battery_notifications: bool,
    },
    MarkTourSeen {
        tour: String,
    },
    ResetToursSeen,
    SetFanFailsafeDuty {
        duty: u8,
    },
    /// Hide or disable one of a device's lighting or cooling channels. Channel
    /// ids are only unique per capability, hence `kind`.
    SetChannelVisibility {
        device_id: String,
        kind: ChannelKind,
        channel_id: String,
        state: VisibilityState,
    },

    // RGB

    // Key remap
    /// Run a macro on the host immediately (editor test play); no device id.
    PlayMacro {
        steps: Vec<MacroStep>,
    },

    // Onboard profiles

    // LCD screen
    ListLcdImages,
    DeleteLcdImage {
        filename: String,
    },
    /// Fetch a plugin's display-only asset; the daemon replies with a `plugin_asset` frame of base64 bytes.
    GetPluginAsset {
        plugin_id: String,
        name: String,
    },
    /// Fetch a declared LCD widget SVG after daemon-side validation and rasterization.
    GetLcdWidgetIcon {
        catalog_id: String,
    },
    /// Fetch and validate a declarative plugin layout preset.
    GetLcdPluginPreset {
        catalog_id: String,
    },
    /// Enumerate the host's serial ports; the daemon replies with a
    /// `serial_ports` frame for a `serial_port` config-field dropdown.
    ListSerialPorts,

    // Canvas
    CanvasUpsertEffect {
        instance_id: String,
        def: EffectDef,
    },
    CanvasRemoveEffect {
        instance_id: String,
    },
    CanvasSetDefaultEffect {
        #[serde(default, skip_serializing_if = "Option::is_none")]
        instance_id: Option<String>,
    },
    CanvasSetSampleRadius {
        radius: f64,
    },
    /// Disable the canvas engine and blank every RGB device it was driving.
    CanvasStop,
    CanvasSubscribe,

    // LCD engine
    LcdEngineSubscribe,
    SaveLcdTemplate {
        name: String,
        def: crate::lcd_custom::CustomTemplateDef,
    },
    LoadLcdTemplate {
        name: String,
    },
    DeleteLcdTemplate {
        name: String,
    },

    // Custom "Designer" direct effects
    SaveCustomEffect {
        name: String,
        params: HashMap<String, EffectParamValue>,
    },
    DeleteCustomEffect {
        name: String,
    },

    // Receiver pairing

    // Misc
    ListRunningApps,
    GetDebugInfo,
    SetEngineConfig {
        engine: EngineKind,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        enabled: Option<bool>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        tick_ms: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        fps: Option<u64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        failsafe_duty: Option<u8>,
    },
    Shutdown,
}

/// One client frame: a command and an optional id echoed on its error reply.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct Request {
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub request_id: Option<String>,
    #[serde(flatten)]
    pub command: DaemonCommand,
}

/// Device-scoped operations, carried by [`DaemonCommand::Device`].
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DeviceOp {
    /// Open the daemon-owned setup flow for an integration.
    BeginIntegrationSetup,
    /// Close and discard transient setup progress. Persisted credentials are untouched.
    CancelIntegrationSetup,
    CanvasMoveZone {
        channel_id: String,
        x: f64,
        y: f64,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rotation: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sampling_mode: Option<SamplingMode>,
    },
    CanvasPlaceZone {
        channel_id: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        x: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        y: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        w: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        h: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        rotation: Option<f64>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        effect: Option<String>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sampling_mode: Option<SamplingMode>,
    },
    CanvasRemoveZone {
        channel_id: String,
    },
    LcdEngineDeactivate,
    LcdEngineSetTemplate {
        template_id: String,
        params: HashMap<String, EffectParamValue>,
    },
    LightingAddSegment {
        channel_id: String,
        name: String,
        led_count: u32,
        topology: ZoneTopology,
    },
    LightingApply {
        state: LightingState,
    },
    LightingDetectSegments {
        channel_id: String,
    },
    LightingRemoveSegment {
        channel_id: String,
        segment_id: String,
    },
    LightingReorderSegment {
        channel_id: String,
        segment_id: String,
        new_index: usize,
    },
    LightingSetChannelTransform {
        channel_id: String,
        transform: ZoneContentTransform,
    },
    OnboardProfileRestore {
        slot: u16,
    },
    OnboardProfileSetEnabled {
        slot: u16,
        enabled: bool,
    },
    OnboardProfileSwitch {
        slot: u16,
    },
    ReceiverStartPairing {
        timeout_secs: u8,
    },
    ReceiverStopPairing,
    ReceiverUnpair {
        slot: u8,
    },
    RemoveCoolingCurve {
        channel_id: String,
    },
    /// Editor request: render each widget of `def` to its own sprite bitmap
    /// against the device's canvas. `known` is the signature (id → content
    /// signature) the requester already has cached, so the daemon can reply
    /// with only the widgets that changed. The daemon replies with an
    /// `lcd_editor_render` frame carrying an [`LcdEditorRender`].
    RenderLcdEditor {
        def: crate::lcd_custom::CustomTemplateDef,
        known: HashMap<String, u64>,
    },
    ResetAllButtonMappings,
    ResetButtonMapping {
        cid: u16,
    },
    /// Delete an integration's persisted setup data and start its setup flow again.
    ResetIntegrationSetup,
    /// Retry the manifest-declared button-pairing hook.
    RetryIntegrationPairing,
    /// Choose automatic discovery or manual configuration.
    SelectIntegrationSetupMode {
        mode: IntegrationSetupMode,
    },
    SetBoolean {
        key: String,
        value: bool,
    },
    SetButtonMapping {
        mapping: ButtonMapping,
    },
    SetChoice {
        key: String,
        selected: usize,
    },
    /// Channel-addressed curve commands for devices that expose `cooling`.
    SetCoolingCurvePoints {
        channel_id: String,
        points: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sensor_id: Option<String>,
    },
    SetCoolingCurvePreset {
        channel_id: String,
        preset: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sensor_id: Option<String>,
    },
    SetDeviceName {
        name: String,
    },
    SetDeviceVisibility {
        state: VisibilityState,
    },
    SetDpiSteps {
        steps: Vec<u32>,
    },
    SetEqBands {
        values: Vec<f32>,
    },
    SetEqPreset {
        preset_index: usize,
    },
    /// Choose a keyboard's physical variant and/or language layout. Either axis
    /// absent (Auto) resolves from the firmware-detected language.
    SetKeyboardLayout {
        selection: crate::keyboard::KeyboardLayoutSelection,
    },
    SetRange {
        key: String,
        value: i32,
    },
    SetScreenBrightness {
        brightness: u8,
    },
    SetScreenDefault,
    SetScreenImage {
        data_b64: String,
    },
    SetScreenImageFromLibrary {
        filename: String,
    },
    SetScreenRawStreaming {
        enabled: bool,
    },
    SetScreenRotation {
        rotation: ScreenRotation,
    },
    SetScreenVideo {
        path: String,
    },
    SetSoftwareDpiSteps {
        steps: Vec<u32>,
    },
    /// Select an automatic candidate or submit manual/configuration values.
    SubmitIntegrationSetup {
        candidate_id: Option<String>,
        values: HashMap<String, String>,
    },
    TriggerAction {
        key: String,
    },
}

impl DeviceOp {
    /// Whether a failure means a requested hardware write did not reach the
    /// device; configuration-only operations stay on the generic error path.
    pub fn writes_device(&self) -> bool {
        match self {
            Self::LightingApply { .. }
            | Self::LightingDetectSegments { .. }
            | Self::OnboardProfileRestore { .. }
            | Self::OnboardProfileSetEnabled { .. }
            | Self::OnboardProfileSwitch { .. }
            | Self::ReceiverStartPairing { .. }
            | Self::ReceiverStopPairing
            | Self::ReceiverUnpair { .. }
            | Self::ResetAllButtonMappings
            | Self::ResetButtonMapping { .. }
            | Self::SetBoolean { .. }
            | Self::SetButtonMapping { .. }
            | Self::SetChoice { .. }
            | Self::SetDpiSteps { .. }
            | Self::SetEqBands { .. }
            | Self::SetEqPreset { .. }
            | Self::SetKeyboardLayout { .. }
            | Self::SetRange { .. }
            | Self::SetScreenBrightness { .. }
            | Self::SetScreenDefault
            | Self::SetScreenImage { .. }
            | Self::SetScreenImageFromLibrary { .. }
            | Self::SetScreenRawStreaming { .. }
            | Self::SetScreenRotation { .. }
            | Self::SetScreenVideo { .. }
            | Self::SetSoftwareDpiSteps { .. }
            | Self::TriggerAction { .. } => true,
            Self::BeginIntegrationSetup
            | Self::CancelIntegrationSetup
            | Self::CanvasMoveZone { .. }
            | Self::CanvasPlaceZone { .. }
            | Self::CanvasRemoveZone { .. }
            | Self::LcdEngineDeactivate
            | Self::LcdEngineSetTemplate { .. }
            | Self::LightingAddSegment { .. }
            | Self::LightingRemoveSegment { .. }
            | Self::LightingReorderSegment { .. }
            | Self::LightingSetChannelTransform { .. }
            | Self::RemoveCoolingCurve { .. }
            | Self::RenderLcdEditor { .. }
            | Self::ResetIntegrationSetup
            | Self::RetryIntegrationPairing
            | Self::SelectIntegrationSetupMode { .. }
            | Self::SetCoolingCurvePoints { .. }
            | Self::SetCoolingCurvePreset { .. }
            | Self::SetDeviceName { .. }
            | Self::SetDeviceVisibility { .. }
            | Self::SubmitIntegrationSetup { .. } => false,
        }
    }
}

impl DaemonCommand {
    fn set_engine_config(
        engine: EngineKind,
        enabled: Option<bool>,
        tick_ms: Option<u64>,
        fps: Option<u64>,
        failsafe_duty: Option<u8>,
    ) -> Self {
        Self::SetEngineConfig {
            engine,
            enabled,
            tick_ms,
            fps,
            failsafe_duty,
        }
    }

    /// `SetEngineConfig` toggling only an engine's enabled flag.
    pub fn set_engine_enabled(engine: EngineKind, enabled: bool) -> Self {
        Self::set_engine_config(engine, Some(enabled), None, None, None)
    }

    /// `SetEngineConfig` setting only an engine's fps.
    pub fn set_engine_fps(engine: EngineKind, fps: u64) -> Self {
        Self::set_engine_config(engine, None, None, Some(fps), None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::types::RgbColor;
    use serde_json::json;

    fn roundtrip(cmd: &DaemonCommand) -> serde_json::Value {
        serde_json::to_value(cmd).unwrap()
    }

    fn widget() -> crate::types::HomeWidget {
        crate::types::HomeWidget {
            id: "w1".into(),
            kind: crate::types::HomeWidgetKind::Chart {
                sensor_id: "cpu".into(),
            },
            color: 2,
            label: "CPU".into(),
        }
    }

    #[test]
    fn wire_formats_are_stable_and_round_trip() {
        let host = HashMap::from([("host".to_string(), "127.0.0.1".to_string())]);
        let cases = [
            (
                DaemonCommand::SetEngineConfig {
                    engine: EngineKind::FanCurve,
                    enabled: Some(true),
                    tick_ms: None,
                    fps: None,
                    failsafe_duty: None,
                },
                json!({"type": "set_engine_config", "engine": "fan_curve", "enabled": true}),
            ),
            (
                DaemonCommand::set_engine_enabled(EngineKind::Canvas, false),
                json!({"type": "set_engine_config", "engine": "canvas", "enabled": false}),
            ),
            (
                DaemonCommand::set_engine_fps(EngineKind::Lcd, 30),
                json!({"type": "set_engine_config", "engine": "lcd", "fps": 30}),
            ),
            (
                DaemonCommand::MarkTourSeen {
                    tour: "page:home".into(),
                },
                json!({"type": "mark_tour_seen", "tour": "page:home"}),
            ),
            (
                DaemonCommand::ResetToursSeen,
                json!({"type": "reset_tours_seen"}),
            ),
            (
                DaemonCommand::SetLanguage { lang: "it".into() },
                json!({"type": "set_language", "lang": "it"}),
            ),
            (
                DaemonCommand::SetHomeWidgets {
                    widgets: vec![widget()],
                },
                json!({"type": "set_home_widgets", "widgets": [{
                    "id": "w1", "kind": {"type": "chart", "sensor_id": "cpu"}, "color": 2, "label": "CPU",
                }]}),
            ),
            (
                DaemonCommand::SetCoolingWidgets {
                    widgets: vec![widget()],
                },
                json!({"type": "set_cooling_widgets", "widgets": [{
                    "id": "w1", "kind": {"type": "chart", "sensor_id": "cpu"}, "color": 2, "label": "CPU",
                }]}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "dev1".into(),
                    op: DeviceOp::SetChoice {
                        key: "profile".into(),
                        selected: 2,
                    },
                },
                json!({"type": "device", "device_id": "dev1", "op": {"type": "set_choice", "key": "profile", "selected": 2}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "dev2".into(),
                    op: DeviceOp::SetRange {
                        key: "brightness".into(),
                        value: 75,
                    },
                },
                json!({"type": "device", "device_id": "dev2", "op": {"type": "set_range", "key": "brightness", "value": 75}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "dev3".into(),
                    op: DeviceOp::SetBoolean {
                        key: "enabled".into(),
                        value: true,
                    },
                },
                json!({"type": "device", "device_id": "dev3", "op": {"type": "set_boolean", "key": "enabled", "value": true}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "dev4".into(),
                    op: DeviceOp::TriggerAction {
                        key: "reboot".into(),
                    },
                },
                json!({"type": "device", "device_id": "dev4", "op": {"type": "trigger_action", "key": "reboot"}}),
            ),
            (DaemonCommand::Rediscover, json!({"type": "rediscover"})),
            (
                DaemonCommand::GetUdevRulesStatus,
                json!({"type": "get_udev_rules_status"}),
            ),
            (
                DaemonCommand::SetPluginEnabled {
                    id: "nzxt_kraken".into(),
                    enabled: false,
                },
                json!({"type": "set_plugin_enabled", "id": "nzxt_kraken", "enabled": false}),
            ),
            (
                DaemonCommand::GetPluginAsset {
                    plugin_id: "my_driver".into(),
                    name: "logo.png".into(),
                },
                json!({"type": "get_plugin_asset", "plugin_id": "my_driver", "name": "logo.png"}),
            ),
            (
                DaemonCommand::AddPluginRepo {
                    source: PluginRepoLocation::Url(
                        "https://example.github.io/p/catalog.yaml".into(),
                    ),
                },
                json!({"type": "add_plugin_repo", "source": {"kind": "url", "value": "https://example.github.io/p/catalog.yaml"}}),
            ),
            (
                DaemonCommand::InstallPlugin {
                    repo: "official".into(),
                    id: "nzxt_kraken".into(),
                },
                json!({"type": "install_plugin", "repo": "official", "id": "nzxt_kraken"}),
            ),
            (
                DaemonCommand::RemovePluginRepo { slug: "foo".into() },
                json!({"type": "remove_plugin_repo", "slug": "foo"}),
            ),
            (
                DaemonCommand::RefreshPluginRepos,
                json!({"type": "refresh_plugin_repos"}),
            ),
            (
                DaemonCommand::UpdateAllPlugins,
                json!({"type": "update_all_plugins"}),
            ),
            (
                DaemonCommand::SetPluginConfig {
                    id: "openrgb".into(),
                    values: host.clone(),
                },
                json!({"type": "set_plugin_config", "id": "openrgb", "values": {"host": "127.0.0.1"}}),
            ),
            (
                DaemonCommand::SetIntegrationEnabled {
                    id: "openrgb".into(),
                    enabled: false,
                },
                json!({"type": "set_integration_enabled", "id": "openrgb", "enabled": false}),
            ),
            (
                DaemonCommand::SetIntegrationConfig {
                    id: "openrgb".into(),
                    values: host,
                },
                json!({"type": "set_integration_config", "id": "openrgb", "values": {"host": "127.0.0.1"}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "nanoleaf".into(),
                    op: DeviceOp::BeginIntegrationSetup,
                },
                json!({"type": "device", "device_id": "nanoleaf", "op": {"type": "begin_integration_setup"}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "nanoleaf".into(),
                    op: DeviceOp::ResetIntegrationSetup,
                },
                json!({"type": "device", "device_id": "nanoleaf", "op": {"type": "reset_integration_setup"}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "nanoleaf".into(),
                    op: DeviceOp::SelectIntegrationSetupMode {
                        mode: crate::types::IntegrationSetupMode::Automatic,
                    },
                },
                json!({"type": "device", "device_id": "nanoleaf", "op": {"type": "select_integration_setup_mode", "mode": "automatic"}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "nanoleaf".into(),
                    op: DeviceOp::SubmitIntegrationSetup {
                        candidate_id: Some("device-1".into()),
                        values: HashMap::new(),
                    },
                },
                json!({"type": "device", "device_id": "nanoleaf", "op": {"type": "submit_integration_setup", "candidate_id": "device-1", "values": {}}}),
            ),
            (
                DaemonCommand::LoadLcdTemplate {
                    name: "My Preset".into(),
                },
                json!({"type": "load_lcd_template", "name": "My Preset"}),
            ),
            (
                DaemonCommand::DeleteLcdTemplate {
                    name: "My Preset".into(),
                },
                json!({"type": "delete_lcd_template", "name": "My Preset"}),
            ),
            (
                DaemonCommand::SaveCustomEffect {
                    name: "Comet".into(),
                    params: HashMap::from([("speed".to_string(), EffectParamValue::Float(50.0))]),
                },
                json!({"type": "save_custom_effect", "name": "Comet", "params": {"speed": 50.0}}),
            ),
            (
                DaemonCommand::DeleteCustomEffect {
                    name: "Comet".into(),
                },
                json!({"type": "delete_custom_effect", "name": "Comet"}),
            ),
            (DaemonCommand::CanvasStop, json!({"type": "canvas_stop"})),
            (
                DaemonCommand::SwitchProfile {
                    name: "gaming".into(),
                },
                json!({"type": "switch_profile", "name": "gaming"}),
            ),
            (
                DaemonCommand::SetLightingTargets {
                    device_ids: vec!["dev1".into()],
                    channels: HashMap::from([("dev1".to_string(), vec!["z0".to_string()])]),
                },
                json!({"type": "set_lighting_targets", "device_ids": ["dev1"], "channels": {"dev1": ["z0"]}}),
            ),
            (
                DaemonCommand::RenameProfile {
                    old_name: "old".into(),
                    new_name: "new".into(),
                },
                json!({"type": "rename_profile", "old_name": "old", "new_name": "new"}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "kraken".into(),
                    op: DeviceOp::SetCoolingCurvePoints {
                        channel_id: "pump".into(),
                        points: vec![[30.0, 20.0], [80.0, 100.0]],
                        sensor_id: None,
                    },
                },
                json!({"type": "device", "device_id": "kraken", "op": {"type": "set_cooling_curve_points", "channel_id": "pump", "points": [[30.0, 20.0], [80.0, 100.0]]}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "d".into(),
                    op: DeviceOp::LightingApply {
                        state: LightingState::Static {
                            color: RgbColor { r: 1, g: 2, b: 3 },
                        },
                    },
                },
                json!({"type": "device", "device_id": "d", "op": {"type": "lighting_apply", "state": {"mode": "static", "color": {"r": 1, "g": 2, "b": 3}}}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "d".into(),
                    op: DeviceOp::LightingApply {
                        state: LightingState::DirectEffect {
                            id: "rainbow".into(),
                            params: HashMap::new(),
                        },
                    },
                },
                json!({"type": "device", "device_id": "d", "op": {"type": "lighting_apply", "state": {"mode": "direct_effect", "id": "rainbow", "params": {}}}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "rcv".into(),
                    op: DeviceOp::ReceiverStartPairing { timeout_secs: 30 },
                },
                json!({"type": "device", "device_id": "rcv", "op": {"type": "receiver_start_pairing", "timeout_secs": 30}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "rcv".into(),
                    op: DeviceOp::ReceiverStopPairing,
                },
                json!({"type": "device", "device_id": "rcv", "op": {"type": "receiver_stop_pairing"}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "rcv".into(),
                    op: DeviceOp::ReceiverUnpair { slot: 2 },
                },
                json!({"type": "device", "device_id": "rcv", "op": {"type": "receiver_unpair", "slot": 2}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "kbd".into(),
                    op: DeviceOp::SetKeyboardLayout {
                        selection: crate::keyboard::KeyboardLayoutSelection {
                            variant: Some(crate::keyboard::KeyVariant::Iso),
                            language: Some(crate::types::KeyboardLayout::CH),
                        },
                    },
                },
                json!({"type": "device", "device_id": "kbd", "op": {"type": "set_keyboard_layout", "selection": {"variant": "iso", "language": "c_h"}}}),
            ),
            (
                DaemonCommand::Device {
                    device_id: "kbd".into(),
                    op: DeviceOp::SetKeyboardLayout {
                        selection: Default::default(),
                    },
                },
                json!({"type": "device", "device_id": "kbd", "op": {"type": "set_keyboard_layout", "selection": {}}}),
            ),
            (DaemonCommand::Shutdown, json!({"type": "shutdown"})),
            (DaemonCommand::Ping, json!({"type": "ping"})),
        ];
        for (cmd, wire) in cases {
            assert_eq!(roundtrip(&cmd), wire);
            let parsed: DaemonCommand = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(roundtrip(&parsed), wire);
            let mut framed = wire.clone();
            framed["request_id"] = json!("req-1");
            let request: Request = serde_json::from_value(framed.clone()).unwrap();
            assert_eq!(request.request_id.as_deref(), Some("req-1"));
            assert_eq!(roundtrip(&request.command), wire);
            assert_eq!(serde_json::to_value(&request).unwrap(), framed);
        }
    }

    #[test]
    fn screen_rotation_is_a_string() {
        assert_eq!(
            serde_json::to_value(ScreenRotation::R270).unwrap(),
            json!("r270")
        );
        assert_eq!(
            serde_json::from_value::<ScreenRotation>(json!("r90")).unwrap(),
            ScreenRotation::R90
        );
    }

    #[test]
    fn engine_kind_rejects_unknown_engine() {
        assert!(serde_json::from_value::<EngineKind>(json!("nonsense")).is_err());
    }

    #[test]
    fn upsert_effect_name_defaults_to_none() {
        let cmd: DaemonCommand = serde_json::from_value(json!({
            "type": "canvas_upsert_effect",
            "instance_id": "a",
            "def": { "effect_id": "screen_sampler" }
        }))
        .unwrap();
        match cmd {
            DaemonCommand::CanvasUpsertEffect { def, .. } => assert_eq!(def.name, None),
            other => panic!("wrong variant: {other:?}"),
        }
    }

    #[test]
    fn ignores_unmodelled_fields() {
        let cmd: DaemonCommand =
            serde_json::from_value(json!({"type": "canvas_subscribe", "ignored": true})).unwrap();
        assert!(matches!(cmd, DaemonCommand::CanvasSubscribe));
        let cmd: DaemonCommand = serde_json::from_value(
            json!({"type": "device", "device_id": "d", "op": {"type": "canvas_place_zone", "channel_id": "z", "x": 0.1, "y": 0.2}}),
        )
        .unwrap();
        assert!(matches!(
            cmd,
            DaemonCommand::Device {
                op: DeviceOp::CanvasPlaceZone { .. },
                ..
            }
        ));
    }

    #[test]
    fn parses_ui_payloads() {
        for payload in [
            json!({"type": "device", "device_id": "d", "op": {"type": "set_device_visibility", "state": "hidden"}}),
            json!({"type": "set_channel_visibility", "device_id": "d", "kind": "cooling", "channel_id": "fan1", "state": "disabled"}),
            json!({"type": "device", "device_id": "d", "op": {"type": "set_device_name", "name": "My Fan"}}),
            json!({"type": "remove_profile_override", "profile": "Gaming",
                "target": { "kind": "device_capability", "device_id": "d", "state_key": "fan_curve" }}),
            json!({"type": "remove_profile_override", "profile": "Gaming", "target": { "kind": "canvas" }}),
        ] {
            let cmd: DaemonCommand = serde_json::from_value(payload.clone()).unwrap();
            assert_eq!(roundtrip(&cmd), payload);
        }
    }
}
