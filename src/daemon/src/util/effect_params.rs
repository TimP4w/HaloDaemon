// SPDX-License-Identifier: GPL-3.0-or-later
use std::collections::HashMap;

use halod_shared::types::{ColorStep, EffectParamValue, RgbColor};

pub(crate) fn param_color(
    params: &HashMap<String, EffectParamValue>,
    id: &str,
    default: RgbColor,
) -> RgbColor {
    match params.get(id) {
        Some(EffectParamValue::Color(c)) => *c,
        _ => default,
    }
}

pub(crate) fn param_f64(params: &HashMap<String, EffectParamValue>, id: &str, default: f64) -> f64 {
    match params.get(id) {
        Some(EffectParamValue::Float(f)) => *f,
        _ => default,
    }
}

pub(crate) fn param_str(
    params: &HashMap<String, EffectParamValue>,
    id: &str,
    default: &str,
) -> String {
    match params.get(id) {
        Some(EffectParamValue::Str(s)) => s.clone(),
        _ => default.to_string(),
    }
}

pub(crate) fn param_bool(
    params: &HashMap<String, EffectParamValue>,
    id: &str,
    default: bool,
) -> bool {
    match params.get(id) {
        Some(EffectParamValue::Bool(b)) => *b,
        _ => default,
    }
}

pub(crate) fn param_steps(
    params: &HashMap<String, EffectParamValue>,
    id: &str,
    default: &[ColorStep],
) -> Vec<ColorStep> {
    match params.get(id) {
        Some(EffectParamValue::Steps(s)) if !s.is_empty() => s.clone(),
        _ => default.to_vec(),
    }
}
