// SPDX-License-Identifier: GPL-3.0-or-later
//! Sensor list derived from the daemon's `AppState`, for the home dashboard.

use halod_shared::types::{AppState, DeviceCapability, VisibilityState};

use super::device::unit;

/// A sensor surfaced on the home dashboard.
pub struct SensorView {
    pub id: String,
    pub label: String,
    pub value: f64,
    pub unit: &'static str,
    pub hidden: bool,
}

/// Sensors across all devices, in device order. Hidden sensors are included only
/// when `include_hidden` is set. Sensors without their own name (e.g. an hwmon
/// channel with a blank `_label`) fall back to the device name.
pub fn sensors(state: &AppState, include_hidden: bool) -> Vec<SensorView> {
    let mut out = Vec::new();
    for d in &state.devices {
        for cap in &d.capabilities {
            let DeviceCapability::Sensors(ss) = cap else {
                continue;
            };
            for s in ss {
                if !include_hidden && s.visibility != VisibilityState::Visible {
                    continue;
                }
                let label = if s.name.trim().is_empty() {
                    d.name.clone()
                } else {
                    s.name.clone()
                };
                out.push(SensorView {
                    id: s.id.clone(),
                    label,
                    value: s.value,
                    unit: unit(&s.unit),
                    hidden: s.visibility != VisibilityState::Visible,
                });
            }
        }
    }
    out
}

/// Number of hidden sensors across all devices. Used to decide whether the
/// home "show hidden" toggle should be offered even when no device is hidden.
pub fn hidden_count(state: &AppState) -> usize {
    state
        .devices
        .iter()
        .flat_map(|d| &d.capabilities)
        .filter_map(|cap| match cap {
            DeviceCapability::Sensors(ss) => Some(ss),
            _ => None,
        })
        .flatten()
        .filter(|s| s.visibility != VisibilityState::Visible)
        .count()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{DeviceType, Sensor, SensorType, SensorUnit, WireDevice};

    fn dev(ty: DeviceType, caps: Vec<DeviceCapability>) -> WireDevice {
        WireDevice {
            device_type: ty,
            connected: true,
            capabilities: caps,
            ..Default::default()
        }
    }

    fn sensor(id: &str, ty: SensorType, vis: VisibilityState) -> Sensor {
        Sensor {
            id: id.into(),
            name: id.into(),
            value: 50.0,
            unit: SensorUnit::Celsius,
            sensor_type: ty,
            visibility: vis,
        }
    }

    #[test]
    fn sensors_filters_hidden() {
        let d = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![
                sensor("vis", SensorType::Temperature, VisibilityState::Visible),
                sensor("hid", SensorType::Temperature, VisibilityState::Hidden),
            ])],
        );
        let state = AppState {
            devices: vec![d],
            ..Default::default()
        };
        let visible: Vec<_> = sensors(&state, false).into_iter().map(|s| s.id).collect();
        assert_eq!(visible, vec!["vis".to_string()]);
        let all: Vec<_> = sensors(&state, true).into_iter().map(|s| s.id).collect();
        assert_eq!(all, vec!["vis".to_string(), "hid".to_string()]);
        // The hidden flag is set independently of the include filter.
        let hidden_flags: Vec<_> = sensors(&state, true)
            .into_iter()
            .map(|s| s.hidden)
            .collect();
        assert_eq!(hidden_flags, vec![false, true]);
    }

    #[test]
    fn hidden_count_counts_non_visible_sensors() {
        let d = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![
                sensor("vis", SensorType::Temperature, VisibilityState::Visible),
                sensor("hid", SensorType::Temperature, VisibilityState::Hidden),
                sensor("hid2", SensorType::Load, VisibilityState::Hidden),
            ])],
        );
        let state = AppState {
            devices: vec![d],
            ..Default::default()
        };
        assert_eq!(hidden_count(&state), 2);
    }

    #[test]
    fn sensors_includes_non_temperature_types() {
        let d = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![
                sensor("load", SensorType::Load, VisibilityState::Visible),
                sensor("temp", SensorType::Temperature, VisibilityState::Visible),
            ])],
        );
        let state = AppState {
            devices: vec![d],
            ..Default::default()
        };
        let ids: Vec<_> = sensors(&state, false).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["load".to_string(), "temp".to_string()]);
    }

    #[test]
    fn sensors_blank_name_falls_back_to_device_name() {
        let mut d = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![Sensor {
                id: "s".into(),
                name: "   ".into(),
                value: 50.0,
                unit: SensorUnit::Celsius,
                sensor_type: SensorType::Temperature,
                visibility: VisibilityState::Visible,
            }])],
        );
        d.name = "My Device".into();
        let state = AppState {
            devices: vec![d],
            ..Default::default()
        };
        let s = sensors(&state, false);
        assert_eq!(s[0].label, "My Device");
    }

    #[test]
    fn unit_maps_all_variants() {
        let cases = [
            (SensorUnit::Celsius, "°C"),
            (SensorUnit::Fahrenheit, "°F"),
            (SensorUnit::Percent, "%"),
            (SensorUnit::Megahertz, "MHz"),
            (SensorUnit::Hours, "h"),
            (SensorUnit::Rpm, "RPM"),
        ];
        for (unit_variant, expected) in cases {
            let d = dev(
                DeviceType::Hub,
                vec![DeviceCapability::Sensors(vec![Sensor {
                    id: "s".into(),
                    name: "s".into(),
                    value: 1.0,
                    unit: unit_variant,
                    sensor_type: SensorType::Temperature,
                    visibility: VisibilityState::Visible,
                }])],
            );
            let state = AppState {
                devices: vec![d],
                ..Default::default()
            };
            let sv = sensors(&state, false);
            assert_eq!(sv[0].unit, expected, "unit() for {expected}");
        }
    }
}
