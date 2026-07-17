// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared select/drag/resize/rotate handle affordances for the Effects Canvas
//! and the LCD editor.
//!
//! Both screens use the same handle *layout* and selection *behaviour*, shared
//! here: a single bottom-right resize handle, a top-right remove badge, the
//! rotation nub above the top edge, click-to-select (modifier toggles), and
//! marquee union. What stays per-file is the coordinate-space math that only
//! looks alike — `snap_rotation` (different snap windows and normalization, see
//! each file's tests), rotated-corner math (canvas rotates in normalized canvas
//! space to track the daemon's LED sampler; the editor rotates a plain
//! screen-space rect), and resize application (a `PlacedZone` pins the opposite
//! corner vs. the editor's widget scale factor — different data models).
//! Unifying those would silently change one screen's feel to match the other's.

use egui::{Pos2, Rect, Vec2};
use std::collections::HashSet;
use std::hash::Hash;

/// Screen distance from a widget's top edge to its rotation handle.
const ROTATE_DIST: f32 = 20.0;
/// Side length of the (square) bottom-right resize handle's hit/paint rect.
pub const RESIZE_HANDLE: f32 = 14.0;
/// Side length of the top-right remove badge's click target.
pub const REMOVE_BADGE: f32 = 18.0;
/// Drawn radius of the remove badge's filled circle.
pub const REMOVE_BADGE_R: f32 = 9.0;

/// Screen position of a rotation handle: `ROTATE_DIST` outward from the
/// top-midpoint of a selected widget/zone, perpendicular to its top edge.
pub fn rotation_handle_pos(top_mid: Pos2, deg: f32) -> Pos2 {
    let (s, c) = deg.to_radians().sin_cos();
    Pos2::new(top_mid.x + s * ROTATE_DIST, top_mid.y - c * ROTATE_DIST)
}

/// Bottom-right resize handle rect, centred on the (already rotated) corner 2.
/// `corners` is egui order: 0=TL 1=TR 2=BR 3=BL.
pub fn resize_handle_rect(corners: &[Pos2; 4]) -> Rect {
    Rect::from_center_size(corners[2], Vec2::splat(RESIZE_HANDLE))
}

/// Top-right remove badge rect, centred on the (already rotated) corner 1.
pub fn remove_badge_rect(corners: &[Pos2; 4]) -> Rect {
    Rect::from_center_size(corners[1], Vec2::splat(REMOVE_BADGE))
}

/// Midpoint of the top edge (corners 0→1) — where the rotation stem starts.
pub fn top_mid(corners: &[Pos2; 4]) -> Pos2 {
    Pos2::new(
        (corners[0].x + corners[1].x) / 2.0,
        (corners[0].y + corners[1].y) / 2.0,
    )
}

/// Click-selection reducer shared by both editors: a modifier (Ctrl/Shift)
/// click toggles `key` in the current set; a plain click selects only `key`.
pub fn click_select<K: Eq + Hash>(selected: &mut HashSet<K>, key: K, additive: bool) {
    if additive {
        if !selected.remove(&key) {
            selected.insert(key);
        }
    } else {
        selected.clear();
        selected.insert(key);
    }
}

/// Marquee result: the widgets/channels under the rubber-band, unioned with the
/// pre-drag `base` selection when the drag was additive (modifier held).
pub fn marquee_result<K: Eq + Hash + Clone>(
    base: &HashSet<K>,
    hits: impl IntoIterator<Item = K>,
    additive: bool,
) -> HashSet<K> {
    let mut out: HashSet<K> = hits.into_iter().collect();
    if additive {
        out.extend(base.iter().cloned());
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn handles_sit_on_their_corners() {
        // TL, TR, BR, BL of an axis-aligned box.
        let corners = [
            Pos2::new(0.0, 0.0),
            Pos2::new(10.0, 0.0),
            Pos2::new(10.0, 8.0),
            Pos2::new(0.0, 8.0),
        ];
        assert_eq!(resize_handle_rect(&corners).center(), corners[2]);
        assert_eq!(remove_badge_rect(&corners).center(), corners[1]);
        assert_eq!(top_mid(&corners), Pos2::new(5.0, 0.0));
    }

    #[test]
    fn plain_click_selects_only_the_clicked_key() {
        let mut sel: HashSet<i32> = HashSet::from([1, 2, 3]);
        click_select(&mut sel, 5, false);
        assert_eq!(sel, HashSet::from([5]));
    }

    #[test]
    fn modifier_click_toggles_membership() {
        let mut sel: HashSet<i32> = HashSet::from([1, 2]);
        click_select(&mut sel, 3, true); // add
        assert_eq!(sel, HashSet::from([1, 2, 3]));
        click_select(&mut sel, 2, true); // remove
        assert_eq!(sel, HashSet::from([1, 3]));
    }

    #[test]
    fn marquee_replaces_unless_additive() {
        let base = HashSet::from([1, 2]);
        assert_eq!(marquee_result(&base, [3, 4], false), HashSet::from([3, 4]));
        assert_eq!(
            marquee_result(&base, [3, 4], true),
            HashSet::from([1, 2, 3, 4])
        );
    }
}
