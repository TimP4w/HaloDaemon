// SPDX-License-Identifier: GPL-3.0-or-later
//! Template parameter label translation and the shared per-param editing widget.

use std::collections::HashMap;

use halod_shared::types::{
    DeviceCapability, EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor,
};

use super::TabCtx;
use crate::ui::components as widgets;
use crate::ui::theme;

fn param_label(param: &EffectParamDescriptor) -> &str {
    &param.label
}

fn param_heading(ui: &mut egui::Ui, label: &str) {
    widgets::field_label(ui, label);
}

/// Translated display text for one `ParamKind::Enum` option, keyed by the
/// param's `id` and the raw option value (e.g. `lcd.enum_variant.24h`).
fn enum_option_label(_param_id: &str, option: &str) -> String {
    option.to_owned()
}

/// Render one template parameter widget. Returns true if the value changed.
pub(super) fn render_param(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    param: &EffectParamDescriptor,
    values: &mut HashMap<String, EffectParamValue>,
) -> bool {
    match &param.kind {
        ParamKind::Range { min, max, .. } => {
            let current = match values.get(&param.id) {
                Some(EffectParamValue::Float(f)) => *f as f32,
                _ => *min as f32,
            };
            let mut v = current;
            let readout = format!("{v:.0}");
            if widgets::slider_row(
                ui,
                param_label(param),
                &mut v,
                (*min as f32)..=(*max as f32),
                &readout,
            ) {
                values.insert(param.id.clone(), EffectParamValue::Float(v as f64));
                return true;
            }
        }

        ParamKind::Color => {
            let current = match values.get(&param.id) {
                Some(EffectParamValue::Color(c)) => *c,
                _ => RgbColor {
                    r: 255,
                    g: 255,
                    b: 255,
                },
            };
            param_heading(ui, param_label(param));
            if let Some(c) =
                widgets::color_swatch_row(ui, ("lcd_param_color", param.id.as_str()), current)
            {
                values.insert(param.id.clone(), EffectParamValue::Color(c));
                return true;
            }
        }

        ParamKind::Number { min, max } => {
            let current = match values.get(&param.id) {
                Some(EffectParamValue::Float(value)) => *value as f32,
                _ => match param.default {
                    EffectParamValue::Float(value) => value as f32,
                    _ => 0.0,
                },
            };
            let mut value = current;
            let (edited, committed) = widgets::num_input_row(
                ui,
                param_label(param),
                &mut value,
                *min as f32..=*max as f32,
            );
            if edited || committed {
                values.insert(param.id.clone(), EffectParamValue::Float(value as f64));
                return true;
            }
        }

        ParamKind::Text => {
            let current = match values.get(&param.id) {
                Some(EffectParamValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            let mut s = current.clone();
            param_heading(ui, param_label(param));
            widgets::text_field(ui, &mut s, "", ui.available_width());
            if s != current {
                values.insert(param.id.clone(), EffectParamValue::Str(s));
                return true;
            }
        }

        ParamKind::Boolean => {
            // A missing key means the schema default, not `false`.
            let current = match values.get(&param.id) {
                Some(EffectParamValue::Bool(b)) => *b,
                _ => matches!(param.default, EffectParamValue::Bool(true)),
            };
            let mut b = current;
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(param_label(param))
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                },
                |ui| {
                    b = widgets::toggle(ui, current);
                },
            );
            if b != current {
                values.insert(param.id.clone(), EffectParamValue::Bool(b));
                return true;
            }
        }

        ParamKind::Sensor => {
            let sensors: Vec<(String, String)> = ctx
                .state
                .devices
                .iter()
                .flat_map(|d| d.capabilities.iter())
                .filter_map(|c| match c {
                    DeviceCapability::Sensors(ss) => Some(ss.iter()),
                    _ => None,
                })
                .flatten()
                .map(|s| (s.id.clone(), s.name.clone()))
                .collect();

            let current = match values.get(&param.id) {
                Some(EffectParamValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            param_heading(ui, param_label(param));
            let new_val = widgets::combo_picker_full(
                ui,
                &param.id,
                &sensors,
                &current,
                Some(&t!("lcd.none")),
            );
            if let Some(new_val) = new_val {
                if new_val != current {
                    values.insert(param.id.clone(), EffectParamValue::Str(new_val));
                    return true;
                }
            }
        }

        ParamKind::Enum { options } => {
            let current = match values.get(&param.id) {
                Some(EffectParamValue::Str(s)) => s.clone(),
                _ => options.first().cloned().unwrap_or_default(),
            };
            let opts: Vec<(String, String)> = options
                .iter()
                .map(|o| (o.clone(), enum_option_label(&param.id, o)))
                .collect();
            param_heading(ui, param_label(param));
            let new_val = widgets::combo_picker_full(ui, &param.id, &opts, &current, None);
            if let Some(new_val) = new_val {
                if new_val != current {
                    values.insert(param.id.clone(), EffectParamValue::Str(new_val));
                    return true;
                }
            }
        }

        ParamKind::Image => {} // handled by the image grid
        // No LCD template declares threshold/color steps.
        ParamKind::Steps => {}
    }
    false
}
