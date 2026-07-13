// SPDX-License-Identifier: GPL-3.0-or-later
//! The unified RGB Lighting view — a shared header (title + canvas transport
//! chrome) over a tab switcher between two sub-views:
//!
//! - **Effects Canvas** ([`crate::ui::screens::canvas`]): the live canvas stage
//!   with draggable zone placement and per-instance effects.
//! - **Direct Effects** (this module): apply one color or effect to every
//!   device at once — a master color card (shown only for Static or effects
//!   with a `Color` param), an effect grid, brightness/saturation sliders, and
//!   a target devices & zones picker.

use crate::ui::components as widgets;
use std::collections::HashMap;

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::effect_designer::DESIGNER_EFFECT_ID;
use halod_shared::types::{
    Animation, AppState, CanvasFrame, ColorStep, DeviceCapability, EffectDef,
    EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor, RgbState, WireDevice,
};

use crate::domain::models::device as model;
use crate::domain::state::Page;
use crate::runtime::ipc::CommandTx;
use crate::ui::components::rgb_to_color32;
use crate::ui::screens::canvas::{self, CanvasUi};
use crate::ui::screens::effect_designer::{self, DesignerUi};
use crate::ui::theme;

/// Synthetic grid id prefix for a saved custom effect: `"custom:<name>"`.
const CUSTOM_PREFIX: &str = "custom:";

/// Tab indices for the unified RGB Lighting view.
pub(crate) const TAB_CANVAS: usize = 0;
#[expect(dead_code, reason = "stable direct-lighting tab index")]
pub(crate) const TAB_DIRECT: usize = 1;

// ── Constants ─────────────────────────────────────────────────────────────────

/// Collapsed device target card height.
const CARD_BASE_H: f32 = 58.0;
/// Extra height added when zone pills are expanded.
const CARD_ZONE_H: f32 = 56.0;

// ── State ─────────────────────────────────────────────────────────────────────

pub struct LightingUi {
    /// Active sub-view: [`TAB_CANVAS`] (Effects Canvas) or [`TAB_DIRECT`]
    /// (Direct Effects).
    pub tab: usize,
    pub color: RgbColor,
    /// Active effect id. Empty string = Static (RgbState::Static).
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
    sel_ids: Vec<String>,
    zone_sel: HashMap<String, Vec<String>>,
    expanded_id: Option<String>,
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

#[allow(clippy::too_many_arguments)]
pub fn show(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    st: &mut LightingUi,
    canvas_ui: &mut CanvasUi,
    designer_ui: &mut DesignerUi,
    canvas_frame: Option<&CanvasFrame>,
    time: f64,
    page: &mut Page,
) {
    // Shared header: title + canvas transport chrome + tab switcher. Kept above
    // both sub-views so the play/pause/stop and FPS controls stay reachable.
    egui::Panel::top("rgb_lighting_header")
        .show_separator_line(false)
        .frame(egui::Frame::NONE.inner_margin(egui::Margin {
            left: 36,
            right: 36,
            top: 26,
            bottom: 0,
        }))
        .show(ui, |ui| {
            let subtitle = if st.tab == TAB_CANVAS {
                t!("canvas.subtitle")
            } else {
                t!("lighting.subtitle")
            };
            ui.horizontal(|ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(t!("lighting.title"))
                            .font(theme::bold(22.0))
                            .color(theme::TEXT),
                    );
                    ui.add_space(3.0);
                    ui.label(
                        egui::RichText::new(subtitle)
                            .font(theme::body(12.0))
                            .color(theme::TEXT_MUT),
                    );
                });
                canvas::chrome(ui, canvas_ui, cmd, state.lighting.config.canvas_enabled);
            });
            ui.add_space(14.0);
            let labels = [t!("lighting.tab_canvas"), t!("lighting.tab_direct")];
            let refs: Vec<&str> = labels.iter().map(|c| c.as_ref()).collect();
            widgets::tab_bar(ui, &mut st.tab, &refs);
        });

    if st.tab == TAB_CANVAS {
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
    } else {
        direct_effects(ui, state, cmd, st, designer_ui, page);
    }

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

/// The Direct Effects tab: seed targets, apply a just-saved designer effect,
/// then render the scrollable body.
fn direct_effects(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    st: &mut LightingUi,
    designer_ui: &mut DesignerUi,
    page: &mut Page,
) {
    seed_if_profile_changed(st, state);

    // Drain a just-saved designer effect and apply it immediately.
    if let Some((name, params)) = designer_ui.result.take() {
        st.effect = format!("{CUSTOM_PREFIX}{name}");
        apply_direct_params(state, cmd, st, params);
    }

    egui::CentralPanel::default()
        .frame(egui::Frame::NONE)
        .show(ui, |ui| {
            egui::ScrollArea::vertical()
                .auto_shrink([false, false])
                .show(ui, |ui| {
                    widgets::page_frame(ui, |ui| {
                        page_body(ui, state, cmd, st, designer_ui, page);
                    });
                });

            delete_confirm_modal(ui, st, cmd);
        });
}

/// Seed the selection from the active profile's saved targets; reseed on
/// profile switch. Offline devices stay in the saved selection so reconnecting
/// restores them.
fn seed_if_profile_changed(st: &mut LightingUi, state: &AppState) {
    if st.seeded_for.as_deref() != Some(state.profiles.active.as_str()) {
        st.sel_ids = state.lighting.targets.device_ids.clone();
        st.zone_sel = state.lighting.targets.zones.clone();
        st.seeded_for = Some(state.profiles.active.clone());
    }
}

// ── Page body ─────────────────────────────────────────────────────────────────

fn page_body(
    ui: &mut egui::Ui,
    state: &AppState,
    cmd: &CommandTx,
    st: &mut LightingUi,
    designer_ui: &mut DesignerUi,
    page: &mut Page,
) {
    ui.add_space(6.0);

    let effects = available_effects(
        &state.lighting.canvas.available_direct_effects,
        &state.lighting.canvas.custom_direct_effects,
    );

    let mut apply = false;

    let gap = 18.0;
    let left_w = (ui.available_width() * 0.32).min(300.0);
    let selected_anim = state
        .lighting
        .canvas
        .available_direct_effects
        .iter()
        .find(|a| a.id == st.effect);

    let custom = &state.lighting.canvas.custom_direct_effects;
    if show_color(&st.effect, selected_anim) {
        apply |= widgets::split_columns(ui, left_w, gap, |left, right| {
            let mut changed = master_color_card(left, st);
            changed |= global_effect_card(right, st, &effects, cmd, designer_ui, page);
            effect_actions_row(right, st, designer_ui, page, custom);
            changed |= effect_extras(right, state, st, selected_anim);
            changed
        });
    } else {
        apply |= global_effect_card(ui, st, &effects, cmd, designer_ui, page);
        effect_actions_row(ui, st, designer_ui, page, custom);
        apply |= effect_extras(ui, state, st, selected_anim);
    }

    ui.add_space(16.0);
    apply |= targets_card(ui, state, cmd, st);
    ui.add_space(24.0);

    let now = ui.input(|i| i.time);
    if debounce_apply(&mut st.apply_deadline, apply, now) {
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

/// The card shown below the effect grid: master brightness/saturation for
/// Static, or the selected effect's own params. Returns `true` on change.
fn effect_extras(
    ui: &mut egui::Ui,
    state: &AppState,
    st: &mut LightingUi,
    anim: Option<&Animation>,
) -> bool {
    match anim {
        _ if st.effect.is_empty() => {
            ui.add_space(10.0);
            sliders_card(ui, st)
        }
        Some(anim) => {
            ui.add_space(10.0);
            effect_params_card(ui, state, st, anim)
        }
        None => false,
    }
}

/// Every sensor across all devices as `(id, display name)` pairs, for the
/// `ParamKind::Sensor` picker. Hidden sensors stay selectable — visibility
/// only governs the home dashboard.
pub(crate) fn sensor_options(state: &AppState) -> Vec<(String, String)> {
    crate::domain::models::sensors::sensors(state, true)
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
        Some((format!("{CUSTOM_PREFIX}{name}"), format!("\u{2605} {name}")))
    }));
    v
}

/// The saved custom effect currently selected in the grid, if any.
fn selected_custom<'a>(st: &LightingUi, custom: &'a [EffectDef]) -> Option<&'a EffectDef> {
    let name = st.effect.strip_prefix(CUSTOM_PREFIX)?;
    custom.iter().find(|d| d.name.as_deref() == Some(name))
}

// ── Effect Designer actions row ──────────────────────────────────────────────

#[allow(clippy::too_many_arguments)]
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
    ui.add_space(10.0);
    ui.horizontal(|ui| {
        ui.spacing_mut().item_spacing.x = 8.0;
        if widgets::button(
            ui,
            &t!("lighting.edit"),
            widgets::ButtonKind::Ghost,
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
            widgets::ButtonKind::Ghost,
            egui::vec2(72.0, 28.0),
        )
        .clicked()
        {
            effect_designer::spawn_export(ui.ctx(), def);
        }
        if widgets::button(
            ui,
            &t!("lighting.delete"),
            widgets::ButtonKind::Ghost,
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
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 8.0;
                let new_btn = widgets::button(
                    ui,
                    &t!("lighting.new_effect"),
                    widgets::ButtonKind::Ghost,
                    egui::vec2(88.0, 26.0),
                );
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::LightingNewEffect,
                    new_btn.rect,
                );
                if new_btn.clicked() {
                    *designer_ui = DesignerUi::new_effect();
                    *page = Page::EffectDesigner;
                }
                let import_btn = widgets::button(
                    ui,
                    &t!("lighting.import"),
                    widgets::ButtonKind::Ghost,
                    egui::vec2(64.0, 26.0),
                );
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::LightingImport,
                    import_btn.rect,
                );
                if import_btn.clicked() {
                    effect_designer::spawn_import(ui.ctx(), cmd.clone());
                }
            });
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
                        if widgets::effect_cell(ui, name, active, preview, cell_w, 66.0, 34.0, true)
                            && !active
                        {
                            st.effect = id.clone();
                            changed = true;
                        }
                    }
                });
            }
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::LightingEffects,
                ui.min_rect(),
            );
        },
    );
    changed
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
        | ParamKind::Enum { .. } => true,
        ParamKind::Color => d.id != "color",
        ParamKind::Text | ParamKind::Image => false,
    }
}

/// Renders an effect's non-master-color params from its descriptors.
fn effect_params_card(
    ui: &mut egui::Ui,
    state: &AppState,
    st: &mut LightingUi,
    anim: &Animation,
) -> bool {
    let params: Vec<&EffectParamDescriptor> =
        anim.params.iter().filter(|d| wants_own_row(d)).collect();
    if params.is_empty() {
        return false;
    }

    let sensors = sensor_options(state);
    let mut changed = false;
    widgets::card_titled(
        ui,
        &t!("lighting.effect_settings"),
        |_ui| {},
        |ui| {
            for (i, d) in params.iter().enumerate() {
                if i > 0 {
                    ui.add_space(14.0);
                }
                let key = format!("{}:{}", anim.id, d.id);
                match &d.kind {
                    ParamKind::Range { min, max, step } => {
                        let (min, max, step) = (*min, *max, *step);
                        let default = match d.default {
                            EffectParamValue::Float(f) => f as f32,
                            _ => min as f32,
                        };
                        let mut v = *st.param_values.get(&key).unwrap_or(&default);
                        let readout = widgets::range_readout(v, step);
                        let fired = widgets::slider_row_debounced(
                            ui,
                            &d.label,
                            &mut v,
                            min as f32..=max as f32,
                            &readout,
                        );
                        if fired {
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
                                        .font(theme::body(12.0))
                                        .color(theme::TEXT_DIM),
                                );
                            },
                            |ui| next = widgets::toggle(ui, on),
                        );
                        if next != on {
                            changed = true;
                        }
                        st.param_values.insert(key, next as i32 as f32);
                    }
                    ParamKind::Steps => {
                        let default = widgets::steps_default(d);
                        let steps = st.param_steps.entry(key).or_insert(default);
                        changed |= widgets::steps_editor(ui, &d.label, steps);
                    }
                    ParamKind::Sensor => {
                        let current = st.param_strs.get(&key).cloned().unwrap_or_default();
                        changed |= widgets::combo_param_row(
                            ui,
                            &d.label,
                            key,
                            &mut st.param_strs,
                            current,
                            &sensors,
                            Some(t!("lighting.none").as_ref()),
                        );
                    }
                    ParamKind::Enum { options } => {
                        let current = st.param_strs.get(&key).cloned().unwrap_or_else(|| match &d
                            .default
                        {
                            EffectParamValue::Str(s) => s.clone(),
                            _ => options.first().cloned().unwrap_or_default(),
                        });
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
                    ParamKind::Color => {
                        let default = match d.default {
                            EffectParamValue::Color(c) => c,
                            _ => RgbColor::default(),
                        };
                        let current = st.param_colors.get(&key).copied().unwrap_or(default);
                        ui.label(
                            egui::RichText::new(&d.label)
                                .font(theme::body(12.0))
                                .color(theme::TEXT_DIM),
                        );
                        if let Some(new_c) = widgets::color_picker(ui, current) {
                            st.param_colors.insert(key, new_c);
                            changed = true;
                        }
                    }
                    ParamKind::Text | ParamKind::Image => {}
                }
            }
        },
    );
    changed
}

fn param_bool_default(d: &EffectParamDescriptor) -> bool {
    matches!(d.default, EffectParamValue::Bool(true))
}

// ── Sliders card ──────────────────────────────────────────────────────────────

fn sliders_card(ui: &mut egui::Ui, st: &mut LightingUi) -> bool {
    let mut changed = false;
    widgets::card(ui, |ui| {
        let rb = format!("{}%", st.brightness.round() as u32);
        if widgets::slider_row_debounced(
            ui,
            &t!("lighting.master_brightness"),
            &mut st.brightness,
            0.0..=100.0,
            &rb,
        ) {
            changed = true;
        }
        ui.add_space(18.0);
        let rs = format!("{}%", st.saturation.round() as u32);
        if widgets::slider_row_debounced(
            ui,
            &t!("lighting.saturation"),
            &mut st.saturation,
            0.0..=100.0,
            &rs,
        ) {
            changed = true;
        }
    });
    changed
}

// ── Target devices & zones card ───────────────────────────────────────────────

fn targets_card(ui: &mut egui::Ui, state: &AppState, cmd: &CommandTx, st: &mut LightingUi) -> bool {
    let mut apply = false;
    let mut persist = false;
    widgets::card(ui, |ui| {
        // Header: title/subtitle on left, select/deselect-all on right.
        // `shrink_left` constrains the wrapping subtitle column to the space
        // beside the buttons.
        egui::Sides::new().shrink_left().spacing(12.0).show(
            ui,
            |ui| {
                ui.vertical(|ui| {
                    ui.label(
                        egui::RichText::new(t!("lighting.target_devices_zones"))
                            .font(theme::semibold(13.0))
                            .color(theme::TEXT),
                    );
                    ui.add_space(3.0);
                    ui.label(
                        egui::RichText::new(t!("lighting.targets_subtitle"))
                            .font(theme::body(11.0))
                            .color(theme::TEXT_MUT),
                    );
                });
            },
            |ui| {
                if widgets::button(
                    ui,
                    &t!("lighting.deselect_all"),
                    widgets::ButtonKind::Ghost,
                    egui::vec2(92.0, 28.0),
                )
                .clicked()
                    && !st.sel_ids.is_empty()
                {
                    for id in st.sel_ids.drain(..) {
                        release_device(cmd, &id);
                    }
                    persist = true;
                }
                if widgets::button(
                    ui,
                    &t!("lighting.select_all"),
                    widgets::ButtonKind::Ghost,
                    egui::vec2(80.0, 28.0),
                )
                .clicked()
                {
                    let all: Vec<String> = rgb_devices(state).map(|d| d.id.clone()).collect();
                    if st.sel_ids != all {
                        st.sel_ids = all;
                        persist = true;
                        apply = true;
                    }
                }
            },
        );

        ui.add_space(16.0);

        let gap = 10.0;
        let col_w = (ui.available_width() - gap) / 2.0;
        let devices: Vec<&WireDevice> = rgb_devices(state).collect();

        for (row, chunk) in devices.chunks(2).enumerate() {
            if row > 0 {
                ui.add_space(gap);
            }
            let row_h = chunk
                .iter()
                .map(|d| {
                    if st.expanded_id.as_deref() == Some(d.id.as_str()) {
                        CARD_BASE_H + CARD_ZONE_H
                    } else {
                        CARD_BASE_H
                    }
                })
                .fold(CARD_BASE_H, f32::max);

            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for dev in chunk {
                    let sel = st.sel_ids.contains(&dev.id);
                    let expanded = st.expanded_id.as_deref() == Some(dev.id.as_str());
                    let r = device_card(ui, dev, sel, expanded, st, col_w, row_h);
                    if r.toggled {
                        let was_selected = sel;
                        toggle_device(st, &dev.id);
                        persist = true;
                        apply = true;
                        if was_selected {
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
                }
            });
        }
        crate::domain::tour::anchor(
            ui.ctx(),
            crate::domain::tour::AnchorId::LightingTargets,
            ui.min_rect(),
        );
    });
    if persist {
        crate::domain::actions::lighting::set_lighting_targets(
            cmd,
            st.sel_ids.clone(),
            st.zone_sel.clone(),
        );
    }
    apply
}

fn toggle_device(st: &mut LightingUi, id: &str) {
    if let Some(pos) = st.sel_ids.iter().position(|i| i == id) {
        st.sel_ids.remove(pos);
    } else {
        st.sel_ids.push(id.to_string());
    }
}

// ── Device target card ────────────────────────────────────────────────────────

struct CardResult {
    toggled: bool,
    expand_toggled: bool,
    zone_changed: bool,
}

fn device_card(
    ui: &mut egui::Ui,
    dev: &WireDevice,
    selected: bool,
    expanded: bool,
    st: &mut LightingUi,
    w: f32,
    h: f32,
) -> CardResult {
    let mut result = CardResult {
        toggled: false,
        expand_toggled: false,
        zone_changed: false,
    };

    let zone_pairs: Vec<(String, String)> = dev
        .capabilities
        .iter()
        .find_map(|c| match c {
            DeviceCapability::Rgb(r) => Some(
                r.descriptor
                    .zones
                    .iter()
                    .map(|z| (z.id.clone(), z.name.clone()))
                    .collect(),
            ),
            _ => None,
        })
        .unwrap_or_default();
    let all_zone_ids: Vec<String> = zone_pairs.iter().map(|(id, _)| id.clone()).collect();
    let sel_zones = st
        .zone_sel
        .get(&dev.id)
        .cloned()
        .unwrap_or_else(|| all_zone_ids.clone());
    let n_sel_z = if sel_zones.is_empty() || sel_zones == all_zone_ids {
        all_zone_ids.len()
    } else {
        sel_zones.len()
    };
    let zone_summary = if all_zone_ids.is_empty() {
        String::new()
    } else {
        t!(
            "lighting.zones_count",
            sel = n_sel_z,
            total = all_zone_ids.len()
        )
        .to_string()
    };

    let (rect, _) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
    let p = ui.painter();
    let bg = if selected {
        theme::hex(0x131820)
    } else {
        theme::hex(0x0d1017)
    };
    let border = if selected {
        theme::hex(0x1c2a3a)
    } else {
        theme::BORDER
    };
    p.rect_filled(rect, 10.0, bg);
    p.rect_stroke(
        rect,
        10.0,
        Stroke::new(1.0, border),
        egui::StrokeKind::Middle,
    );

    // Header (clickable row).
    let hdr = Rect::from_min_size(rect.min, Vec2::new(rect.width(), CARD_BASE_H));
    let hdr_resp = ui.interact(hdr, ui.id().with(("hdr", &dev.id)), Sense::click());
    if hdr_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if hdr_resp.clicked() {
        if selected && !all_zone_ids.is_empty() {
            result.expand_toggled = true;
        } else {
            result.toggled = true;
        }
    }

    // Checkbox.
    let cb = 18.0;
    let cb_rect = Rect::from_center_size(
        Pos2::new(rect.left() + 14.0 + cb / 2.0, hdr.center().y),
        Vec2::splat(cb),
    );
    let cb_resp = ui.interact(cb_rect, ui.id().with(("cb", &dev.id)), Sense::click());
    if cb_resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    if cb_resp.clicked() {
        result.toggled = true;
    }
    let (cb_fill, cb_border) = if selected {
        (theme::CYAN, theme::CYAN)
    } else {
        (Color32::TRANSPARENT, theme::hex(0x2a3446))
    };
    p.rect_filled(cb_rect, 5.0, cb_fill);
    p.rect_stroke(
        cb_rect,
        5.0,
        Stroke::new(1.5, cb_border),
        egui::StrokeKind::Middle,
    );
    if selected {
        let ink = Stroke::new(1.8, theme::hex(0x0a0d13));
        let c = cb_rect.center();
        let r = cb_rect.width() * 0.5;
        p.line_segment(
            [
                Pos2::new(c.x - r * 0.45, c.y),
                Pos2::new(c.x - r * 0.1, c.y + r * 0.38),
            ],
            ink,
        );
        p.line_segment(
            [
                Pos2::new(c.x - r * 0.1, c.y + r * 0.38),
                Pos2::new(c.x + r * 0.45, c.y - r * 0.32),
            ],
            ink,
        );
    }

    // Type badge.
    let badge = Rect::from_center_size(
        Pos2::new(cb_rect.right() + 26.0, hdr.center().y),
        Vec2::new(30.0, 22.0),
    );
    widgets::device_badge(
        p,
        badge,
        dev.device_type,
        theme::device_color(dev),
        6.0,
        1.4,
    );

    // Device name (clipped to avoid overlapping zone summary).
    let name_x = badge.right() + 10.0;
    let summary_reserve = if zone_summary.is_empty() { 0.0 } else { 72.0 };
    let name_clip = Rect::from_min_max(
        Pos2::new(name_x, hdr.top()),
        Pos2::new(rect.right() - summary_reserve - 10.0, hdr.bottom()),
    );
    p.with_clip_rect(name_clip).text(
        Pos2::new(name_x, hdr.center().y),
        Align2::LEFT_CENTER,
        &dev.name,
        theme::semibold(12.5),
        theme::TEXT,
    );

    // Zone summary (right edge).
    if !zone_summary.is_empty() {
        p.text(
            Pos2::new(rect.right() - 14.0, hdr.center().y),
            Align2::RIGHT_CENTER,
            &zone_summary,
            theme::body(10.5),
            theme::TEXT_FAINT,
        );
    }

    // Zone pills (when expanded).
    if expanded && !zone_pairs.is_empty() {
        let div_y = rect.min.y + CARD_BASE_H;
        p.line_segment(
            [
                Pos2::new(rect.left() + 14.0, div_y),
                Pos2::new(rect.right() - 14.0, div_y),
            ],
            Stroke::new(1.0, theme::BORDER_SOFT),
        );
        let zone_area = Rect::from_min_size(
            Pos2::new(rect.left() + 14.0, div_y + 10.0),
            Vec2::new(rect.width() - 28.0, CARD_ZONE_H - 12.0),
        );
        let mut child = ui.new_child(
            egui::UiBuilder::new()
                .max_rect(zone_area)
                .layout(egui::Layout::left_to_right(egui::Align::Min).with_main_wrap(true)),
        );
        child.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        for (zone_id, zone_name) in &zone_pairs {
            let active = sel_zones.contains(zone_id);
            if widgets::pill(&mut child, zone_name, active) {
                let zv = st
                    .zone_sel
                    .entry(dev.id.clone())
                    .or_insert_with(|| all_zone_ids.clone());
                if let Some(pos) = zv.iter().position(|z| z == zone_id) {
                    if zv.len() > 1 {
                        zv.remove(pos);
                    }
                } else {
                    zv.push(zone_id.clone());
                }
                result.zone_changed = true;
            }
        }
    }

    result
}

// ── Command dispatch ──────────────────────────────────────────────────────────

/// Apply a just-saved designer effect immediately, bypassing the
/// `custom_direct_effects` state lookup — the daemon hasn't broadcast the
/// new entry back yet, but we already have its params in hand.
fn apply_direct_params(
    state: &AppState,
    cmd: &CommandTx,
    st: &LightingUi,
    params: HashMap<String, EffectParamValue>,
) {
    for dev in rgb_devices(state) {
        if !st.sel_ids.contains(&dev.id) {
            continue;
        }
        crate::domain::actions::lighting::send(
            cmd,
            DaemonCommand::RgbApply {
                id: dev.id.clone(),
                state: RgbState::DirectEffect {
                    id: DESIGNER_EFFECT_ID.to_string(),
                    params: params.clone(),
                },
            },
        );
    }
}

/// Stop driving a device that was just removed from the target selection
fn release_device(cmd: &CommandTx, id: &str) {
    crate::domain::actions::lighting::send(
        cmd,
        DaemonCommand::RgbApply {
            id: id.to_string(),
            state: RgbState::Static {
                color: RgbColor { r: 0, g: 0, b: 0 },
            },
        },
    );
}

fn send_to_selected(state: &AppState, cmd: &CommandTx, st: &LightingUi) {
    for dev in rgb_devices(state) {
        if !st.sel_ids.contains(&dev.id) {
            continue;
        }
        let Some(rgb) = dev.capabilities.iter().find_map(|c| match c {
            DeviceCapability::Rgb(r) => Some(r),
            _ => None,
        }) else {
            continue;
        };

        let static_state = || RgbState::Static {
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
            RgbState::DirectEffect {
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
            RgbState::DirectEffect {
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
                Some(e) => RgbState::NativeEffect {
                    id: e.id.clone(),
                    params: Default::default(),
                },
                None => static_state(),
            }
        };

        crate::domain::actions::lighting::send(
            cmd,
            DaemonCommand::RgbApply {
                id: dev.id.clone(),
                state: rgb_state,
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
                ParamKind::Steps => EffectParamValue::Steps(
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
    let (mut confirm, mut cancel) = (false, false);
    let dismissed = widgets::dialog(
        ui.ctx(),
        "lighting_delete_confirm",
        &t!("lighting.delete_effect_title"),
        420.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("lighting.delete_confirm", name = name))
                    .font(theme::body(12.5))
                    .color(theme::TEXT_MUT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("lighting.delete"),
                widgets::ButtonKind::Danger,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                confirm = true;
            }
            ui.add_space(8.0);
            if widgets::button(
                ui,
                &t!("lighting.cancel"),
                widgets::ButtonKind::Ghost,
                egui::vec2(96.0, 32.0),
            )
            .clicked()
            {
                cancel = true;
            }
        },
    );
    if let Some(name) =
        widgets::resolve_delete_confirm(&mut st.confirm_delete, confirm, cancel || dismissed)
    {
        crate::domain::actions::lighting::delete_custom_effect(cmd, &name);
        if st.effect == format!("{CUSTOM_PREFIX}{name}") {
            st.effect = String::new();
        }
    }
}

// ── Helpers ───────────────────────────────────────────────────────────────────

fn rgb_devices(state: &AppState) -> impl Iterator<Item = &WireDevice> {
    state
        .devices
        .iter()
        .filter(|d| model::listable(d) && !model::is_hidden(d) && has_rgb_zones(d))
}

/// A device is an RGB target only if it exposes real zones. Chain hosts get a
/// zone-less `Rgb` carrier synthesized by the daemon and must not appear here.
fn has_rgb_zones(d: &WireDevice) -> bool {
    d.capabilities
        .iter()
        .any(|c| matches!(c, DeviceCapability::Rgb(r) if !r.descriptor.zones.is_empty()))
}

/// Selected devices that are actually present — offline ids stay saved but
/// don't count toward "applies to N devices".
#[cfg(test)]
fn effective_sel_count(state: &AppState, st: &LightingUi) -> usize {
    rgb_devices(state)
        .filter(|d| st.sel_ids.contains(&d.id))
        .count()
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

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
            (
                format!("{CUSTOM_PREFIX}My Comet"),
                "\u{2605} My Comet".to_string()
            )
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
        let state = AppState {
            devices: vec![WireDevice {
                capabilities: vec![DeviceCapability::Sensors(vec![
                    halod_shared::types::Sensor {
                        id: "temp1".into(),
                        name: "CPU".into(),
                        value: 47.6,
                        unit: halod_shared::types::SensorUnit::Celsius,
                        sensor_type: halod_shared::types::SensorType::Temperature,
                        visibility: halod_shared::types::VisibilityState::Visible,
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

    fn rgb_cap(zones: Vec<halod_shared::types::RgbZone>) -> DeviceCapability {
        DeviceCapability::Rgb(halod_shared::types::RgbStatus {
            descriptor: halod_shared::types::RgbDescriptor {
                zones,
                native_effects: vec![],
            },
            state: None,
            zone_transforms: Default::default(),
            chainable_channels: vec![],
        })
    }

    fn zone() -> halod_shared::types::RgbZone {
        halod_shared::types::RgbZone {
            id: "z0".into(),
            name: "Zone".into(),
            topology: halod_shared::types::ZoneTopology::Linear,
            leds: vec![],
        }
    }

    #[test]
    fn zoneless_rgb_carrier_is_not_a_target() {
        // Chain hosts get a synthesized zone-less `Rgb` carrier; it must not be
        // offered as an RGB target (else the daemon rejects the RgbApply).
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

    fn state_with_targets(profile: &str, ids: &[&str]) -> AppState {
        AppState {
            profiles: halod_shared::types::ProfileState {
                active: profile.into(),
                ..Default::default()
            },
            lighting: halod_shared::types::LightingState {
                targets: halod_shared::types::LightingTargets {
                    device_ids: ids.iter().map(|s| s.to_string()).collect(),
                    zones: HashMap::new(),
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
        st.sel_ids.push("b".into());
        seed_if_profile_changed(&mut st, &state_with_targets("default", &["a"]));
        assert_eq!(st.sel_ids, vec!["a", "b"]);

        // Profile switch reseeds from the new profile's saved targets.
        seed_if_profile_changed(&mut st, &state_with_targets("gaming", &["c"]));
        assert_eq!(st.sel_ids, vec!["c"]);
    }

    #[test]
    fn toggle_device_adds_then_removes() {
        let mut st = LightingUi::default();
        toggle_device(&mut st, "a");
        assert_eq!(st.sel_ids, vec!["a"]);
        toggle_device(&mut st, "b");
        assert_eq!(st.sel_ids, vec!["a", "b"]);
        toggle_device(&mut st, "a");
        assert_eq!(st.sel_ids, vec!["b"]);
    }

    #[test]
    fn release_device_sends_black_static_apply() {
        let (tx, mut rx) = tokio::sync::mpsc::unbounded_channel();
        release_device(&tx, "dev1");
        let cmd = rx.try_recv().expect("expected a queued command");
        assert!(matches!(
            cmd,
            DaemonCommand::RgbApply {
                id,
                state: RgbState::Static {
                    color: RgbColor { r: 0, g: 0, b: 0 }
                }
            } if id == "dev1"
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
    fn effective_sel_count_ignores_offline_ids() {
        let mut state = state_with_targets("default", &[]);
        state.devices = vec![WireDevice {
            id: "online".into(),
            capabilities: vec![rgb_cap(vec![zone()])],
            ..Default::default()
        }];
        let st = LightingUi {
            sel_ids: vec!["online".into(), "unplugged".into()],
            ..Default::default()
        };
        assert_eq!(effective_sel_count(&state, &st), 1);
    }
}
