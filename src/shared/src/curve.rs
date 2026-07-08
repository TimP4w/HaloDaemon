// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared fan/pump curve evaluation.
//!
//! The UI's cooling pages preview a temp→duty curve as `[temp, duty]` control
//! points; this is the single canonical evaluator so the global cooling page and
//! the per-device cooling tab can't drift apart.

/// Linear-interpolated duty for `temp` on a `[temp, duty]` curve, clamped at the
/// ends. An empty curve yields `0.0`; points are assumed sorted by temperature.
///
/// NOTE: `daemon::engines::fan_curve::interpolate` implements the same algorithm
/// on tuple points; keep the two in sync.
pub fn duty_at(points: &[[f32; 2]], temp: f32) -> f32 {
    if points.is_empty() {
        return 0.0;
    }
    if temp <= points[0][0] {
        return points[0][1];
    }
    let last = points[points.len() - 1];
    if temp >= last[0] {
        return last[1];
    }
    for w in points.windows(2) {
        let (a, b) = (w[0], w[1]);
        if temp >= a[0] && temp <= b[0] {
            let span = b[0] - a[0];
            if span <= 0.0 {
                return b[1];
            }
            let t = (temp - a[0]) / span;
            return a[1] + t * (b[1] - a[1]);
        }
    }
    last[1]
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn interpolates_between_points_and_clamps_ends() {
        let c = [[20.0_f32, 20.0], [40.0, 40.0], [80.0, 100.0]];
        assert_eq!(duty_at(&c, 10.0), 20.0); // below first → first duty
        assert_eq!(duty_at(&c, 30.0), 30.0); // midpoint of 20→40
        assert_eq!(duty_at(&c, 90.0), 100.0); // above last → last duty
    }

    #[test]
    fn empty_curve_is_zero() {
        assert_eq!(duty_at(&[], 50.0), 0.0);
    }

    #[test]
    fn duplicate_adjacent_temps_avoid_nan() {
        let c = [[40.0_f32, 30.0], [40.0, 70.0]];
        assert!(duty_at(&c, 40.0).is_finite());
    }
}
