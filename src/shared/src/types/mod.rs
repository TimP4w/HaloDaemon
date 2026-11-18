// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::{BTreeMap, BTreeSet, HashMap};

use serde::{Deserialize, Serialize};

pub use crate::commands::ScreenRotation;
use crate::zone_transform::ZoneContentTransform;

mod cooling;
mod devices;
mod gui;
mod input;
mod lcd;
mod lighting;
mod plugins;
mod profiles;

pub use cooling::*;
pub use devices::*;
pub use gui::*;
pub use input::*;
pub use lcd::*;
pub use lighting::*;
pub use plugins::*;
pub use profiles::*;

#[cfg(test)]
mod app_rule_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn app_rule_serde_round_trip() {
        let rule = AppRule {
            process_names: vec!["firefox".into(), "chrome".into()],
            profile: "Web".into(),
            enabled: true,
        };
        let json = serde_json::to_string(&rule).unwrap();
        let back: AppRule = serde_json::from_str(&json).unwrap();
        assert_eq!(back.process_names, rule.process_names);
        assert_eq!(back.profile, rule.profile);
        assert!(back.enabled);
    }

    #[test]
    fn app_rule_enabled_defaults_to_true() {
        let back: AppRule =
            serde_json::from_str(r#"{"process_names":["foo"],"profile":"P"}"#).unwrap();
        assert!(back.enabled);
    }

    #[test]
    fn gui_config_seen_tours_round_trip() {
        let mut cfg = GuiConfig::default();
        cfg.seen_tours.insert("page:home".into());
        cfg.seen_tours.insert("tab:cooling".into());
        let json = serde_json::to_string(&cfg).unwrap();
        let back: GuiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.seen_tours, cfg.seen_tours);
    }

    #[test]
    fn gui_config_seen_tours_defaults_empty_for_old_configs() {
        let cfg: GuiConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.seen_tours.is_empty());
    }

    fn widget(id: &str, kind: HomeWidgetKind) -> HomeWidget {
        HomeWidget {
            id: id.into(),
            kind,
            color: 0,
            label: String::new(),
        }
    }

    #[test]
    fn gui_config_home_widgets_round_trip() {
        let cfg = GuiConfig {
            home_widgets: vec![
                widget(
                    "a",
                    HomeWidgetKind::Chart {
                        sensor_id: "cpu".into(),
                    },
                ),
                widget(
                    "b",
                    HomeWidgetKind::Gauge {
                        sensor_id: "gpu".into(),
                        min: 0.0,
                        max: 100.0,
                    },
                ),
                widget(
                    "c",
                    HomeWidgetKind::Battery {
                        batteries: vec![battery_ref("mouse", "main")],
                    },
                ),
            ],
            ..GuiConfig::default()
        };
        let json = serde_json::to_string(&cfg).unwrap();
        let back: GuiConfig = serde_json::from_str(&json).unwrap();
        assert_eq!(back.home_widgets, cfg.home_widgets);
    }

    #[test]
    fn gui_config_home_widgets_default_empty_for_old_configs() {
        let cfg: GuiConfig = serde_json::from_str("{}").unwrap();
        assert!(cfg.home_widgets.is_empty());
    }

    #[test]
    fn home_widget_kind_wire_format() {
        assert_eq!(
            serde_json::to_value(HomeWidgetKind::Gauge {
                sensor_id: "cpu".into(),
                min: 20.0,
                max: 90.0,
            })
            .unwrap(),
            json!({"type": "gauge", "sensor_id": "cpu", "min": 20.0, "max": 90.0})
        );
    }

    #[test]
    fn battery_ref_round_trips_device_ids_containing_the_separator() {
        let reference = battery_ref("hid/1234:5678", "left");
        assert_eq!(
            split_battery_ref(&reference),
            Some(("hid/1234:5678", "left"))
        );
    }

    #[test]
    fn validate_home_widgets_rejects_malformed_rows() {
        let chart = |id: &str| {
            widget(
                id,
                HomeWidgetKind::Chart {
                    sensor_id: "cpu".into(),
                },
            )
        };
        let cases = [
            (
                (0..=MAX_HOME_WIDGETS)
                    .map(|i| chart(&i.to_string()))
                    .collect::<Vec<_>>(),
                HomeWidgetsError::TooMany,
            ),
            (vec![chart("a"), chart("a")], HomeWidgetsError::DuplicateId),
            (vec![chart(" ")], HomeWidgetsError::EmptyId),
            (
                vec![widget(
                    "a",
                    HomeWidgetKind::Gauge {
                        sensor_id: "cpu".into(),
                        min: 50.0,
                        max: 50.0,
                    },
                )],
                HomeWidgetsError::EmptyRange,
            ),
            (
                vec![widget(
                    "a",
                    HomeWidgetKind::Battery {
                        batteries: (0..=MAX_BATTERY_WIDGET_ENTRIES)
                            .map(|i| battery_ref("d", &i.to_string()))
                            .collect(),
                    },
                )],
                HomeWidgetsError::TooManyBatteries,
            ),
        ];
        for (widgets, expected) in cases {
            assert_eq!(validate_home_widgets(&widgets), Err(expected));
        }
    }

    #[test]
    fn validate_home_widgets_accepts_a_full_row() {
        let widgets: Vec<_> = (0..MAX_HOME_WIDGETS)
            .map(|i| {
                widget(
                    &i.to_string(),
                    HomeWidgetKind::Chart {
                        sensor_id: "cpu".into(),
                    },
                )
            })
            .collect();
        assert_eq!(validate_home_widgets(&widgets), Ok(()));
    }

    #[test]
    fn plugin_config_field_new_metadata_has_backward_compatible_defaults() {
        let field: PluginConfigField =
            serde_json::from_str(r#"{"key":"host","label":"Host","kind":"host"}"#).unwrap();
        assert!(field.options.is_empty());
        assert!(field.visible_when.is_none());
        assert!(field.help.is_none());
        assert!(field.placeholder.is_none());
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn narrow_language_matches_supported_regardless_of_region_or_charset() {
        assert_eq!(narrow_language("it-IT"), "it");
        assert_eq!(narrow_language("en_US.UTF-8"), "en");
        assert_eq!(narrow_language("IT"), "it");
    }

    #[test]
    fn narrow_language_falls_back_to_default_for_unsupported_or_empty() {
        assert_eq!(narrow_language("fr-FR"), DEFAULT_LANGUAGE);
        assert_eq!(narrow_language(""), DEFAULT_LANGUAGE);
        assert!(SUPPORTED_LANGUAGES.contains(&detect_system_language()));
    }

    #[test]
    fn notification_detail_and_severity_for_detail_codes() {
        let connect = NotificationCode::PluginConnectFailed {
            plugin: "wled".parse().unwrap(),
            detail: "127.0.0.1 blocked".into(),
        };
        let runtime = NotificationCode::PluginRuntimeError {
            plugin: "wled".parse().unwrap(),
            detail: "lua boom".into(),
        };
        let device = NotificationCode::DeviceWriteFailed {
            device_id: "kb-1".parse().unwrap(),
            device: "keyboard".into(),
            detail: "HID++ rejected write".into(),
        };
        assert_eq!(connect.detail(), Some("127.0.0.1 blocked"));
        assert_eq!(runtime.detail(), Some("lua boom"));
        assert_eq!(device.detail(), Some("HID++ rejected write"));
        assert_eq!(device.severity(), NotificationSeverity::Error);
        assert_eq!(connect.severity(), NotificationSeverity::Warning);
        assert_eq!(
            NotificationCode::FanStalled { fan: "cpu".into() }.detail(),
            None
        );
    }

    #[test]
    fn plugin_issue_round_trips_and_info_defaults_to_healthy() {
        let issue = PluginIssue {
            kind: PluginIssueKind::ConnectFailed,
            detail: "boom".into(),
            context: None,
            timestamp_ms: 42,
        };
        let back: PluginIssue =
            serde_json::from_str(&serde_json::to_string(&issue).unwrap()).unwrap();
        assert_eq!(back, issue);
        assert_eq!(
            serde_json::to_value(&issue).unwrap()["kind"],
            "connect_failed"
        );
        let info: PluginInfo = serde_json::from_value(serde_json::json!({
            "id": "p", "name": "Weather", "description": "Weather provider",
            "path": "", "enabled": true,
            "translations": {"it": {
                "plugin.name": "Meteo",
                "plugin.description": "Servizio meteo"
            }}
        }))
        .unwrap();
        assert_eq!(info.health, HealthState::default());
        assert!(!info.experimental);
        assert_eq!(info.localized_name("it-CH"), "Meteo");
        assert_eq!(info.localized_description("it"), "Servizio meteo");
        assert_eq!(info.localized_name("de"), "Weather");

        let init_failed = PluginIssue {
            kind: PluginIssueKind::InitFailed,
            detail: "HID++ timeout".into(),
            context: None,
            timestamp_ms: 43,
        };
        assert_eq!(
            serde_json::to_value(&init_failed).unwrap()["kind"],
            "init_failed"
        );
    }

    #[test]
    fn plugin_info_defaults_active_requirements_and_blocker() {
        // Older daemon payloads must stay inert, never accidentally active.
        let info: PluginInfo = serde_json::from_value(serde_json::json!({
            "id": "p", "name": "P", "path": "", "enabled": true
        }))
        .unwrap();
        assert!(!info.active);
        assert!(info.requirements.is_empty());
        assert!(info.activation_blocker.is_none());
    }

    #[test]
    fn requirement_status_and_blocker_round_trip_snake_case() {
        let status = PluginRequirementStatus {
            requirement: PluginRequirement::KernelModule {
                name: "nct6775".into(),
            },
            impact: RequirementImpact::Block,
            satisfied: false,
            reason: Some(RequirementFailureReason::NotLoaded),
            feature: None,
        };
        let value = serde_json::to_value(&status).unwrap();
        assert_eq!(value["requirement"]["kind"], "kernel_module");
        assert_eq!(value["impact"], "block");
        assert_eq!(value["reason"], "not_loaded");

        let blocker = PluginActivationBlocker::MissingRequirements {
            requirements: vec![status.clone()],
        };
        let back: PluginActivationBlocker =
            serde_json::from_str(&serde_json::to_string(&blocker).unwrap()).unwrap();
        assert_eq!(back, blocker);
        assert_eq!(
            serde_json::to_value(&blocker).unwrap()["type"],
            "missing_requirements"
        );

        let degrade = PluginRequirementStatus {
            requirement: PluginRequirement::Command {
                executable: "pactl".into(),
            },
            impact: RequirementImpact::Degrade,
            satisfied: false,
            reason: Some(RequirementFailureReason::NotFound),
            feature: Some("audio_routing".into()),
        };
        let back: PluginRequirementStatus =
            serde_json::from_str(&serde_json::to_string(&degrade).unwrap()).unwrap();
        assert_eq!(back, degrade);

        assert_eq!(
            serde_json::to_value(PluginRequirement::PawnIo).unwrap()["kind"],
            "pawn_io"
        );
        assert_eq!(
            serde_json::to_value(PluginRequirement::LinuxI2c).unwrap()["kind"],
            "linux_i2c"
        );
    }

    #[test]
    fn plugin_recommendation_round_trips_and_defaults_accessible() {
        let rec = PluginRecommendation {
            plugin_id: "nzxt_kraken".parse().unwrap(),
            plugin_name: "NZXT Kraken".into(),
            hardware: PluginRecommendationMatch::Hid {
                vid: 0x1e71,
                pid: 0x2007,
            },
            install_from: Some("official".into()),
            accessible: false,
        };
        let back: PluginRecommendation =
            serde_json::from_str(&serde_json::to_string(&rec).unwrap()).unwrap();
        assert_eq!(back, rec);
        let old: PluginRecommendation = serde_json::from_value(serde_json::json!({
            "plugin_id": "p", "plugin_name": "P",
            "hardware": { "transport": "usb", "vid": 1, "pid": 2 }
        }))
        .unwrap();
        assert!(old.accessible);

        for hardware in [
            PluginRecommendationMatch::Always,
            PluginRecommendationMatch::Command {
                executable: "nvidia-smi".into(),
            },
        ] {
            let value = serde_json::to_value(&hardware).unwrap();
            let back: PluginRecommendationMatch = serde_json::from_value(value).unwrap();
            assert_eq!(back, hardware);
        }
    }

    #[test]
    fn load_failed_and_skipped_round_trip() {
        let failed = PluginIssue {
            kind: PluginIssueKind::LoadFailed,
            detail: "needs network".into(),
            context: None,
            timestamp_ms: 1,
        };
        assert_eq!(
            serde_json::to_value(&failed).unwrap()["kind"],
            "load_failed"
        );
        let skipped = SkippedPlugin {
            path: "/a/b".into(),
            reason: "bad yaml".into(),
        };
        let back: SkippedPlugin =
            serde_json::from_str(&serde_json::to_string(&skipped).unwrap()).unwrap();
        assert_eq!(back, skipped);
    }

    #[test]
    fn effect_param_value_steps_round_trips_untagged() {
        let v = EffectParamValue::Steps(vec![
            ColorStep {
                value: 40.0,
                color: RgbColor { r: 0, g: 255, b: 0 },
            },
            ColorStep {
                value: 80.0,
                color: RgbColor { r: 255, g: 0, b: 0 },
            },
        ]);
        let json = serde_json::to_string(&v).unwrap();
        let back: EffectParamValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, v);

        // The array encoding must not shadow the object-encoded Color variant.
        let c = EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 });
        let json = serde_json::to_string(&c).unwrap();
        let back: EffectParamValue = serde_json::from_str(&json).unwrap();
        assert_eq!(back, c);
    }

    #[test]
    fn connection_capability_roundtrips() {
        let cap = DeviceCapability::Connection(ConnectionStatus {
            connection_type: ConnectionType::Wireless,
        });
        let json = serde_json::to_string(&cap).unwrap();
        assert!(json.contains("\"kind\":\"connection\""));
        let back: DeviceCapability = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            back,
            DeviceCapability::Connection(ConnectionStatus {
                connection_type: ConnectionType::Wireless
            })
        ));
    }

    #[test]
    fn notification_roundtrip() {
        let n = Notification {
            code: NotificationCode::DeviceInitFailed {
                device_id: "kraken-1".parse().unwrap(),
                device: "Kraken".into(),
                detail: "thing exploded".into(),
            },
            show_native: true,
            timestamp_ms: 1234,
        };
        let json = serde_json::to_value(&n).unwrap();
        assert_eq!(json["code"], "device_init_failed");
        assert_eq!(json["device"], "Kraken");
        assert_eq!(json["detail"], "thing exploded");
        assert_eq!(json["timestamp_ms"], 1234);
        assert_eq!(json["showNative"], true);
        let back: Notification = serde_json::from_value(json).unwrap();
        assert_eq!(back.code, n.code);
        assert!(back.show_native);
        assert_eq!(back.code.severity(), NotificationSeverity::Error);
        assert_eq!(back.timestamp_ms, 1234);
    }

    #[test]
    fn capability_accessors_find_only_their_own_kind() {
        let caps = vec![
            DeviceCapability::Battery(vec![]),
            DeviceCapability::Lighting(LightingStatus {
                descriptor: LightingDescriptor {
                    channels: vec![],
                    native_effects: vec![],
                },
                state: None,
                channel_transforms: HashMap::new(),
            }),
            DeviceCapability::Sensors(vec![]),
            DeviceCapability::Children(vec![]),
            DeviceCapability::Pairing(PairingStatus::default()),
        ];
        let full = WireDevice {
            capabilities: caps,
            ..Default::default()
        };
        assert!(full.battery().is_some());
        assert!(full.lighting().is_some());
        assert!(full.sensors().is_some());
        assert!(full.children().is_some());
        assert!(full.pairing().is_some());
        let other = WireDevice {
            capabilities: vec![DeviceCapability::Cooling(CoolingStatus {
                channels: vec![],
            })],
            ..Default::default()
        };
        assert!(other.battery().is_none());
        assert!(other.lighting().is_none());
        assert!(other.pairing().is_none());
    }

    #[test]
    fn button_actions_round_trip_and_default_to_native() {
        assert_eq!(
            serde_json::to_string(&ButtonAction::default()).unwrap(),
            r#"{"type":"native"}"#
        );
        for action in [
            ButtonAction::Native,
            ButtonAction::KeyChord {
                key: 42,
                modifiers: vec![ModKey::Ctrl, ModKey::Shift],
            },
            ButtonAction::Macro {
                steps: vec![MacroStep {
                    kind: MacroAtom::KeyDown { key: 7 },
                    delay_after_ms: 50,
                }],
            },
            ButtonAction::DpiCycle {
                direction: CycleDir::Up,
            },
        ] {
            let json = serde_json::to_string(&action).unwrap();
            assert_eq!(serde_json::from_str::<ButtonAction>(&json).unwrap(), action);
        }
    }

    #[test]
    fn eq_preset_defaults_when_flags_absent() {
        let p: EqPreset =
            serde_json::from_value(serde_json::json!({"id": "custom", "label": "Custom"})).unwrap();
        assert!(!p.is_custom);
        assert!(!p.is_firmware);
        assert_eq!(p.bands, None);
    }

    proptest::proptest! {
        #[test]
        fn eq_preset_json_round_trips(
            id in ".{0,32}",
            label in ".{0,32}",
            is_custom: bool,
            is_firmware: bool,
            bands in proptest::option::of(
                proptest::collection::vec(-100.0f32..100.0, 0..12)
            ),
        ) {
            let p = EqPreset { id, label, is_custom, is_firmware, bands };
            let json = serde_json::to_string(&p).unwrap();
            let back: EqPreset = serde_json::from_str(&json).unwrap();
            proptest::prop_assert_eq!(back, p);
        }
    }
}

#[cfg(test)]
mod default_tests {
    use super::*;
    use serde_json::json;

    #[test]
    fn placed_zone_defaults_when_size_and_rotation_omitted() {
        let z: PlacedZone =
            serde_json::from_str(r#"{"device_id":"d","channel_id":"z","x":1.0,"y":2.0}"#).unwrap();
        assert_eq!(z.w, 0.15, "default_zone_size");
        assert_eq!(z.h, 0.15, "default_zone_size");
        assert_eq!(z.rotation, 0.0, "default_rotation");
    }

    #[test]
    fn canvas_state_defaults_sample_radius_when_omitted() {
        let s: CanvasState =
            serde_json::from_str(r#"{"available_effects":[],"placed_zones":[]}"#).unwrap();
        assert_eq!(s.sample_radius, 3.0, "default_sample_radius");
    }

    #[test]
    fn gui_config_defaults_close_to_tray_when_omitted() {
        let c: GuiConfig = serde_json::from_str(r#"{"language":"en","log_level":"info"}"#).unwrap();
        assert!(c.close_to_tray, "default_close_to_tray");
        assert!(c.low_battery_notifications);
        assert!(c.check_for_updates);
        assert!(c.update_notified.is_empty());
        assert!(
            !c.suppress_dependency_warning,
            "default_suppress_dependency_warning"
        );
    }

    #[test]
    fn validate_image_filename_accepts_uuid_image_names() {
        assert!(validate_image_filename("0e7d4c2a-1234-5678-9abc-def012345678.png").is_ok());
        assert!(validate_image_filename("pic_1.jpg").is_ok());
        assert!(validate_image_filename("a.gif").is_ok());
    }

    #[test]
    fn validate_image_filename_rejects_traversal_and_paths() {
        for bad in [
            "../secret.png",
            "a/b.png",
            "a\\b.png",
            "C:\\x.png",
            ".hidden.png",
            "noext",
            "x.txt",
            "name.png:stream",
            "",
        ] {
            assert!(
                validate_image_filename(bad).is_err(),
                "should reject {bad:?}"
            );
        }
    }

    #[test]
    fn validate_logo_dimensions_accepts_square_and_mild_rectangles() {
        assert!(validate_logo_dimensions(64, 64).is_ok());
        assert!(validate_logo_dimensions(MAX_PLUGIN_LOGO_DIM, MAX_PLUGIN_LOGO_DIM).is_ok());
        assert!(validate_logo_dimensions(200, 100).is_ok());
        assert!(validate_logo_dimensions(100, 200).is_ok());
        assert!(validate_logo_dimensions(201, 100).is_err());
    }

    #[test]
    fn validate_logo_dimensions_rejects_zero_oversize_and_banners() {
        assert!(validate_logo_dimensions(0, 64).is_err());
        assert!(validate_logo_dimensions(64, 0).is_err());
        assert!(validate_logo_dimensions(MAX_PLUGIN_LOGO_DIM + 1, 64).is_err());
        assert!(validate_logo_dimensions(64, MAX_PLUGIN_LOGO_DIM + 1).is_err());
        assert!(validate_logo_dimensions(500, 50).is_err());
    }

    #[test]
    fn validate_image_upload_size_gates_at_the_ceiling() {
        assert!(validate_image_upload_size(0).is_ok());
        assert!(validate_image_upload_size(MAX_LCD_IMAGE_BYTES).is_ok());
        assert!(validate_image_upload_size(MAX_LCD_IMAGE_BYTES + 1).is_err());
        assert!(validate_image_upload_size(u64::MAX).is_err());
    }

    #[test]
    fn canvas_state_sanitize_cleans_nonfinite_and_caps_zones() {
        let mut cs = CanvasState {
            placed_zones: (0..MAX_PLACED_ZONES + 10)
                .map(|_| PlacedZone::default())
                .collect(),
            ..CanvasState::default()
        };
        cs.placed_zones[0].x = f32::NAN;
        cs.placed_zones[0].rotation = f32::INFINITY;
        cs.placed_zones[0].w = -1.0;
        cs.placed_zones[0].h = 100.0;
        cs.sample_radius = -3.0;
        cs.sanitize();
        assert_eq!(cs.placed_zones.len(), MAX_PLACED_ZONES);
        assert!(cs.placed_zones[0].x.is_finite());
        assert!(cs.placed_zones[0].rotation.is_finite());
        assert!(cs.placed_zones[0].w > 0.0);
        assert_eq!(cs.placed_zones[0].h, 4.0);
        assert_eq!(cs.sample_radius, 0.5);
    }

    #[test]
    fn max_lcd_image_base64_fits_in_one_ipc_frame() {
        // An accepted image's base64 (≈4/3) plus envelope must fit the frame cap,
        // or the upload the size check "passes" would be rejected at the wire.
        let base64_len = MAX_LCD_IMAGE_BYTES.div_ceil(3) * 4;
        assert!(base64_len < crate::frames::MAX_PAYLOAD as u64);
    }

    #[test]
    fn sniff_ext_detects_formats() {
        assert_eq!(sniff_ext(b"GIF89aXXX"), "gif");
        assert_eq!(sniff_ext(b"GIF87aXXX"), "gif");
        assert_eq!(sniff_ext(&[0xFF, 0xD8, 0x00]), "jpg");
        assert_eq!(sniff_ext(&[0x89, 0x50, 0x4E, 0x47]), "png");
        assert_eq!(sniff_ext(b"anything"), "unknown");
    }

    #[test]
    fn battery_status_wire_form_is_snake_case() {
        // Lowercase wire form crosses the IPC boundary.
        assert_eq!(
            serde_json::to_value(BatteryStatus::Charging).unwrap(),
            json!("charging")
        );
        assert_eq!(
            serde_json::to_value(BatteryStatus::Discharging).unwrap(),
            json!("discharging")
        );
        assert_eq!(
            serde_json::to_value(BatteryStatus::Unknown).unwrap(),
            json!("unknown")
        );
        let s: BatteryStatus = serde_json::from_value(json!("charging")).unwrap();
        assert_eq!(s, BatteryStatus::Charging);
    }

    #[test]
    fn write_rate_status_defaults_to_no_limit() {
        assert_eq!(WriteRateStatus::default().limit, None);
    }

    #[test]
    fn wire_device_optional_fields_default_when_omitted() {
        let d: WireDevice = serde_json::from_str(
            r#"{"id":"d","name":"d","vendor":"v","model":"m","device_type":"other","connected":true,"capabilities":[]}"#,
        )
        .unwrap();
        assert_eq!(d.write_rate, None);
        assert!(d.control_layout.is_empty());
    }

    #[test]
    fn choice_display_defaults_to_inline() {
        // Must match the inline pills the Controls tab hardcoded before reading this field.
        assert_eq!(ChoiceDisplay::default(), ChoiceDisplay::Inline);
        let c: Choice =
            serde_json::from_str(r#"{"key":"k","label":"K","options":[],"selected":0}"#).unwrap();
        assert_eq!(c.display, ChoiceDisplay::Inline);
    }

    #[test]
    fn category_layout_defaults_span_to_one_when_omitted() {
        let l: CategoryLayout = serde_json::from_str(r#"{"category":"Audio"}"#).unwrap();
        assert_eq!(l.order, 0);
        assert_eq!(l.column, 0);
        assert_eq!(l.span, 1);
    }

    #[test]
    fn visible_when_absent_when_none() {
        let c = Choice {
            key: "k".into(),
            label: "K".into(),
            options: vec![],
            selected: 0,
            category: String::new(),
            display: ChoiceDisplay::default(),
            visible_when: None,
        };
        let json = serde_json::to_string(&c).unwrap();
        assert!(!json.contains("visible_when"));
    }

    #[test]
    fn visible_when_round_trips_when_present() {
        let r = Range {
            key: "nc_level".into(),
            label: "Transparency Level".into(),
            min: 1,
            max: 10,
            step: 1,
            value: 5,
            read_only: false,
            category: "Noise Cancelling".into(),
            start_label: None,
            end_label: None,
            display: RangeDisplay::default(),
            visible_when: Some(VisibleWhen {
                key: "nc_mode".into(),
                equals: vec![1],
            }),
        };
        let json = serde_json::to_string(&r).unwrap();
        let back: Range = serde_json::from_str(&json).unwrap();
        assert_eq!(back.visible_when, r.visible_when);
    }
}

#[cfg(test)]
mod rgb_state_tests {
    use super::*;

    #[test]
    fn per_led_uses_string_keys_for_led_ids() {
        let state = LightingState::PerLed {
            channels: {
                let mut channels = std::collections::HashMap::new();
                let mut led_map = std::collections::HashMap::new();
                led_map.insert("12".to_string(), RgbColor { r: 255, g: 0, b: 0 });
                channels.insert("z0".to_string(), led_map);
                channels
            },
        };
        let json = serde_json::to_value(&state).unwrap();
        assert_eq!(json["channels"]["z0"]["12"]["r"], 255);
        let back: LightingState = serde_json::from_value(json).unwrap();
        assert!(matches!(back, LightingState::PerLed { .. }));
    }

    #[test]
    fn lcd_preview_lease_is_at_least_double_the_keepalive() {
        // One lost/late keepalive must never flap the preview stream.
        assert!(LCD_PREVIEW_LEASE_SECS as f64 >= 2.0 * LCD_PREVIEW_KEEPALIVE_SECS);
    }
}

#[cfg(test)]
mod color_order_tests {
    use super::*;

    #[test]
    fn color_orders_cover_every_rgb_permutation() {
        let color = RgbColor { r: 1, g: 2, b: 3 };
        assert_eq!(ColorOrder::Rgb.encode(color), [1, 2, 3]);
        assert_eq!(ColorOrder::Rbg.encode(color), [1, 3, 2]);
        assert_eq!(ColorOrder::Grb.encode(color), [2, 1, 3]);
        assert_eq!(ColorOrder::Gbr.encode(color), [2, 3, 1]);
        assert_eq!(ColorOrder::Brg.encode(color), [3, 1, 2]);
        assert_eq!(ColorOrder::Bgr.encode(color), [3, 2, 1]);
    }
}
