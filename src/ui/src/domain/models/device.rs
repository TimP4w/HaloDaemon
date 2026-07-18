// SPDX-License-Identifier: GPL-3.0-or-later
//! Maps the daemon's `WireDevice`/`AppState` onto the fields the Prism design
//! expects (short code, accent hue, two headline metrics, status). All values
//! are derived from real device capabilities — there is no mock data. Colors
//! themselves are a presentation concern; this module only classifies (see
//! [`BatteryLevel`], [`hue_index`]) and `ui::theme` maps the classification
//! to a `Color32`.

use halod_shared::types::{
    AppState, BatteryStatus, ConflictConfidence, ConnectionType, DeviceCapability, DeviceType,
    SensorType, SensorUnit, VisibilityState, WireDevice, WriteRateStatus,
};

#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ConflictPresentation {
    pub peer_names: Vec<String>,
    pub recommended_name: String,
    pub confidence: ConflictConfidence,
    pub is_recommended: bool,
    pub can_disable: bool,
}

pub fn conflict_presentation(
    d: &WireDevice,
    devices: &[WireDevice],
) -> Option<ConflictPresentation> {
    let conflict = d.conflict.as_ref()?;
    let peer_names = conflict
        .peer_ids
        .iter()
        .map(|id| {
            devices
                .iter()
                .find(|other| other.id == *id)
                .map(|other| other.name.clone())
                .unwrap_or_else(|| id.clone())
        })
        .collect();
    let recommended_name = devices
        .iter()
        .find(|other| other.id == conflict.recommended_id)
        .map(|other| other.name.clone())
        .unwrap_or_else(|| conflict.recommended_id.clone());
    let is_recommended = conflict.recommended_id == d.id;
    Some(ConflictPresentation {
        peer_names,
        recommended_name,
        confidence: conflict.confidence,
        is_recommended,
        can_disable: !is_recommended && d.active_state != VisibilityState::Disabled,
    })
}

/// Find the parent hub whose RGB chain lists `device_id` and return its
/// write-rate status. Chain accessories share their parent's transport, so
/// their own `write_rate` is always `None`.
pub(crate) fn find_hub_write_rate(state: &AppState, device_id: &str) -> Option<WriteRateStatus> {
    state.devices.iter().find_map(|parent| {
        let is_hub_for_device = parent.lighting().is_some_and(|lighting| {
            lighting.descriptor.channels.iter().any(|channel| {
                matches!(
                    &channel.division,
                    halod_shared::types::LightingDivision::Divisible { segments, .. }
                        if segments.iter().any(|segment| segment.device_id == device_id)
                )
            })
        });
        is_hub_for_device.then_some(parent.write_rate).flatten()
    })
}

/// A device's own write-rate status, falling back to its parent hub's when
/// it hasn't wired up its own (chain accessories).
pub(crate) fn effective_write_rate(state: &AppState, dev: &WireDevice) -> Option<WriteRateStatus> {
    dev.write_rate
        .or_else(|| find_hub_write_rate(state, &dev.id))
}

/// A single metric chip on a device card.
pub struct Metric {
    pub label: String,
    pub value: String,
}

/// First battery's `(level, charging)`, if the device reports one and its
/// status is known. `Unknown` status means the wireless link is down (headset
/// off/out of range) — treat it as "no data" so the card falls back to the
/// transport/offline chip instead of showing a stale 0%.
pub fn battery(d: &WireDevice) -> Option<(u8, bool)> {
    d.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Battery(b) => b
            .first()
            .filter(|b| !matches!(b.status, BatteryStatus::Unknown))
            .map(|b| (b.level, matches!(b.status, BatteryStatus::Charging))),
        _ => None,
    })
}

/// Battery accent classification: charging or full is `Ok`, else `Critical`
/// below 25%, `Low` below 50%, `Ok` above.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum BatteryLevel {
    Ok,
    Low,
    Critical,
}

pub fn battery_level(level: u8, charging: bool) -> BatteryLevel {
    if charging {
        BatteryLevel::Ok
    } else if level < 25 {
        BatteryLevel::Critical
    } else if level < 50 {
        BatteryLevel::Low
    } else {
        BatteryLevel::Ok
    }
}

/// Whether `d` *is* an integration's root (e.g. the OpenRGB SDK client)
/// rather than a real device — it belongs on the Integrations page, not
/// Home/sidebar. The devices an integration exposes as children have no
/// `integration_id` of their own, so they stay listable.
pub fn is_integration_root(d: &WireDevice) -> bool {
    d.integration_id.is_some()
}

pub fn listable(d: &WireDevice) -> bool {
    !matches!(d.device_type, DeviceType::Sensor) && !is_integration_root(d)
}

pub fn is_hidden(d: &WireDevice) -> bool {
    d.active_state != VisibilityState::Visible
}

/// Whether a device matches a free-text filter (case-insensitive substring of
/// its name or vendor). An empty/whitespace query matches everything.
pub fn matches_query(d: &WireDevice, query: &str) -> bool {
    let q = query.trim();
    if q.is_empty() {
        return true;
    }
    let q = q.to_lowercase();
    d.name.to_lowercase().contains(&q) || d.vendor.to_lowercase().contains(&q)
}

/// Short badge abbreviation, translated GUI-side from the `DeviceType` enum —
/// the daemon ships the enum, never the abbreviation text.
pub fn code(d: &WireDevice) -> std::borrow::Cow<'static, str> {
    match d.device_type {
        DeviceType::AIO => t!("device_code.aio"),
        DeviceType::Fan => t!("device_code.fan"),
        DeviceType::Hub => t!("device_code.hub"),
        DeviceType::Dongle => t!("device_code.dongle"),
        DeviceType::Keyboard => t!("device_code.keyboard"),
        DeviceType::Mouse => t!("device_code.mouse"),
        DeviceType::Headset => t!("device_code.headset"),
        DeviceType::Monitor => t!("device_code.monitor"),
        DeviceType::Gpu => t!("device_code.gpu"),
        DeviceType::LedStrip => t!("device_code.led_strip"),
        DeviceType::Motherboard => t!("device_code.motherboard"),
        DeviceType::Ram => t!("device_code.ram"),
        DeviceType::Sensor => t!("device_code.sensor"),
        DeviceType::Speaker => t!("device_code.speaker"),
        DeviceType::Computer => t!("device_code.computer"),
        DeviceType::Other => t!("device_code.other"),
    }
}

/// Human-readable device-category label, translated GUI-side from the
/// `DeviceType` enum the daemon ships — the daemon never sends prose.
pub fn type_label(d: &WireDevice) -> std::borrow::Cow<'static, str> {
    device_type_label(d.device_type)
}

/// Like [`type_label`] but from a bare [`DeviceType`], for contexts that only
/// carry the enum (e.g. a captured conflict group).
pub fn device_type_label(ty: DeviceType) -> std::borrow::Cow<'static, str> {
    match ty {
        DeviceType::AIO => t!("device_type.aio"),
        DeviceType::Fan => t!("device_type.fan"),
        DeviceType::Hub => t!("device_type.hub"),
        DeviceType::Dongle => t!("device_type.dongle"),
        DeviceType::Keyboard => t!("device_type.keyboard"),
        DeviceType::Mouse => t!("device_type.mouse"),
        DeviceType::Headset => t!("device_type.headset"),
        DeviceType::Monitor => t!("device_type.monitor"),
        DeviceType::Gpu => t!("device_type.gpu"),
        DeviceType::LedStrip => t!("device_type.led_strip"),
        DeviceType::Motherboard => t!("device_type.motherboard"),
        DeviceType::Ram => t!("device_type.ram"),
        DeviceType::Sensor => t!("device_type.sensor"),
        DeviceType::Speaker => t!("device_type.speaker"),
        DeviceType::Computer => t!("device_type.computer"),
        DeviceType::Other => t!("device_type.other"),
    }
}

/// Number of distinct accent hues a device can map to (see [`hue_index`]).
/// Pinned against `ui::theme::DEVICE_HUES.len()` on the theme side.
#[cfg(test)]
pub const DEVICE_HUE_COUNT: usize = 10;

/// Which accent hue slot (`0..DEVICE_HUE_COUNT`) a device type maps to.
pub fn hue_index(d: &WireDevice) -> usize {
    match d.device_type {
        DeviceType::Keyboard => 0,
        DeviceType::Mouse => 1,
        DeviceType::Headset => 2,
        DeviceType::Ram => 3,
        DeviceType::Gpu => 4,
        DeviceType::Fan => 5,
        DeviceType::AIO => 6,
        DeviceType::LedStrip => 7,
        DeviceType::Monitor | DeviceType::Motherboard => 8,
        _ => 9,
    }
}

/// Up to two headline metrics derived from the device's capabilities. Battery
/// is intentionally excluded — it renders as the top-right hint, not a metric.
pub fn metrics(d: &WireDevice) -> Vec<Metric> {
    let mut out = Vec::new();
    for cap in &d.capabilities {
        if out.len() >= 2 {
            break;
        }
        match cap {
            DeviceCapability::Cooling(cooling) => {
                if let Some(channel) = cooling.channels.first() {
                    if let Some(rpm) = channel.rpm {
                        out.push(Metric {
                            label: t!("model.metric_speed").into(),
                            value: format!("{rpm} RPM"),
                        });
                    }
                }
            }
            DeviceCapability::Sensors(ss) => {
                if let Some(s) = ss.iter().find(|s| s.sensor_type == SensorType::Temperature) {
                    out.push(Metric {
                        label: t!("model.metric_temp").into(),
                        value: format!("{:.0}{}", s.value, unit(&s.unit)),
                    });
                }
            }
            DeviceCapability::Dpi(dpi) => {
                if let Some(v) = dpi.steps.get(dpi.current_index) {
                    out.push(Metric {
                        label: "DPI".into(),
                        value: v.to_string(),
                    });
                }
            }
            _ => {}
        }
    }
    if out.is_empty() {
        if let Some(ct) = &d.connection_type {
            out.push(Metric {
                label: t!("model.metric_link").into(),
                value: match ct {
                    ConnectionType::Wired => t!("model.wired").into(),
                    ConnectionType::Wireless => t!("model.wireless").into(),
                },
            });
        }
    }
    out
}

pub(super) fn unit(u: &SensorUnit) -> &'static str {
    match u {
        SensorUnit::Celsius => "°C",
        SensorUnit::Fahrenheit => "°F",
        SensorUnit::Percent => "%",
        SensorUnit::Megahertz => "MHz",
        SensorUnit::Hours => "h",
        SensorUnit::Rpm => "RPM",
    }
}

/// The transport shown top-right of a device card. Prefers the daemon-reported
/// byte-movement transport (HID, SMBus, USB, …) uppercased; falls back to the
/// wired/wireless link type when no transport string is available.
pub fn transport_label(d: &WireDevice) -> String {
    if let Some(t) = d.transport.as_deref().filter(|t| !t.is_empty()) {
        return transport_display(t);
    }
    match d.connection_type {
        Some(ConnectionType::Wireless) => t!("model.wireless").into(),
        Some(ConnectionType::Wired) | None => t!("model.wired").into(),
    }
}

/// Human-facing rendering of a raw transport id (`hid` → `HID`, `smbus` →
/// `SMBus`, otherwise upper-cased).
fn transport_display(t: &str) -> String {
    for (key, label) in [
        ("hid", "HID"),
        ("smbus", "SMBus"),
        ("usb", "USB"),
        ("i2c", "I2C"),
        ("pawnio", "PawnIO"),
        ("lpcio", "SuperIO"),
        ("superio", "SuperIO"),
        ("hwmon", "hwmon"),
    ] {
        if t.eq_ignore_ascii_case(key) {
            return label.into();
        }
    }
    t.to_uppercase()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{
        Battery, CoolingChannel, CoolingChannelKind, CoolingStatus, DpiMode, DpiStatus, Sensor,
    };

    fn dev(ty: DeviceType, caps: Vec<DeviceCapability>) -> WireDevice {
        WireDevice {
            device_type: ty,
            connected: true,
            capabilities: caps,
            ..Default::default()
        }
    }

    #[test]
    fn conflict_presentation_resolves_peer_and_recommendation() {
        use halod_shared::types::{ConflictConfidence, DeviceConflictSummary};
        let primary = WireDevice {
            id: "native".into(),
            name: "Native".into(),
            ..Default::default()
        };
        let duplicate = WireDevice {
            id: "openrgb".into(),
            name: "OpenRGB".into(),
            conflict: Some(DeviceConflictSummary {
                peer_ids: vec!["native".into()],
                recommended_id: "native".into(),
                confidence: ConflictConfidence::Confirmed,
                participants: vec![],
            }),
            ..Default::default()
        };
        let p = conflict_presentation(&duplicate, &[primary, duplicate.clone()]).unwrap();
        assert_eq!(p.peer_names, vec!["Native"]);
        assert_eq!(p.recommended_name, "Native");
        assert!(p.can_disable);
    }

    #[test]
    fn matches_query_is_case_insensitive_over_name_and_vendor() {
        let mut d = dev(DeviceType::Keyboard, vec![]);
        d.name = "Apex Pro".into();
        d.vendor = "SteelSeries".into();
        // Empty / whitespace matches everything.
        assert!(matches_query(&d, ""));
        assert!(matches_query(&d, "   "));
        // Name and vendor both match, ignoring case.
        assert!(matches_query(&d, "apex"));
        assert!(matches_query(&d, "STEEL"));
        // A non-substring does not match.
        assert!(!matches_query(&d, "logitech"));
    }

    #[test]
    fn code_and_hue_are_stable_per_type() {
        let a = dev(DeviceType::Keyboard, vec![]);
        let b = dev(DeviceType::Keyboard, vec![]);
        assert_eq!(code(&a), "KB");
        assert_eq!(hue_index(&a), hue_index(&b));
    }

    #[test]
    fn battery_is_a_hint_not_a_metric() {
        let d = dev(
            DeviceType::Mouse,
            vec![DeviceCapability::Battery(vec![Battery {
                key: "k".into(),
                label: "l".into(),
                level: 40,
                status: BatteryStatus::Discharging,
            }])],
        );
        // Battery surfaces via battery(), never as a metric chip.
        assert_eq!(battery(&d), Some((40, false)));
        assert!(metrics(&d).iter().all(|m| m.label != "Battery"));
    }

    #[test]
    fn battery_level_thresholds_and_charging() {
        assert_eq!(battery_level(20, false), BatteryLevel::Critical);
        assert_eq!(battery_level(40, false), BatteryLevel::Low);
        assert_eq!(battery_level(90, false), BatteryLevel::Ok);
        assert_eq!(battery_level(10, true), BatteryLevel::Ok);
    }

    #[test]
    fn metrics_capped_at_two() {
        let d = dev(
            DeviceType::Hub,
            vec![
                DeviceCapability::Cooling(CoolingStatus {
                    channels: vec![CoolingChannel {
                        id: "fan0".into(),
                        name: "Fan".into(),
                        kind: CoolingChannelKind::Fan,
                        controllable: true,
                        rpm: Some(1000),
                        duty: Some(50),
                    }],
                }),
                DeviceCapability::Sensors(vec![Sensor {
                    id: "s".into(),
                    name: "t".into(),
                    value: 40.0,
                    unit: SensorUnit::Celsius,
                    sensor_type: SensorType::Temperature,
                    visibility: VisibilityState::Visible,
                }]),
                DeviceCapability::Cooling(CoolingStatus {
                    channels: vec![CoolingChannel {
                        id: "fan1".into(),
                        name: "Fan".into(),
                        kind: CoolingChannelKind::Fan,
                        controllable: true,
                        rpm: Some(1200),
                        duty: Some(60),
                    }],
                }),
            ],
        );
        assert_eq!(metrics(&d).len(), 2);
    }

    #[test]
    fn transport_label_maps_connection_type() {
        use halod_shared::types::ConnectionType;
        let mut wl = dev(DeviceType::Mouse, vec![]);
        wl.connection_type = Some(ConnectionType::Wireless);
        assert_eq!(transport_label(&wl), "Wireless");
        let wired = dev(DeviceType::Keyboard, vec![]);
        assert_eq!(transport_label(&wired), "Wired");
    }

    #[test]
    fn transport_label_prefers_transport_over_link() {
        use halod_shared::types::ConnectionType;
        let mut d = dev(DeviceType::Keyboard, vec![]);
        d.connection_type = Some(ConnectionType::Wired);
        d.transport = Some("hid".into());
        assert_eq!(transport_label(&d), "HID");
        d.transport = Some("smbus".into());
        assert_eq!(transport_label(&d), "SMBus");
        // An empty transport string falls back to the link type.
        d.transport = Some(String::new());
        assert_eq!(transport_label(&d), "Wired");
    }

    const ALL_TYPES: [DeviceType; 16] = [
        DeviceType::Other,
        DeviceType::Fan,
        DeviceType::Hub,
        DeviceType::Dongle,
        DeviceType::Keyboard,
        DeviceType::Mouse,
        DeviceType::Headset,
        DeviceType::Monitor,
        DeviceType::Gpu,
        DeviceType::LedStrip,
        DeviceType::Motherboard,
        DeviceType::Ram,
        DeviceType::Sensor,
        DeviceType::AIO,
        DeviceType::Speaker,
        DeviceType::Computer,
    ];

    #[test]
    fn code_and_type_label_nonempty_for_every_type() {
        for ty in ALL_TYPES {
            let d = dev(ty, vec![]);
            assert!(!code(&d).is_empty());
            assert!(!type_label(&d).is_empty());
        }
    }

    #[test]
    fn hue_index_is_in_range_for_every_type() {
        // hue_index() indexes theme::DEVICE_HUES; every DeviceType must map to
        // a valid in-range index (no panic, no out-of-bounds).
        for ty in ALL_TYPES {
            let d = dev(ty, vec![]);
            assert!(hue_index(&d) < DEVICE_HUE_COUNT);
        }
    }

    #[test]
    fn battery_unknown_status_is_no_data() {
        let d = dev(
            DeviceType::Headset,
            vec![DeviceCapability::Battery(vec![Battery {
                key: "k".into(),
                label: "l".into(),
                level: 0,
                status: BatteryStatus::Unknown,
            }])],
        );
        assert_eq!(battery(&d), None);
    }

    #[test]
    fn battery_charging_flag() {
        let d = dev(
            DeviceType::Mouse,
            vec![DeviceCapability::Battery(vec![Battery {
                key: "k".into(),
                label: "l".into(),
                level: 80,
                status: BatteryStatus::Charging,
            }])],
        );
        assert_eq!(battery(&d), Some((80, true)));
    }

    #[test]
    fn listable_excludes_sensors_and_integration_roots() {
        assert!(listable(&dev(DeviceType::Keyboard, vec![])));
        assert!(!listable(&dev(DeviceType::Sensor, vec![])));

        let mut integration_root = dev(DeviceType::Other, vec![]);
        integration_root.integration_id = Some("openrgb".into());
        assert!(is_integration_root(&integration_root));
        assert!(!listable(&integration_root));

        // The devices an integration exposes as children carry no
        // integration_id of their own, so they stay listable.
        let child = dev(DeviceType::LedStrip, vec![]);
        assert!(!is_integration_root(&child));
        assert!(listable(&child));
    }

    #[test]
    fn is_hidden_tracks_visibility() {
        let mut d = dev(DeviceType::Mouse, vec![]);
        assert!(!is_hidden(&d));
        d.active_state = VisibilityState::Hidden;
        assert!(is_hidden(&d));
        d.active_state = VisibilityState::Disabled;
        assert!(is_hidden(&d));
    }

    #[test]
    fn metrics_temperature_arm() {
        let d = dev(
            DeviceType::Hub,
            vec![DeviceCapability::Sensors(vec![Sensor {
                id: "s".into(),
                name: "t".into(),
                value: 42.4,
                unit: SensorUnit::Celsius,
                sensor_type: SensorType::Temperature,
                visibility: VisibilityState::Visible,
            }])],
        );
        let m = metrics(&d);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].label, "Temp");
        assert_eq!(m[0].value, "42°C");
    }

    fn dpi(steps: Vec<u16>, current_index: usize) -> DpiStatus {
        DpiStatus {
            steps,
            current_index,
            current_dpi: 800,
            available_dpis: vec![],
            mode: DpiMode::Host,
        }
    }

    #[test]
    fn metrics_dpi_uses_current_step() {
        let d = dev(
            DeviceType::Mouse,
            vec![DeviceCapability::Dpi(dpi(vec![400, 800, 1600], 1))],
        );
        let m = metrics(&d);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].label, "DPI");
        assert_eq!(m[0].value, "800");
    }

    #[test]
    fn metrics_dpi_out_of_range_index_yields_no_metric() {
        let d = dev(
            DeviceType::Mouse,
            vec![DeviceCapability::Dpi(dpi(vec![400, 800], 5))],
        );
        assert!(metrics(&d).is_empty());
    }

    #[test]
    fn metrics_fall_back_to_link_when_empty() {
        use halod_shared::types::ConnectionType;
        let mut d = dev(DeviceType::Mouse, vec![]);
        d.connection_type = Some(ConnectionType::Wireless);
        let m = metrics(&d);
        assert_eq!(m.len(), 1);
        assert_eq!(m[0].label, "Link");
        assert_eq!(m[0].value, "Wireless");
    }

    fn hub_with_child(
        hub_id: &str,
        child_id: &str,
        rate: Option<halod_shared::types::WriteRateStatus>,
    ) -> WireDevice {
        use halod_shared::types::{
            LightingChannel, LightingDescriptor, LightingDivision, LightingSegmentInfo,
            LightingStatus, ZoneTopology,
        };
        WireDevice {
            id: hub_id.into(),
            capabilities: vec![DeviceCapability::Lighting(LightingStatus {
                descriptor: LightingDescriptor {
                    channels: vec![LightingChannel {
                        id: "0".into(),
                        name: "Ext".into(),
                        topology: ZoneTopology::Linear,
                        leds: vec![],
                        color_order: Default::default(),
                        division: LightingDivision::Divisible {
                            max_leds: 40,
                            segments: vec![LightingSegmentInfo {
                                device_id: child_id.into(),
                                channel_id: "lighting".into(),
                                name: "Fan".into(),
                                topology: ZoneTopology::Ring,
                                led_count: 8,
                                color_order: None,
                                locked: false,
                            }],
                        },
                    }],
                    native_effects: vec![],
                },
                state: None,
                channel_transforms: Default::default(),
            })],
            write_rate: rate,
            ..Default::default()
        }
    }

    #[test]
    fn effective_write_rate_prefers_own_status_over_hub() {
        use halod_shared::types::WriteRateStatus;
        let hub_rate = WriteRateStatus {
            limit: None,
            current_writes_per_sec: 1.0,
            current_bytes_per_sec: 10.0,
            rejected_total: 0,
        };
        let own_rate = WriteRateStatus {
            limit: None,
            current_writes_per_sec: 2.0,
            current_bytes_per_sec: 20.0,
            rejected_total: 0,
        };
        let hub = hub_with_child("hub1", "fan1", Some(hub_rate));
        let child = WireDevice {
            id: "fan1".into(),
            write_rate: Some(own_rate),
            ..Default::default()
        };
        let state = AppState {
            devices: vec![hub, child.clone()],
            ..Default::default()
        };

        assert_eq!(effective_write_rate(&state, &child), Some(own_rate));
    }

    #[test]
    fn effective_write_rate_falls_back_to_hub_when_own_is_unset() {
        use halod_shared::types::WriteRateStatus;
        let hub_rate = WriteRateStatus {
            limit: None,
            current_writes_per_sec: 1.0,
            current_bytes_per_sec: 10.0,
            rejected_total: 0,
        };
        let hub = hub_with_child("hub1", "fan1", Some(hub_rate));
        let child = WireDevice {
            id: "fan1".into(),
            write_rate: None,
            ..Default::default()
        };
        let state = AppState {
            devices: vec![hub, child.clone()],
            ..Default::default()
        };

        assert_eq!(effective_write_rate(&state, &child), Some(hub_rate));
    }

    #[test]
    fn find_hub_write_rate_finds_parent_by_chain_link() {
        use halod_shared::types::{WriteRateLimit, WriteRateStatus};
        let hub_rate = WriteRateStatus {
            limit: Some(WriteRateLimit {
                max_bytes_per_sec: 30_000,
            }),
            current_writes_per_sec: 12.0,
            current_bytes_per_sec: 480.0,
            rejected_total: 0,
        };
        let state = AppState {
            devices: vec![hub_with_child("hub1", "fan1", Some(hub_rate))],
            ..Default::default()
        };

        assert_eq!(find_hub_write_rate(&state, "fan1"), Some(hub_rate));
        assert_eq!(find_hub_write_rate(&state, "unrelated"), None);
    }

    #[test]
    fn find_hub_write_rate_none_when_hub_has_not_wired_up_stats() {
        let state = AppState {
            devices: vec![hub_with_child("hub1", "fan1", None)],
            ..Default::default()
        };

        assert_eq!(find_hub_write_rate(&state, "fan1"), None);
    }
}
