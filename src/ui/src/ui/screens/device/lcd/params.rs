// SPDX-License-Identifier: GPL-3.0-or-later
//! Template parameter label translation and the shared per-param editing widget.

use std::collections::HashMap;

use egui::Color32;
use halod_shared::lcd_custom::WidgetType;
use halod_shared::types::{
    DeviceCapability, EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor,
};

use super::TabCtx;
use crate::ui::components as widgets;
use crate::ui::theme;

fn param_label(widget_type: WidgetType, param: &EffectParamDescriptor) -> String {
    let key = match (widget_type, param.id.as_str()) {
        (WidgetType::AudioSpectrum, "fill") => "lcd.param_fill_bar".to_string(),
        (WidgetType::Shape, "fill") => "lcd.param_fill_plain".to_string(),
        (WidgetType::AudioSpectrum, "gradient") => "lcd.param_gradient_height".to_string(),
        _ => format!("lcd.param_{}", param.id),
    };
    t!(key).to_string()
}

/// Translated display text for one `ParamKind::Enum` option, keyed by the
/// param's `id` and the raw option value (e.g. `lcd.enum_variant.24h`).
fn enum_option_label(param_id: &str, option: &str) -> String {
    t!(format!("lcd.enum_{param_id}.{option}")).to_string()
}

/// Render one template parameter widget. Returns true if the value changed.
pub(super) fn render_param(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    widget_type: WidgetType,
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
                &param_label(widget_type, param),
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
                Some(EffectParamValue::Color(c)) => Color32::from_rgb(c.r, c.g, c.b),
                _ => Color32::WHITE,
            };
            let mut c = current;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(param_label(widget_type, param))
                        .font(theme::body(11.0))
                        .color(theme::TEXT_MUT),
                );
                ui.color_edit_button_srgba(&mut c);
            });
            if c != current {
                values.insert(
                    param.id.clone(),
                    EffectParamValue::Color(RgbColor {
                        r: c.r(),
                        g: c.g(),
                        b: c.b(),
                    }),
                );
                return true;
            }
        }

        ParamKind::Text => {
            let current = match values.get(&param.id) {
                Some(EffectParamValue::Str(s)) => s.clone(),
                _ => String::new(),
            };
            let mut s = current.clone();
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(param_label(widget_type, param))
                        .font(theme::body(11.0))
                        .color(theme::TEXT_MUT),
                );
                ui.text_edit_singleline(&mut s);
            });
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
            ui.horizontal(|ui| {
                ui.checkbox(&mut b, param_label(widget_type, param));
            });
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
            let mut new_val = None;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(param_label(widget_type, param))
                        .font(theme::body(11.0))
                        .color(theme::TEXT_MUT),
                );
                new_val =
                    widgets::combo_picker(ui, &param.id, &sensors, &current, Some(&t!("lcd.none")));
            });
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
            let mut new_val = None;
            ui.horizontal(|ui| {
                ui.label(
                    egui::RichText::new(param_label(widget_type, param))
                        .font(theme::body(11.0))
                        .color(theme::TEXT_MUT),
                );
                new_val = widgets::combo_picker(ui, &param.id, &opts, &current, None);
            });
            if let Some(new_val) = new_val {
                if new_val != current {
                    values.insert(param.id.clone(), EffectParamValue::Str(new_val));
                    return true;
                }
            }
        }

        ParamKind::Image => {} // handled by the image grid
        // No LCD template declares these.
        ParamKind::Number { .. } | ParamKind::Steps => {}
    }
    false
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_widget_param_and_enum_option_is_translated() {
        use halod_shared::lcd_custom::{widget_schema, WidgetType};

        for wt in [
            WidgetType::Clock,
            WidgetType::Date,
            WidgetType::Sensor,
            WidgetType::Text,
            WidgetType::Image,
            WidgetType::AudioSpectrum,
            WidgetType::AudioLevel,
            WidgetType::NowPlaying,
            WidgetType::Logo,
            WidgetType::Shape,
        ] {
            for param in widget_schema(wt) {
                assert_ne!(
                    param_label(wt, &param),
                    format!("lcd.param_{}", param.id),
                    "missing param label for {wt:?}/{}",
                    param.id
                );
                if let ParamKind::Enum { options } = &param.kind {
                    for option in options {
                        assert_ne!(
                            enum_option_label(&param.id, option),
                            format!("lcd.enum_{}.{option}", param.id),
                            "missing enum option translation for {wt:?}/{}/{option}",
                            param.id
                        );
                    }
                }
            }
        }
    }
}
