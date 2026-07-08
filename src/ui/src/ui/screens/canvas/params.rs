// SPDX-License-Identifier: GPL-3.0-or-later
//! Effect-instance command builders and per-effect param editing widgets.

use std::collections::HashMap;

use halod_shared::{
    commands::DaemonCommand,
    types::{
        EffectDef, EffectParamDescriptor, EffectParamValue, ParamKind, PlacedZone, RgbColor,
        SamplingMode,
    },
};

use crate::runtime::ipc::CommandTx;
use crate::ui::components as widgets;
use crate::ui::theme;

use super::CanvasUi;

pub(super) fn upsert_instance_cmd(
    instance_id: String,
    effect_id: String,
    name: Option<String>,
    params: HashMap<String, EffectParamValue>,
) -> DaemonCommand {
    DaemonCommand::CanvasUpsertEffect {
        instance_id,
        def: EffectDef {
            effect_id,
            name,
            params,
        },
    }
}

/// Merge an instance's stored params over the effect's declared defaults.
pub(super) fn build_instance_params(
    eff: &halod_shared::types::Animation,
    def: &EffectDef,
    instance_id: &str,
    canvas_ui: &CanvasUi,
) -> HashMap<String, EffectParamValue> {
    let mut params: HashMap<String, EffectParamValue> = eff
        .params
        .iter()
        .map(|p| (p.id.clone(), p.default.clone()))
        .collect();
    for (k, v) in &def.params {
        params.insert(k.clone(), v.clone());
    }
    if let Some(edits) = canvas_ui.param_edits.get(instance_id) {
        for (k, v) in edits {
            params.insert(k.clone(), v.clone());
        }
    }
    params
}

/// Send a `CanvasMoveZone` toggling the zone's `sampling_mode`.
pub(super) fn unroll_cmd(cmd: &CommandTx, zone: &PlacedZone, currently_unrolled: bool) {
    crate::domain::actions::canvas::send(
        cmd,
        DaemonCommand::CanvasMoveZone {
            device_id: zone.device_id.clone(),
            zone_id: zone.zone_id.clone(),
            x: zone.x as f64,
            y: zone.y as f64,
            w: None,
            h: None,
            rotation: None,
            effect: None,
            sampling_mode: Some(if currently_unrolled {
                SamplingMode::Spatial
            } else {
                SamplingMode::Unrolled
            }),
        },
    );
}

/// Toggle a zone's membership in an instance (`None` = follow the canvas default).
pub(super) fn assign_zone_cmd(
    device_id: &str,
    zone_id: &str,
    assign: Option<&str>,
    on: bool,
    placed: Option<&PlacedZone>,
) -> DaemonCommand {
    let effect = assign.map(str::to_string);
    if on {
        DaemonCommand::CanvasRemoveZone {
            device_id: device_id.to_string(),
            zone_id: zone_id.to_string(),
        }
    } else if let Some(p) = placed {
        // MoveZone treats `effect: None` as "leave unchanged"; clearing needs PlaceZone.
        match effect {
            Some(_) => DaemonCommand::CanvasMoveZone {
                device_id: device_id.to_string(),
                zone_id: zone_id.to_string(),
                x: p.x as f64,
                y: p.y as f64,
                w: Some(p.w as f64),
                h: Some(p.h as f64),
                rotation: Some(p.rotation as f64),
                effect,
                sampling_mode: None,
            },
            None => DaemonCommand::CanvasPlaceZone {
                device_id: device_id.to_string(),
                zone_id: zone_id.to_string(),
                x: Some(p.x as f64),
                y: Some(p.y as f64),
                w: Some(p.w as f64),
                h: Some(p.h as f64),
                rotation: Some(p.rotation as f64),
                effect: None,
                sampling_mode: None,
            },
        }
    } else {
        DaemonCommand::CanvasPlaceZone {
            device_id: device_id.to_string(),
            zone_id: zone_id.to_string(),
            x: None,
            y: None,
            w: None,
            h: None,
            rotation: None,
            effect,
            sampling_mode: None,
        }
    }
}

/// Translated display label for a canvas-effect param, keyed by its `id`
/// (stable across effects — see `daemon::engines::rgb_engine::canvas::effects`).
fn effect_param_label(param: &EffectParamDescriptor) -> String {
    t!(format!("canvas.effect_param_{}", param.id)).to_string()
}

/// Translated display text for an enum option. `monitor` options are
/// OS-provided monitor names and render as-is; `fill` has real display words.
fn effect_enum_option_label(param_id: &str, option: &str) -> String {
    if param_id == "fill" {
        t!(format!("canvas.effect_enum_fill.{option}")).to_string()
    } else {
        option.to_string()
    }
}

/// Editable params for one instance; returns `true` when any changed. Edits are
/// buffered in `param_edits[instance_id]` and flushed by the caller (debounced).
pub(super) fn instance_params(
    ui: &mut egui::Ui,
    canvas_ui: &mut CanvasUi,
    instance_id: &str,
    def: &EffectDef,
    eff: &halod_shared::types::Animation,
) -> bool {
    if eff.params.is_empty() {
        return false;
    }
    let edits = canvas_ui
        .param_edits
        .entry(instance_id.to_string())
        .or_default();
    let mut changed = false;
    for param in &eff.params {
        let current = edits
            .get(&param.id)
            .or_else(|| def.params.get(&param.id))
            .cloned()
            .unwrap_or_else(|| param.default.clone());
        match &param.kind {
            ParamKind::Range { min, max, step } => {
                let mut val = match &current {
                    EffectParamValue::Float(v) => *v as f32,
                    _ => *min as f32,
                };
                ui.add_space(6.0);
                let readout = format!("{val:.0}%");
                if widgets::slider_row(
                    ui,
                    &effect_param_label(param),
                    &mut val,
                    *min as f32..=*max as f32,
                    &readout,
                ) {
                    let step = *step as f32;
                    let snapped = if step > 0.0 {
                        (val / step).round() * step
                    } else {
                        val
                    };
                    edits.insert(param.id.clone(), EffectParamValue::Float(snapped as f64));
                    changed = true;
                }
            }
            ParamKind::Enum { options } => {
                let selected = match &current {
                    EffectParamValue::Str(s) => s.clone(),
                    _ => options.first().cloned().unwrap_or_default(),
                };
                ui.add_space(6.0);
                ui.horizontal_wrapped(|ui| {
                    for opt in options {
                        let label = effect_enum_option_label(&param.id, opt);
                        if widgets::pill(ui, &label, *opt == selected) {
                            edits.insert(param.id.clone(), EffectParamValue::Str(opt.clone()));
                            changed = true;
                        }
                    }
                });
            }
            ParamKind::Color => {
                let color = match &current {
                    EffectParamValue::Color(c) => *c,
                    _ => RgbColor {
                        r: 90,
                        g: 209,
                        b: 232,
                    },
                };
                ui.add_space(6.0);
                if let Some(new_color) = widgets::color_picker(ui, color) {
                    edits.insert(param.id.clone(), EffectParamValue::Color(new_color));
                    changed = true;
                }
            }
            ParamKind::Boolean => {
                let on = matches!(&current, EffectParamValue::Bool(true));
                ui.add_space(4.0);
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(effect_param_label(param))
                                .font(theme::body(11.5))
                                .color(theme::TEXT_DIM),
                        );
                    },
                    |ui| {
                        if super::rack::toggle_switch(
                            ui,
                            on,
                            egui::Id::new(("inst_bool", instance_id, &param.id)),
                        ) {
                            edits.insert(param.id.clone(), EffectParamValue::Bool(!on));
                            changed = true;
                        }
                    },
                );
            }
            ParamKind::Text
            | ParamKind::Sensor
            | ParamKind::Image
            | ParamKind::Number { .. }
            | ParamKind::Steps => {}
        }
    }
    changed
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_canvas_effect_param_and_enum_option_is_translated() {
        // Mirrors the ids emitted by
        // `daemon::engines::rgb_engine::canvas::effects::all_descriptors()`
        // (Range/Boolean params only — Color params render no label, and
        // `monitor`'s enum options are OS-provided device names, not copy).
        for id in [
            "speed",
            "scale",
            "thickness",
            "cells",
            "interval",
            "decay",
            "random_color",
        ] {
            let param = EffectParamDescriptor {
                id: id.to_string(),
                label: String::new(),
                kind: ParamKind::Boolean,
                default: EffectParamValue::Bool(false),
            };
            assert_ne!(
                effect_param_label(&param),
                format!("canvas.effect_param_{id}"),
                "missing param label for {id:?}"
            );
        }
        for option in ["bars", "solid"] {
            assert_ne!(
                effect_enum_option_label("fill", option),
                format!("canvas.effect_enum_fill.{option}"),
                "missing enum option translation for fill/{option}"
            );
        }
        // Non-translatable pass-through, not an i18n key.
        assert_eq!(
            effect_enum_option_label("monitor", "Display 1"),
            "Display 1"
        );
    }

    #[test]
    fn upsert_instance_cmd_carries_name() {
        let cmd = upsert_instance_cmd(
            "effect-1".into(),
            "static_color".into(),
            Some("Desk glow".into()),
            HashMap::new(),
        );
        match cmd {
            DaemonCommand::CanvasUpsertEffect { instance_id, def } => {
                assert_eq!(instance_id, "effect-1");
                assert_eq!(def.name.as_deref(), Some("Desk glow"));
            }
            other => panic!("wrong command: {other:?}"),
        }
    }

    #[test]
    fn build_instance_params_layers_defaults_stored_and_edits() {
        use halod_shared::types::{Animation, EffectParamDescriptor, ParamKind};
        let eff = Animation {
            id: "e".into(),
            name: "E".into(),
            params: vec![
                EffectParamDescriptor {
                    id: "a".into(),
                    label: "A".into(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.1,
                    },
                    default: EffectParamValue::Float(0.0),
                },
                EffectParamDescriptor {
                    id: "b".into(),
                    label: "B".into(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 1.0,
                        step: 0.1,
                    },
                    default: EffectParamValue::Float(0.0),
                },
            ],
        };
        let def = EffectDef {
            effect_id: "e".into(),
            name: None,
            params: [("a".to_string(), EffectParamValue::Float(0.5))]
                .into_iter()
                .collect(),
        };
        let mut ui = CanvasUi::default();
        ui.param_edits.insert(
            "inst".into(),
            [("b".to_string(), EffectParamValue::Float(0.9))]
                .into_iter()
                .collect(),
        );
        let out = build_instance_params(&eff, &def, "inst", &ui);
        // stored def value for `a`, live edit for `b`.
        assert_eq!(out["a"], EffectParamValue::Float(0.5));
        assert_eq!(out["b"], EffectParamValue::Float(0.9));
    }
}
