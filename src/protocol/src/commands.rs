// SPDX-License-Identifier: GPL-3.0-or-later
//
// Typed representation of every IPC command the daemon understands.
//
// The wire format uses `"type"` as the discriminator tag in snake_case, which
// is what the UI has always produced via hand-written `json!({…})` blocks.
// Variants whose names don't naturally snake_case to the right string carry an
// explicit `#[serde(rename = "…")]`.
//
// Free-form / deeply-nested payloads (RGB state, button mapping, zone transform,
// effect params, chain topology) are kept as opaque `serde_json::Value` fields;
// the use-case re-parses them from the raw message.

use serde::{Deserialize, Serialize};
use serde_json::Value;

/// Typed IPC commands shared between the daemon and the UI.
///
/// Serialised with `serde(tag = "type", rename_all = "snake_case")` so that
/// the discriminator field on the wire is always `"type"` in snake_case,
/// exactly matching what the UI's `Command::to_json()` produces.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(tag = "type", rename_all = "snake_case")]
pub enum DaemonCommand {
    // ── Device capability settings ─────────────────────────────────────────
    SetChoice {
        id:       String,
        key:      String,
        selected: usize,
    },
    SetRange {
        id:    String,
        key:   String,
        value: i32,
    },
    SetBoolean {
        id:    String,
        key:   String,
        value: bool,
    },
    TriggerAction {
        id:  String,
        key: String,
    },

    // ── Profile management ─────────────────────────────────────────────────
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

    // ── App rules ──────────────────────────────────────────────────────────
    AddAppRule {
        process_names: Vec<String>,
        profile:       String,
        enabled:       bool,
    },
    UpdateAppRule {
        index:         usize,
        process_names: Vec<String>,
        profile:       String,
        enabled:       bool,
    },
    RemoveAppRule {
        index: usize,
    },

    // ── Misc / global ──────────────────────────────────────────────────────
    Rediscover,
    SetLogLevel {
        level: String,
    },
    SetUiConfig {
        close_to_tray: bool,
    },
    SetFanFailsafeDuty {
        fan_id: String,
        duty:   u8,
    },
    ResetAllButtonMappings {
        id: String,
    },
    ResetButtonMapping {
        id:  String,
        cid: u16,
    },
    SetEqPreset {
        id:           String,
        preset_index: usize,
    },
    SetEqBands {
        id:     String,
        values: Vec<f32>,
    },
    SetDpiSteps {
        id:    String,
        steps: Vec<u32>,
    },
    SetDeviceVisibility {
        id:      String,
        visible: bool,
    },
    SetSensorVisibility {
        id:      String,
        visible: bool,
    },
    SetDeviceName {
        id:   String,
        name: String,
    },

    // ── Fan speed / curves ─────────────────────────────────────────────────
    #[serde(alias = "set_pump_duty")]
    SetFanSpeed {
        id:   String,
        duty: u8,
    },
    SetFanCurvePoints {
        fan_id: String,
        points: Vec<[f64; 2]>,
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

    // ── RGB ────────────────────────────────────────────────────────────────
    RgbApply {
        id:    String,
        state: Value,
    },
    RgbSetZoneTransform {
        id:        String,
        zone_id:   String,
        transform: Value,
    },
    RgbChainAddLink {
        id:         String,
        channel_id: String,
        name:       String,
        led_count:  u32,
        topology:   Value,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        kind: Option<String>,
    },
    RgbChainRemoveLink {
        id:              String,
        channel_id:      String,
        child_device_id: String,
    },
    RgbChainReorderLink {
        id:              String,
        channel_id:      String,
        child_device_id: String,
        new_index:       usize,
    },
    RgbChainDetectChannel {
        id:         String,
        channel_id: String,
    },

    // ── Key remap ──────────────────────────────────────────────────────────
    SetButtonMapping {
        id:      String,
        mapping: Value,
    },
    SetSoftwareDpiSteps {
        id:    String,
        steps: Vec<u32>,
    },

    // ── Onboard profiles ───────────────────────────────────────────────────
    OnboardProfileSwitch {
        id:   String,
        slot: u8,
    },
    OnboardProfileRestore {
        id:   String,
        slot: u8,
    },
    OnboardProfileSetEnabled {
        id:      String,
        slot:    u8,
        enabled: bool,
    },

    // ── LCD screen ─────────────────────────────────────────────────────────
    SetScreenImage {
        id:       String,
        data_b64: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    SetScreenImageFromLibrary {
        id:       String,
        filename: String,
        #[serde(default, skip_serializing_if = "Option::is_none")]
        request_id: Option<String>,
    },
    SetScreenRotation {
        id:      String,
        degrees: u32,
    },
    SetScreenBrightness {
        id:         String,
        brightness: u8,
    },
    SetScreenDefault {
        id: String,
    },
    ListLcdImages,
    DeleteLcdImage {
        filename: String,
    },

    // ── Canvas ─────────────────────────────────────────────────────────────
    CanvasSetEffect {
        effect_id: String,
        params:    Value,
    },
    CanvasPlaceZone {
        device_id: String,
        zone_id:   String,
    },
    CanvasRemoveZone {
        device_id: String,
        zone_id:   String,
    },
    CanvasMoveZone {
        device_id: String,
        zone_id:   String,
        x:         f64,
        y:         f64,
    },
    CanvasSetSampleRadius {
        radius: f64,
    },
    CanvasSubscribe,

    // ── LCD engine ─────────────────────────────────────────────────────────
    LcdEngineSetTemplate {
        device_id:   String,
        template_id: String,
        params:      Value,
    },
    LcdEngineDeactivate {
        device_id: String,
    },
    LcdEngineSubscribe,

    // ── Misc ───────────────────────────────────────────────────────────────
    ListRunningApps,
    GetDebugInfo,
    SetEngineConfig {
        engine: String,
    },
    Shutdown,
}

#[cfg(test)]
mod tests {
    use super::*;
    use serde_json::json;

    fn roundtrip(cmd: &DaemonCommand) -> serde_json::Value {
        serde_json::to_value(cmd).unwrap()
    }

    #[test]
    fn set_choice_wire_format() {
        let v = roundtrip(&DaemonCommand::SetChoice {
            id:       "dev1".into(),
            key:      "profile".into(),
            selected: 2,
        });
        assert_eq!(v, json!({"type": "set_choice", "id": "dev1", "key": "profile", "selected": 2}));
    }

    #[test]
    fn set_range_wire_format() {
        let v = roundtrip(&DaemonCommand::SetRange {
            id:    "dev2".into(),
            key:   "brightness".into(),
            value: 75,
        });
        assert_eq!(v, json!({"type": "set_range", "id": "dev2", "key": "brightness", "value": 75}));
    }

    #[test]
    fn set_boolean_wire_format() {
        let v = roundtrip(&DaemonCommand::SetBoolean {
            id:    "dev3".into(),
            key:   "enabled".into(),
            value: true,
        });
        assert_eq!(v, json!({"type": "set_boolean", "id": "dev3", "key": "enabled", "value": true}));
    }

    #[test]
    fn trigger_action_wire_format() {
        let v = roundtrip(&DaemonCommand::TriggerAction {
            id:  "dev4".into(),
            key: "reboot".into(),
        });
        assert_eq!(v, json!({"type": "trigger_action", "id": "dev4", "key": "reboot"}));
    }

    #[test]
    fn rediscover_wire_format() {
        let v = roundtrip(&DaemonCommand::Rediscover);
        assert_eq!(v, json!({"type": "rediscover"}));
    }

    #[test]
    fn set_choice_roundtrip_deserialise() {
        let json_str = r#"{"type":"set_choice","id":"x","key":"k","selected":1}"#;
        let cmd: DaemonCommand = serde_json::from_str(json_str).unwrap();
        assert!(matches!(cmd, DaemonCommand::SetChoice { selected: 1, .. }));
    }

    #[test]
    fn switch_profile_wire_format() {
        let v = roundtrip(&DaemonCommand::SwitchProfile { name: "gaming".into() });
        assert_eq!(v, json!({"type": "switch_profile", "name": "gaming"}));
    }

    #[test]
    fn rename_profile_wire_format() {
        let v = roundtrip(&DaemonCommand::RenameProfile {
            old_name: "old".into(),
            new_name: "new".into(),
        });
        assert_eq!(v, json!({"type": "rename_profile", "old_name": "old", "new_name": "new"}));
    }

    #[test]
    fn set_pump_duty_aliases_set_fan_speed() {
        let cmd: DaemonCommand =
            serde_json::from_value(json!({"type": "set_pump_duty", "id": "p", "duty": 60})).unwrap();
        assert!(matches!(cmd, DaemonCommand::SetFanSpeed { duty: 60, .. }));
    }

    #[test]
    fn set_fan_curve_points_omits_absent_sensor_id() {
        let v = roundtrip(&DaemonCommand::SetFanCurvePoints {
            fan_id: "f".into(),
            points: vec![[30.0, 20.0], [80.0, 100.0]],
            sensor_id: None,
        });
        assert_eq!(v, json!({"type": "set_fan_curve_points", "fan_id": "f", "points": [[30.0, 20.0], [80.0, 100.0]]}));
    }

    #[test]
    fn rgb_apply_carries_opaque_state() {
        let v = roundtrip(&DaemonCommand::RgbApply {
            id: "d".into(),
            state: json!({"mode": "static", "color": [1, 2, 3]}),
        });
        assert_eq!(v, json!({"type": "rgb_apply", "id": "d", "state": {"mode": "static", "color": [1, 2, 3]}}));
    }

    #[test]
    fn shutdown_wire_format() {
        let cmd: DaemonCommand =
            serde_json::from_value(json!({"type": "shutdown"})).unwrap();
        assert!(matches!(cmd, DaemonCommand::Shutdown));
    }

    #[test]
    fn unit_variant_ignores_extra_fields() {
        let cmd: DaemonCommand =
            serde_json::from_value(json!({"type": "canvas_subscribe", "ignored": true})).unwrap();
        assert!(matches!(cmd, DaemonCommand::CanvasSubscribe));
    }

    #[test]
    fn struct_variant_ignores_unmodelled_fields() {
        // place_zone's optional x/y/w/h are re-parsed from the raw message, so the
        // variant must still deserialise when they are present.
        let cmd: DaemonCommand = serde_json::from_value(
            json!({"type": "canvas_place_zone", "device_id": "d", "zone_id": "z", "x": 0.1, "y": 0.2}),
        )
        .unwrap();
        assert!(matches!(cmd, DaemonCommand::CanvasPlaceZone { .. }));
    }
}
