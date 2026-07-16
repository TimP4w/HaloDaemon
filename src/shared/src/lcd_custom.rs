// SPDX-License-Identifier: GPL-3.0-or-later
//! Shared wire model for plugin-owned LCD layouts.

use crate::types::{
    EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor, MAX_EFFECT_PARAMS,
};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

pub const WIDGETS_JSON_PARAM: &str = "widgets_json";
pub const FONT_SANS: &str = "Noto Sans";
pub const FONT_MONO: &str = "JetBrains Mono";
pub const FONT_INTER: &str = "Inter Tight";
pub const MAX_LCD_WIDGETS: usize = 64;
pub const MAX_WIDGET_ID_BYTES: usize = 64;
pub const MAX_WIDGET_TEXT_BYTES: usize = 4096;
pub const TEXT_WEIGHT_PARAM: &str = "text_weight";
pub const TEXT_ITALIC_PARAM: &str = "text_italic";
pub const TEXT_UNDERLINE_PARAM: &str = "text_underline";
pub const TEXT_STRIKETHROUGH_PARAM: &str = "text_strikethrough";

pub fn text_style_params() -> [EffectParamDescriptor; 4] {
    [
        EffectParamDescriptor {
            id: TEXT_WEIGHT_PARAM.to_owned(),
            label: "Weight".to_owned(),
            kind: ParamKind::Enum {
                options: vec![
                    "normal".to_owned(),
                    "semibold".to_owned(),
                    "bold".to_owned(),
                ],
            },
            default: EffectParamValue::Str("normal".to_owned()),
        },
        EffectParamDescriptor {
            id: TEXT_ITALIC_PARAM.to_owned(),
            label: "Italic".to_owned(),
            kind: ParamKind::Boolean,
            default: EffectParamValue::Bool(false),
        },
        EffectParamDescriptor {
            id: TEXT_UNDERLINE_PARAM.to_owned(),
            label: "Underline".to_owned(),
            kind: ParamKind::Boolean,
            default: EffectParamValue::Bool(false),
        },
        EffectParamDescriptor {
            id: TEXT_STRIKETHROUGH_PARAM.to_owned(),
            label: "Strikethrough".to_owned(),
            kind: ParamKind::Boolean,
            default: EffectParamValue::Bool(false),
        },
    ]
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CustomTemplateDef {
    pub widgets: Vec<WidgetDef>,
    pub style: ScreenStyle,
}

impl Default for CustomTemplateDef {
    fn default() -> Self {
        Self {
            widgets: vec![WidgetDef {
                id: "w1".to_owned(),
                widget: "halo_lcd:logo".to_owned(),
                x: 0.5,
                y: 0.5,
                scale: 2.0,
                rotation: 0.0,
                color: None,
                font: Some(FONT_INTER.to_owned()),
                params: HashMap::from([
                    ("show_img".to_owned(), EffectParamValue::Bool(true)),
                    ("show_text".to_owned(), EffectParamValue::Bool(true)),
                ]),
            }],
            style: ScreenStyle::default(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct ScreenStyle {
    pub accent: RgbColor,
    pub background: BgKind,
    pub font: String,
}

impl Default for ScreenStyle {
    fn default() -> Self {
        Self {
            accent: RgbColor {
                r: 0,
                g: 200,
                b: 220,
            },
            background: BgKind::Flow,
            font: FONT_SANS.to_owned(),
        }
    }
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum BgKind {
    Flow,
    Solid,
    Grid,
    Glow,
    Image { filename: String, dim: f64 },
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetDef {
    pub id: String,
    pub widget: String,
    pub x: f32,
    pub y: f32,
    pub scale: f32,
    #[serde(default)]
    pub rotation: f32,
    pub color: Option<RgbColor>,
    pub font: Option<String>,
    #[serde(default)]
    pub params: HashMap<String, EffectParamValue>,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct WidgetSprite {
    pub id: String,
    pub signature: u64,
    pub rgba_b64: String,
    pub w: u32,
    pub h: u32,
}

#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct LcdEditorRender {
    pub device_id: String,
    pub canvas_w: u32,
    pub canvas_h: u32,
    pub sprites: Vec<WidgetSprite>,
    pub signatures: Vec<(String, u64)>,
    #[serde(default)]
    pub widgets: Vec<WidgetRenderState>,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum WidgetRenderStatus {
    Ready,
    Pending,
    Missing,
}

#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct WidgetRenderState {
    pub id: String,
    pub status: WidgetRenderStatus,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub signature: Option<u64>,
}

pub fn param_str(widget: &WidgetDef, key: &str) -> String {
    match widget.params.get(key) {
        Some(EffectParamValue::Str(value)) => value.clone(),
        _ => String::new(),
    }
}

pub fn param_f64(widget: &WidgetDef, key: &str, default: f64) -> f64 {
    match widget.params.get(key) {
        Some(EffectParamValue::Float(value)) if value.is_finite() => *value,
        _ => default,
    }
}

pub fn param_bool(widget: &WidgetDef, key: &str, default: bool) -> bool {
    match widget.params.get(key) {
        Some(EffectParamValue::Bool(value)) => *value,
        _ => default,
    }
}

pub fn param_variant(widget: &WidgetDef, default: &str) -> String {
    let value = param_str(widget, "variant");
    if value.is_empty() {
        default.to_owned()
    } else {
        value
    }
}

pub fn scale_y(widget: &WidgetDef) -> f32 {
    param_f64(widget, "scale_y", f64::from(widget.scale)) as f32
}

pub fn validate_widgets(def: &CustomTemplateDef) -> Result<(), String> {
    if def.widgets.len() > MAX_LCD_WIDGETS {
        return Err(format!("template has more than {MAX_LCD_WIDGETS} widgets"));
    }
    let mut ids = std::collections::HashSet::with_capacity(def.widgets.len());
    for widget in &def.widgets {
        if widget.id.is_empty()
            || widget.id.len() > MAX_WIDGET_ID_BYTES
            || widget.id.contains('\0')
            || !ids.insert(&widget.id)
        {
            return Err(format!(
                "widget '{}' has an invalid or duplicate id",
                widget.id
            ));
        }
        if !valid_catalog_id(&widget.widget) {
            return Err(format!("widget '{}' has an invalid catalog id", widget.id));
        }
        if !widget.x.is_finite()
            || !(0.0..=1.0).contains(&widget.x)
            || !widget.y.is_finite()
            || !(0.0..=1.0).contains(&widget.y)
        {
            return Err(format!("widget '{}' position out of bounds", widget.id));
        }
        if !valid_scale(widget.scale) || !widget.rotation.is_finite() {
            return Err(format!("widget '{}' has invalid geometry", widget.id));
        }
        if widget.params.len() > MAX_EFFECT_PARAMS {
            return Err(format!("widget '{}' has too many params", widget.id));
        }
        for (key, value) in &widget.params {
            if key.is_empty() || key.len() > MAX_WIDGET_ID_BYTES || key.contains('\0') {
                return Err(format!(
                    "widget '{}' has an invalid parameter key",
                    widget.id
                ));
            }
            match value {
                EffectParamValue::Str(value)
                    if value.len() > MAX_WIDGET_TEXT_BYTES || value.contains('\0') =>
                {
                    return Err(format!("widget '{}' contains invalid text", widget.id));
                }
                EffectParamValue::Float(value)
                    if key == "scale_y" && !valid_scale(*value as f32) =>
                {
                    return Err(format!("widget '{}' scale_y out of range", widget.id));
                }
                EffectParamValue::Float(value) if !value.is_finite() => {
                    return Err(format!(
                        "widget '{}' contains a non-finite number",
                        widget.id
                    ));
                }
                EffectParamValue::Steps(steps)
                    if steps.iter().any(|step| !step.value.is_finite()) =>
                {
                    return Err(format!("widget '{}' contains an invalid step", widget.id));
                }
                _ => {}
            }
        }
    }
    if let BgKind::Image { filename, dim } = &def.style.background {
        if !filename.is_empty() && crate::types::validate_image_filename(filename).is_err() {
            return Err("background image filename is invalid".to_owned());
        }
        if !dim.is_finite() || !(0.0..=100.0).contains(dim) {
            return Err("background image dim must be between 0 and 100".to_owned());
        }
    }
    Ok(())
}

fn valid_catalog_id(value: &str) -> bool {
    let Some((plugin, item)) = value.split_once(':') else {
        return false;
    };
    !plugin.is_empty()
        && !item.is_empty()
        && value.len() <= MAX_WIDGET_TEXT_BYTES
        && !value.contains('\0')
        && !item.contains(':')
}

fn valid_scale(value: f32) -> bool {
    value.is_finite() && value > 0.0 && value <= 100.0
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_layout_is_plugin_only_and_round_trips() {
        let def = CustomTemplateDef::default();
        assert_eq!(def.widgets[0].widget, "halo_lcd:logo");
        let json = serde_json::to_string(&def).unwrap();
        assert_eq!(
            serde_json::from_str::<CustomTemplateDef>(&json).unwrap(),
            def
        );
    }

    #[test]
    fn missing_plugin_references_are_structurally_valid() {
        let mut def = CustomTemplateDef::default();
        def.widgets[0].widget = "not_installed:anything".to_owned();
        assert!(validate_widgets(&def).is_ok());
        def.widgets[0].widget = "not-namespaced".to_owned();
        assert!(validate_widgets(&def).is_err());
    }

    #[test]
    fn duplicate_ids_and_non_finite_geometry_are_rejected() {
        let mut def = CustomTemplateDef::default();
        def.widgets.push(def.widgets[0].clone());
        assert!(validate_widgets(&def).is_err());
        def.widgets.pop();
        def.widgets[0].x = f32::NAN;
        assert!(validate_widgets(&def).is_err());
    }
}
