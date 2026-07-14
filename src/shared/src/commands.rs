// SPDX-License-Identifier: GPL-3.0-or-later
//
// Typed representation of every IPC command the daemon understands.
//
// All payloads use typed protocol types from the crate; serde tags produce
// the same JSON wire format as the previous opaque Value fields.

use std::collections::HashMap;

use crate::types::{
    ButtonMapping, EffectDef, EffectParamValue, MacroStep, PluginAuthority, RgbState, SamplingMode,
    VisibilityState, ZoneTopology,
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
        target: OverrideTarget,
    },
    /// Persist the RGB Lighting view's device/zone selection in the active profile.
    SetLightingTargets {
        device_ids: Vec<String>,
        #[serde(default)]
        zones: HashMap<String, Vec<String>>,
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
    /// Enable or disable a device plugin by id. Applies immediately and only
    /// reconciles devices owned by that plugin.
    SetPluginEnabled {
        id: String,
        enabled: bool,
    },
    /// Install a plugin package (a directory containing `plugin.yaml` + its
    /// entry script) into the plugins directory. `source_dir` is a local
    /// filesystem path — the GUI's folder picker already runs on the same
    /// host as the daemon. The daemon validates the package before copying
    /// it in. Applies immediately.
    ImportPlugin {
        source_dir: String,
    },
    /// Delete a user plugin script by id. Built-in plugins cannot be deleted
    /// and the daemon rejects the request. Applies immediately.
    DeletePlugin {
        id: String,
    },
    /// Confirm the exact manifest-derived authority currently displayed in the
    /// enable modal, then enable the plugin. The daemon rejects stale snapshots
    /// so an update cannot race a user's confirmation.
    ConfirmPluginEnable {
        id: String,
        authority: PluginAuthority,
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
    /// Register a git-repo plugin source, cloning it and pinning `locked_sha` to the checked-out commit.
    AddPluginRepo {
        url: String,
        branch: Option<String>,
    },
    /// Unregister a git-repo plugin source, purging every plugin id it contributed.
    RemovePluginRepo {
        slug: String,
    },
    /// List a remote git repo's branches without cloning it, replying with a
    /// `repo_branches` frame. Used to populate the Add-repository branch picker.
    ListRepoBranches {
        url: String,
    },
    /// Check every registered repo's remote tip against `locked_sha`, replying with `plugin_repo_updates`.
    CheckPluginRepoUpdates,
    /// Fetch and check out a repo's remote tip, advancing `locked_sha`.
    UpdatePluginRepo {
        slug: String,
    },
    /// Fetch a repository and restore one skipped plugin directory from the
    /// remote tip. Unlike `UpdatePluginRepo`, sibling paths are untouched.
    RepairPluginRepoDir {
        slug: String,
        subpath: String,
    },
    /// Check repo-sourced plugins for a per-plugin content update, replying
    /// with `plugin_updates`. `slug` scopes the check to one repo; `None`
    /// checks every repo. Finer-grained than `CheckPluginRepoUpdates`.
    CheckPluginUpdates {
        slug: Option<String>,
    },
    /// Update one plugin: check out only its subtree from its repo's remote
    /// tip, leaving sibling plugins in the same repo untouched. Never automatic.
    UpdatePlugin {
        plugin_id: String,
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
    SetLogLevel {
        level: String,
    },
    SetLanguage {
        lang: String,
    },
    SetUiConfig {
        close_to_tray: bool,
        suppress_dependency_warning: bool,
        hide_window_controls: bool,
    },
    /// Allow or deny the daemon contacting GitHub for official plugins and
    /// automatic update checks. Granting triggers the deferred official-repo
    /// clone and a startup update check.
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
    SetSensorVisibility {
        sensor_id: String,
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

    // Fan speed / curves
    SetFanSpeed {
        id: String,
        duty: u8,
    },
    SetFanCurvePoints {
        fan_id: String,
        /// Points as [temp_celsius, duty_percent] pairs. Uses `f32` to match
        /// the wire type in `WireFanCurve`, avoiding a runtime cast.
        points: Vec<[f32; 2]>,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sensor_id: Option<String>,
    },
    SetFanCurvePreset {
        fan_id: String,
        preset: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        sensor_id: Option<String>,
    },
    RemoveFanCurve {
        fan_id: String,
    },

    // RGB
    RgbApply {
        id: String,
        state: RgbState,
    },
    RgbSetZoneTransform {
        id: String,
        zone_id: String,
        transform: ZoneContentTransform,
    },
    RgbChainAddLink {
        id: String,
        channel_id: String,
        name: String,
        led_count: u32,
        topology: ZoneTopology,
    },
    RgbChainRemoveLink {
        id: String,
        channel_id: String,
        child_device_id: String,
    },
    RgbChainReorderLink {
        id: String,
        channel_id: String,
        child_device_id: String,
        new_index: usize,
    },
    RgbChainDetectChannel {
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
        slot: u8,
    },
    OnboardProfileRestore {
        id: String,
        slot: u8,
    },
    OnboardProfileSetEnabled {
        id: String,
        slot: u8,
        enabled: bool,
    },

    // LCD screen
    SetScreenImage {
        id: String,
        data_b64: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    SetScreenImageFromLibrary {
        id: String,
        filename: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
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
        zone_id: String,
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
        zone_id: String,
    },
    CanvasMoveZone {
        device_id: String,
        zone_id: String,
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

    /// `SetEngineConfig` setting only an engine's tick interval.
    pub fn set_engine_tick_ms(engine: EngineKind, tick_ms: u64) -> Self {
        Self::set_engine_config(engine, None, Some(tick_ms), None, None)
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

    #[test]
    fn screen_rotation_serializes_as_string() {
        assert_eq!(
            serde_json::to_value(ScreenRotation::R270).unwrap(),
            json!("r270")
        );
    }

    #[test]
    fn screen_rotation_round_trips_as_string() {
        assert_eq!(
            serde_json::from_value::<ScreenRotation>(json!("r90")).unwrap(),
            ScreenRotation::R90
        );
        assert_eq!(
            serde_json::from_value::<ScreenRotation>(
                serde_json::to_value(ScreenRotation::R180).unwrap()
            )
            .unwrap(),
            ScreenRotation::R180
        );
    }

    #[test]
    fn set_engine_config_wire_format_uses_snake_case_engine() {
        let v = roundtrip(&DaemonCommand::SetEngineConfig {
            engine: EngineKind::FanCurve,
            enabled: Some(true),
            tick_ms: None,
            fps: None,
            failsafe_duty: None,
        });
        assert_eq!(
            v,
            json!({"type": "set_engine_config", "engine": "fan_curve", "enabled": true})
        );
    }

    #[test]
    fn mark_tour_seen_wire_format() {
        let v = roundtrip(&DaemonCommand::MarkTourSeen {
            tour: "page:home".into(),
        });
        assert_eq!(v, json!({"type": "mark_tour_seen", "tour": "page:home"}));
    }

    #[test]
    fn reset_tours_seen_wire_format() {
        let v = roundtrip(&DaemonCommand::ResetToursSeen);
        assert_eq!(v, json!({"type": "reset_tours_seen"}));
    }

    #[test]
    fn set_language_wire_format() {
        let v = roundtrip(&DaemonCommand::SetLanguage { lang: "it".into() });
        assert_eq!(v, json!({"type": "set_language", "lang": "it"}));
    }

    #[test]
    fn engine_kind_rejects_unknown_engine() {
        assert!(serde_json::from_value::<EngineKind>(json!("nonsense")).is_err());
        assert_eq!(
            serde_json::from_value::<EngineKind>(json!("canvas")).unwrap(),
            EngineKind::Canvas
        );
    }

    #[test]
    fn set_choice_wire_format() {
        let v = roundtrip(&DaemonCommand::SetChoice {
            id: "dev1".into(),
            key: "profile".into(),
            selected: 2,
        });
        assert_eq!(
            v,
            json!({"type": "set_choice", "id": "dev1", "key": "profile", "selected": 2})
        );
    }

    #[test]
    fn set_range_wire_format() {
        let v = roundtrip(&DaemonCommand::SetRange {
            id: "dev2".into(),
            key: "brightness".into(),
            value: 75,
        });
        assert_eq!(
            v,
            json!({"type": "set_range", "id": "dev2", "key": "brightness", "value": 75})
        );
    }

    #[test]
    fn set_boolean_wire_format() {
        let v = roundtrip(&DaemonCommand::SetBoolean {
            id: "dev3".into(),
            key: "enabled".into(),
            value: true,
        });
        assert_eq!(
            v,
            json!({"type": "set_boolean", "id": "dev3", "key": "enabled", "value": true})
        );
    }

    #[test]
    fn trigger_action_wire_format() {
        let v = roundtrip(&DaemonCommand::TriggerAction {
            id: "dev4".into(),
            key: "reboot".into(),
        });
        assert_eq!(
            v,
            json!({"type": "trigger_action", "id": "dev4", "key": "reboot"})
        );
    }

    #[test]
    fn rediscover_wire_format() {
        let v = roundtrip(&DaemonCommand::Rediscover);
        assert_eq!(v, json!({"type": "rediscover"}));
    }

    #[test]
    fn set_plugin_enabled_wire_format() {
        let v = roundtrip(&DaemonCommand::SetPluginEnabled {
            id: "nzxt_kraken".into(),
            enabled: false,
        });
        assert_eq!(
            v,
            json!({"type": "set_plugin_enabled", "id": "nzxt_kraken", "enabled": false})
        );
    }

    #[test]
    fn import_plugin_wire_format() {
        let v = roundtrip(&DaemonCommand::ImportPlugin {
            source_dir: "/home/user/my-driver".into(),
        });
        assert_eq!(
            v,
            json!({"type": "import_plugin", "source_dir": "/home/user/my-driver"})
        );
    }

    #[test]
    fn delete_plugin_wire_format() {
        let v = roundtrip(&DaemonCommand::DeletePlugin {
            id: "my_driver".into(),
        });
        assert_eq!(v, json!({"type": "delete_plugin", "id": "my_driver"}));
    }

    #[test]
    fn get_plugin_asset_wire_format() {
        let v = roundtrip(&DaemonCommand::GetPluginAsset {
            plugin_id: "my_driver".into(),
            name: "logo.png".into(),
        });
        assert_eq!(
            v,
            json!({"type": "get_plugin_asset", "plugin_id": "my_driver", "name": "logo.png"})
        );
    }

    #[test]
    fn add_plugin_repo_wire_format() {
        let v = roundtrip(&DaemonCommand::AddPluginRepo {
            url: "https://example.com/foo.git".into(),
            branch: Some("main".into()),
        });
        assert_eq!(
            v,
            json!({"type": "add_plugin_repo", "url": "https://example.com/foo.git", "branch": "main"})
        );
    }

    #[test]
    fn remove_plugin_repo_wire_format() {
        let v = roundtrip(&DaemonCommand::RemovePluginRepo { slug: "foo".into() });
        assert_eq!(v, json!({"type": "remove_plugin_repo", "slug": "foo"}));
    }

    #[test]
    fn list_repo_branches_wire_format() {
        let v = roundtrip(&DaemonCommand::ListRepoBranches {
            url: "https://example.com/foo.git".into(),
        });
        assert_eq!(
            v,
            json!({"type": "list_repo_branches", "url": "https://example.com/foo.git"})
        );
    }

    #[test]
    fn check_plugin_repo_updates_wire_format() {
        let v = roundtrip(&DaemonCommand::CheckPluginRepoUpdates);
        assert_eq!(v, json!({"type": "check_plugin_repo_updates"}));
    }

    #[test]
    fn update_plugin_repo_wire_format() {
        let v = roundtrip(&DaemonCommand::UpdatePluginRepo { slug: "foo".into() });
        assert_eq!(v, json!({"type": "update_plugin_repo", "slug": "foo"}));
    }

    #[test]
    fn repair_plugin_repo_dir_wire_format() {
        let v = roundtrip(&DaemonCommand::RepairPluginRepoDir {
            slug: "official".into(),
            subpath: "nzxt_kraken".into(),
        });
        assert_eq!(
            v,
            json!({"type": "repair_plugin_repo_dir", "slug": "official", "subpath": "nzxt_kraken"})
        );
    }

    #[test]
    fn check_plugin_updates_wire_format() {
        let v = roundtrip(&DaemonCommand::CheckPluginUpdates {
            slug: Some("foo".into()),
        });
        assert_eq!(v, json!({"type": "check_plugin_updates", "slug": "foo"}));
    }

    #[test]
    fn update_plugin_wire_format() {
        let v = roundtrip(&DaemonCommand::UpdatePlugin {
            plugin_id: "wled_udp".into(),
        });
        assert_eq!(v, json!({"type": "update_plugin", "plugin_id": "wled_udp"}));
    }

    #[test]
    fn update_all_plugins_wire_format() {
        let v = roundtrip(&DaemonCommand::UpdateAllPlugins);
        assert_eq!(v, json!({"type": "update_all_plugins"}));
    }

    #[test]
    fn set_plugin_config_wire_format() {
        let mut values = HashMap::new();
        values.insert("host".to_string(), "127.0.0.1".to_string());
        let v = roundtrip(&DaemonCommand::SetPluginConfig {
            id: "openrgb".into(),
            values,
        });
        assert_eq!(
            v,
            json!({"type": "set_plugin_config", "id": "openrgb", "values": {"host": "127.0.0.1"}})
        );
    }

    #[test]
    fn set_integration_enabled_wire_format() {
        let v = roundtrip(&DaemonCommand::SetIntegrationEnabled {
            id: "openrgb".into(),
            enabled: false,
        });
        assert_eq!(
            v,
            json!({"type": "set_integration_enabled", "id": "openrgb", "enabled": false})
        );
    }

    #[test]
    fn set_integration_config_wire_format() {
        let mut values = HashMap::new();
        values.insert("host".to_string(), "127.0.0.1".to_string());
        let v = roundtrip(&DaemonCommand::SetIntegrationConfig {
            id: "openrgb".into(),
            values,
        });
        assert_eq!(
            v,
            json!({"type": "set_integration_config", "id": "openrgb", "values": {"host": "127.0.0.1"}})
        );
    }

    #[test]
    fn engine_config_constructors_set_only_their_field() {
        assert_eq!(
            roundtrip(&DaemonCommand::set_engine_enabled(
                EngineKind::Canvas,
                false
            )),
            json!({"type": "set_engine_config", "engine": "canvas", "enabled": false})
        );
        assert_eq!(
            roundtrip(&DaemonCommand::set_engine_fps(EngineKind::Lcd, 30)),
            json!({"type": "set_engine_config", "engine": "lcd", "fps": 30})
        );
        assert_eq!(
            roundtrip(&DaemonCommand::set_engine_tick_ms(
                EngineKind::FanCurve,
                2000
            )),
            json!({"type": "set_engine_config", "engine": "fan_curve", "tick_ms": 2000})
        );
    }

    #[test]
    fn save_lcd_template_wire_format() {
        let v = roundtrip(&DaemonCommand::SaveLcdTemplate {
            name: "My Preset".into(),
            def: crate::lcd_custom::CustomTemplateDef::default(),
        });
        assert_eq!(v["type"], "save_lcd_template");
        assert_eq!(v["name"], "My Preset");
    }

    #[test]
    fn load_and_delete_lcd_template_wire_format() {
        assert_eq!(
            roundtrip(&DaemonCommand::LoadLcdTemplate {
                name: "My Preset".into(),
            }),
            json!({"type": "load_lcd_template", "name": "My Preset"})
        );
        assert_eq!(
            roundtrip(&DaemonCommand::DeleteLcdTemplate {
                name: "My Preset".into(),
            }),
            json!({"type": "delete_lcd_template", "name": "My Preset"})
        );
    }

    #[test]
    fn render_lcd_editor_wire_format() {
        let mut known = HashMap::new();
        known.insert("w1".to_string(), 42u64);
        let v = roundtrip(&DaemonCommand::RenderLcdEditor {
            device_id: "dev1".into(),
            def: crate::lcd_custom::CustomTemplateDef::default(),
            known: known.clone(),
        });
        assert_eq!(v["type"], "render_lcd_editor");
        assert_eq!(v["device_id"], "dev1");
        assert!(v["def"]["widgets"].is_array());
        assert_eq!(v["known"]["w1"], 42);
        let back: DaemonCommand = serde_json::from_value(v).unwrap();
        match back {
            DaemonCommand::RenderLcdEditor { known: k, .. } => assert_eq!(k, known),
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn save_and_delete_custom_effect_wire_format() {
        let mut params = HashMap::new();
        params.insert("speed".to_string(), EffectParamValue::Float(50.0));
        assert_eq!(
            roundtrip(&DaemonCommand::SaveCustomEffect {
                name: "Comet".into(),
                params: params.clone(),
            }),
            json!({"type": "save_custom_effect", "name": "Comet", "params": {"speed": 50.0}})
        );
        assert_eq!(
            roundtrip(&DaemonCommand::DeleteCustomEffect {
                name: "Comet".into(),
            }),
            json!({"type": "delete_custom_effect", "name": "Comet"})
        );
    }

    #[test]
    fn canvas_stop_wire_format() {
        assert_eq!(
            roundtrip(&DaemonCommand::CanvasStop),
            json!({"type": "canvas_stop"})
        );
        let cmd: DaemonCommand = serde_json::from_value(json!({"type": "canvas_stop"})).unwrap();
        assert!(matches!(cmd, DaemonCommand::CanvasStop));
    }

    #[test]
    fn set_choice_roundtrip_deserialise() {
        let json_str = r#"{"type":"set_choice","id":"x","key":"k","selected":1}"#;
        let cmd: DaemonCommand = serde_json::from_str(json_str).unwrap();
        assert!(matches!(cmd, DaemonCommand::SetChoice { selected: 1, .. }));
    }

    #[test]
    fn switch_profile_wire_format() {
        let v = roundtrip(&DaemonCommand::SwitchProfile {
            name: "gaming".into(),
        });
        assert_eq!(v, json!({"type": "switch_profile", "name": "gaming"}));
    }

    #[test]
    fn set_lighting_targets_wire_format() {
        let v = roundtrip(&DaemonCommand::SetLightingTargets {
            device_ids: vec!["dev1".into()],
            zones: HashMap::from([("dev1".to_string(), vec!["z0".to_string()])]),
        });
        assert_eq!(
            v,
            json!({
                "type": "set_lighting_targets",
                "device_ids": ["dev1"],
                "zones": {"dev1": ["z0"]}
            })
        );
    }

    #[test]
    fn rename_profile_wire_format() {
        let v = roundtrip(&DaemonCommand::RenameProfile {
            old_name: "old".into(),
            new_name: "new".into(),
        });
        assert_eq!(
            v,
            json!({"type": "rename_profile", "old_name": "old", "new_name": "new"})
        );
    }

    #[test]
    fn set_fan_curve_points_omits_absent_sensor_id() {
        let v = roundtrip(&DaemonCommand::SetFanCurvePoints {
            fan_id: "f".into(),
            points: vec![[30.0, 20.0], [80.0, 100.0]],
            sensor_id: None,
        });
        assert_eq!(
            v,
            json!({"type": "set_fan_curve_points", "fan_id": "f", "points": [[30.0, 20.0], [80.0, 100.0]]})
        );
    }

    #[test]
    fn rgb_apply_carries_rgb_state() {
        let v = roundtrip(&DaemonCommand::RgbApply {
            id: "d".into(),
            state: RgbState::Static {
                color: RgbColor { r: 1, g: 2, b: 3 },
            },
        });
        assert_eq!(
            v,
            json!({"type": "rgb_apply", "id": "d", "state": {"mode": "static", "color": {"r": 1, "g": 2, "b": 3}}})
        );
    }

    #[test]
    fn rgb_apply_carries_direct_effect_state() {
        let v = roundtrip(&DaemonCommand::RgbApply {
            id: "d".into(),
            state: RgbState::DirectEffect {
                id: "rainbow".into(),
                params: HashMap::new(),
            },
        });
        assert_eq!(
            v,
            json!({"type": "rgb_apply", "id": "d", "state": {"mode": "direct_effect", "id": "rainbow", "params": {}}})
        );
        let back: DaemonCommand = serde_json::from_value(v).unwrap();
        assert!(matches!(
            back,
            DaemonCommand::RgbApply {
                state: RgbState::DirectEffect { .. },
                ..
            }
        ));
    }

    #[test]
    fn canvas_effect_crud_roundtrips() {
        for cmd in [
            DaemonCommand::CanvasUpsertEffect {
                instance_id: "a".into(),
                def: crate::types::EffectDef {
                    effect_id: "screen_sampler".into(),
                    name: Some("Desk glow".into()),
                    params: HashMap::new(),
                },
            },
            DaemonCommand::CanvasRemoveEffect {
                instance_id: "a".into(),
            },
            DaemonCommand::CanvasSetDefaultEffect {
                instance_id: Some("a".into()),
            },
        ] {
            let back: DaemonCommand = serde_json::from_value(roundtrip(&cmd)).unwrap();
            assert_eq!(std::mem::discriminant(&back), std::mem::discriminant(&cmd));
        }
    }

    #[test]
    fn upsert_effect_name_roundtrips_and_defaults_to_none() {
        let v = roundtrip(&DaemonCommand::CanvasUpsertEffect {
            instance_id: "a".into(),
            def: crate::types::EffectDef {
                effect_id: "screen_sampler".into(),
                name: Some("Desk glow".into()),
                params: HashMap::new(),
            },
        });
        assert_eq!(v["def"]["name"], json!("Desk glow"));
        // Pre-name payloads (and configs) must still parse.
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
    fn place_zone_carries_effect_instance() {
        let v = roundtrip(&DaemonCommand::CanvasPlaceZone {
            device_id: "d".into(),
            zone_id: "z".into(),
            x: None,
            y: None,
            w: None,
            h: None,
            rotation: None,
            effect: Some("bars".into()),
            sampling_mode: None,
        });
        assert_eq!(v["effect"], json!("bars"));
    }

    #[test]
    fn receiver_start_pairing_wire_format() {
        let v = roundtrip(&DaemonCommand::ReceiverStartPairing {
            id: "rcv".into(),
            timeout_secs: 30,
        });
        assert_eq!(
            v,
            json!({"type": "receiver_start_pairing", "id": "rcv", "timeout_secs": 30})
        );
    }

    #[test]
    fn receiver_stop_pairing_wire_format() {
        let v = roundtrip(&DaemonCommand::ReceiverStopPairing { id: "rcv".into() });
        assert_eq!(v, json!({"type": "receiver_stop_pairing", "id": "rcv"}));
    }

    #[test]
    fn receiver_unpair_wire_format() {
        let v = roundtrip(&DaemonCommand::ReceiverUnpair {
            id: "rcv".into(),
            slot: 2,
        });
        assert_eq!(
            v,
            json!({"type": "receiver_unpair", "id": "rcv", "slot": 2})
        );
    }

    #[test]
    fn shutdown_wire_format() {
        let cmd: DaemonCommand = serde_json::from_value(json!({"type": "shutdown"})).unwrap();
        assert!(matches!(cmd, DaemonCommand::Shutdown));
    }

    #[test]
    fn ping_wire_format() {
        // The GUI heartbeat writes this exact frame; guard against drift.
        let cmd: DaemonCommand = serde_json::from_value(json!({"type": "ping"})).unwrap();
        assert!(matches!(cmd, DaemonCommand::Ping));
    }

    #[test]
    fn unit_variant_ignores_extra_fields() {
        let cmd: DaemonCommand =
            serde_json::from_value(json!({"type": "canvas_subscribe", "ignored": true})).unwrap();
        assert!(matches!(cmd, DaemonCommand::CanvasSubscribe));
    }

    // UI-shaped payload deserialisation
    // Feed the exact JSON the UI sends through the enum, guarding against drift.

    #[test]
    fn set_device_visibility_parses_ui_payload() {
        let cmd: DaemonCommand = serde_json::from_value(
            json!({"type": "set_device_visibility", "device_id": "d", "state": "hidden"}),
        )
        .expect("UI payload must deserialise");
        assert!(matches!(
            cmd,
            DaemonCommand::SetDeviceVisibility {
                state: VisibilityState::Hidden,
                ..
            }
        ));
    }

    #[test]
    fn set_sensor_visibility_parses_ui_payload() {
        let cmd: DaemonCommand = serde_json::from_value(
            json!({"type": "set_sensor_visibility", "sensor_id": "s", "state": "disabled"}),
        )
        .expect("UI payload must deserialise");
        assert!(matches!(
            cmd,
            DaemonCommand::SetSensorVisibility {
                state: VisibilityState::Disabled,
                ..
            }
        ));
    }

    #[test]
    fn set_device_name_parses_ui_payload() {
        let cmd: DaemonCommand = serde_json::from_value(
            json!({"type": "set_device_name", "device_id": "d", "name": "My Fan"}),
        )
        .expect("UI payload must deserialise");
        assert!(matches!(cmd, DaemonCommand::SetDeviceName { .. }));
    }

    #[test]
    fn remove_profile_override_parses_device_capability() {
        let cmd: DaemonCommand = serde_json::from_value(json!({
            "type": "remove_profile_override",
            "target": { "kind": "device_capability", "device_id": "d", "state_key": "fan_curve" }
        }))
        .unwrap();
        assert!(matches!(cmd, DaemonCommand::RemoveProfileOverride { .. }));
    }

    #[test]
    fn remove_profile_override_parses_canvas() {
        let cmd: DaemonCommand = serde_json::from_value(json!({
            "type": "remove_profile_override",
            "target": { "kind": "canvas" }
        }))
        .unwrap();
        assert!(matches!(
            cmd,
            DaemonCommand::RemoveProfileOverride {
                target: OverrideTarget::Canvas
            }
        ));
    }

    #[test]
    fn struct_variant_ignores_unmodelled_fields() {
        let cmd: DaemonCommand = serde_json::from_value(
            json!({"type": "canvas_place_zone", "device_id": "d", "zone_id": "z", "x": 0.1, "y": 0.2}),
        )
        .unwrap();
        assert!(matches!(cmd, DaemonCommand::CanvasPlaceZone { .. }));
    }

    #[test]
    fn set_keyboard_layout_roundtrips() {
        use crate::keyboard::{KeyVariant, KeyboardLayoutSelection};
        use crate::types::KeyboardLayout;
        let cmd = DaemonCommand::SetKeyboardLayout {
            id: "kbd".into(),
            selection: KeyboardLayoutSelection {
                variant: Some(KeyVariant::Iso),
                language: Some(KeyboardLayout::CH),
            },
        };
        let v = roundtrip(&cmd);
        assert_eq!(v["type"], "set_keyboard_layout");
        assert_eq!(v["id"], "kbd");
        assert_eq!(v["selection"]["variant"], "iso");
        assert_eq!(v["selection"]["language"], "c_h");
        let back: DaemonCommand = serde_json::from_value(v).unwrap();
        match back {
            DaemonCommand::SetKeyboardLayout { selection, .. } => {
                assert_eq!(selection.variant, Some(KeyVariant::Iso));
                assert_eq!(selection.language, Some(KeyboardLayout::CH));
            }
            _ => panic!("wrong variant"),
        }
    }

    #[test]
    fn set_keyboard_layout_auto_omits_axes() {
        use crate::keyboard::KeyboardLayoutSelection;
        let v = roundtrip(&DaemonCommand::SetKeyboardLayout {
            id: "kbd".into(),
            selection: KeyboardLayoutSelection::default(),
        });
        assert!(v["selection"].get("variant").is_none());
        assert!(v["selection"].get("language").is_none());
    }

    #[test]
    fn set_fan_speed_serializes_correct_type_discriminator() {
        let v = roundtrip(&DaemonCommand::SetFanSpeed {
            id: "pump1".into(),
            duty: 80,
        });
        assert_eq!(v["type"], "set_fan_speed");
        assert_eq!(v["duty"], 80);
    }
}
