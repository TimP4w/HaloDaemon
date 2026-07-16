// SPDX-License-Identifier: GPL-3.0-or-later
//! Semantic type-scale roles over the raw weight helpers in [`super`].

use egui::FontId;

use super::{body, mono, mono_semibold, semibold};

pub fn title() -> FontId {
    semibold(15.0)
}
pub fn heading() -> FontId {
    semibold(13.0)
}
pub fn subhead() -> FontId {
    semibold(12.0)
}

pub fn body_lg() -> FontId {
    body(13.0)
}
pub fn body_md() -> FontId {
    body(12.0)
}
pub fn body_sm() -> FontId {
    body(11.0)
}
pub fn caption() -> FontId {
    body(10.5)
}
pub fn micro() -> FontId {
    body(9.0)
}

pub fn value() -> FontId {
    mono_semibold(12.0)
}
pub fn value_sm() -> FontId {
    mono(11.0)
}
pub fn value_xs() -> FontId {
    mono(9.5)
}
