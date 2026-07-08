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
    let id = ui.id().with(("srd", label));
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
                    .font(theme::body(12.0))
                    .color(theme::TEXT_DIM),
            );
        },
        |ui| {
            ui.label(
                egui::RichText::new(readout)
                    .font(theme::mono_semibold(11.5))
                    .color(theme::CYAN),
            );
        },
    );
    ui.add_space(8.0);
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
    let mut edited = false;
    let mut committed = false;
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new(label)
                    .font(theme::body(12.0))
                    .color(theme::TEXT_DIM),
            );
        },
        |ui| {
            let resp = ui.add(
                egui::DragValue::new(value)
                    .range(range)
                    .speed(1.0)
                    .max_decimals(1),
            );
            edited = resp.changed();
            committed = resp.drag_stopped() || resp.lost_focus();
        },
    );
    (edited, committed)
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
                    .font(theme::body(12.0))
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
fn stepper(ui: &mut egui::Ui, id_src: &str, value_text: &str) -> i32 {
    let (frame, _) = ui.allocate_exact_size(Vec2::new(110.0, 30.0), Sense::hover());
    let p = ui.painter();
    p.rect_stroke(
        frame,
        8.0,
        egui::Stroke::new(1.0, theme::hex(0x222b3a)),
        egui::StrokeKind::Middle,
    );

    let btn_w = 28.0;
    let dec_rect = Rect::from_min_size(frame.min, Vec2::new(btn_w, frame.height()));
    let inc_rect = Rect::from_min_size(
        Pos2::new(frame.right() - btn_w, frame.top()),
        Vec2::new(btn_w, frame.height()),
    );

    let dec_id = ui.id().with(("stepper_dec", id_src));
    let inc_id = ui.id().with(("stepper_inc", id_src));
    let dec_resp = ui.interact(dec_rect, dec_id, Sense::click());
    let inc_resp = ui.interact(inc_rect, inc_id, Sense::click());

    if dec_resp.hovered() || inc_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }

    let t_dec = ui
        .ctx()
        .animate_bool_with_time(dec_id, dec_resp.hovered(), 0.1);
    let t_inc = ui
        .ctx()
        .animate_bool_with_time(inc_id, inc_resp.hovered(), 0.1);

    let p = ui.painter();
    p.rect_filled(dec_rect, 8.0, theme::a(egui::Color32::WHITE, 0.05 * t_dec));
    p.text(
        dec_rect.center(),
        egui::Align2::CENTER_CENTER,
        "−",
        theme::body(15.0),
        theme::lerp_color(theme::TEXT_MUT, theme::TEXT, t_dec),
    );
    p.rect_filled(inc_rect, 8.0, theme::a(egui::Color32::WHITE, 0.05 * t_inc));
    p.text(
        inc_rect.center(),
        egui::Align2::CENTER_CENTER,
        "+",
        theme::body(15.0),
        theme::lerp_color(theme::TEXT_MUT, theme::TEXT, t_inc),
    );

    let center = Rect::from_min_max(
        Pos2::new(dec_rect.right(), frame.top()),
        Pos2::new(inc_rect.left(), frame.bottom()),
    );
    p.text(
        center.center(),
        egui::Align2::CENTER_CENTER,
        value_text,
        theme::mono(12.0),
        theme::TEXT_BRIGHT,
    );

    if dec_resp.clicked() {
        -1
    } else if inc_resp.clicked() {
        1
    } else {
        0
    }
}

/// A bare horizontal slider (track + fill + glowing thumb). Returns `true` on change.
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
    p.rect_filled(track, 2.0, theme::hex(0x222936));

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
    theme::glow(p, Pos2::new(cx, cy), 9.0, theme::CYAN, 0.25);
    p.circle_filled(Pos2::new(cx, cy), 7.0, theme::hex(0xdfe6f2));
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
