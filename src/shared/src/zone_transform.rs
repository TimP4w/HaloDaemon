//! Per-zone LED-content transform.
//!
//! A device's RGB zone can carry a transform that permutes its LED colour
//! array before the colours reach hardware — used to correct for physical
//! mounting (a fan installed upside-down, a ring whose first LED faces the
//! "wrong" way). The transform is **topology-dependent**:
//!
//! - **Ring topologies** (`Ring`, `Rings { count }`) use an *index-based*
//!   transform: [`ZoneContentTransform::ring_source`] cyclically shifts
//!   (`led_offset`) and/or reverses (`reverse`) the LED sequence. For
//!   `Rings { count }` it is applied to each ring slice individually (see
//!   [`ring_slice`]), and `swap_rings` additionally reverses the order of the
//!   ring slices themselves (e.g. the leftmost fan becomes the rightmost).
//! - **Non-ring topologies** (`Linear`, `Grid`, `Keyboard`) use a *geometric*
//!   transform: `flip_h` / `flip_v` mirror the LED order across the zone's
//!   horizontal / vertical centre axis (a position-based permutation).
//!
//! The transform is always expressed as a permutation of a zone's colour
//! array: `output[i] = colors[perm[i]]` (see [`build_permutation`] /
//! [`transform_colors`]).

use serde::{Deserialize, Serialize};

use crate::types::{LedPosition, RgbColor, RgbZone, ZoneTopology};

/// LED-content transform parameters for one RGB zone.
#[derive(Debug, Clone, Copy, Default, PartialEq, Serialize, Deserialize)]
pub struct ZoneContentTransform {
    /// Mirror horizontally (non-ring topologies).
    #[serde(default)]
    pub flip_h: bool,
    /// Mirror vertically (non-ring topologies).
    #[serde(default)]
    pub flip_v: bool,
    /// Reverse the LED direction within each ring (ring topologies).
    #[serde(default)]
    pub reverse: bool,
    /// Cyclic LED-index offset within each ring (ring topologies).
    #[serde(default)]
    pub led_offset: i32,
    /// Reverse the order of fan rings (`Rings` topology only).
    #[serde(default)]
    pub swap_rings: bool,
}

impl ZoneContentTransform {
    /// Returns `true` when the transform has no effect.
    pub fn is_identity(&self) -> bool {
        !self.flip_h && !self.flip_v && !self.reverse && self.led_offset == 0 && !self.swap_rings
    }

    /// Source ring-local index that output ring-local index `j` samples from,
    /// for a ring of `ring_len` LEDs.
    pub fn ring_source(&self, j: usize, ring_len: usize) -> usize {
        if ring_len == 0 {
            return 0;
        }
        debug_assert!(j < ring_len, "j must be < ring_len");
        let base = if self.reverse {
            ring_len.saturating_sub(1).saturating_sub(j)
        } else {
            j
        };
        let shifted = (base as i64 + self.led_offset as i64).rem_euclid(ring_len as i64);
        shifted as usize
    }
}

/// Normalized canvas position of an LED at in-zone coords `(lx, ly)` ∈ [0,1]
/// for a placed zone at `(x, y)` with size `(w, h)` rotated by `rotation_deg`.
///
/// Pure geometry, before any pixel/screen scaling — both the daemon canvas
/// sampler and the GUI canvas overlay delegate here so the placement math stays
/// in one place.
pub fn led_canvas_norm(
    lx: f32,
    ly: f32,
    x: f32,
    y: f32,
    w: f32,
    h: f32,
    rotation_deg: f32,
) -> (f32, f32) {
    let cx = x + w / 2.0;
    let cy = y + h / 2.0;
    let mut dx = (lx - 0.5) * w;
    let mut dy = (ly - 0.5) * h;
    if rotation_deg.abs() > 1e-6 {
        let (s, c) = rotation_deg.to_radians().sin_cos();
        (dx, dy) = (dx * c - dy * s, dx * s + dy * c);
    }
    (cx + dx, cy + dy)
}

/// `[start, end)` LED-index range of ring `ring_idx`, assuming `total_leds` is
/// split into `ring_count` contiguous equal-sized slices.
pub fn ring_slice(total_leds: usize, ring_count: usize, ring_idx: usize) -> (usize, usize) {
    let per = total_leds / ring_count.max(1);
    let start = ring_idx.saturating_mul(per);
    (start, start.saturating_add(per))
}

/// Synthetic position for a ring zone's LED when unrolled: `(px, 0.5)`, 0..1.
pub fn unrolled_led_pos(index: usize, total: usize) -> (f32, f32) {
    let px = if total > 1 {
        index as f32 / (total - 1) as f32
    } else {
        0.5
    };
    (px, 0.5)
}

/// Output→source LED-index permutation for `zone` under transform `t`:
/// `output[i] = colors[perm[i]]`. Length equals `zone.leds.len()`.
///
/// Ring topologies permute via [`ZoneContentTransform::ring_source`] (per ring
/// slice for `Rings`); non-ring topologies use a position-mirror — each LED is
/// mapped to the LED nearest the mirror of its position about the zone centre.
/// An identity transform (or an unpartitionable `Rings` zone) yields `0..n`.
pub fn build_permutation(zone: &RgbZone, t: &ZoneContentTransform) -> Vec<usize> {
    let n = zone.leds.len();
    if t.is_identity() || n == 0 {
        return (0..n).collect();
    }
    match zone.topology {
        ZoneTopology::Ring => (0..n).map(|j| t.ring_source(j, n)).collect(),
        ZoneTopology::Rings { count } => {
            let count = count as usize;
            if count == 0 || !n.is_multiple_of(count) {
                return (0..n).collect();
            }
            let mut perm = Vec::with_capacity(n);
            for r in 0..count {
                // Output rings stay in order 0..count; `swap_rings` makes each
                // output ring sample from the opposite ring slice.
                let src_ring = if t.swap_rings { count - 1 - r } else { r };
                let (start, end) = ring_slice(n, count, src_ring);
                let ring_len = end - start;
                for j in 0..ring_len {
                    perm.push(start + t.ring_source(j, ring_len));
                }
            }
            perm
        }
        ZoneTopology::Linear | ZoneTopology::Grid | ZoneTopology::Keyboard { .. } => {
            flip_permutation(&zone.leds, t)
        }
    }
}

/// Position-mirror permutation for non-ring topologies.
fn flip_permutation(leds: &[LedPosition], t: &ZoneContentTransform) -> Vec<usize> {
    if (!t.flip_h && !t.flip_v) || leds.is_empty() {
        return (0..leds.len()).collect();
    }
    let (mut min_x, mut max_x) = (f32::MAX, f32::MIN);
    let (mut min_y, mut max_y) = (f32::MAX, f32::MIN);
    for l in leds {
        min_x = min_x.min(l.x);
        max_x = max_x.max(l.x);
        min_y = min_y.min(l.y);
        max_y = max_y.max(l.y);
    }
    let cx = (min_x + max_x) / 2.0;
    let cy = (min_y + max_y) / 2.0;
    let perm: Vec<usize> = leds
        .iter()
        .map(|l| {
            let tx = if t.flip_h { 2.0 * cx - l.x } else { l.x };
            let ty = if t.flip_v { 2.0 * cy - l.y } else { l.y };
            let mut best = 0usize;
            let mut best_d = f32::MAX;
            for (k, c) in leds.iter().enumerate() {
                let d = (c.x - tx).powi(2) + (c.y - ty).powi(2);
                if d < best_d {
                    best_d = d;
                    best = k;
                }
            }
            best
        })
        .collect();
    if is_bijection(&perm) {
        perm
    } else {
        (0..leds.len()).collect()
    }
}

/// `true` when `perm` is a permutation of `0..perm.len()` (every index hit once).
pub(crate) fn is_bijection(perm: &[usize]) -> bool {
    let mut seen = vec![false; perm.len()];
    for &i in perm {
        match seen.get_mut(i) {
            Some(slot) if !*slot => *slot = true,
            _ => return false,
        }
    }
    true
}

/// Apply [`build_permutation`] to a zone's colour array.
pub fn transform_colors(
    colors: &[RgbColor],
    zone: &RgbZone,
    t: &ZoneContentTransform,
) -> Vec<RgbColor> {
    if t.is_identity() {
        return colors.to_vec();
    }
    debug_assert_eq!(
        colors.len(),
        zone.leds.len(),
        "colors/zone LED count mismatch"
    );
    build_permutation(zone, t)
        .into_iter()
        .map(|i| colors.get(i).copied().unwrap_or_default())
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ring_zone(topology: ZoneTopology, n: usize) -> RgbZone {
        RgbZone {
            id: "z".into(),
            name: "Z".into(),
            topology,
            leds: (0..n)
                .map(|i| LedPosition {
                    id: i as u32,
                    x: i as f32,
                    y: 0.0,
                })
                .collect(),
        }
    }

    fn grid_zone() -> RgbZone {
        // 2x2 grid: ids 0,1 top row; 2,3 bottom row.
        RgbZone {
            id: "g".into(),
            name: "G".into(),
            topology: ZoneTopology::Grid,
            leds: vec![
                LedPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.0,
                },
                LedPosition {
                    id: 1,
                    x: 1.0,
                    y: 0.0,
                },
                LedPosition {
                    id: 2,
                    x: 0.0,
                    y: 1.0,
                },
                LedPosition {
                    id: 3,
                    x: 1.0,
                    y: 1.0,
                },
            ],
        }
    }

    #[test]
    fn led_canvas_norm_zero_rotation_places_led() {
        // Unit zone at origin; LED at (0.25, 0.75) lands at (0.25, 0.75).
        let (nx, ny) = led_canvas_norm(0.25, 0.75, 0.0, 0.0, 1.0, 1.0, 0.0);
        assert!((nx - 0.25).abs() < 1e-6, "nx={nx}");
        assert!((ny - 0.75).abs() < 1e-6, "ny={ny}");
    }

    #[test]
    fn led_canvas_norm_90_rotation_maps_offset_to_rotated_axis() {
        // Unit zone centred at (0.5, 0.5); LED at top-centre (0.5, 0.0) has
        // local offset (0, -0.5). A +90° rotation maps it to (+0.5, 0), i.e.
        // canvas (1.0, 0.5).
        let (nx, ny) = led_canvas_norm(0.5, 0.0, 0.0, 0.0, 1.0, 1.0, 90.0);
        assert!((nx - 1.0).abs() < 1e-5, "nx={nx}");
        assert!((ny - 0.5).abs() < 1e-5, "ny={ny}");
    }

    #[test]
    fn led_canvas_norm_center_is_invariant_under_rotation() {
        // The centre LED (0.5, 0.5) sits at the zone centre regardless of angle.
        for &deg in &[0.0f32, 30.0, 90.0, 180.0, 270.0] {
            let (nx, ny) = led_canvas_norm(0.5, 0.5, 0.25, 0.25, 0.5, 0.5, deg);
            assert!((nx - 0.5).abs() < 1e-6, "deg={deg} nx={nx}");
            assert!((ny - 0.5).abs() < 1e-6, "deg={deg} ny={ny}");
        }
    }

    #[test]
    fn identity_transform_is_noop() {
        let t = ZoneContentTransform::default();
        assert!(t.is_identity());
        assert_eq!(
            build_permutation(&ring_zone(ZoneTopology::Ring, 6), &t),
            vec![0, 1, 2, 3, 4, 5]
        );
    }

    #[test]
    fn ring_source_offset_shifts_cyclically() {
        let t = ZoneContentTransform {
            led_offset: 2,
            ..Default::default()
        };
        assert_eq!(t.ring_source(0, 8), 2);
        assert_eq!(t.ring_source(6, 8), 0);
    }

    #[test]
    fn ring_source_wraps_negative_and_large() {
        let neg = ZoneContentTransform {
            led_offset: -1,
            ..Default::default()
        };
        assert_eq!(neg.ring_source(0, 8), 7);
        let big = ZoneContentTransform {
            led_offset: 19,
            ..Default::default()
        };
        assert_eq!(big.ring_source(0, 8), 3);
    }

    #[test]
    fn ring_source_reverse_then_offset_order_is_locked() {
        let t = ZoneContentTransform {
            reverse: true,
            led_offset: 2,
            ..Default::default()
        };
        // reverse: j=0 -> 7, then +2 -> 9 % 8 == 1.
        assert_eq!(t.ring_source(0, 8), 1);
    }

    #[test]
    fn ring_slice_partitions_evenly() {
        assert_eq!(ring_slice(24, 3, 0), (0, 8));
        assert_eq!(ring_slice(24, 3, 1), (8, 16));
        assert_eq!(ring_slice(24, 3, 2), (16, 24));
    }

    #[test]
    fn build_permutation_ring_offset() {
        let t = ZoneContentTransform {
            led_offset: 1,
            ..Default::default()
        };
        assert_eq!(
            build_permutation(&ring_zone(ZoneTopology::Ring, 4), &t),
            vec![1, 2, 3, 0]
        );
    }

    #[test]
    fn build_permutation_rings_is_per_ring() {
        // 4 LEDs, 2 rings of 2; offset 1 swaps within each ring, no leakage.
        let t = ZoneContentTransform {
            led_offset: 1,
            ..Default::default()
        };
        let perm = build_permutation(&ring_zone(ZoneTopology::Rings { count: 2 }, 4), &t);
        assert_eq!(perm, vec![1, 0, 3, 2]);
    }

    #[test]
    fn build_permutation_rings_uneven_count_is_identity() {
        let t = ZoneContentTransform {
            led_offset: 2,
            ..Default::default()
        };
        let perm = build_permutation(&ring_zone(ZoneTopology::Rings { count: 3 }, 4), &t);
        assert_eq!(perm, vec![0, 1, 2, 3]);
    }

    #[test]
    fn swap_rings_alone_is_not_identity() {
        let t = ZoneContentTransform {
            swap_rings: true,
            ..Default::default()
        };
        assert!(!t.is_identity());
    }

    #[test]
    fn build_permutation_rings_swap_reorders_whole_rings() {
        // 4 LEDs, 2 rings of 2; swap_rings alone swaps the ring slices.
        let t = ZoneContentTransform {
            swap_rings: true,
            ..Default::default()
        };
        let perm = build_permutation(&ring_zone(ZoneTopology::Rings { count: 2 }, 4), &t);
        assert_eq!(perm, vec![2, 3, 0, 1]);
    }

    #[test]
    fn build_permutation_rings_swap_composes_with_reverse() {
        // 4 LEDs, 2 rings of 2; swap reorders rings, reverse mirrors within each.
        let t = ZoneContentTransform {
            swap_rings: true,
            reverse: true,
            ..Default::default()
        };
        let perm = build_permutation(&ring_zone(ZoneTopology::Rings { count: 2 }, 4), &t);
        assert_eq!(perm, vec![3, 2, 1, 0]);
    }

    #[test]
    fn build_permutation_rings_swap_uneven_count_is_identity() {
        let t = ZoneContentTransform {
            swap_rings: true,
            ..Default::default()
        };
        let perm = build_permutation(&ring_zone(ZoneTopology::Rings { count: 3 }, 4), &t);
        assert_eq!(perm, vec![0, 1, 2, 3]);
    }

    #[test]
    fn build_permutation_grid_flip_h_mirrors_columns() {
        let t = ZoneContentTransform {
            flip_h: true,
            ..Default::default()
        };
        // flip_h swaps left/right: 0<->1, 2<->3.
        assert_eq!(build_permutation(&grid_zone(), &t), vec![1, 0, 3, 2]);
    }

    #[test]
    fn build_permutation_grid_flip_v_mirrors_rows() {
        let t = ZoneContentTransform {
            flip_v: true,
            ..Default::default()
        };
        // flip_v swaps top/bottom: 0<->2, 1<->3.
        assert_eq!(build_permutation(&grid_zone(), &t), vec![2, 3, 0, 1]);
    }

    #[test]
    fn flip_on_irregular_layout_falls_back_to_identity() {
        // Three collinear LEDs with no mirror partner for the middle one: the
        // nearest-neighbour mirror collides (both ends map to the same source),
        // so the flip must degrade to identity rather than drop/duplicate a LED.
        let zone = RgbZone {
            id: "k".into(),
            name: "K".into(),
            topology: ZoneTopology::Keyboard {
                form_factor: crate::types::KeyboardFormFactor::Compact60,
                layout: crate::types::KeyboardLayout::US,
            },
            leds: vec![
                LedPosition {
                    id: 0,
                    x: 0.0,
                    y: 0.0,
                },
                LedPosition {
                    id: 1,
                    x: 0.1,
                    y: 0.0,
                },
                LedPosition {
                    id: 2,
                    x: 10.0,
                    y: 0.0,
                },
            ],
        };
        let t = ZoneContentTransform {
            flip_h: true,
            ..Default::default()
        };
        let perm = build_permutation(&zone, &t);
        // Identity fallback: every LED preserved exactly once.
        assert_eq!(perm, vec![0, 1, 2]);
    }

    #[test]
    fn transform_colors_permutes_array() {
        let colors = vec![
            RgbColor { r: 1, g: 0, b: 0 },
            RgbColor { r: 2, g: 0, b: 0 },
            RgbColor { r: 3, g: 0, b: 0 },
            RgbColor { r: 4, g: 0, b: 0 },
        ];
        let t = ZoneContentTransform {
            reverse: true,
            ..Default::default()
        };
        let out = transform_colors(&colors, &ring_zone(ZoneTopology::Ring, 4), &t);
        assert_eq!(
            out.iter().map(|c| c.r).collect::<Vec<_>>(),
            vec![4, 3, 2, 1]
        );
    }

    #[test]
    fn transform_colors_identity_is_clone() {
        let colors = vec![RgbColor { r: 9, g: 9, b: 9 }; 3];
        let t = ZoneContentTransform::default();
        assert_eq!(
            transform_colors(&colors, &ring_zone(ZoneTopology::Ring, 3), &t),
            colors
        );
    }
}

#[cfg(test)]
mod prop_tests {
    use super::*;
    use proptest::prelude::*;

    fn line_zone(topology: ZoneTopology, n: usize) -> RgbZone {
        RgbZone {
            id: "z".into(),
            name: "Z".into(),
            topology,
            leds: (0..n)
                .map(|i| LedPosition {
                    id: i as u32,
                    x: i as f32,
                    y: 0.0,
                })
                .collect(),
        }
    }

    /// Delegates to the shared `is_bijection` (module-level, test-gated).
    fn any_transform() -> impl Strategy<Value = ZoneContentTransform> {
        (
            any::<bool>(),
            any::<bool>(),
            any::<bool>(),
            -1024i32..1024,
            any::<bool>(),
        )
            .prop_map(|(flip_h, flip_v, reverse, led_offset, swap_rings)| {
                ZoneContentTransform {
                    flip_h,
                    flip_v,
                    reverse,
                    led_offset,
                    swap_rings,
                }
            })
    }

    proptest! {
        /// At zero rotation an LED's normalized position stays within the zone
        /// bounding box for any in-zone coordinate.
        #[test]
        fn led_canvas_norm_unrotated_stays_in_bbox(
            lx in 0.0f32..=1.0,
            ly in 0.0f32..=1.0,
            x in 0.0f32..1.0,
            y in 0.0f32..1.0,
            w in 0.01f32..1.0,
            h in 0.01f32..1.0,
        ) {
            let (px, py) = led_canvas_norm(lx, ly, x, y, w, h, 0.0);
            prop_assert!(px >= x - 1e-4 && px <= x + w + 1e-4);
            prop_assert!(py >= y - 1e-4 && py <= y + h + 1e-4);
        }

        /// Rotating a full turn is the identity transform.
        #[test]
        fn led_canvas_norm_full_turn_is_identity(
            lx in 0.0f32..=1.0,
            ly in 0.0f32..=1.0,
            x in 0.0f32..1.0,
            y in 0.0f32..1.0,
            w in 0.01f32..1.0,
            h in 0.01f32..1.0,
        ) {
            let (ax, ay) = led_canvas_norm(lx, ly, x, y, w, h, 0.0);
            let (bx, by) = led_canvas_norm(lx, ly, x, y, w, h, 360.0);
            prop_assert!((ax - bx).abs() < 1e-3);
            prop_assert!((ay - by).abs() < 1e-3);
        }

        /// `ring_source` always maps into `0..ring_len`.
        #[test]
        fn ring_source_is_in_bounds(
            t in any_transform(),
            ring_len in 1usize..512,
            j in 0usize..512,
        ) {
            let j = j % ring_len;
            prop_assert!(t.ring_source(j, ring_len) < ring_len);
        }

        /// For a fixed transform, `ring_source` over `0..ring_len` is a bijection
        /// of the ring's indices — reverse+offset only permute, never collide.
        #[test]
        fn ring_source_is_a_bijection(t in any_transform(), ring_len in 1usize..256) {
            let mapped: Vec<usize> = (0..ring_len).map(|j| t.ring_source(j, ring_len)).collect();
            prop_assert!(super::is_bijection(&mapped));
        }

        /// `build_permutation` always returns a vector the length of the zone.
        #[test]
        fn build_permutation_length_matches_zone(
            t in any_transform(),
            n in 0usize..256,
        ) {
            let perm = build_permutation(&line_zone(ZoneTopology::Ring, n), &t);
            prop_assert_eq!(perm.len(), n);
        }

        /// Ring-topology permutations are bijections of `0..n` — no LED dropped
        /// or duplicated, for any transform.
        #[test]
        fn build_permutation_ring_is_a_bijection(t in any_transform(), n in 1usize..256) {
            let perm = build_permutation(&line_zone(ZoneTopology::Ring, n), &t);
            prop_assert!(super::is_bijection(&perm));
        }

        /// `Rings` permutations are bijections when the ring count evenly divides
        /// the LED count (the configured, controllable case).
        #[test]
        fn build_permutation_rings_is_a_bijection(
            t in any_transform(),
            count in 1usize..8,
            per_ring in 1usize..32,
        ) {
            let n = count * per_ring;
            let zone = line_zone(ZoneTopology::Rings { count: count as u8 }, n);
            let perm = build_permutation(&zone, &t);
            prop_assert!(super::is_bijection(&perm));
        }

        /// Non-ring (geometric flip) permutations are bijections of `0..n` for
        /// ANY layout, including irregular ones where the nearest-neighbour mirror
        /// would otherwise collide — the identity fallback guarantees it.
        #[test]
        fn build_permutation_flip_is_always_a_bijection(
            t in any_transform(),
            xs in prop::collection::vec(-100.0f32..100.0, 1..40),
            ys in prop::collection::vec(-100.0f32..100.0, 1..40),
        ) {
            let n = xs.len().min(ys.len());
            let zone = RgbZone {
                id: "z".into(),
                name: "Z".into(),
                topology: ZoneTopology::Grid,
                leds: (0..n)
                    .map(|i| LedPosition { id: i as u32, x: xs[i], y: ys[i] })
                    .collect(),
            };
            let perm = build_permutation(&zone, &t);
            prop_assert_eq!(perm.len(), n);
            prop_assert!(super::is_bijection(&perm));
        }

        /// Because a ring permutation is a bijection, `transform_colors` must
        /// preserve the multiset of colours — it only reorders, never invents or
        /// drops a colour.
        #[test]
        fn transform_colors_preserves_color_multiset(
            t in any_transform(),
            colors in prop::collection::vec(
                (any::<u8>(), any::<u8>(), any::<u8>())
                    .prop_map(|(r, g, b)| RgbColor { r, g, b }),
                1..64,
            ),
        ) {
            let zone = line_zone(ZoneTopology::Ring, colors.len());
            let out = transform_colors(&colors, &zone, &t);
            prop_assert_eq!(out.len(), colors.len());
            let mut a: Vec<_> = colors.iter().map(|c| (c.r, c.g, c.b)).collect();
            let mut b: Vec<_> = out.iter().map(|c| (c.r, c.g, c.b)).collect();
            a.sort_unstable();
            b.sort_unstable();
            prop_assert_eq!(a, b);
        }
    }
}
