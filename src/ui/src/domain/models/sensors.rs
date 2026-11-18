// SPDX-License-Identifier: GPL-3.0-or-later
//! Sensor list derived from the daemon's `TopicStore`, for the home dashboard.

use crate::domain::topic_store::TopicStore;
use halod_shared::types::{DeviceCapability, SensorType, WireDevice};

use super::device::unit;

/// A sensor surfaced on the home dashboard.
pub struct SensorView {
    pub id: String,
    pub label: String,
    pub device: String,
    pub value: f64,
    pub unit: &'static str,
    pub sensor_type: SensorType,
}

/// One device's sensors. Sensors without their own name (e.g. an hwmon channel
/// with a blank `_label`) fall back to the device name.
pub fn views(d: &WireDevice) -> impl Iterator<Item = SensorView> + '_ {
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
            device: d.name.clone(),
            value: s.value,
            unit: unit(&s.unit),
            sensor_type: s.sensor_type.clone(),
        })
}

/// Lowest and highest sample in `history`, or `None` when it is empty.
pub fn range(history: impl IntoIterator<Item = f32>) -> Option<(f32, f32)> {
    history.into_iter().fold(None, |acc, v| match acc {
        None => Some((v, v)),
        Some((lo, hi)) => Some((lo.min(v), hi.max(v))),
    })
}

/// Sensors across all devices, in device order.
pub fn sensors(state: &TopicStore) -> Vec<SensorView> {
    state.devices.iter().flat_map(views).collect()
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
    fn views_carry_the_sensor_type() {
        let d = dev(
            DeviceType::Sensor,
            vec![DeviceCapability::Sensors(vec![sensor(
                "rpm",
                SensorType::FanSpeed,
            )])],
        );
        assert_eq!(views(&d).next().unwrap().sensor_type, SensorType::FanSpeed);
    }

    #[test]
    fn range_is_none_when_empty() {
        assert_eq!(range(std::iter::empty()), None);
    }

    #[test]
    fn range_spans_lowest_to_highest() {
        assert_eq!(range([3.0, -1.5, 7.25, 2.0]), Some((-1.5, 7.25)));
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

    proptest::proptest! {
        #[test]
        fn range_bounds_every_sample_and_is_attained(
            samples in proptest::collection::vec(-1.0e6f32..1.0e6, 1..120)
        ) {
            let (lo, hi) = range(samples.iter().copied()).unwrap();
            proptest::prop_assert!(samples.iter().all(|&v| lo <= v && v <= hi));
            proptest::prop_assert!(samples.contains(&lo) && samples.contains(&hi));
        }
    }
}
