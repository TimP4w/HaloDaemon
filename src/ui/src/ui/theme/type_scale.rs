// SPDX-License-Identifier: GPL-3.0-or-later
//! Semantic type-scale roles over the raw weight helpers in [`super`].

use egui::FontId;

use super::{body, medium, semibold};

/// Page titles.
pub fn display() -> FontId {
    semibold(26.0)
}
pub fn title() -> FontId {
    semibold(17.0)
}
pub fn heading() -> FontId {
    semibold(15.0)
}
pub fn subhead() -> FontId {
    semibold(13.0)
}

pub fn body_lg() -> FontId {
    body(14.0)
}
pub fn body_md() -> FontId {
    body(13.0)
}
pub fn body_sm() -> FontId {
    body(12.0)
}
pub fn caption() -> FontId {
    body(11.0)
}
pub fn micro() -> FontId {
    body(10.0)
}

pub fn value() -> FontId {
    semibold(13.0)
}
pub fn value_sm() -> FontId {
    medium(12.0)
}
pub fn value_xs() -> FontId {
    medium(11.0)
}
