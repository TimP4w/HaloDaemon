// SPDX-License-Identifier: GPL-3.0-or-later
//! The editable threshold→color step list (`ParamKind::Steps`).

use std::collections::HashMap;

use egui::{Align2, Pos2, Sense, Stroke, Vec2};
use halod_shared::types::{ColorStep, EffectParamDescriptor, EffectParamValue, RgbColor};

use super::button::{button, ButtonKind};
use super::color_picker::rgb_to_color32;
use super::combo::combo_picker;
use crate::ui::theme;

/// A `Steps` param's descriptor default, or a single placeholder step so the
/// editor always has a row to edit.
pub fn steps_default(d: &EffectParamDescriptor) -> Vec<ColorStep> {
    match &d.default {
        EffectParamValue::Steps(s) if !s.is_empty() => s.clone(),
        _ => vec![next_step(&[])],
    }
}

/// A labeled combo-box param row backed by `param_strs`. Returns `true` on change.
pub fn combo_param_row(
    ui: &mut egui::Ui,
    label: &str,
    key: String,
    param_strs: &mut HashMap<String, String>,
    current: String,
    options: &[(String, String)],
    none_label: Option<&str>,
) -> bool {
    let mut changed = false;
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(label)
                .font(theme::body_md())
                .color(theme::TEXT_DIM),
        );
        if let Some(new_val) = combo_picker(ui, &key, options, &current, none_label) {
            if new_val != current {
                param_strs.insert(key, new_val);
                changed = true;
            }
        }
    });
    changed
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

/// An editable threshold→color list (`ParamKind::Steps`): one row per step
/// (color swatch, threshold input, remove) plus an add button. Readings below
/// the first threshold take the first step's color, so the first row reads
/// "up to"; the rest read "from". Returns `true` when an edit should apply.
pub fn steps_editor(ui: &mut egui::Ui, label: &str, steps: &mut Vec<ColorStep>) -> bool {
    let mut changed = false;
    ui.label(
        egui::RichText::new(label)
            .font(theme::body_md())
            .color(theme::TEXT_DIM),
    );
    ui.add_space(6.0);
    let mut remove: Option<usize> = None;
    for (i, step) in steps.iter_mut().enumerate() {
        ui.horizontal(|ui| {
            ui.spacing_mut().item_spacing.x = 8.0;
            let mut c = rgb_to_color32(step.color);
            if ui.color_edit_button_srgba(&mut c).changed() {
                step.color = RgbColor {
                    r: c.r(),
                    g: c.g(),
                    b: c.b(),
                };
                changed = true;
            }
            // Fixed-width, right-aligned label column so the inputs line up.
            let (label_rect, _) = ui.allocate_exact_size(Vec2::new(38.0, 22.0), Sense::hover());
            ui.painter().text(
                Pos2::new(label_rect.right(), label_rect.center().y),
                Align2::RIGHT_CENTER,
                if i == 0 {
                    t!("misc.widget_up_to")
                } else {
                    t!("misc.widget_from")
                },
                theme::body_sm(),
                theme::TEXT_MUT,
            );
            let mut v = step.value as f32;
            let resp = ui.add_sized(
                Vec2::new(56.0, 22.0),
                egui::DragValue::new(&mut v).speed(1.0).max_decimals(1),
            );
            if resp.changed() {
                step.value = v as f64;
            }
            if resp.drag_stopped() || resp.lost_focus() {
                changed = true;
            }
            // Painted ✕ — the bundled font has no glyph for it.
            let (x_rect, x_resp) = ui.allocate_exact_size(Vec2::splat(22.0), Sense::click());
            let color = if x_resp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
                theme::TEXT
            } else {
                theme::TEXT_MUT
            };
            let s = Stroke::new(1.5, color);
            let r = 3.5;
            let c = x_rect.center();
            ui.painter()
                .line_segment([c + Vec2::new(-r, -r), c + Vec2::new(r, r)], s);
            ui.painter()
                .line_segment([c + Vec2::new(-r, r), c + Vec2::new(r, -r)], s);
            if x_resp.clicked() {
                remove = Some(i);
            }
        });
        ui.add_space(4.0);
    }
    if let Some(i) = remove {
        if steps.len() > 1 {
            steps.remove(i);
            changed = true;
        }
    }
    if button(
        ui,
        &t!("misc.widget_add_step"),
        ButtonKind::Ghost,
        Vec2::new(92.0, 24.0),
    )
    .clicked()
    {
        steps.push(next_step(steps));
        changed = true;
    }
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
}
