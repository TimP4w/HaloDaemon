// SPDX-License-Identifier: GPL-3.0-or-later
use serde::{Deserialize, Serialize};

/// A single fan curve assignment: links a fan device to a temperature sensor and curve points.
#[derive(Debug, Clone, Serialize, Deserialize)]
pub struct FanCurveRecord {
    /// Sensor device ID to read temperature from. None = defined but not yet assigned.
    pub sensor_id: Option<String>,
    /// (temp_celsius, duty_percent) control points, must be in ascending temp order.
    pub points: Vec<(f32, f32)>,
}

impl FanCurveRecord {
    /// Lowest temperature a control point may specify, in °C. Sub-ambient is
    /// allowed (chilled loops) but absurd values are clamped.
    pub const MIN_TEMP_C: f32 = -50.0;
    /// Highest temperature a control point may specify, in °C.
    pub const MAX_TEMP_C: f32 = 150.0;

    /// Clamps and sorts points so `cooling::fan_curve::interpolate` never sees
    /// out-of-range or unsorted data, even from a hand-edited or corrupted
    /// `config.yaml` restored via `restore_state`.
    pub fn sanitize(&mut self) {
        fn clamp_or_low(v: f32, lo: f32, hi: f32) -> f32 {
            if v.is_nan() {
                lo
            } else {
                v.clamp(lo, hi)
            }
        }
        for (temp, duty) in &mut self.points {
            *temp = clamp_or_low(*temp, Self::MIN_TEMP_C, Self::MAX_TEMP_C);
            *duty = clamp_or_low(*duty, 0.0, 100.0);
        }
        self.points.sort_by(|a, b| a.0.total_cmp(&b.0));
        self.points.dedup_by(|a, b| a.0 == b.0);
    }

    pub fn serialize(
        &self,
        fan_id: String,
        status: halod_shared::types::FanCurveStatus,
    ) -> halod_shared::types::WireFanCurve {
        halod_shared::types::WireFanCurve {
            fan_id,
            sensor_id: self.sensor_id.clone(),
            points: self.points.iter().map(|&(t, d)| [t, d]).collect(),
            status,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn fan_curve_record_serde_round_trip() {
        let record = FanCurveRecord {
            sensor_id: Some("hwmon_pci_temp1".to_string()),
            points: vec![(30.0, 20.0), (60.0, 60.0), (85.0, 100.0)],
        };
        let yaml = serde_yaml::to_string(&record).unwrap();
        let decoded: FanCurveRecord = serde_yaml::from_str(&yaml).unwrap();
        assert_eq!(decoded.sensor_id, record.sensor_id);
        assert_eq!(decoded.points.len(), 3);
        assert!((decoded.points[0].0 - 30.0).abs() < 0.001);
        assert!((decoded.points[2].1 - 100.0).abs() < 0.001);
    }

    #[test]
    fn sanitize_sorts_points_by_ascending_temperature() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(80.0, 100.0), (30.0, 20.0), (55.0, 50.0)],
        };
        record.sanitize();
        assert_eq!(
            record.points,
            vec![(30.0, 20.0), (55.0, 50.0), (80.0, 100.0)]
        );
    }

    #[test]
    fn sanitize_clamps_duty_and_temperature_to_sane_ranges() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(-999.0, -10.0), (999.0, 250.0)],
        };
        record.sanitize();
        assert_eq!(
            record.points,
            vec![
                (FanCurveRecord::MIN_TEMP_C, 0.0),
                (FanCurveRecord::MAX_TEMP_C, 100.0),
            ]
        );
    }

    #[test]
    fn sanitize_drops_duplicate_temperatures_keeping_first() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(50.0, 30.0), (50.0, 90.0), (70.0, 80.0)],
        };
        record.sanitize();
        assert_eq!(record.points, vec![(50.0, 30.0), (70.0, 80.0)]);
    }

    #[test]
    fn sanitize_replaces_nan_with_lower_bound() {
        let mut record = FanCurveRecord {
            sensor_id: None,
            points: vec![(f32::NAN, f32::NAN), (40.0, 50.0)],
        };
        record.sanitize();
        assert_eq!(
            record.points,
            vec![(FanCurveRecord::MIN_TEMP_C, 0.0), (40.0, 50.0)]
        );
    }

    proptest::proptest! {
        #[test]
        fn sanitize_property_all_invariants_hold(
            raw_points in proptest::collection::vec((proptest::num::f32::ANY, proptest::num::f32::ANY), 0..20)
        ) {
            let mut record = FanCurveRecord {
                sensor_id: None,
                points: raw_points,
            };
            record.sanitize();

            // 1. All temps in [MIN_TEMP_C, MAX_TEMP_C]
            for &(temp, _) in &record.points {
                assert!(
                    (FanCurveRecord::MIN_TEMP_C..=FanCurveRecord::MAX_TEMP_C).contains(&temp),
                    "temp {temp} out of range"
                );
                assert!(!temp.is_nan(), "temp must not be NaN");
            }

            // 2. All duties in [0.0, 100.0]
            for &(_, duty) in &record.points {
                assert!((0.0..=100.0).contains(&duty), "duty {duty} out of range");
            }

            // 3. Temps sorted in ascending order
            for w in record.points.windows(2) {
                assert!(w[0].0 <= w[1].0, "temps must be sorted ascending: {:?} > {:?}", w[0].0, w[1].0);
            }

            // 4. No two consecutive points share the same temp
            for w in record.points.windows(2) {
                assert_ne!(w[0].0, w[1].0, "duplicate temp {} found", w[0].0);
            }
        }
    }
}
