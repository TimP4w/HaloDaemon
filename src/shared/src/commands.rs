// SPDX-License-Identifier: GPL-3.0-or-later
//
// Typed representation of every IPC command the daemon understands.
//
// All payloads use typed protocol types from the crate; serde tags produce
// the same JSON wire format as the previous opaque Value fields.

use std::collections::HashMap;

use crate::types::{
    ButtonMapping, ChannelKind, EffectDef, EffectParamValue, IntegrationSetupMode, LightingState,
    MacroStep, PluginAuthority, SamplingMode, VisibilityState, ZoneTopology,
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

/// One authority snapshot in a batch plugin-enable confirmation.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct PluginEnableConfirmation {
    pub id: String,
    pub authority: PluginAuthority,
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
    /// No-op keepalive
    Ping,

    // Device capability settings
    SetChoice {
        id: String,
        key: String,
        selected: usize,
    },
    SetRange {
        id: String,
        key: String,
        value: i32,
    },
    SetBoolean {
        id: String,
        key: String,
        value: bool,
    },
    TriggerAction {
        id: String,
        key: String,
    },

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
    /// Import a complete local repository from a Git directory or a
    /// `.tar`/`.tar.gz` archive. Standalone plugin packages are not accepted.
    ImportPluginRepository {
        source_path: String,
    },
    /// Confirm the exact manifest-derived authority currently displayed in the
    /// enable modal, then enable the plugin. The daemon rejects stale snapshots
    /// so an update cannot race a user's confirmation.
    ConfirmPluginEnable {
        id: String,
        authority: PluginAuthority,
    },
    /// Confirm and enable several plugins as one operation. Every authority is
    /// validated independently; valid plugins are enabled together even when
    /// another entry fails validation.
    ConfirmPluginEnableBatch {
        plugins: Vec<PluginEnableConfirmation>,
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
    /// Register an immutable GitHub plugin release source.
    AddPluginRepo {
        url: String,
    },
    /// Unregister a plugin release source, purging every plugin id it contributed.
    RemovePluginRepo {
        slug: String,
    },
    /// List immutable plugin releases for a GitHub source.
    ListPluginReleases {
        url: String,
    },
    /// Check every registered source for a newer selected release.
    CheckPluginRepoUpdates,
    /// Install the selected release for a plugin source.
    UpdatePluginRepo {
        slug: String,
    },
    /// Atomically install a specific release and pin the source to its tag.
    InstallPluginRelease {
        slug: String,
        tag: String,
    },
    /// Follow the newest non-prerelease again after a source was pinned.
    FollowLatestPluginRelease {
        slug: String,
    },
    /// Update every plugin currently flagged with an update available, across every repo.
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
    /// Open the daemon-owned setup flow for an integration.
    BeginIntegrationSetup {
        id: String,
    },
    /// Delete an integration's persisted setup data and start its setup flow again.
    ResetIntegrationSetup {
        id: String,
    },
    /// Choose automatic discovery or manual configuration.
    SelectIntegrationSetupMode {
        id: String,
        mode: IntegrationSetupMode,
    },
    /// Select an automatic candidate or submit manual/configuration values.
    SubmitIntegrationSetup {
        id: String,
        candidate_id: Option<String>,
        values: HashMap<String, String>,
    },
    /// Retry the manifest-declared button-pairing hook.
    RetryIntegrationPairing {
        id: String,
    },
    /// Close and discard transient setup progress. Persisted credentials are untouched.
    CancelIntegrationSetup {
        id: String,
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
    /// Allow or deny the daemon contacting GitHub for official plugins and
    /// automatic update checks. An allowed request also retries a missing
    /// official-repo checkout and starts an update check.
    SetPluginDownloadConsent {
        allowed: bool,
    },
    MarkTourSeen {
        tour: String,
    },
    ResetToursSeen,
    SetFanFailsafeDuty {
        duty: u8,
    },
    ResetAllButtonMappings {
        id: String,
    },
    ResetButtonMapping {
        id: String,
        cid: u16,
    },
    SetEqPreset {
        id: String,
        preset_index: usize,
    },
    SetEqBands {
        id: String,
        values: Vec<f32>,
    },
    SetDpiSteps {
        id: String,
        steps: Vec<u32>,
    },
    SetDeviceVisibility {
        device_id: String,
        state: VisibilityState,
    },
    /// Hide or disable one of a device's lighting or cooling channels. Channel
    /// ids are only unique per capability, hence `kind`.
    SetChannelVisibility {
        device_id: String,
        kind: ChannelKind,
        channel_id: String,
        state: VisibilityState,
    },
    SetDeviceName {
        device_id: String,
        name: String,
    },
    /// Choose a keyboard's physical variant and/or language layout. Either axis
    /// absent (Auto) resolves from the firmware-detected language.
    SetKeyboardLayout {
        id: String,
        selection: crate::keyboard::KeyboardLayoutSelection,
    },

    /// Channel-addressed curve commands for devices that expose `cooling`.
    SetCoolingCurvePoints {
        device_id: String,
        channel_id: String,
        points: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sensor_id: Option<String>,
    },
    SetCoolingCurvePreset {
        device_id: String,
        channel_id: String,
        preset: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sensor_id: Option<String>,
    },
    RemoveCoolingCurve {
        device_id: String,
        channel_id: String,
    },

    // RGB
    LightingApply {
        id: String,
        state: LightingState,
    },
    LightingSetChannelTransform {
        id: String,
        channel_id: String,
        transform: ZoneContentTransform,
    },
    LightingAddSegment {
        id: String,
        channel_id: String,
        name: String,
        led_count: u32,
        topology: ZoneTopology,
    },
    LightingRemoveSegment {
        id: String,
        channel_id: String,
        device_id: String,
    },
    LightingReorderSegment {
        id: String,
        channel_id: String,
        device_id: String,
        new_index: usize,
    },
    LightingDetectSegments {
        id: String,
        channel_id: String,
    },

    // Key remap
    SetButtonMapping {
        id: String,
        mapping: ButtonMapping,
    },
    SetSoftwareDpiSteps {
        id: String,
        steps: Vec<u32>,
    },
    /// Run a macro on the host immediately (editor test play); no device id.
    PlayMacro {
        steps: Vec<MacroStep>,
    },

    // Onboard profiles
    OnboardProfileSwitch {
        id: String,
        slot: u16,
    },
    OnboardProfileRestore {
        id: String,
        slot: u16,
    },
    OnboardProfileSetEnabled {
        id: String,
        slot: u16,
        enabled: bool,
    },

    // LCD screen
    SetScreenImage {
        id: String,
        data_b64: String,
    },
    SetScreenImageFromLibrary {
        id: String,
        filename: String,
    },
    SetScreenRotation {
        id: String,
        rotation: ScreenRotation,
    },
    SetScreenBrightness {
        id: String,
        brightness: u8,
    },
    SetScreenDefault {
        id: String,
    },
    SetScreenRawStreaming {
        id: String,
        enabled: bool,
    },
    SetScreenVideo {
        id: String,
        path: String,
    },
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
    CanvasPlaceZone {
        device_id: String,
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
        device_id: String,
        channel_id: String,
    },
    CanvasMoveZone {
        device_id: String,
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
    CanvasSetSampleRadius {
        radius: f64,
    },
    /// Disable the canvas engine and blank every RGB device it was driving.
    CanvasStop,
    CanvasSubscribe,

    // LCD engine
    LcdEngineSetTemplate {
        device_id: String,
        template_id: String,
        params: HashMap<String, EffectParamValue>,
    },
    LcdEngineDeactivate {
        device_id: String,
    },
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
    /// Editor request: render each widget of `def` to its own sprite bitmap
    /// against the device's canvas. `known` is the signature (id → content
    /// signature) the requester already has cached, so the daemon can reply
    /// with only the widgets that changed. The daemon replies with an
    /// `lcd_editor_render` frame carrying an [`LcdEditorRender`].
    RenderLcdEditor {
        device_id: String,
        def: crate::lcd_custom::CustomTemplateDef,
        known: HashMap<String, u64>,
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
    ReceiverStartPairing {
        id: String,
        timeout_secs: u8,
    },
    ReceiverStopPairing {
        id: String,
    },
    ReceiverUnpair {
        id: String,
        slot: u8,
    },

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
                DaemonCommand::SetChoice {
                    id: "dev1".into(),
                    key: "profile".into(),
                    selected: 2,
                },
                json!({"type": "set_choice", "id": "dev1", "key": "profile", "selected": 2}),
            ),
            (
                DaemonCommand::SetRange {
                    id: "dev2".into(),
                    key: "brightness".into(),
                    value: 75,
                },
                json!({"type": "set_range", "id": "dev2", "key": "brightness", "value": 75}),
            ),
            (
                DaemonCommand::SetBoolean {
                    id: "dev3".into(),
                    key: "enabled".into(),
                    value: true,
                },
                json!({"type": "set_boolean", "id": "dev3", "key": "enabled", "value": true}),
            ),
            (
                DaemonCommand::TriggerAction {
                    id: "dev4".into(),
                    key: "reboot".into(),
                },
                json!({"type": "trigger_action", "id": "dev4", "key": "reboot"}),
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
                DaemonCommand::ImportPluginRepository {
                    source_path: "/r.tar.gz".into(),
                },
                json!({"type": "import_plugin_repository", "source_path": "/r.tar.gz"}),
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
                    url: "https://github.com/example/foo".into(),
                },
                json!({"type": "add_plugin_repo", "url": "https://github.com/example/foo"}),
            ),
            (
                DaemonCommand::RemovePluginRepo { slug: "foo".into() },
                json!({"type": "remove_plugin_repo", "slug": "foo"}),
            ),
            (
                DaemonCommand::CheckPluginRepoUpdates,
                json!({"type": "check_plugin_repo_updates"}),
            ),
            (
                DaemonCommand::UpdatePluginRepo { slug: "foo".into() },
                json!({"type": "update_plugin_repo", "slug": "foo"}),
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
                DaemonCommand::BeginIntegrationSetup {
                    id: "nanoleaf".into(),
                },
                json!({"type": "begin_integration_setup", "id": "nanoleaf"}),
            ),
            (
                DaemonCommand::ResetIntegrationSetup {
                    id: "nanoleaf".into(),
                },
                json!({"type": "reset_integration_setup", "id": "nanoleaf"}),
            ),
            (
                DaemonCommand::SelectIntegrationSetupMode {
                    id: "nanoleaf".into(),
                    mode: crate::types::IntegrationSetupMode::Automatic,
                },
                json!({"type": "select_integration_setup_mode", "id": "nanoleaf", "mode": "automatic"}),
            ),
            (
                DaemonCommand::SubmitIntegrationSetup {
                    id: "nanoleaf".into(),
                    candidate_id: Some("device-1".into()),
                    values: HashMap::new(),
                },
                json!({"type": "submit_integration_setup", "id": "nanoleaf", "candidate_id": "device-1", "values": {}}),
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
                DaemonCommand::SetCoolingCurvePoints {
                    device_id: "kraken".into(),
                    channel_id: "pump".into(),
                    points: vec![[30.0, 20.0], [80.0, 100.0]],
                    sensor_id: None,
                },
                json!({
                    "type": "set_cooling_curve_points", "device_id": "kraken", "channel_id": "pump",
                    "points": [[30.0, 20.0], [80.0, 100.0]]
                }),
            ),
            (
                DaemonCommand::LightingApply {
                    id: "d".into(),
                    state: LightingState::Static {
                        color: RgbColor { r: 1, g: 2, b: 3 },
                    },
                },
                json!({"type": "lighting_apply", "id": "d", "state": {"mode": "static", "color": {"r": 1, "g": 2, "b": 3}}}),
            ),
            (
                DaemonCommand::LightingApply {
                    id: "d".into(),
                    state: LightingState::DirectEffect {
                        id: "rainbow".into(),
                        params: HashMap::new(),
                    },
                },
                json!({"type": "lighting_apply", "id": "d", "state": {"mode": "direct_effect", "id": "rainbow", "params": {}}}),
            ),
            (
                DaemonCommand::ReceiverStartPairing {
                    id: "rcv".into(),
                    timeout_secs: 30,
                },
                json!({"type": "receiver_start_pairing", "id": "rcv", "timeout_secs": 30}),
            ),
            (
                DaemonCommand::ReceiverStopPairing { id: "rcv".into() },
                json!({"type": "receiver_stop_pairing", "id": "rcv"}),
            ),
            (
                DaemonCommand::ReceiverUnpair {
                    id: "rcv".into(),
                    slot: 2,
                },
                json!({"type": "receiver_unpair", "id": "rcv", "slot": 2}),
            ),
            (
                DaemonCommand::SetKeyboardLayout {
                    id: "kbd".into(),
                    selection: crate::keyboard::KeyboardLayoutSelection {
                        variant: Some(crate::keyboard::KeyVariant::Iso),
                        language: Some(crate::types::KeyboardLayout::CH),
                    },
                },
                json!({"type": "set_keyboard_layout", "id": "kbd", "selection": {"variant": "iso", "language": "c_h"}}),
            ),
            (
                DaemonCommand::SetKeyboardLayout {
                    id: "kbd".into(),
                    selection: Default::default(),
                },
                json!({"type": "set_keyboard_layout", "id": "kbd", "selection": {}}),
            ),
            (DaemonCommand::Shutdown, json!({"type": "shutdown"})),
            (DaemonCommand::Ping, json!({"type": "ping"})),
        ];
        for (cmd, wire) in cases {
            assert_eq!(roundtrip(&cmd), wire);
            let parsed: DaemonCommand = serde_json::from_value(wire.clone()).unwrap();
            assert_eq!(roundtrip(&parsed), wire);
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
            json!({"type": "canvas_place_zone", "device_id": "d", "channel_id": "z", "x": 0.1, "y": 0.2}),
        )
        .unwrap();
        assert!(matches!(cmd, DaemonCommand::CanvasPlaceZone { .. }));
    }

    #[test]
    fn parses_ui_payloads() {
        for payload in [
            json!({"type": "set_device_visibility", "device_id": "d", "state": "hidden"}),
            json!({"type": "set_channel_visibility", "device_id": "d", "kind": "cooling", "channel_id": "fan1", "state": "disabled"}),
            json!({"type": "set_device_name", "device_id": "d", "name": "My Fan"}),
            json!({"type": "remove_profile_override", "profile": "Gaming",
                "target": { "kind": "device_capability", "device_id": "d", "state_key": "fan_curve" }}),
            json!({"type": "remove_profile_override", "profile": "Gaming", "target": { "kind": "canvas" }}),
        ] {
            let cmd: DaemonCommand = serde_json::from_value(payload.clone()).unwrap();
            assert_eq!(roundtrip(&cmd), payload);
        }
    }
}
