// SPDX-License-Identifier: GPL-3.0-or-later
//! The unified RGB Lighting view — a shared header (title + canvas transport
//! chrome) over a tab switcher between two sub-views:
//!
//! - **Effects Canvas** ([`crate::ui::screens::canvas`]): the live canvas stage
//!   with draggable zone placement and per-instance effects.
//! - **Direct Effects** (this module): apply one color or effect to every
//!   device at once — a master color card (shown only for Static or effects
//!   with a `Color` param), an effect grid, brightness/saturation sliders, and
//!   a target devices & channels picker.

use crate::ui::components as widgets;
use std::collections::HashMap;

use crate::domain::topic_store::TopicStore;
use halod_shared::commands::{DaemonCommand, DeviceOp};
use halod_shared::effect_designer::DESIGNER_EFFECT_ID;
use halod_shared::types::{
    Animation, CanvasFrame, ColorStep, DeviceCapability, EffectDef, EffectParamDescriptor,
    EffectParamValue, LightingState, ParamKind, RgbColor, WireDevice,
};

use crate::domain::models::device as model;
use crate::domain::state::Page;
use crate::domain::tour;
use crate::domain::tour::AnchorId;
use crate::runtime::ipc::CommandTx;
use crate::ui::components::rgb_to_color32;
use crate::ui::screens::canvas::{self, CanvasUi};
use crate::ui::screens::effect_designer::{self, DesignerUi};
use crate::ui::theme;
use halod_shared::ids::DeviceId;

/// Synthetic grid id prefix for a saved custom effect: `"custom:<name>"`.
const CUSTOM_PREFIX: &str = "custom:";

pub(crate) const TAB_CANVAS: usize = 0;

// ── State ─────────────────────────────────────────────────────────────────────

pub struct LightingUi {
    pub tab: usize,
    pub color: RgbColor,
    /// Active effect id. Empty string = Static (LightingState::Static).
    pub effect: String,
    pub brightness: f32,
    pub saturation: f32,
    /// Live Range param values, keyed `"{effect_id}:{param_id}"`.
    param_values: HashMap<String, f32>,
    /// Live Sensor/Enum param values, keyed the same way as `param_values`.
    param_strs: HashMap<String, String>,
    /// Live non-master Color param values (any `Color` param id other than
    /// `"color"`, which is bound to the master swatch instead).
    param_colors: HashMap<String, RgbColor>,
    /// Live Steps param values, keyed the same way as `param_values`.
    param_steps: HashMap<String, Vec<ColorStep>>,
    color_slot: usize,
    sel_ids: Vec<DeviceId>,
    zone_sel: HashMap<DeviceId, Vec<String>>,
    expanded_id: Option<DeviceId>,
    seeded_for: Option<String>,
    /// Name of a custom effect pending delete confirmation.
    confirm_delete: Option<String>,
    apply_deadline: Option<f64>,
}

impl Default for LightingUi {
    fn default() -> Self {
        Self {
            tab: TAB_CANVAS,
            color: RgbColor {
                r: 0x38,
                g: 0xbd,
                b: 0xf8,
            },
            effect: String::new(), // Static
            brightness: 80.0,
            saturation: 100.0,
            param_values: HashMap::new(),
            param_strs: HashMap::new(),
            param_colors: HashMap::new(),
            param_steps: HashMap::new(),
            color_slot: 0,
            sel_ids: Vec::new(),
            zone_sel: HashMap::new(),
            expanded_id: None,
            seeded_for: None,
            confirm_delete: None,
            apply_deadline: None,
        }
    }
}

// ── Entry point ───────────────────────────────────────────────────────────────

pub fn show(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut LightingUi,
    canvas_ui: &mut CanvasUi,
    designer_ui: &mut DesignerUi,
    canvas_frame: Option<&CanvasFrame>,
    time: f64,
    page: &mut Page,
) {
    if st.tab != TAB_CANVAS {
        seed_if_profile_changed(st, state);
        if let Some((name, params)) = designer_ui.result.take() {
            st.effect = format!("{CUSTOM_PREFIX}{name}");
            apply_direct_params(state, cmd, st, params);
        }
    }

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| {
            if st.tab == TAB_CANVAS {
                widgets::page_frame(ui, |ui| {
                    header(ui, state, cmd, st, canvas_ui);
                    canvas::body(
                        ui,
                        state,
                        cmd,
                        canvas_ui,
                        canvas_frame,
                        time,
                        designer_ui,
                        page,
                    );
                });
            } else {
                egui::ScrollArea::vertical()
                    .auto_shrink([false, false])
                    .show(ui, |ui| {
                        widgets::page_frame(ui, |ui| {
                            header(ui, state, cmd, st, canvas_ui);
                            page_body(ui, state, cmd, st, designer_ui, page);
                        });
                    });
                delete_confirm_modal(ui, st, cmd);
            }
        });

    if canvas_ui.fps_modal_open {
        canvas::fps_modal(ui.ctx(), state, canvas_ui, time);
    }

    // The FPS modal (opened from the transport chrome) queues a debounced
    // command, but only the Canvas tab's `body` flushes the queue. Drain it here
    // so setting FPS works from the Direct tab too, and keep repainting until
    // the trailing flush lands.
    if canvas_ui.flush_pending(cmd, time) {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(120));
    }
}

fn header(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut LightingUi,
    canvas_ui: &mut CanvasUi,
) {
    widgets::page_title_with(
        ui,
        &t!("lighting.title"),
        |ui| {
            ui.add_space(theme::SPACE_4);
            let labels = [t!("lighting.tab_canvas"), t!("lighting.tab_direct")];
            let refs: Vec<&str> = labels.iter().map(|c| c.as_ref()).collect();
            widgets::segmented_styled(
                ui,
                &refs,
                &mut st.tab,
                false,
                widgets::SegmentedStyle {
                    raised: true,
                    md: true,
                },
            );
            if st.tab != TAB_CANVAS {
                ui.add_space(theme::SPACE_4);
                ui.label(
                    egui::RichText::new(t!("lighting.subtitle"))
                        .font(theme::body_lg())
                        .color(theme::TEXT_DIM),
                );
            }
        },
        |ui| canvas::chrome(ui, canvas_ui, cmd, state.lighting.config.canvas_enabled),
    );
}

/// Seed the selection from the active profile's saved targets; reseed on
/// profile switch. Offline devices stay in the saved selection so reconnecting
/// restores them.
fn seed_if_profile_changed(st: &mut LightingUi, state: &TopicStore) {
    if st.seeded_for.as_deref() != Some(state.profiles.active.as_str()) {
        st.sel_ids = state.lighting.targets.device_ids.clone();
        st.zone_sel = state.lighting.targets.channels.clone();
        st.seeded_for = Some(state.profiles.active.clone());
    }
}

// ── Page body ─────────────────────────────────────────────────────────────────

fn page_body(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut LightingUi,
    designer_ui: &mut DesignerUi,
    page: &mut Page,
) {
    let effects = available_effects(
        &state.lighting.canvas.available_direct_effects,
        &state.lighting.canvas.custom_direct_effects,
    );

    let selected_anim = state
        .lighting
        .canvas
        .available_direct_effects
        .iter()
        .find(|a| a.id == st.effect);
    let effect_name = effects
        .iter()
        .find(|(id, _)| *id == st.effect)
        .map(|(_, name)| name.clone())
        .unwrap_or_default();

    let custom = &state.lighting.canvas.custom_direct_effects;
    let main_w = (ui.available_width() - 376.0 - 16.0).max(360.0);
    let (apply, apply_now) = widgets::split_columns(ui, main_w, 16.0, |main, aside| {
        let mut changed = global_effect_card(main, st, &effects, cmd, designer_ui, page);
        effect_actions_row(main, st, designer_ui, page, custom);
        if show_color(&st.effect, selected_anim) {
            main.add_space(theme::SPACE_8);
            changed |= master_color_card(main, st);
        }
        changed |= effect_settings_card(main, state, st, selected_anim, &effect_name);
        let (targets_changed, apply_now) = targets_card(aside, state, cmd, st);
        (changed | targets_changed, apply_now)
    });

    let now = ui.input(|i| i.time);
    if apply_now || debounce_apply(&mut st.apply_deadline, apply, now) {
        st.apply_deadline = None;
        send_to_selected(state, cmd, st);
    } else if st.apply_deadline.is_some() {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_secs_f64(APPLY_DEBOUNCE));
    }
}

const APPLY_DEBOUNCE: f64 = 0.12;

fn debounce_apply(deadline: &mut Option<f64>, apply: bool, now: f64) -> bool {
    if apply {
        *deadline = Some(now + APPLY_DEBOUNCE);
    }
    match *deadline {
        Some(d) if now >= d => {
            *deadline = None;
            true
        }
        _ => false,
    }
}

/// Every sensor across all devices as `(id, display name)` pairs, for the
/// `ParamKind::Sensor` picker.
pub(crate) fn sensor_options(state: &TopicStore) -> Vec<(String, String)> {
    crate::domain::models::sensors::sensors(state)
        .into_iter()
        .map(|s| {
            let label = format!("{} ({:.0}{})", s.label, s.value, s.unit);
            (s.id, label)
        })
        .collect()
}

// ── Available effects ─────────────────────────────────────────────────────────

fn available_effects(direct: &[Animation], custom: &[EffectDef]) -> Vec<(String, String)> {
    let mut v = vec![(String::new(), t!("lighting.static").to_string())];
    // The raw `designer` id is a parameter bag, not a user-facing grid
    // entry — only its saved custom instances (below) belong in the grid.
    v.extend(
        direct
            .iter()
            .filter(|a| a.id != DESIGNER_EFFECT_ID)
            .map(|a| (a.id.clone(), a.name.clone())),
    );
    v.extend(custom.iter().filter_map(|d| {
        let name = d.name.as_ref()?;
        Some((format!("{CUSTOM_PREFIX}{name}"), name.clone()))
    }));
    v
}

/// The saved custom effect currently selected in the grid, if any.
fn selected_custom<'a>(st: &LightingUi, custom: &'a [EffectDef]) -> Option<&'a EffectDef> {
    let name = st.effect.strip_prefix(CUSTOM_PREFIX)?;
    custom.iter().find(|d| d.name.as_deref() == Some(name))
}

// ── Effect Designer actions row ──────────────────────────────────────────────

/// The card shown below the effect grid: edit/export/delete for the selected
/// custom effect. Returns `false` (these actions don't need a re-apply).
fn effect_actions_row(
    ui: &mut egui::Ui,
    st: &mut LightingUi,
    designer_ui: &mut DesignerUi,
    page: &mut Page,
    custom: &[EffectDef],
) {
    let Some(def) = selected_custom(st, custom) else {
        return;
    };
    ui.add_space(theme::SPACE_5);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if widgets::button(
            ui,
            &t!("lighting.edit"),
            widgets::ButtonKind::Secondary,
            egui::vec2(64.0, 28.0),
        )
        .clicked()
        {
            *designer_ui = DesignerUi::edit(def);
            *page = Page::EffectDesigner;
        }
        if widgets::button(
            ui,
            &t!("lighting.export"),
            widgets::ButtonKind::Secondary,
            egui::vec2(72.0, 28.0),
        )
        .clicked()
        {
            effect_designer::spawn_export(ui.ctx(), def);
        }
        if widgets::button(
            ui,
            &t!("lighting.delete"),
            widgets::ButtonKind::Secondary,
            egui::vec2(72.0, 28.0),
        )
        .clicked()
        {
            st.confirm_delete = def.name.clone();
        }
    });
}

// ── Master color card ─────────────────────────────────────────────────────────

fn master_color_card(ui: &mut egui::Ui, st: &mut LightingUi) -> bool {
    let mut changed = false;
    widgets::card_titled(
        ui,
        &t!("lighting.master_color"),
        |_ui| {},
        |ui| {
            // The shared colour picker — same component used by the device
            // Lighting tab and the Effects Canvas.
            if let Some(c) = widgets::color_picker(ui, st.color) {
                st.color = c;
                changed = true;
            }
        },
    );
    changed
}

// ── Global effect card ────────────────────────────────────────────────────────

fn global_effect_card(
    ui: &mut egui::Ui,
    st: &mut LightingUi,
    effects: &[(String, String)],
    cmd: &CommandTx,
    designer_ui: &mut DesignerUi,
    page: &mut Page,
) -> bool {
    let mut changed = false;
    widgets::card_titled(
        ui,
        &t!("lighting.global_effect"),
        |ui| {
            ui.spacing_mut().item_spacing.x = theme::SPACE_3;
            let size = egui::vec2(0.0, 30.0);
            let new_btn = widgets::button(
                ui,
                &t!("lighting.new_effect"),
                widgets::ButtonKind::Secondary,
                size,
            );
            tour::anchor(ui.ctx(), AnchorId::LightingNewEffect, new_btn.rect);
            if new_btn.clicked() {
                *designer_ui = DesignerUi::new_effect();
                *page = Page::EffectDesigner;
            }
            let import_btn = widgets::button(
                ui,
                &t!("lighting.import"),
                widgets::ButtonKind::Secondary,
                size,
            );
            tour::anchor(ui.ctx(), AnchorId::LightingImport, import_btn.rect);
            if import_btn.clicked() {
                effect_designer::spawn_import(ui.ctx(), cmd.clone());
            }
        },
        |ui| {
            let gap = 10.0;
            let cols = 4usize;
            let cell_w =
                ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).max(60.0);

            for (row, chunk) in effects.chunks(cols).enumerate() {
                if row > 0 {
                    ui.add_space(gap);
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (id, name) in chunk {
                        let active = &st.effect == id;
                        let preview = if id.is_empty() {
                            widgets::CellPreview::Solid(rgb_to_color32(st.color))
                        } else {
                            widgets::CellPreview::Spectrum
                        };
                        let note = effect_source(id);
                        if widgets::effect_cell_noted(
                            ui,
                            name,
                            Some(&note),
                            active,
                            preview,
                            cell_w,
                            76.0,
                            28.0,
                        ) {
                            st.effect = id.clone();
                            changed = true;
                        }
                    }
                });
            }
            tour::anchor(ui.ctx(), AnchorId::LightingEffects, ui.min_rect());
        },
    );
    changed
}

/// Where a grid entry comes from: built in, a direct effect, or the user's own.
fn effect_source(id: &str) -> std::borrow::Cow<'static, str> {
    if id.is_empty() {
        t!("lighting.source_builtin")
    } else if id.starts_with(CUSTOM_PREFIX) {
        t!("lighting.source_mine")
    } else {
        t!("lighting.source_effects")
    }
}

// ── Effect params card ────────────────────────────────────────────────────────

/// Whether the master colour picker applies: Static always, or a direct
/// effect that declares a `Color` param named `"color"`. Effects with more
/// than one colour (e.g. a two-stop gradient) render each individually in
/// [`effect_params_card`] instead of sharing the master swatch.
fn show_color(effect: &str, anim: Option<&Animation>) -> bool {
    effect.is_empty()
        || anim.is_some_and(|a| {
            a.params
                .iter()
                .any(|p| p.id == "color" && matches!(p.kind, ParamKind::Color))
        })
}

/// Whether a param descriptor gets its own row in [`effect_params_card`].
/// `Color` is skipped when its id is `"color"` — that one is edited via the
/// shared master swatch (per [`show_color`]); any other `Color` param (e.g. a
/// two-stop gradient's second colour) gets its own inline picker.
fn wants_own_row(d: &EffectParamDescriptor) -> bool {
    match &d.kind {
        ParamKind::Range { .. }
        | ParamKind::Number { .. }
        | ParamKind::Boolean
        | ParamKind::Sensor
        | ParamKind::Steps
        | ParamKind::Gradient
        | ParamKind::Enum { .. } => true,
        ParamKind::Color => d.id != "color",
        ParamKind::Text | ParamKind::Image => false,
    }
}

/// The settings card below the effect grid: master brightness/saturation for
/// Static, else the selected effect's params, in two columns. Returns `true`
/// on change.
fn effect_settings_card(
    ui: &mut egui::Ui,
    state: &TopicStore,
    st: &mut LightingUi,
    anim: Option<&Animation>,
    effect_name: &str,
) -> bool {
    let params: Vec<&EffectParamDescriptor> = anim
        .map(|a| a.params.iter().filter(|d| wants_own_row(d)).collect())
        .unwrap_or_default();
    if !st.effect.is_empty() && params.is_empty() {
        return false;
    }
    let sensors = sensor_options(state);
    let mut changed = false;
    ui.add_space(theme::SPACE_8);
    widgets::card_titled(
        ui,
        &t!("lighting.effect_settings_for", name = effect_name),
        |_ui| {},
        |ui| {
            ui.spacing_mut().item_spacing.x = 28.0;
            ui.columns(2, |cols| {
                let Some(anim) = anim.filter(|_| !st.effect.is_empty()) else {
                    changed |= percent_slider(
                        &mut cols[0],
                        &t!("lighting.master_brightness"),
                        &mut st.brightness,
                    );
                    changed |= percent_slider(
                        &mut cols[1],
                        &t!("lighting.saturation"),
                        &mut st.saturation,
                    );
                    return;
                };
                for (i, item) in settings_items(&params).into_iter().enumerate() {
                    let col = &mut cols[i % 2];
                    if i >= 2 {
                        col.add_space(theme::SPACE_7);
                    }
                    changed |= match item {
                        SettingsItem::Param(d) => param_control(col, st, &anim.id, d, &sensors),
                        SettingsItem::Colors(colors) => color_group(col, st, &anim.id, &colors),
                    };
                }
            });
        },
    );
    changed
}

enum SettingsItem<'a> {
    Param(&'a EffectParamDescriptor),
    Colors(Vec<&'a EffectParamDescriptor>),
}

/// The settings grid's cells: every colour param shares one slot-picker cell
/// at the first colour's position; the rest keep their own cell.
fn settings_items<'a>(params: &[&'a EffectParamDescriptor]) -> Vec<SettingsItem<'a>> {
    let colors: Vec<&EffectParamDescriptor> = params
        .iter()
        .copied()
        .filter(|d| matches!(d.kind, ParamKind::Color))
        .collect();
    let mut items = Vec::new();
    let mut colors = Some(colors).filter(|c| !c.is_empty());
    for d in params {
        if matches!(d.kind, ParamKind::Color) {
            if let Some(group) = colors.take() {
                items.push(SettingsItem::Colors(group));
            }
        } else {
            items.push(SettingsItem::Param(d));
        }
    }
    items
}

fn color_group(
    ui: &mut egui::Ui,
    st: &mut LightingUi,
    anim_id: &str,
    params: &[&EffectParamDescriptor],
) -> bool {
    let current: Vec<(&str, RgbColor)> = params
        .iter()
        .map(|d| {
            let default = match d.default {
                EffectParamValue::Color(c) => c,
                _ => RgbColor::default(),
            };
            let key = format!("{anim_id}:{}", d.id);
            (
                d.label.as_str(),
                st.param_colors.get(&key).copied().unwrap_or(default),
            )
        })
        .collect();
    match widgets::color_slots(ui, &t!("canvas.colors"), &current, &mut st.color_slot) {
        Some((i, c)) => {
            st.param_colors
                .insert(format!("{anim_id}:{}", params[i].id), c);
            true
        }
        None => false,
    }
}

fn percent_slider(ui: &mut egui::Ui, label: &str, v: &mut f32) -> bool {
    let readout = format!("{}%", v.round() as u32);
    widgets::slider_row_debounced(ui, label, v, 0.0..=100.0, &readout)
}

/// One effect param's control. Returns `true` when its value changed.
fn param_control(
    ui: &mut egui::Ui,
    st: &mut LightingUi,
    anim_id: &str,
    d: &EffectParamDescriptor,
    sensors: &[(String, String)],
) -> bool {
    let key = format!("{anim_id}:{}", d.id);
    let mut changed = false;
    match &d.kind {
        ParamKind::Range { min, max, step } => {
            let (min, max, step) = (*min, *max, *step);
            let default = match d.default {
                EffectParamValue::Float(f) => f as f32,
                _ => min as f32,
            };
            let mut v = *st.param_values.get(&key).unwrap_or(&default);
            let readout = widgets::range_readout(v, step);
            if widgets::slider_row_debounced(
                ui,
                &d.label,
                &mut v,
                min as f32..=max as f32,
                &readout,
            ) {
                v = widgets::snap_to_step(v, min as f32, max as f32, step as f32);
                changed = true;
            }
            st.param_values.insert(key, v);
        }
        ParamKind::Number { min, max } => {
            let default = match d.default {
                EffectParamValue::Float(f) => f as f32,
                _ => 0.0,
            };
            let mut v = *st.param_values.get(&key).unwrap_or(&default);
            let (_, committed) =
                widgets::num_input_row(ui, &d.label, &mut v, *min as f32..=*max as f32);
            changed |= committed;
            st.param_values.insert(key, v);
        }
        ParamKind::Boolean => {
            let default = param_bool_default(d);
            let on = st.param_values.get(&key).map_or(default, |&v| v != 0.0);
            let mut next = on;
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(&d.label)
                            .font(theme::body_md())
                            .color(theme::TEXT),
                    );
                },
                |ui| next = widgets::toggle(ui, on),
            );
            changed = next != on;
            st.param_values.insert(key, next as i32 as f32);
        }
        ParamKind::Steps | ParamKind::Gradient => {
            let default = widgets::steps_default(d);
            let steps = st.param_steps.entry(key).or_insert(default);
            let fill = widgets::StopFill::of(&d.kind);
            changed |= widgets::steps_editor(ui, &d.label, steps, fill);
        }
        ParamKind::Sensor => {
            let current = st.param_strs.get(&key).cloned().unwrap_or_default();
            changed |= widgets::combo_param_row(
                ui,
                &d.label,
                key,
                &mut st.param_strs,
                current,
                sensors,
                Some(t!("lighting.none").as_ref()),
            );
        }
        ParamKind::Enum { options } => {
            let current = st
                .param_strs
                .get(&key)
                .cloned()
                .unwrap_or_else(|| match &d.default {
                    EffectParamValue::Str(s) => s.clone(),
                    _ => options.first().cloned().unwrap_or_default(),
                });
            if options.len() <= ENUM_SEGMENTED_MAX {
                ui.label(
                    egui::RichText::new(&d.label)
                        .font(theme::body_md())
                        .color(theme::TEXT),
                );
                ui.add_space(theme::SPACE_4);
                let refs: Vec<&str> = options.iter().map(String::as_str).collect();
                let mut idx = options.iter().position(|o| *o == current).unwrap_or(0);
                if widgets::segmented(ui, &refs, &mut idx, false) {
                    st.param_strs.insert(key, options[idx].clone());
                    changed = true;
                }
            } else {
                let opts: Vec<(String, String)> =
                    options.iter().map(|o| (o.clone(), o.clone())).collect();
                changed |= widgets::combo_param_row(
                    ui,
                    &d.label,
                    key,
                    &mut st.param_strs,
                    current,
                    &opts,
                    None,
                );
            }
        }
        ParamKind::Color | ParamKind::Text | ParamKind::Image => {}
    }
    changed
}

/// Enum params with at most this many options render as a segmented control.
const ENUM_SEGMENTED_MAX: usize = 4;

fn param_bool_default(d: &EffectParamDescriptor) -> bool {
    matches!(d.default, EffectParamValue::Bool(true))
}

// ── Target devices & channels card ───────────────────────────────────────────

/// The target picker: one checkbox row per RGB device (a selected device's row
/// expands to its zone pills), a warning for devices the canvas drives, and the
/// apply button. Returns `(selection changed, apply now)`.
fn targets_card(
    ui: &mut egui::Ui,
    state: &TopicStore,
    cmd: &CommandTx,
    st: &mut LightingUi,
) -> (bool, bool) {
    let mut apply = false;
    let mut apply_now = false;
    let mut persist = false;
    let devices: Vec<&WireDevice> = rgb_devices(state).collect();
    widgets::card(ui, |ui| {
        egui::Sides::new().shrink_left().spacing(12.0).show(
            ui,
            |ui| {
                ui.label(
                    egui::RichText::new(t!("lighting.target_devices_zones"))
                        .font(theme::heading())
                        .color(theme::TEXT),
                );
            },
            |ui| {
                let all: Vec<DeviceId> = devices.iter().map(|d| d.id.clone()).collect();
                let everything = !all.is_empty() && all.iter().all(|id| st.sel_ids.contains(id));
                let label = if everything {
                    t!("lighting.deselect_all")
                } else {
                    t!("lighting.select_all")
                };
                if widgets::text_link(ui, &label).clicked() {
                    if everything {
                        for id in st.sel_ids.drain(..) {
                            release_device(cmd, &id);
                        }
                    } else {
                        st.sel_ids = all;
                        apply = true;
                    }
                    persist = true;
                }
            },
        );
        ui.add_space(theme::SPACE_3);
        ui.label(
            egui::RichText::new(t!("lighting.targets_subtitle"))
                .font(theme::body_sm())
                .color(theme::TEXT_MUT),
        );
        ui.add_space(theme::SPACE_7);

        for dev in &devices {
            let sel = st.sel_ids.contains(&dev.id);
            let expanded = st.expanded_id.as_deref() == Some(dev.id.as_str());
            let r = device_row(ui, dev, sel, expanded, st);
            if r.toggled {
                toggle_device(st, &dev.id);
                persist = true;
                apply = true;
                if sel {
                    release_device(cmd, &dev.id);
                }
            }
            if r.expand_toggled {
                st.expanded_id = if expanded { None } else { Some(dev.id.clone()) };
            }
            if r.zone_changed {
                persist = true;
                apply = true;
            }
            ui.add_space(theme::SPACE_4);
        }
        tour::anchor(ui.ctx(), AnchorId::LightingTargets, ui.min_rect());

        let on_canvas = canvas_driven(state, &st.sel_ids);
        if !on_canvas.is_empty() {
            ui.add_space(theme::SPACE_2);
            let names = on_canvas.join(", ");
            widgets::Banner::warn(&t!("lighting.on_canvas_warning", names = names))
                .title_font(theme::body_md())
                .title_color(theme::WARN_TEXT)
                .show(ui);
        }
        ui.add_space(theme::SPACE_9);
        let n = st.sel_ids.len();
        apply_now = widgets::button_enabled(
            ui,
            &t!("lighting.apply_to", n = n),
            widgets::ButtonKind::Primary,
            egui::vec2(ui.available_width(), 40.0),
            n > 0,
        );
    });
    if persist {
        crate::runtime::ipc::send(
            cmd,
            DaemonCommand::SetLightingTargets {
                device_ids: st.sel_ids.clone(),
                channels: st.zone_sel.clone(),
            },
        );
    }
    (apply, apply_now)
}

/// Names of the selected devices that have zones placed on the canvas — a
/// direct effect takes them off it.
fn canvas_driven(state: &TopicStore, selected: &[DeviceId]) -> Vec<String> {
    state
        .devices
        .iter()
        .filter(|d| selected.contains(&d.id))
        .filter(|d| {
            state
                .lighting
                .canvas
                .placed_zones
                .iter()
                .any(|z| z.device_id == d.id)
        })
        .map(|d| d.name.clone())
        .collect()
}

fn toggle_device(st: &mut LightingUi, id: &DeviceId) {
    if let Some(pos) = st.sel_ids.iter().position(|i| i == id) {
        st.sel_ids.remove(pos);
    } else {
        st.sel_ids.push(id.into());
    }
}

// ── Device target row ─────────────────────────────────────────────────────────

struct RowResult {
    toggled: bool,
    expand_toggled: bool,
    zone_changed: bool,
}

#[derive(Debug, PartialEq, Eq)]
enum CardClick {
    Toggle,
    Expand,
}

fn card_click(on_checkbox: bool, selected: bool, has_zones: bool) -> CardClick {
    if !on_checkbox && selected && has_zones {
        CardClick::Expand
    } else {
        CardClick::Toggle
    }
}

/// The `(zone id, zone name)` pairs a device exposes via its `Rgb` capability.
fn rgb_zone_pairs(dev: &WireDevice) -> Vec<(String, String)> {
    model::plain_channels(dev)
        .into_iter()
        .map(|z| (z.id.clone(), z.name.clone()))
        .collect()
}

/// Width of a target row's leading checkbox column (padding + box + gap).
const CHECK_COL_W: f32 = 40.0;

fn device_row(
    ui: &mut egui::Ui,
    dev: &WireDevice,
    selected: bool,
    expanded: bool,
    st: &mut LightingUi,
) -> RowResult {
    let mut result = RowResult {
        toggled: false,
        expand_toggled: false,
        zone_changed: false,
    };
    let zone_pairs = rgb_zone_pairs(dev);
    let all_zone_ids: Vec<String> = zone_pairs.iter().map(|(id, _)| id.clone()).collect();
    let sel_zones = st
        .zone_sel
        .get(&dev.id)
        .cloned()
        .unwrap_or_else(|| all_zone_ids.clone());
    let n_sel = if selected {
        all_zone_ids
            .iter()
            .filter(|z| sel_zones.contains(z))
            .count()
    } else {
        0
    };
    let sub = zone_pairs
        .iter()
        .map(|(_, name)| name.as_str())
        .collect::<Vec<_>>()
        .join(" · ");
    let count = format!("{n_sel}/{}", all_zone_ids.len());
    let resp = widgets::check_row(
        ui,
        &dev.name,
        Some(&sub),
        Some((&count, theme::TEXT)),
        selected,
    );
    if resp.clicked() {
        let on_checkbox = resp
            .interact_pointer_pos()
            .is_some_and(|p| p.x < resp.rect.left() + CHECK_COL_W);
        match card_click(on_checkbox, selected, !all_zone_ids.is_empty()) {
            CardClick::Expand => result.expand_toggled = true,
            CardClick::Toggle => result.toggled = true,
        }
    }

    if expanded && selected && !zone_pairs.is_empty() {
        ui.add_space(theme::SPACE_3);
        ui.horizontal_wrapped(|ui| {
            ui.add_space(CHECK_COL_W);
            ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
            for (channel_id, zone_name) in &zone_pairs {
                let active = sel_zones.contains(channel_id);
                if widgets::pill(ui, zone_name, active) {
                    let zv = st
                        .zone_sel
                        .entry(dev.id.clone())
                        .or_insert_with(|| all_zone_ids.clone());
                    if let Some(pos) = zv.iter().position(|z| z == channel_id) {
                        if zv.len() > 1 {
                            zv.remove(pos);
                        }
                    } else {
                        zv.push(channel_id.clone());
                    }
                    result.zone_changed = true;
                }
            }
        });
    }
    result
}

// ── Command dispatch ──────────────────────────────────────────────────────────

/// Apply a just-saved designer effect immediately, bypassing the
/// `custom_direct_effects` state lookup — the daemon hasn't broadcast the
/// new entry back yet, but we already have its params in hand.
fn apply_direct_params(
    state: &TopicStore,
    cmd: &CommandTx,
    st: &LightingUi,
    params: HashMap<String, EffectParamValue>,
) {
    for dev in rgb_devices(state) {
        if !st.sel_ids.contains(&dev.id) {
            continue;
        }
        crate::runtime::ipc::send(
            cmd,
            DaemonCommand::Device {
                device_id: dev.id.clone(),
                op: DeviceOp::LightingApply {
                    state: LightingState::DirectEffect {
                        id: DESIGNER_EFFECT_ID.to_string(),
                        params: params.clone(),
                    },
                },
            },
        );
    }
}

/// Stop driving a device that was just removed from the target selection
fn release_device(cmd: &CommandTx, id: &DeviceId) {
    crate::runtime::ipc::send(
        cmd,
        DaemonCommand::Device {
            device_id: id.clone(),
            op: DeviceOp::LightingApply {
                state: LightingState::Static {
                    color: RgbColor { r: 0, g: 0, b: 0 },
                },
            },
        },
    );
}

fn send_to_selected(state: &TopicStore, cmd: &CommandTx, st: &LightingUi) {
    for dev in rgb_devices(state) {
        if !st.sel_ids.contains(&dev.id) {
            continue;
        }
        let Some(rgb) = dev.capabilities.iter().find_map(|c| match c {
            DeviceCapability::Lighting(r) => Some(r),
            _ => None,
        }) else {
            continue;
        };

        let static_state = || LightingState::Static {
            color: adjusted(st.color, st.brightness, st.saturation),
        };
        let rgb_state = if st.effect.is_empty() {
            static_state()
        } else if let Some(anim) = state
            .lighting
            .canvas
            .available_direct_effects
            .iter()
            .find(|a| a.id == st.effect)
        {
            LightingState::DirectEffect {
                id: anim.id.clone(),
                params: direct_params(
                    anim,
                    st.color,
                    &st.param_values,
                    &st.param_strs,
                    &st.param_colors,
                    &st.param_steps,
                ),
            }
        } else if let Some(def) = selected_custom(st, &state.lighting.canvas.custom_direct_effects)
        {
            LightingState::DirectEffect {
                id: DESIGNER_EFFECT_ID.to_string(),
                params: def.params.clone(),
            }
        } else {
            match rgb
                .descriptor
                .native_effects
                .iter()
                .find(|e| e.id == st.effect)
            {
                Some(e) => LightingState::NativeEffect {
                    id: e.id.clone(),
                    params: Default::default(),
                },
                None => static_state(),
            }
        };

        crate::runtime::ipc::send(
            cmd,
            DaemonCommand::Device {
                device_id: dev.id.clone(),
                op: DeviceOp::LightingApply { state: rgb_state },
            },
        );
    }
}

// The `"color"` param comes from the master swatch; Range/Boolean/Sensor/Enum
// and any other `Color` param use the live value from `effect_params_card`
// (falling back to the descriptor default when the user hasn't touched that
// control yet); Text/Image keep their default — brightness/saturation are left
// to the effect's own animation (e.g. breathing pulses its own brightness).
fn direct_params(
    anim: &Animation,
    color: RgbColor,
    param_values: &HashMap<String, f32>,
    param_strs: &HashMap<String, String>,
    param_colors: &HashMap<String, RgbColor>,
    param_steps: &HashMap<String, Vec<ColorStep>>,
) -> HashMap<String, EffectParamValue> {
    anim.params
        .iter()
        .map(|d| {
            let key = format!("{}:{}", anim.id, d.id);
            let v = match &d.kind {
                ParamKind::Color if d.id == "color" => EffectParamValue::Color(color),
                ParamKind::Color => {
                    let default = match d.default {
                        EffectParamValue::Color(c) => c,
                        _ => RgbColor::default(),
                    };
                    EffectParamValue::Color(param_colors.get(&key).copied().unwrap_or(default))
                }
                ParamKind::Range { min, .. } | ParamKind::Number { min, .. } => {
                    let default = match d.default {
                        EffectParamValue::Float(f) => f as f32,
                        _ => *min as f32,
                    };
                    let f = *param_values.get(&key).unwrap_or(&default);
                    EffectParamValue::Float(f as f64)
                }
                ParamKind::Boolean => {
                    let default = param_bool_default(d);
                    let on = param_values.get(&key).map_or(default, |&v| v != 0.0);
                    EffectParamValue::Bool(on)
                }
                ParamKind::Sensor => {
                    let default = match &d.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => String::new(),
                    };
                    EffectParamValue::Str(param_strs.get(&key).cloned().unwrap_or(default))
                }
                ParamKind::Steps | ParamKind::Gradient => EffectParamValue::Steps(
                    param_steps
                        .get(&key)
                        .cloned()
                        .unwrap_or_else(|| widgets::steps_default(d)),
                ),
                ParamKind::Enum { options } => {
                    let default = match &d.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => options.first().cloned().unwrap_or_default(),
                    };
                    EffectParamValue::Str(param_strs.get(&key).cloned().unwrap_or(default))
                }
                ParamKind::Text | ParamKind::Image => d.default.clone(),
            };
            (d.id.clone(), v)
        })
        .collect()
}

/// Applies brightness (0–100) and saturation (0–100) adjustments to a color.
fn adjusted(c: RgbColor, brightness: f32, saturation: f32) -> RgbColor {
    let scale = brightness / 100.0;
    let r = c.r as f32 / 255.0;
    let g = c.g as f32 / 255.0;
    let b = c.b as f32 / 255.0;
    let luma = r * 0.299 + g * 0.587 + b * 0.114;
    let sat = saturation / 100.0;
    let r2 = ((luma + (r - luma) * sat) * scale).clamp(0.0, 1.0);
    let g2 = ((luma + (g - luma) * sat) * scale).clamp(0.0, 1.0);
    let b2 = ((luma + (b - luma) * sat) * scale).clamp(0.0, 1.0);
    RgbColor {
        r: (r2 * 255.0).round() as u8,
        g: (g2 * 255.0).round() as u8,
        b: (b2 * 255.0).round() as u8,
    }
}

// ── Delete confirmation modal ─────────────────────────────────────────────

fn delete_confirm_modal(ui: &mut egui::Ui, st: &mut LightingUi, cmd: &CommandTx) {
    let Some(name) = st.confirm_delete.clone() else {
        return;
    };
    if let Some(name) = widgets::confirm_delete_dialog(
        ui.ctx(),
        "lighting_delete_confirm",
        &t!("lighting.delete_effect_title"),
        &t!("lighting.delete_confirm", name = name),
        &t!("lighting.delete"),
        &mut st.confirm_delete,
    ) {
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::DeleteCustomEffect { name: name.clone() },
        );
        if st.effect == format!("{CUSTOM_PREFIX}{name}") {
            st.effect = String::new();
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rgb_devices(state: &TopicStore) -> impl Iterator<Item = &WireDevice> {
    state
        .devices
        .iter()
        .filter(|d| model::listable(d) && !model::is_hidden(d) && has_rgb_zones(d))
}

/// A device is an RGB target only if it exposes directly drivable channels.
/// Chainable channels are painted through their links, not the host.
fn has_rgb_zones(d: &WireDevice) -> bool {
    !model::plain_channels(d).is_empty()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn checkbox_click_only_toggles_never_expands() {
        // Regression: a checkbox hit must never open the zone picker, even on a
        // selected device with channels — otherwise one click both toggles and
        // expands.
        assert_eq!(card_click(true, true, true), CardClick::Toggle);
        assert_eq!(card_click(true, false, true), CardClick::Toggle);
    }

    #[test]
    fn row_expands_only_when_selected_with_zones() {
        assert_eq!(card_click(false, true, true), CardClick::Expand);
        // Not selected → row click selects; no channels → nothing to expand.
        assert_eq!(card_click(false, false, true), CardClick::Toggle);
        assert_eq!(card_click(false, true, false), CardClick::Toggle);
    }

    #[test]
    fn adjusted_full_brightness_identity() {
        let c = RgbColor { r: 255, g: 0, b: 0 };
        assert_eq!(adjusted(c, 100.0, 100.0), c);
    }

    #[test]
    fn adjusted_zero_brightness_black() {
        let c = RgbColor {
            r: 200,
            g: 100,
            b: 50,
        };
        assert_eq!(adjusted(c, 0.0, 100.0), RgbColor { r: 0, g: 0, b: 0 });
    }

    #[test]
    fn adjusted_zero_saturation_gray() {
        let c = RgbColor { r: 255, g: 0, b: 0 };
        let out = adjusted(c, 100.0, 0.0);
        assert_eq!(out.r, out.g);
        assert_eq!(out.g, out.b);
        assert!((out.r as i32 - 76).abs() <= 1);
    }

    #[test]
    fn adjusted_combined_brightness_and_saturation() {
        let c = RgbColor { r: 255, g: 0, b: 0 };
        let out = adjusted(c, 50.0, 50.0);
        let mix = |chan: f32| ((0.299 + (chan - 0.299) * 0.5) * 0.5 * 255.0).round() as u8;
        assert_eq!(out.r, mix(1.0));
        assert_eq!(out.g, mix(0.0));
        assert_eq!(out.b, mix(0.0));
        assert!(out.r > out.g && out.g == out.b);
    }

    #[test]
    fn adjusted_zero_saturation_is_gray_at_partial_brightness() {
        let c = RgbColor {
            r: 200,
            g: 100,
            b: 50,
        };
        let out = adjusted(c, 60.0, 0.0);
        assert_eq!(out.r, out.g);
        assert_eq!(out.g, out.b);
    }

    #[test]
    fn available_effects_lists_static_then_direct() {
        let effects = available_effects(&[], &[]);
        assert_eq!(effects.len(), 1);
        assert_eq!(effects[0], (String::new(), "Static".to_string()));

        let anim = Animation {
            id: "breathing".into(),
            name: "Breathing".into(),
            params: vec![],
        };
        let effects = available_effects(std::slice::from_ref(&anim), &[]);
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0].0, "");
        assert_eq!(
            effects[1],
            ("breathing".to_string(), "Breathing".to_string())
        );
    }

    #[test]
    fn available_effects_filters_raw_designer_and_lists_custom() {
        let designer_anim = Animation {
            id: DESIGNER_EFFECT_ID.to_string(),
            name: "Designer".into(),
            params: vec![],
        };
        let custom = EffectDef {
            effect_id: DESIGNER_EFFECT_ID.to_string(),
            name: Some("My Comet".to_string()),
            params: HashMap::new(),
        };
        let effects = available_effects(
            std::slice::from_ref(&designer_anim),
            std::slice::from_ref(&custom),
        );
        assert_eq!(effects.len(), 2);
        assert_eq!(effects[0], (String::new(), "Static".to_string()));
        assert_eq!(
            effects[1],
            (format!("{CUSTOM_PREFIX}My Comet"), "My Comet".to_string())
        );
    }

    #[test]
    fn direct_params_overrides_color_keeps_other_defaults() {
        let anim = Animation {
            id: "breathing".into(),
            name: "Breathing".into(),
            params: vec![
                halod_shared::types::EffectParamDescriptor {
                    id: "color".into(),
                    label: "Color".into(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor { r: 0, g: 0, b: 0 }),
                },
                halod_shared::types::EffectParamDescriptor {
                    id: "speed".into(),
                    label: "Speed".into(),
                    kind: ParamKind::Range {
                        min: 0.0,
                        max: 3.0,
                        step: 0.1,
                    },
                    default: EffectParamValue::Float(0.5),
                },
            ],
        };
        let master = RgbColor {
            r: 10,
            g: 20,
            b: 30,
        };
        let params = direct_params(
            &anim,
            master,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(params["color"], EffectParamValue::Color(master));
        assert_eq!(params["speed"], EffectParamValue::Float(0.5));
    }

    #[test]
    fn direct_params_uses_live_value_when_present() {
        let anim = Animation {
            id: "audio_beat".into(),
            name: "Audio Beat".into(),
            params: vec![halod_shared::types::EffectParamDescriptor {
                id: "sensitivity".into(),
                label: "Sensitivity".into(),
                kind: ParamKind::Range {
                    min: 0.0,
                    max: 1.0,
                    step: 0.05,
                },
                default: EffectParamValue::Float(0.5),
            }],
        };
        let mut live = HashMap::new();
        live.insert("audio_beat:sensitivity".to_string(), 0.8f32);
        let params = direct_params(
            &anim,
            RgbColor::default(),
            &live,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        let EffectParamValue::Float(f) = params["sensitivity"] else {
            panic!("expected Float");
        };
        assert!((f - 0.8).abs() < 1e-6);
    }

    fn desc(id: &str, kind: ParamKind, default: EffectParamValue) -> EffectParamDescriptor {
        EffectParamDescriptor {
            id: id.into(),
            label: id.into(),
            kind,
            default,
        }
    }

    fn anim_with(id: &str, params: Vec<EffectParamDescriptor>) -> Animation {
        Animation {
            id: id.into(),
            name: id.into(),
            params,
        }
    }

    #[test]
    fn show_color_true_for_static_and_color_effects_false_otherwise() {
        // Static (empty effect id) always shows the master swatch.
        assert!(show_color("", None));

        let with_color = anim_with(
            "breathing",
            vec![desc(
                "color",
                ParamKind::Color,
                EffectParamValue::Color(RgbColor::default()),
            )],
        );
        assert!(show_color("breathing", Some(&with_color)));

        let no_color = anim_with(
            "sparkle",
            vec![desc(
                "speed",
                ParamKind::Range {
                    min: 0.0,
                    max: 1.0,
                    step: 0.1,
                },
                EffectParamValue::Float(0.5),
            )],
        );
        assert!(!show_color("sparkle", Some(&no_color)));
    }

    #[test]
    fn show_color_false_for_two_stop_gradient_effect() {
        // A two-colour effect (no param literally named "color") renders both
        // colours inline in effect_params_card instead of the master swatch.
        let two_stop = anim_with(
            "sensor_gradient",
            vec![
                desc(
                    "color_a",
                    ParamKind::Color,
                    EffectParamValue::Color(RgbColor::default()),
                ),
                desc(
                    "color_b",
                    ParamKind::Color,
                    EffectParamValue::Color(RgbColor::default()),
                ),
            ],
        );
        assert!(!show_color("sensor_gradient", Some(&two_stop)));
    }

    #[test]
    fn sensor_options_labels_include_live_reading() {
        let state = TopicStore {
            devices: vec![WireDevice {
                capabilities: vec![DeviceCapability::Sensors(vec![
                    halod_shared::types::Sensor {
                        id: "temp1".into(),
                        name: "CPU".into(),
                        value: 47.6,
                        unit: halod_shared::types::SensorUnit::Celsius,
                        sensor_type: halod_shared::types::SensorType::Temperature,
                    },
                ])],
                ..Default::default()
            }],
            ..Default::default()
        };
        assert_eq!(
            sensor_options(&state),
            vec![("temp1".to_string(), "CPU (48°C)".to_string())]
        );
    }

    #[test]
    fn direct_params_number_uses_live_value_then_default() {
        let anim = anim_with(
            "sensor_gradient",
            vec![desc(
                "min",
                ParamKind::Number {
                    min: -100.0,
                    max: 100.0,
                },
                EffectParamValue::Float(20.0),
            )],
        );
        let params = direct_params(
            &anim,
            RgbColor::default(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(params["min"], EffectParamValue::Float(20.0));

        let mut live = HashMap::new();
        live.insert("sensor_gradient:min".to_string(), 35.0f32);
        let params = direct_params(
            &anim,
            RgbColor::default(),
            &live,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(params["min"], EffectParamValue::Float(35.0));
    }

    #[test]
    fn colour_params_share_one_cell_at_the_first_colours_position() {
        let color = |id| {
            desc(
                id,
                ParamKind::Color,
                EffectParamValue::Color(RgbColor::default()),
            )
        };
        let (sensor, a, mode, b) = (
            desc(
                "sensor",
                ParamKind::Sensor,
                EffectParamValue::Str(String::new()),
            ),
            color("color_a"),
            desc("mode", ParamKind::Boolean, EffectParamValue::Bool(false)),
            color("color_b"),
        );
        let items = settings_items(&[&sensor, &a, &mode, &b]);
        let shape: Vec<String> = items
            .iter()
            .map(|i| match i {
                SettingsItem::Param(d) => d.id.clone(),
                SettingsItem::Colors(c) => c
                    .iter()
                    .map(|d| d.id.as_str())
                    .collect::<Vec<_>>()
                    .join("+"),
            })
            .collect();
        assert_eq!(shape, ["sensor", "color_a+color_b", "mode"]);
    }

    #[test]
    fn wants_own_row_matches_master_swatch_narrowing() {
        assert!(!wants_own_row(&desc(
            "color",
            ParamKind::Color,
            EffectParamValue::Color(RgbColor::default()),
        )));
        assert!(wants_own_row(&desc(
            "color_b",
            ParamKind::Color,
            EffectParamValue::Color(RgbColor::default()),
        )));
        assert!(wants_own_row(&desc(
            "sensor",
            ParamKind::Sensor,
            EffectParamValue::Str(String::new()),
        )));
        assert!(wants_own_row(&desc(
            "min",
            ParamKind::Number {
                min: -100.0,
                max: 100.0,
            },
            EffectParamValue::Float(20.0),
        )));
        assert!(wants_own_row(&desc(
            "mode",
            ParamKind::Enum { options: vec![] },
            EffectParamValue::Str(String::new()),
        )));
        assert!(!wants_own_row(&desc(
            "note",
            ParamKind::Text,
            EffectParamValue::Str(String::new()),
        )));
    }

    #[test]
    fn direct_params_boolean_uses_live_value_then_default() {
        let anim = anim_with(
            "audio_level",
            vec![desc(
                "hue_shift",
                ParamKind::Boolean,
                EffectParamValue::Bool(false),
            )],
        );

        // Untouched → descriptor default.
        let params = direct_params(
            &anim,
            RgbColor::default(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(params["hue_shift"], EffectParamValue::Bool(false));

        // Live 1.0 → true.
        let mut live = HashMap::new();
        live.insert("audio_level:hue_shift".to_string(), 1.0f32);
        let params = direct_params(
            &anim,
            RgbColor::default(),
            &live,
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(params["hue_shift"], EffectParamValue::Bool(true));
    }

    #[test]
    fn direct_params_second_color_uses_param_colors_not_master_swatch() {
        let anim = anim_with(
            "sensor_gradient",
            vec![
                desc(
                    "color_a",
                    ParamKind::Color,
                    EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 }),
                ),
                desc(
                    "color_b",
                    ParamKind::Color,
                    EffectParamValue::Color(RgbColor { r: 4, g: 5, b: 6 }),
                ),
            ],
        );
        let master = RgbColor {
            r: 200,
            g: 200,
            b: 200,
        };
        let mut colors = HashMap::new();
        colors.insert(
            "sensor_gradient:color_b".to_string(),
            RgbColor { r: 9, g: 9, b: 9 },
        );
        let params = direct_params(
            &anim,
            master,
            &HashMap::new(),
            &HashMap::new(),
            &colors,
            &HashMap::new(),
        );
        // Untouched color_a keeps its own descriptor default, not the master swatch.
        assert_eq!(
            params["color_a"],
            EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 })
        );
        assert_eq!(
            params["color_b"],
            EffectParamValue::Color(RgbColor { r: 9, g: 9, b: 9 })
        );
    }

    #[test]
    fn direct_params_sensor_and_enum_use_live_strings_then_default() {
        let anim = anim_with(
            "sensor_gradient",
            vec![
                desc(
                    "sensor",
                    ParamKind::Sensor,
                    EffectParamValue::Str(String::new()),
                ),
                desc(
                    "mode",
                    ParamKind::Enum {
                        options: vec!["gradient".into(), "meter".into()],
                    },
                    EffectParamValue::Str("gradient".into()),
                ),
            ],
        );
        // Untouched → defaults.
        let params = direct_params(
            &anim,
            RgbColor::default(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(params["sensor"], EffectParamValue::Str(String::new()));
        assert_eq!(params["mode"], EffectParamValue::Str("gradient".into()));

        // Live values win.
        let mut strs = HashMap::new();
        strs.insert("sensor_gradient:sensor".to_string(), "temp1".to_string());
        strs.insert("sensor_gradient:mode".to_string(), "meter".to_string());
        let params = direct_params(
            &anim,
            RgbColor::default(),
            &HashMap::new(),
            &strs,
            &HashMap::new(),
            &HashMap::new(),
        );
        assert_eq!(params["sensor"], EffectParamValue::Str("temp1".into()));
        assert_eq!(params["mode"], EffectParamValue::Str("meter".into()));
    }

    fn rgb_cap(channels: Vec<halod_shared::types::LightingChannel>) -> DeviceCapability {
        DeviceCapability::Lighting(halod_shared::types::LightingStatus {
            descriptor: halod_shared::types::LightingDescriptor {
                channels,
                native_effects: vec![],
            },
            state: None,
            channel_transforms: Default::default(),
        })
    }

    fn zone() -> halod_shared::types::LightingChannel {
        halod_shared::types::LightingChannel {
            id: "z0".into(),
            name: "Zone".into(),
            topology: halod_shared::types::ZoneTopology::Linear,
            leds: vec![],
            color_order: Default::default(),
            division: Default::default(),
            visibility: Default::default(),
        }
    }

    #[test]
    fn zoneless_rgb_carrier_is_not_a_target() {
        // Chain hosts get a synthesized zone-less `Rgb` carrier; it must not be
        // offered as an RGB target (else the daemon rejects the LightingApply).
        let d = WireDevice {
            capabilities: vec![rgb_cap(vec![])],
            ..Default::default()
        };
        assert!(!has_rgb_zones(&d));
    }

    #[test]
    fn rgb_device_with_zones_is_a_target() {
        let d = WireDevice {
            capabilities: vec![rgb_cap(vec![zone()])],
            ..Default::default()
        };
        assert!(has_rgb_zones(&d));
    }

    fn state_with_targets(profile: &str, ids: &[&str]) -> TopicStore {
        TopicStore {
            profiles: halod_shared::types::ProfileState {
                active: profile.into(),
                ..Default::default()
            },
            lighting: halod_shared::types::LightingOverviewState {
                targets: halod_shared::types::LightingTargets {
                    device_ids: ids.iter().map(|s| (*s).parse().unwrap()).collect(),
                    channels: HashMap::new(),
                },
                ..Default::default()
            },
            ..Default::default()
        }
    }

    #[test]
    fn seeds_from_profile_and_reseeds_only_on_switch() {
        let mut st = LightingUi::default();
        assert!(st.sel_ids.is_empty(), "starts with nothing selected");

        seed_if_profile_changed(&mut st, &state_with_targets("default", &["a"]));
        assert_eq!(st.sel_ids, vec!["a"]);

        // Local edits survive same-profile state refreshes (e.g. the broadcast
        // triggered by our own SetLightingTargets).
        st.sel_ids.push("b".parse().unwrap());
        seed_if_profile_changed(&mut st, &state_with_targets("default", &["a"]));
        assert_eq!(st.sel_ids, vec!["a", "b"]);

        // Profile switch reseeds from the new profile's saved targets.
        seed_if_profile_changed(&mut st, &state_with_targets("gaming", &["c"]));
        assert_eq!(st.sel_ids, vec!["c"]);
    }

    #[test]
    fn toggle_device_adds_then_removes() {
        let mut st = LightingUi::default();
        toggle_device(&mut st, &"a".parse().unwrap());
        assert_eq!(st.sel_ids, vec!["a"]);
        toggle_device(&mut st, &"b".parse().unwrap());
        assert_eq!(st.sel_ids, vec!["a", "b"]);
        toggle_device(&mut st, &"a".parse().unwrap());
        assert_eq!(st.sel_ids, vec!["b"]);
    }

    #[test]
    fn release_device_sends_black_static_apply() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        release_device(&tx, &"dev1".parse().unwrap());
        let cmd = rx.try_recv().expect("expected a queued command");
        assert!(matches!(
            cmd,
            DaemonCommand::Device { device_id: id, op: DeviceOp::LightingApply { state: LightingState::Static {
                    color: RgbColor { r: 0, g: 0, b: 0 }
                } } } if id == "dev1"
        ));
    }

    #[test]
    fn debounce_apply_collapses_a_continuous_drag_to_one_flush() {
        let mut deadline = None;
        // A drag reports `apply` every frame; none should flush until it quiets.
        assert!(!debounce_apply(&mut deadline, true, 0.00));
        assert!(!debounce_apply(&mut deadline, true, 0.05));
        assert!(!debounce_apply(&mut deadline, true, 0.10));
        assert!(deadline.is_some(), "still armed mid-drag");
        // Drag stops: no more `apply`. Once the quiet period elapses, exactly one
        // flush fires and the deadline clears.
        assert!(!debounce_apply(&mut deadline, false, 0.15));
        assert!(debounce_apply(&mut deadline, false, 0.10 + APPLY_DEBOUNCE));
        assert_eq!(deadline, None);
        // No further flush without a new change.
        assert!(!debounce_apply(&mut deadline, false, 10.0));
    }

    #[test]
    fn canvas_warning_names_only_selected_devices_placed_on_canvas() {
        let dev = |id: &str| WireDevice {
            id: id.parse().unwrap(),
            name: id.to_uppercase(),
            ..Default::default()
        };
        let mut state = TopicStore {
            devices: vec![dev("a"), dev("b"), dev("c")],
            ..Default::default()
        };
        state.lighting.canvas.placed_zones = ["a", "b"]
            .iter()
            .map(|d| halod_shared::types::PlacedZone {
                device_id: (*d).parse().unwrap(),
                ..Default::default()
            })
            .collect();
        let sel: Vec<DeviceId> = vec!["b".parse().unwrap(), "c".parse().unwrap()];
        assert_eq!(canvas_driven(&state, &sel), ["B"]);
    }
}
