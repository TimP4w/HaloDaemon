// SPDX-License-Identifier: GPL-3.0-or-later
//! Sliders and numeric-input rows.

use std::ops::RangeInclusive;

use egui::{Pos2, Rect, Sense, Vec2};

use crate::ui::theme;

const SLIDER_DEBOUNCE_SECS: f64 = 0.14;

/// Temp-data payload stored per debounced slider (keyed by widget Id).
#[derive(Clone, Copy)]
struct SliderDb {
    /// egui time of the most recent value change; -1.0 = never changed.
    last_change: f64,
    /// egui time of the most recent debounce fire; -2.0 = never fired.
    last_fire: f64,
}

/// Like `slider_row` but returns `true` at most once per 140 ms quiet period.
/// Use only when the slider fires a command directly — device tabs already
/// debounce via `DeviceUi::queue`.
#[must_use]
pub fn slider_row_debounced(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    readout: &str,
) -> bool {
    let id = ui.scope_id().with(("srd", label));
    let raw = slider_row(ui, label, value, range, readout);
    let now = ui.input(|i| i.time);

    let mut db = ui
        .ctx()
        .data(|d| d.get_temp::<SliderDb>(id))
        .unwrap_or(SliderDb {
            last_change: -1.0,
            last_fire: -2.0,
        });

    if raw {
        db.last_change = now;
    }

    let fire = slider_debounce_ready(db.last_change, db.last_fire, now);
    if fire {
        db.last_fire = now;
    }

    ui.ctx().data_mut(|d| d.insert_temp(id, db));
    fire
}

fn slider_debounce_ready(last_change: f64, last_fire: f64, now: f64) -> bool {
    last_change >= 0.0 && last_change > last_fire && (now - last_change) >= SLIDER_DEBOUNCE_SECS
}

/// A label + mono readout header followed by a design-styled slider. Returns
/// `true` while the user is changing the value.
#[must_use]
pub fn slider_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
    readout: &str,
) -> bool {
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new(label)
                    .font(theme::body_md())
                    .color(theme::TEXT),
            );
        },
        |ui| {
            ui.label(
                egui::RichText::new(readout)
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
        },
    );
    ui.add_space(theme::SPACE_4);
    slider(ui, value, range)
}

/// Formats a `ParamKind::Range` value for the slider readout, with precision
/// matched to the descriptor's step size.
pub fn range_readout(v: f32, step: f64) -> String {
    if step >= 1.0 {
        format!("{}", v.round() as i32)
    } else if step >= 0.1 {
        format!("{v:.1}")
    } else {
        format!("{v:.2}")
    }
}

/// A label + numeric input field row (`ParamKind::Number`). Returns
/// `(edited, committed)`: `edited` while the value is being dragged/typed,
/// `committed` once the drag ends or the field loses focus.
pub fn num_input_row(
    ui: &mut egui::Ui,
    label: &str,
    value: &mut f32,
    range: RangeInclusive<f32>,
) -> (bool, bool) {
    ui.label(
        egui::RichText::new(label)
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
    );
    ui.add_space(theme::SPACE_3);
    let resp = ui.add_sized(
        egui::vec2(ui.available_width(), 32.0),
        egui::DragValue::new(value)
            .range(range)
            .speed(1.0)
            .max_decimals(1),
    );
    (resp.changed(), resp.drag_stopped() || resp.lost_focus())
}

/// A label + `−  value  +` stepper row. `value_text` is drawn between the
/// buttons. Returns the step direction clicked this frame: `-1`, `0`, or `+1`.
#[must_use]
pub fn stepper_row(ui: &mut egui::Ui, label: &str, value_text: &str) -> i32 {
    let mut delta = 0;
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new(label)
                    .font(theme::body_md())
                    .color(theme::TEXT_DIM),
            );
        },
        |ui| {
            delta = stepper(ui, label, value_text);
        },
    );
    delta
}

/// The bare `−  value  +` stepper control. Returns `-1`, `0`, or `+1`.
#[must_use]
pub fn stepper(
    ui: &mut egui::Ui,
    id_src: impl std::hash::Hash + std::fmt::Debug,
    value_text: &str,
) -> i32 {
    let (frame, resp) = ui.allocate_exact_size(Vec2::new(120.0, 34.0), Sense::hover());
    let btn_w = 34.0;
    let dec_rect = Rect::from_min_size(frame.min, Vec2::new(btn_w, frame.height()));
    let inc_rect = Rect::from_min_size(
        Pos2::new(frame.right() - btn_w, frame.top()),
        Vec2::new(btn_w, frame.height()),
    );
    let dec = ui.interact(dec_rect, resp.id.with(("dec", &id_src)), Sense::click());
    let inc = ui.interact(inc_rect, resp.id.with(("inc", &id_src)), Sense::click());

    let p = ui.painter();
    theme::paint_surface(p, frame, theme::RADIUS_SM, theme::INNER_BG, theme::TRACK);
    let rule = egui::Stroke::new(1.0, theme::TRACK);
    p.vline(dec_rect.right(), frame.y_range(), rule);
    p.vline(inc_rect.left(), frame.y_range(), rule);
    for (r, glyph, resp) in [(dec_rect, "−", &dec), (inc_rect, "+", &inc)] {
        if resp.hovered() {
            p.rect_filled(r.shrink(1.0), theme::RADIUS_SM - 1.0, theme::RAISED_BG);
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        p.text(
            r.center(),
            egui::Align2::CENTER_CENTER,
            glyph,
            theme::body(15.0),
            if resp.hovered() {
                theme::TEXT
            } else {
                theme::TEXT_DIM
            },
        );
    }
    p.text(
        frame.center(),
        egui::Align2::CENTER_CENTER,
        value_text,
        theme::body_lg(),
        theme::TEXT,
    );

    if dec.clicked() {
        -1
    } else if inc.clicked() {
        1
    } else {
        0
    }
}

/// A bare horizontal slider (track + accent fill + ringed thumb). Returns `true` on change.
#[must_use]
pub fn slider(ui: &mut egui::Ui, value: &mut f32, range: RangeInclusive<f32>) -> bool {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 16.0),
        Sense::click_and_drag(),
    );
    let (lo, hi) = (*range.start(), *range.end());
    let cy = rect.center().y;
    let p = ui.painter();
    let track = Rect::from_min_max(
        Pos2::new(rect.left(), cy - 2.0),
        Pos2::new(rect.right(), cy + 2.0),
    );
    p.rect_filled(track, 2.0, theme::TRACK);

    let mut changed = false;
    if (resp.dragged() || resp.clicked()) && hi > lo {
        if let Some(pos) = resp.interact_pointer_pos() {
            let t = ((pos.x - rect.left()) / rect.width()).clamp(0.0, 1.0);
            *value = lo + t * (hi - lo);
            changed = true;
        }
    }
    let t = if hi > lo {
        ((*value - lo) / (hi - lo)).clamp(0.0, 1.0)
    } else {
        0.0
    };
    let cx = rect.left() + t * rect.width();
    p.rect_filled(
        Rect::from_min_max(track.min, Pos2::new(cx, track.max.y)),
        2.0,
        theme::CYAN,
    );
    p.circle_filled(Pos2::new(cx, cy), 8.0, theme::TEXT);
    p.circle_stroke(Pos2::new(cx, cy), 6.5, egui::Stroke::new(3.0, theme::CYAN));
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn slider_debounce_ready_fires_after_pause() {
        let secs = SLIDER_DEBOUNCE_SECS;
        // Not fired before debounce elapses.
        assert!(!slider_debounce_ready(5.0, -2.0, 5.0));
        assert!(!slider_debounce_ready(5.0, -2.0, 5.0 + secs - 0.001));
        // Fires once the pause window is clearly past (avoid exact-boundary float issues).
        assert!(slider_debounce_ready(5.0, -2.0, 5.0 + secs + 0.001));
        // Doesn't fire again after last_fire is updated.
        assert!(!slider_debounce_ready(5.0, 5.0 + secs, 5.0 + secs + 1.0));
        // Fires again after a new change.
        assert!(slider_debounce_ready(6.0, 5.0 + secs, 6.0 + secs + 0.001));
        // Never fires if never changed (last_change = -1.0).
        assert!(!slider_debounce_ready(-1.0, -2.0, 999.0));
    }
}
