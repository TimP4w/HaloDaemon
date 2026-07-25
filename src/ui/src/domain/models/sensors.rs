// SPDX-License-Identifier: GPL-3.0-or-later
//! Sensor list derived from the daemon's `TopicStore`, for the home dashboard.

use crate::domain::topic_store::TopicStore;
use halod_shared::types::{DeviceCapability, WireDevice};

use super::device::unit;

/// A sensor surfaced on the home dashboard.
pub struct SensorView {
    pub id: String,
    pub label: String,
    pub value: f64,
    pub unit: &'static str,
}

/// One device's sensors. Sensors without their own name (e.g. an hwmon channel
/// with a blank `_label`) fall back to the device name.
fn views(d: &WireDevice) -> impl Iterator<Item = SensorView> + '_ {
    d.capabilities
        .iter()
        .filter_map(|cap| match cap {
            DeviceCapability::Sensors(ss) => Some(ss),
            _ => None,
        })
        .flatten()
        .map(move |s| SensorView {
            id: s.id.clone(),
            label: if s.name.trim().is_empty() {
                d.name.clone()
            } else {
                s.name.clone()
            },
            value: s.value,
            unit: unit(&s.unit),
        })
}

/// Sensors across all devices, in device order.
pub fn sensors(state: &TopicStore) -> Vec<SensorView> {
    state.devices.iter().flat_map(views).collect()
}

/// One device's sensors, in report order.
pub fn device_sensors(state: &TopicStore, device_id: &str) -> Vec<SensorView> {
    state
        .devices
        .iter()
        .filter(|d| d.id == device_id)
        .flat_map(views)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::{DeviceType, Sensor, SensorType, SensorUnit};

    fn dev(ty: DeviceType, caps: Vec<DeviceCapability>) -> WireDevice {
        WireDevice {
            device_type: ty,
            connected: true,
            capabilities: caps,
            ..Default::default()
        }
    }

    fn sensor(id: &str, ty: SensorType) -> Sensor {
        Sensor {
            id: id.into(),
            name: id.into(),
            value: 50.0,
            unit: SensorUnit::Celsius,
            sensor_type: ty,
        }
    }

    #[test]
    fn sensors_includes_every_type_in_device_order() {
        let d = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![
                sensor("load", SensorType::Load),
                sensor("temp", SensorType::Temperature),
            ])],
        );
        let state = TopicStore {
            devices: vec![d],
            ..Default::default()
        };
        let ids: Vec<_> = sensors(&state).into_iter().map(|s| s.id).collect();
        assert_eq!(ids, vec!["load".to_string(), "temp".to_string()]);
    }

    #[test]
    fn sensors_blank_name_falls_back_to_device_name() {
        let mut d = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![Sensor {
                name: "   ".into(),
                ..sensor("s", SensorType::Temperature)
            }])],
        );
        d.name = "My Device".into();
        let state = TopicStore {
            devices: vec![d],
            ..Default::default()
        };
        let s = sensors(&state);
        assert_eq!(s[0].label, "My Device");
    }

    #[test]
    fn device_sensors_selects_only_the_named_device() {
        let mut a = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![sensor(
                "a1",
                SensorType::Load,
            )])],
        );
        a.id = "a".into();
        let mut b = dev(
            DeviceType::Gpu,
            vec![DeviceCapability::Sensors(vec![
                sensor("b1", SensorType::Temperature),
                sensor("b2", SensorType::FanSpeed),
            ])],
        );
        b.id = "b".into();
        let state = TopicStore {
            devices: vec![a, b],
            ..Default::default()
        };
        let ids: Vec<_> = device_sensors(&state, "b")
            .into_iter()
            .map(|s| s.id)
            .collect();
        assert_eq!(ids, vec!["b1".to_string(), "b2".to_string()]);
        assert!(device_sensors(&state, "missing").is_empty());
    }

    #[test]
    fn unit_maps_all_variants() {
        let cases = [
            (SensorUnit::Celsius, "\u{b0}C"),
            (SensorUnit::Fahrenheit, "\u{b0}F"),
            (SensorUnit::Percent, "%"),
            (SensorUnit::Megahertz, "MHz"),
            (SensorUnit::Hours, "h"),
            (SensorUnit::Rpm, "RPM"),
        ];
        for (unit_variant, expected) in cases {
            let d = dev(
                DeviceType::Hub,
                vec![DeviceCapability::Sensors(vec![Sensor {
                    unit: unit_variant,
                    ..sensor("s", SensorType::Temperature)
                }])],
            );
            let state = TopicStore {
                devices: vec![d],
                ..Default::default()
            };
            let sv = sensors(&state);
            assert_eq!(sv[0].unit, expected, "unit() for {expected}");
        }
    }
}
