// SPDX-License-Identifier: GPL-3.0-or-later
//! The editable threshold→color step list (`ParamKind::Steps`).

use std::collections::HashMap;

use egui::{Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{ColorStep, EffectParamDescriptor, EffectParamValue, RgbColor};

use super::button::{button, ButtonKind};
use super::color_picker::rgb_to_color32;
use super::combo::combo_picker_full;
use crate::ui::theme;

/// A `Steps` param's descriptor default, or a single placeholder step so the
/// editor always has a row to edit.
pub fn steps_default(d: &EffectParamDescriptor) -> Vec<ColorStep> {
    match &d.default {
        EffectParamValue::Steps(s) if !s.is_empty() => s.clone(),
        _ => vec![next_step(&[])],
    }
}

/// A labelled full-width select param row backed by `param_strs`. Returns
/// `true` on change.
pub fn combo_param_row(
    ui: &mut egui::Ui,
    label: &str,
    key: String,
    param_strs: &mut HashMap<String, String>,
    current: String,
    options: &[(String, String)],
    none_label: Option<&str>,
) -> bool {
    super::field_label(ui, label);
    match combo_picker_full(ui, &key, options, &current, none_label) {
        Some(new_val) if new_val != current => {
            param_strs.insert(key, new_val);
            true
        }
        _ => false,
    }
}

/// New step appended by the editor's add button: 10 above the current top
/// threshold, in white so it's obviously a placeholder to recolor.
pub fn next_step(steps: &[ColorStep]) -> ColorStep {
    ColorStep {
        value: steps.last().map(|s| s.value + 10.0).unwrap_or(50.0),
        color: RgbColor {
            r: 255,
            g: 255,
            b: 255,
        },
    }
}

const BAR_H: f32 = 28.0;
const MARKER: f32 = 16.0;
const MARKER_GAP: f32 = 6.0;

/// The value range the bar spans: the thresholds plus a margin either side, so
/// the first band and the last band always show.
fn stop_domain(steps: &[ColorStep]) -> (f64, f64) {
    let lo = steps.first().map_or(0.0, |s| s.value);
    let hi = steps.last().map_or(100.0, |s| s.value);
    let pad = ((hi - lo) * 0.2).max(10.0);
    (lo - pad, hi + pad)
}

fn value_to_t(v: f64, (lo, hi): (f64, f64)) -> f32 {
    ((v - lo) / (hi - lo)).clamp(0.0, 1.0) as f32
}

fn t_to_value(t: f32, (lo, hi): (f64, f64)) -> f64 {
    lo + t.clamp(0.0, 1.0) as f64 * (hi - lo)
}

/// The colour band each step paints as `(from_t, to_t, color)`. Readings
/// below the first threshold take the first step's colour.
fn bands(steps: &[ColorStep], domain: (f64, f64)) -> Vec<(f32, f32, RgbColor)> {
    steps
        .iter()
        .enumerate()
        .map(|(i, s)| {
            let from = if i == 0 {
                0.0
            } else {
                value_to_t(s.value, domain)
            };
            let to = steps
                .get(i + 1)
                .map_or(1.0, |next| value_to_t(next.value, domain));
            (from, to, s.color)
        })
        .collect()
}

/// `v` rounded and kept strictly between step `i`'s neighbours, so dragging
/// never reorders the thresholds.
fn clamp_stop(steps: &[ColorStep], i: usize, v: f64) -> f64 {
    let lo = if i == 0 {
        f64::MIN
    } else {
        steps[i - 1].value + 1.0
    };
    let hi = steps.get(i + 1).map_or(f64::MAX, |s| s.value - 1.0);
    v.round().clamp(lo, hi.max(lo))
}

/// The colour-stops editor for a threshold→colour list (`ParamKind::Steps`):
/// a banded bar with one draggable marker per threshold, add/remove for the
/// selected stop, and its colour and exact value below. Returns `true` when an
/// edit should apply.
pub fn steps_editor(ui: &mut egui::Ui, label: &str, steps: &mut Vec<ColorStep>) -> bool {
    let sel_id = ui.make_persistent_id(("steps_sel", label));
    let mut sel: usize = ui.data(|d| d.get_temp(sel_id)).unwrap_or(0);
    sel = sel.min(steps.len().saturating_sub(1));
    let domain_id = ui.make_persistent_id(("steps_domain", label));
    let mut changed = false;

    super::field_label(ui, label);
    let (rect, _) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), BAR_H + MARKER_GAP + MARKER),
        Sense::hover(),
    );
    let inner = rect.shrink2(Vec2::new(MARKER / 2.0, 0.0));
    let bar = Rect::from_min_size(rect.min, Vec2::new(rect.width(), BAR_H));
    let dragging = ui
        .ctx()
        .is_being_dragged(ui.make_persistent_id(("stop", label, sel)));
    let domain = match ui.data(|d| d.get_temp::<(f64, f64)>(domain_id)) {
        Some(frozen) if dragging => frozen,
        _ => stop_domain(steps),
    };
    ui.data_mut(|d| d.insert_temp(domain_id, domain));

    let p = ui.painter();
    p.rect_filled(bar, theme::RADIUS_SM, theme::BORDER);
    let x_at = |t: f32| inner.left() + t * inner.width();
    for (from, to, c) in bands(steps, domain) {
        let left = if from <= 0.0 { bar.left() } else { x_at(from) };
        let right = if to >= 1.0 { bar.right() } else { x_at(to) };
        let band = Rect::from_x_y_ranges(left..=right, bar.y_range());
        let corners = egui::CornerRadius {
            nw: if from <= 0.0 {
                theme::RADIUS_SM as u8
            } else {
                0
            },
            sw: if from <= 0.0 {
                theme::RADIUS_SM as u8
            } else {
                0
            },
            ne: if to >= 1.0 { theme::RADIUS_SM as u8 } else { 0 },
            se: if to >= 1.0 { theme::RADIUS_SM as u8 } else { 0 },
        };
        p.rect_filled(band, corners, rgb_to_color32(c));
    }

    for i in 0..steps.len() {
        let x = x_at(value_to_t(steps[i].value, domain));
        let m = Rect::from_center_size(
            Pos2::new(x, bar.bottom() + MARKER_GAP + MARKER / 2.0),
            Vec2::splat(MARKER),
        );
        let resp = ui.interact(
            m.expand(4.0),
            ui.make_persistent_id(("stop", label, i)),
            Sense::click_and_drag(),
        );
        if resp.clicked() || resp.drag_started() {
            sel = i;
        }
        if resp.dragged() {
            if let Some(pos) = resp.interact_pointer_pos() {
                let t = (pos.x - inner.left()) / inner.width();
                steps[i].value = clamp_stop(steps, i, t_to_value(t, domain));
            }
        }
        if resp.drag_stopped() {
            changed = true;
        }
        if resp.hovered() || resp.dragged() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }
        let p = ui.painter();
        if i > 0 {
            p.line_segment(
                [Pos2::new(x, bar.top()), Pos2::new(x, bar.bottom())],
                Stroke::new(1.0, theme::CARD_BG),
            );
        }
        p.rect_filled(m, 4.0, rgb_to_color32(steps[i].color));
        let ring = if i == sel { theme::CYAN } else { theme::TEXT };
        p.rect_stroke(m, 4.0, Stroke::new(2.0, ring), egui::StrokeKind::Inside);
    }

    ui.add_space(theme::SPACE_4);
    ui.horizontal(|ui| {
        if button(
            ui,
            &t!("misc.widget_add_stop"),
            ButtonKind::Secondary,
            Vec2::new(80.0, 30.0),
        )
        .clicked()
        {
            steps.push(next_step(steps));
            sel = steps.len() - 1;
            changed = true;
        }
        let can_remove = steps.len() > 1;
        ui.add_enabled_ui(can_remove, |ui| {
            if button(
                ui,
                &t!("misc.widget_remove_stop"),
                ButtonKind::Ghost,
                Vec2::new(70.0, 30.0),
            )
            .clicked()
            {
                steps.remove(sel);
                sel = sel.min(steps.len() - 1);
                changed = true;
            }
        });
        ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
            ui.label(
                egui::RichText::new(t!(
                    "misc.widget_stop_readout",
                    n = sel + 1,
                    value = format!("{}", steps[sel].value)
                ))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
            );
        });
    });

    ui.add_space(theme::SPACE_4);
    let mut v = steps[sel].value as f32;
    let (edited, committed) = super::num_input_row(
        ui,
        &t!("misc.widget_stop_value"),
        &mut v,
        f32::MIN..=f32::MAX,
    );
    if edited {
        steps[sel].value = clamp_stop(steps, sel, v as f64);
    }
    changed |= committed;
    ui.add_space(theme::SPACE_4);
    if let Some(c) = super::color_picker(ui, steps[sel].color) {
        steps[sel].color = c;
        changed = true;
    }

    ui.data_mut(|d| d.insert_temp(sel_id, sel));
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn next_step_appends_above_current_top_threshold() {
        let first = next_step(&[]);
        assert_eq!(first.value, 50.0);

        let steps = vec![
            ColorStep {
                value: 40.0,
                color: RgbColor { r: 0, g: 255, b: 0 },
            },
            ColorStep {
                value: 80.0,
                color: RgbColor { r: 255, g: 0, b: 0 },
            },
        ];
        assert_eq!(next_step(&steps).value, 90.0);
    }

    fn arb_steps() -> impl proptest::strategy::Strategy<Value = Vec<ColorStep>> {
        proptest::collection::btree_set(-200i32..400, 1..6).prop_map(|vals| {
            vals.into_iter()
                .map(|v| ColorStep {
                    value: v as f64,
                    color: RgbColor { r: 1, g: 2, b: 3 },
                })
                .collect()
        })
    }

    use proptest::prelude::*;

    proptest! {
        #[test]
        fn bands_tile_the_bar_in_order(steps in arb_steps()) {
            let b = bands(&steps, stop_domain(&steps));
            prop_assert_eq!(b.len(), steps.len());
            prop_assert_eq!(b[0].0, 0.0);
            prop_assert_eq!(b[b.len() - 1].1, 1.0);
            for pair in b.windows(2) {
                prop_assert_eq!(pair[0].1, pair[1].0);
            }
            for (from, to, _) in b {
                prop_assert!(from <= to);
            }
        }

        #[test]
        fn every_threshold_sits_inside_the_bar(steps in arb_steps()) {
            let domain = stop_domain(&steps);
            for s in &steps {
                let t = value_to_t(s.value, domain);
                prop_assert!(t > 0.0 && t < 1.0);
                prop_assert!((t_to_value(t, domain) - s.value).abs() < 1e-3);
            }
        }

        #[test]
        fn dragging_a_stop_never_reorders_thresholds(
            steps in arb_steps(), i in 0usize..6, v in -1000.0f64..1000.0
        ) {
            let mut steps = steps;
            let i = i % steps.len();
            steps[i].value = clamp_stop(&steps, i, v);
            prop_assert!(steps.windows(2).all(|w| w[0].value <= w[1].value));
        }
    }
}
