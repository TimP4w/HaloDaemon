// SPDX-License-Identifier: GPL-3.0-or-later
//! The Effect Designer page: create/edit a procedural Designer effect with live preview.

use crate::ui::components as widgets;
use std::collections::HashMap;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::effect_designer::{
    validate_effect_name, ColorMode, DesignerParams, Direction, Generator, RingScope,
};
use halod_shared::types::{EffectDef, EffectParamValue};

use crate::domain::state::Page;
use crate::runtime::ipc::CommandTx;
use crate::ui::theme;

const EXPORT_VERSION: u64 = 1;

pub struct DesignerUi {
    params: DesignerParams,
    name: String,
    /// `Some(name)` = editing an existing custom effect.
    editing: Option<String>,
    /// Page to return to on Save/back.
    back_page: Page,
    /// `(instance_id, effect_id)` — Save also pushes CanvasUpsertEffect.
    canvas_instance: Option<(String, String)>,
    error: Option<String>,
    /// Set when Save succeeds; consumed by the RGB Lighting page to apply immediately.
    pub result: Option<(String, HashMap<String, EffectParamValue>)>,
}

impl DesignerUi {
    pub fn new_effect() -> Self {
        Self {
            params: DesignerParams::default(),
            name: t!("misc.designer_new_effect_name").to_string(),
            editing: None,
            back_page: Page::Lighting,
            canvas_instance: None,
            error: None,
            result: None,
        }
    }

    pub fn edit(def: &EffectDef) -> Self {
        let name = def.name.clone().unwrap_or_default();
        Self {
            params: DesignerParams::from_params(&def.params),
            editing: Some(name.clone()),
            name,
            back_page: Page::Lighting,
            canvas_instance: None,
            error: None,
            result: None,
        }
    }

    /// Opens the designer seeded from a canvas rack instance's params
    /// (the pixmap renderer `designer_pixmap`), so Save also
    /// updates that instance and Back returns to the canvas. `display_name`
    /// seeds the name field but isn't treated as an existing library preset
    /// (a canvas instance's own name lives in a separate namespace) — Save
    /// always creates/updates a library entry under it, it never deletes one.
    pub fn edit_for_canvas_instance(
        def: &EffectDef,
        instance_id: String,
        display_name: String,
    ) -> Self {
        Self {
            params: DesignerParams::from_params(&def.params),
            editing: None,
            name: display_name,
            back_page: Page::Lighting,
            canvas_instance: Some((instance_id, def.effect_id.clone())),
            error: None,
            result: None,
        }
    }

    pub fn show(&mut self, ui: &mut egui::Ui, cmd: &CommandTx, page: &mut Page) {
        ui.ctx().request_repaint();
        egui::ScrollArea::vertical()
            .auto_shrink([false, false])
            .show(ui, |ui| {
                widgets::page_frame(ui, |ui| {
                    let back_label = if self.canvas_instance.is_some() {
                        t!("misc.designer_back_canvas")
                    } else {
                        t!("misc.designer_back_lighting")
                    };
                    if widgets::back_link(ui, &back_label) {
                        *page = self.back_page.clone();
                    }
                    self.title_row(ui, cmd, page);
                    ui.add_space(6.0);
                    self.subtitle_row(ui);
                    ui.add_space(20.0);

                    let t = ui.input(|i| i.time) as f32;
                    let gap = 20.0;
                    let left_w = (ui.available_width() * 0.45).min(520.0);
                    let ctx_ref = ui.ctx().clone();
                    widgets::split_columns(ui, left_w, gap, |left, right| {
                        let preview_rect = left.min_rect();
                        crate::domain::tour::anchor(
                            &ctx_ref,
                            crate::domain::tour::AnchorId::EffectDesignerPreview,
                            preview_rect,
                        );
                        self.preview(left, t);
                        let controls_rect = right.min_rect();
                        crate::domain::tour::anchor(
                            &ctx_ref,
                            crate::domain::tour::AnchorId::EffectDesignerControls,
                            controls_rect,
                        );
                        self.controls(right);
                    });
                });
            });
    }

    /// "Effect Designer" title (left) + Save/Reset (right), one row.
    fn title_row(&mut self, ui: &mut egui::Ui, cmd: &CommandTx, page: &mut Page) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(t!("misc.designer_title"))
                    .font(theme::bold(22.0))
                    .color(theme::TEXT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                let save_btn = widgets::button(
                    ui,
                    &t!("misc.designer_save"),
                    widgets::ButtonKind::Primary,
                    egui::vec2(120.0, 32.0),
                );
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::EffectDesignerSave,
                    save_btn.rect,
                );
                if save_btn.clicked() {
                    let name = self.name.trim().to_string();
                    if validate_effect_name(&name) {
                        let params = self.params.to_params();
                        crate::domain::actions::lighting::save_custom_effect(
                            cmd,
                            &name,
                            params.clone(),
                        );
                        if let Some(old) = &self.editing {
                            if old != &name {
                                crate::domain::actions::lighting::delete_custom_effect(cmd, old);
                            }
                        }
                        if let Some((instance_id, effect_id)) = &self.canvas_instance {
                            crate::domain::actions::lighting::canvas_upsert_effect(
                                cmd,
                                instance_id,
                                EffectDef {
                                    effect_id: effect_id.clone(),
                                    name: Some(name.clone()),
                                    params: params.clone(),
                                },
                            );
                        }
                        // Only the Direct Effects tab drains `result` (on the
                        // frame after navigating back to it) — leaving it set
                        // when returning to a canvas instance would misapply it
                        // whenever the user later opens Direct Effects.
                        if self.canvas_instance.is_none() {
                            self.result = Some((name, params));
                        }
                        *page = self.back_page.clone();
                    } else {
                        self.error = Some(t!("misc.designer_invalid_name").to_string());
                    }
                }
                ui.add_space(8.0);
                if widgets::button(
                    ui,
                    &t!("misc.designer_reset"),
                    widgets::ButtonKind::Ghost,
                    egui::vec2(80.0, 32.0),
                )
                .clicked()
                {
                    self.params = DesignerParams::default();
                }
            });
        });
    }

    /// Subtitle (left) + name field/validation error (right), one row.
    fn subtitle_row(&mut self, ui: &mut egui::Ui) {
        ui.horizontal(|ui| {
            ui.label(
                egui::RichText::new(t!("misc.designer_subtitle"))
                    .font(theme::body(12.0))
                    .color(theme::TEXT_MUT),
            );
            ui.with_layout(egui::Layout::right_to_left(egui::Align::Center), |ui| {
                widgets::text_field(ui, &mut self.name, &t!("misc.designer_name_hint"), 240.0);
                if let Some(err) = &self.error {
                    ui.label(
                        egui::RichText::new(err)
                            .font(theme::body(11.0))
                            .color(theme::hex(0xef5f63)),
                    );
                }
            });
        });
    }

    // ── Preview ───────────────────────────────────────────────────────────

    fn preview(&self, ui: &mut egui::Ui, t: f32) {
        widgets::card_titled(
            ui,
            &t!("misc.designer_live_preview"),
            |_ui| {},
            |ui| {
                self.rig(
                    ui,
                    &t!("misc.designer_rig_strip"),
                    &strip_positions(40),
                    120.0,
                    None,
                    t,
                );
                ui.add_space(12.0);
                self.rig(
                    ui,
                    &t!("misc.designer_rig_ring"),
                    &ring_positions(32),
                    180.0,
                    Some((1.0, 1.0)),
                    t,
                );
                ui.add_space(12.0);
                self.rig(
                    ui,
                    &t!("misc.designer_rig_matrix"),
                    &matrix_positions(8, 4),
                    140.0,
                    Some((8.0, 4.0)),
                    t,
                );
                ui.add_space(12.0);
                self.rig(
                    ui,
                    &t!("misc.designer_rig_triple"),
                    &multi_ring_positions(3, 12),
                    110.0,
                    Some((3.0, 1.0)),
                    t,
                );
                ui.add_space(10.0);
                ui.separator();
                ui.add_space(8.0);
                self.pixmap_strip(ui, t);
            },
        );
        ui.add_space(12.0);
        widgets::card_titled(
            ui,
            &t!("misc.designer_brightness_formula"),
            |ui| {
                ui.label(
                    egui::RichText::new(formula_text(self.params.generator))
                        .font(theme::mono_semibold(11.0))
                        .color(theme::CYAN),
                );
            },
            |ui| self.brightness_graph(ui, t),
        );
    }

    /// A thin horizontal strip showing the pixmap effect as it would appear
    /// on the canvas: `color(p, p, 0.5, t)` sampled across the width.
    fn pixmap_strip(&self, ui: &mut egui::Ui, t: f32) {
        let w = ui.available_width();
        let h = 28.0;
        let (rect, _resp) = ui.allocate_exact_size(Vec2::new(w, h), Sense::hover());
        let p = ui.painter();
        let steps = rect.width() as usize;
        if steps > 0 {
            for i in 0..steps {
                let x = rect.left() + i as f32;
                let px = i as f32 / (steps - 1).max(1) as f32;
                let (r, g, b) = self.params.color(px, px, 0.5, t);
                let col = Color32::from_rgb(
                    (r.clamp(0.0, 1.0) * 255.0) as u8,
                    (g.clamp(0.0, 1.0) * 255.0) as u8,
                    (b.clamp(0.0, 1.0) * 255.0) as u8,
                );
                p.line_segment(
                    [Pos2::new(x, rect.top()), Pos2::new(x, rect.bottom())],
                    Stroke::new(1.0, col),
                );
            }
        }
        p.rect_stroke(
            rect,
            0.0,
            Stroke::new(1.0, theme::hex(0x2e3a50)),
            egui::StrokeKind::Inside,
        );
    }

    /// The `p` this instance's `ring_scope` selects for a LED at whole-zone
    /// position `p_zone` and ring-local position `p_ring` — mirrors the
    /// daemon's `Designer::led_color`.
    fn effective_p(&self, p_zone: f32, p_ring: f32) -> f32 {
        match self.params.ring_scope {
            RingScope::Zone => p_zone,
            RingScope::PerRing => p_ring,
        }
    }

    /// Paints `positions` — `(p_zone, p_ring, nx, ny)`: `p_zone`/`p_ring` are
    /// the LED's fractional chain position across the whole zone and within
    /// its own ring respectively (see `effective_p`, matching the daemon's
    /// real LED order); `(nx, ny)` is its normalized on-screen position
    /// (matches the daemon's spatial coordinates, used only for the twinkle
    /// hash) — inside the allocated rig area. `aspect`, when set, keeps the
    /// plotted region at that width:height ratio (e.g. `(1,1)` so a ring
    /// stays a circle, never stretched into an ellipse by an arbitrary rect)
    /// instead of filling the whole rect.
    fn rig(
        &self,
        ui: &mut egui::Ui,
        label: &str,
        positions: &[(f32, f32, f32, f32)],
        height: f32,
        aspect: Option<(f32, f32)>,
        t: f32,
    ) {
        ui.label(
            egui::RichText::new(label)
                .font(theme::body(10.5))
                .color(theme::TEXT_FAINT),
        );
        ui.add_space(4.0);
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), height), Sense::hover());
        let painter = ui.painter();
        painter.rect_filled(rect, 8.0, theme::hex(0x070a0f));
        painter.rect_stroke(
            rect,
            8.0,
            Stroke::new(1.0, theme::BORDER),
            egui::StrokeKind::Middle,
        );
        let margin = 16.0;
        let inner = rect.shrink(margin);
        let plot = match aspect {
            Some((aw, ah)) => fit_aspect_rect(inner, aw, ah),
            None => inner,
        };
        for &(p_zone, p_ring, nx, ny) in positions {
            let pos = Pos2::new(
                plot.left() + nx * plot.width(),
                plot.top() + ny * plot.height(),
            );
            let p = self.effective_p(p_zone, p_ring);
            let (r, g, b) = self.params.color(p, nx, ny, t);
            let color = Color32::from_rgb((r * 255.0) as u8, (g * 255.0) as u8, (b * 255.0) as u8);
            painter.circle_filled(pos, 4.0, color);
        }
    }

    fn brightness_graph(&self, ui: &mut egui::Ui, t: f32) {
        let (rect, _) =
            ui.allocate_exact_size(Vec2::new(ui.available_width(), 90.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 6.0, theme::hex(0x070a0f));
        const SAMPLES: usize = 64;
        let mut pts = Vec::with_capacity(SAMPLES);
        for i in 0..SAMPLES {
            let x = i as f32 / (SAMPLES - 1) as f32;
            let b = self.params.brightness(x, x, 0.5, t);
            pts.push(Pos2::new(
                rect.left() + x * rect.width(),
                rect.bottom() - 6.0 - b * (rect.height() - 12.0),
            ));
        }
        // Faded primary accent under the curve, solid primary for the line.
        widgets::fill_under_line(p, &pts, rect.bottom(), theme::a(theme::CYAN, 0.12));
        p.add(egui::Shape::line(pts, Stroke::new(1.6, theme::CYAN)));
    }

    // ── Controls ──────────────────────────────────────────────────────────

    fn controls(&mut self, ui: &mut egui::Ui) {
        widgets::card_titled(
            ui,
            &t!("misc.designer_generator"),
            |_ui| {},
            |ui| {
                let gap = 8.0;
                let cols = 3usize;
                let cell_w =
                    ((ui.available_width() - gap * (cols as f32 - 1.0)) / cols as f32).max(60.0);
                for (row, chunk) in Generator::ALL.chunks(cols).enumerate() {
                    if row > 0 {
                        ui.add_space(gap);
                    }
                    ui.horizontal(|ui| {
                        ui.spacing_mut().item_spacing.x = gap;
                        for &g in chunk {
                            let active = self.params.generator == g;
                            let shape = generator_shape_samples(&self.params, g, 64);
                            if widgets::effect_cell(
                                ui,
                                &generator_label(g),
                                active,
                                widgets::CellPreview::Curve(shape),
                                cell_w,
                                58.0,
                                26.0,
                                true,
                            ) {
                                self.params.generator = g;
                            }
                        }
                    });
                }
            },
        );

        ui.add_space(16.0);
        widgets::card_titled(
            ui,
            &t!("misc.designer_motion"),
            |_ui| {},
            |ui| {
                ui.label(
                    egui::RichText::new(t!("misc.designer_direction"))
                        .font(theme::body(10.5))
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for &d in Direction::ALL.iter() {
                        if widgets::pill(ui, &direction_label(d), self.params.direction == d) {
                            self.params.direction = d;
                        }
                    }
                });
                ui.add_space(14.0);
                let mut speed = self.params.speed;
                let readout = format!("{}", speed.round() as i32);
                if widgets::slider_row(
                    ui,
                    &t!("misc.designer_speed"),
                    &mut speed,
                    0.0..=100.0,
                    &readout,
                ) {
                    self.params.speed = speed;
                }
                ui.add_space(14.0);
                let mut density = self.params.density;
                let readout = format!("{}x", density.round() as i32);
                if widgets::slider_row(
                    ui,
                    &t!("misc.designer_repeats"),
                    &mut density,
                    1.0..=8.0,
                    &readout,
                ) {
                    self.params.density = density.round();
                }
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(t!("misc.designer_ring_scope"))
                        .font(theme::body(10.5))
                        .color(theme::TEXT_MUT),
                );
                ui.add_space(6.0);
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for &s in RingScope::ALL.iter() {
                        if widgets::pill(ui, &ring_scope_label(s), self.params.ring_scope == s) {
                            self.params.ring_scope = s;
                        }
                    }
                });
                ui.add_space(14.0);
                let mut spread = self.params.phase_spread;
                let readout = format!("{}%", spread.round() as i32);
                if widgets::slider_row(
                    ui,
                    &t!("misc.designer_phase_spread"),
                    &mut spread,
                    0.0..=100.0,
                    &readout,
                ) {
                    self.params.phase_spread = spread;
                }
            },
        );

        ui.add_space(16.0);
        let rel = slider_relevance(self.params.generator);
        widgets::card_titled(
            ui,
            &t!("misc.designer_shape"),
            |_ui| {},
            |ui| {
                ui.add_enabled_ui(rel.decay, |ui| {
                    let mut decay = self.params.decay;
                    let readout = format!("{}%", decay.round() as i32);
                    if widgets::slider_row(
                        ui,
                        &t!("misc.designer_decay"),
                        &mut decay,
                        0.0..=100.0,
                        &readout,
                    ) {
                        self.params.decay = decay;
                    }
                });
                ui.add_space(14.0);
                ui.add_enabled_ui(rel.width, |ui| {
                    let mut width = self.params.width;
                    let readout = format!("{}%", width.round() as i32);
                    if widgets::slider_row(
                        ui,
                        &t!("misc.designer_width"),
                        &mut width,
                        0.0..=100.0,
                        &readout,
                    ) {
                        self.params.width = width;
                    }
                });
                ui.add_space(14.0);
                let mut sharp = self.params.sharpness;
                let readout = format!("{}%", sharp.round() as i32);
                if widgets::slider_row(
                    ui,
                    &t!("misc.designer_sharpness"),
                    &mut sharp,
                    0.0..=100.0,
                    &readout,
                ) {
                    self.params.sharpness = sharp;
                }
            },
        );

        ui.add_space(16.0);
        widgets::card_titled(
            ui,
            &t!("misc.designer_color"),
            |_ui| {},
            |ui| {
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = 6.0;
                    for &m in ColorMode::ALL.iter() {
                        if widgets::pill(ui, &color_mode_label(m), self.params.color_mode == m) {
                            self.params.color_mode = m;
                        }
                    }
                });
                ui.add_space(12.0);
                ui.label(
                    egui::RichText::new(t!("misc.designer_color_a"))
                        .font(theme::body(10.5))
                        .color(theme::TEXT_MUT),
                );
                if let Some(c) = widgets::color_picker(ui, self.params.color_a) {
                    self.params.color_a = c;
                }
                if self.params.color_mode == ColorMode::Gradient {
                    ui.add_space(10.0);
                    ui.label(
                        egui::RichText::new(t!("misc.designer_color_b"))
                            .font(theme::body(10.5))
                            .color(theme::TEXT_MUT),
                    );
                    if let Some(c) = widgets::color_picker(ui, self.params.color_b) {
                        self.params.color_b = c;
                    }
                }
                ui.add_space(14.0);
                let mut floor = self.params.floor;
                let readout = format!("{}%", floor.round() as i32);
                if widgets::slider_row(
                    ui,
                    &t!("misc.designer_brightness_floor"),
                    &mut floor,
                    0.0..=100.0,
                    &readout,
                ) {
                    self.params.floor = floor;
                }
                if self.params.color_mode != ColorMode::Gradient {
                    ui.add_space(14.0);
                    let mut sat = self.params.saturation;
                    let readout = format!("{}%", sat.round() as i32);
                    if widgets::slider_row(
                        ui,
                        &t!("misc.designer_saturation"),
                        &mut sat,
                        0.0..=100.0,
                        &readout,
                    ) {
                        self.params.saturation = sat;
                    }
                    ui.add_space(14.0);
                    let mut hue = self.params.hue_drift;
                    let sign = if hue > 0.0 { "+" } else { "" };
                    let readout = format!("{sign}{}", hue.round() as i32);
                    if widgets::slider_row(
                        ui,
                        &t!("misc.designer_hue_drift"),
                        &mut hue,
                        -100.0..=100.0,
                        &readout,
                    ) {
                        self.params.hue_drift = hue;
                    }
                }
                ui.add_space(14.0);
                let mut ccs = self.params.color_cycle_speed;
                let readout = format!("{}%", ccs.round() as i32);
                if widgets::slider_row(
                    ui,
                    &t!("misc.designer_color_cycle_speed"),
                    &mut ccs,
                    0.0..=100.0,
                    &readout,
                ) {
                    self.params.color_cycle_speed = ccs;
                }
                ui.add_space(14.0);
                ui.label(
                    egui::RichText::new(t!("misc.designer_ambient_color"))
                        .font(theme::body(10.5))
                        .color(theme::TEXT_MUT),
                );
                if let Some(c) = widgets::color_picker(ui, self.params.ambient_color) {
                    self.params.ambient_color = c;
                }
            },
        );
    }
}

// ── Export / import (GUI-side file ops) ──────────────────────────────────

/// Serialize a saved effect to `<name>.halofx.json` via a native save dialog,
/// run on a background thread so the picker never blocks the UI event loop.
pub fn spawn_export(ctx: &egui::Context, def: &EffectDef) {
    let name = def.name.clone().unwrap_or_default();
    let params = def.params.clone();
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        if let Some(path) = rfd::FileDialog::new()
            .set_file_name(format!("{name}.halofx.json"))
            .add_filter("HaloDaemon effect", &["json"])
            .save_file()
        {
            let value = serde_json::json!({
                "halofx": EXPORT_VERSION,
                "name": name,
                "params": params,
            });
            if let Ok(bytes) = serde_json::to_vec_pretty(&value) {
                let _ = std::fs::write(path, bytes);
            }
        }
        ctx.request_repaint();
    });
}

/// Open a native file picker, parse+validate the chosen file, and save it as
/// a custom effect directly (no modal round-trip). `cmd` is a cheap-to-clone
/// channel handle, so the send happens straight from the background thread.
pub fn spawn_import(ctx: &egui::Context, cmd: CommandTx) {
    let ctx = ctx.clone();
    std::thread::spawn(move || {
        let Some(path) = rfd::FileDialog::new()
            .add_filter("HaloDaemon effect", &["json"])
            .pick_file()
        else {
            return;
        };
        if let Ok(bytes) = std::fs::read(&path) {
            match parse_import(&bytes) {
                Ok((name, params)) => {
                    crate::domain::actions::lighting::save_custom_effect(
                        &cmd,
                        &name,
                        params.to_params(),
                    );
                }
                Err(e) => log::warn!("failed to import effect from {path:?}: {e}"),
            }
        }
        ctx.request_repaint();
    });
}

fn parse_import(bytes: &[u8]) -> Result<(String, DesignerParams), String> {
    let value: serde_json::Value = serde_json::from_slice(bytes).map_err(|e| e.to_string())?;
    let version = value.get("halofx").and_then(|v| v.as_u64()).unwrap_or(0);
    if version != EXPORT_VERSION {
        return Err(format!("unsupported halofx version {version}"));
    }
    let name = value
        .get("name")
        .and_then(|v| v.as_str())
        .unwrap_or_default()
        .to_string();
    if !validate_effect_name(&name) {
        return Err("invalid effect name".to_string());
    }
    let params = value
        .get("params")
        .cloned()
        .and_then(|p| serde_json::from_value(p).ok())
        .unwrap_or_default();
    Ok((name, DesignerParams::from_params(&params)))
}

// ── Pure geometry / labels (unit-testable) ───────────────────────────────

/// The largest `aspect_w:aspect_h` rect centered inside `inner`, so plotted
/// content (e.g. a ring) keeps its true proportions instead of stretching to
/// fill whatever rect the layout happened to allocate.
fn fit_aspect_rect(inner: Rect, aspect_w: f32, aspect_h: f32) -> Rect {
    let target_ratio = aspect_w / aspect_h;
    let inner_ratio = inner.width() / inner.height();
    let size = if inner_ratio > target_ratio {
        Vec2::new(inner.height() * target_ratio, inner.height())
    } else {
        Vec2::new(inner.width(), inner.width() / target_ratio)
    };
    Rect::from_center_size(inner.center(), size)
}

/// Each rig returns `(p_zone, p_ring, nx, ny)`: `p_zone`/`p_ring` are the
/// LED's fractional position in chain (wiring) order across the whole zone
/// and within its own ring (equal for a single-ring rig — strip/ring/matrix
/// have exactly one "ring"), `(nx, ny)` its normalized on-screen position —
/// mirroring `direct_zone_colors`, which derives these from a real zone's
/// `leds` index rather than spatial x/y (a ring or matrix has no "linear"
/// screen axis to sweep over, but it does have a wiring order).
fn strip_positions(n: usize) -> Vec<(f32, f32, f32, f32)> {
    let denom = (n.max(2) - 1) as f32;
    (0..n)
        .map(|i| {
            let p = i as f32 / denom;
            (p, p, p, 0.5)
        })
        .collect()
}

fn ring_positions(n: usize) -> Vec<(f32, f32, f32, f32)> {
    let denom = (n.max(2) - 1) as f32;
    (0..n)
        .map(|i| {
            let a = -std::f32::consts::FRAC_PI_2 + (i as f32 / n as f32) * std::f32::consts::TAU;
            let p = i as f32 / denom;
            (p, p, 0.5 + 0.5 * a.cos(), 0.5 + 0.5 * a.sin())
        })
        .collect()
}

fn matrix_positions(cols: usize, rows: usize) -> Vec<(f32, f32, f32, f32)> {
    let mut v = Vec::with_capacity(cols * rows);
    let denom = (cols * rows).max(2) as f32 - 1.0;
    let mut i = 0usize;
    for r in 0..rows {
        for c in 0..cols {
            let p = i as f32 / denom;
            v.push((
                p,
                p,
                c as f32 / (cols.max(2) - 1) as f32,
                r as f32 / (rows.max(2) - 1) as f32,
            ));
            i += 1;
        }
    }
    v
}

/// A multi-ring hub (e.g. three fans wired as one zone): `rings` independent
/// circles of `per_ring` LEDs each, arranged in a row. `p_zone` runs once
/// nose-to-tail across every ring; `p_ring` restarts at each ring's first
/// LED — the same distinction `ring_scope` selects between on real hardware.
/// Pass aspect `(rings as f32, 1.0)` to `rig` so each ring plots as an
/// undistorted circle.
fn multi_ring_positions(rings: usize, per_ring: usize) -> Vec<(f32, f32, f32, f32)> {
    let total = rings * per_ring;
    let denom_zone = (total.max(2) - 1) as f32;
    let denom_ring = (per_ring.max(2) - 1) as f32;
    let mut v = Vec::with_capacity(total);
    for r in 0..rings {
        for j in 0..per_ring {
            let i = r * per_ring + j;
            let p_zone = i as f32 / denom_zone;
            let p_ring = j as f32 / denom_ring;
            let a =
                -std::f32::consts::FRAC_PI_2 + (j as f32 / per_ring as f32) * std::f32::consts::TAU;
            let ux = 0.5 + 0.35 * a.cos();
            let uy = 0.5 + 0.35 * a.sin();
            let nx = (r as f32 + ux) / rings as f32;
            v.push((p_zone, p_ring, nx, uy));
        }
    }
    v
}

struct Relevance {
    decay: bool,
    width: bool,
}

fn slider_relevance(g: Generator) -> Relevance {
    Relevance {
        decay: matches!(
            g,
            Generator::Pulse | Generator::Comet | Generator::Twinkle | Generator::Rain
        ),
        width: matches!(g, Generator::Pulse | Generator::Comet | Generator::Rain),
    }
}

fn generator_label(g: Generator) -> std::borrow::Cow<'static, str> {
    match g {
        Generator::Sine => t!("misc.designer_gen_sine"),
        Generator::Pulse => t!("misc.designer_gen_pulse"),
        Generator::Comet => t!("misc.designer_gen_comet"),
        Generator::Sawtooth => t!("misc.designer_gen_sawtooth"),
        Generator::Twinkle => t!("misc.designer_gen_twinkle"),
        Generator::Noise => t!("misc.designer_gen_noise"),
        Generator::Rain => t!("misc.designer_gen_rain"),
    }
}

fn direction_label(d: Direction) -> std::borrow::Cow<'static, str> {
    match d {
        Direction::Forward => t!("misc.designer_dir_forward"),
        Direction::Reverse => t!("misc.designer_dir_reverse"),
        Direction::Center => t!("misc.designer_dir_center"),
    }
}

fn color_mode_label(m: ColorMode) -> std::borrow::Cow<'static, str> {
    match m {
        ColorMode::Solid => t!("misc.designer_cmode_solid"),
        ColorMode::Gradient => t!("misc.designer_cmode_gradient"),
        ColorMode::Spectrum => t!("misc.designer_cmode_spectrum"),
    }
}

fn ring_scope_label(s: RingScope) -> std::borrow::Cow<'static, str> {
    match s {
        RingScope::Zone => t!("misc.designer_ring_whole"),
        RingScope::PerRing => t!("misc.designer_ring_separate"),
    }
}

/// `n` brightness samples of `base` with its generator swapped to `g`, for
/// the generator picker's shape preview — shows the actual waveform rather
/// than a generic swatch, and reflects the instance's own decay/width/
/// sharpness/floor so it updates live as those sliders move. Density is
/// pinned to a few repeats regardless of the instance's own setting — at
/// density 1 most waveforms show only a single hump in the small cell,
/// which reads as a blob rather than a recognizable shape. Callers must
/// pass enough samples (`n`) to render `PREVIEW_DENSITY` repeats smoothly —
/// each repeat needs several samples or the straight line segments between
/// points make the curve look jagged instead of round.
fn generator_shape_samples(base: &DesignerParams, g: Generator, n: usize) -> Vec<f32> {
    const PREVIEW_DENSITY: f32 = 3.0;
    let params = DesignerParams {
        generator: g,
        density: PREVIEW_DENSITY,
        ..*base
    };
    (0..n)
        .map(|i| {
            let p = i as f32 / (n.max(2) - 1) as f32;
            params.brightness(p, p, 0.5, 0.0)
        })
        .collect()
}

fn formula_text(g: Generator) -> &'static str {
    match g {
        Generator::Sine => {
            "b = \u{00BD} + \u{00BD}\u{00B7}sin(2\u{03C0}(p\u{00B7}\u{03C1} \u{2212} v\u{00B7}t))"
        }
        Generator::Pulse => {
            "b = e^(\u{2212}\u{0394}\u{00B2}/2w\u{00B2}) \u{2228} e^(\u{2212}\u{0394}/decay)"
        }
        Generator::Comet => "b = e^(\u{2212}\u{03B4}/decay) \u{00B7} head(w)",
        Generator::Sawtooth => "b = frac(p\u{00B7}\u{03C1} \u{2212} v\u{00B7}t) ^ sharp",
        Generator::Twinkle => "b = e^(\u{2212}local k\u{00B7}(6\u{2212}decay))",
        Generator::Noise => "b = noise(p\u{00B7}\u{03C1} + v\u{00B7}t) ^ sharp",
        Generator::Rain => "b = \u{03A3} drop(i, p, t) \u{00B7} life(i)",
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::EffectParamValue;

    #[test]
    fn fit_aspect_rect_keeps_square_aspect_inside_a_wide_rect() {
        let inner = Rect::from_min_size(Pos2::ZERO, Vec2::new(400.0, 100.0));
        let square = fit_aspect_rect(inner, 1.0, 1.0);
        assert!((square.width() - square.height()).abs() < 1e-4);
        assert!((square.height() - inner.height()).abs() < 1e-4);
        assert!((square.center() - inner.center()).length() < 1e-4);
    }

    #[test]
    fn fit_aspect_rect_keeps_square_aspect_inside_a_tall_rect() {
        let inner = Rect::from_min_size(Pos2::ZERO, Vec2::new(100.0, 400.0));
        let square = fit_aspect_rect(inner, 1.0, 1.0);
        assert!((square.width() - square.height()).abs() < 1e-4);
        assert!((square.width() - inner.width()).abs() < 1e-4);
    }

    #[test]
    fn fit_aspect_rect_preserves_a_non_square_ratio() {
        let inner = Rect::from_min_size(Pos2::ZERO, Vec2::new(500.0, 500.0));
        let wide = fit_aspect_rect(inner, 8.0, 4.0);
        assert!((wide.width() / wide.height() - 2.0).abs() < 1e-4);
    }

    #[test]
    fn strip_positions_spans_0_to_1_and_is_monotonic() {
        let positions = strip_positions(40);
        assert_eq!(positions.len(), 40);
        assert_eq!(positions[0].0, 0.0);
        assert!((positions[39].0 - 1.0).abs() < 1e-6);
        for w in positions.windows(2) {
            assert!(w[1].0 > w[0].0, "chain position p must be monotonic");
        }
    }

    #[test]
    fn ring_positions_stay_on_unit_circle_and_p_is_monotonic() {
        let positions = ring_positions(32);
        for &(p_zone, p_ring, nx, ny) in &positions {
            assert_eq!(
                p_zone, p_ring,
                "a single ring has no separate ring-local position"
            );
            let dx = nx - 0.5;
            let dy = ny - 0.5;
            let r = (dx * dx + dy * dy).sqrt();
            assert!((r - 0.5).abs() < 1e-4, "r={r}");
        }
        for w in positions.windows(2) {
            assert!(w[1].0 > w[0].0, "chain position p must be monotonic");
        }
    }

    #[test]
    fn matrix_positions_counts_bounds_and_p_is_monotonic() {
        let positions = matrix_positions(8, 4);
        assert_eq!(positions.len(), 32);
        for &(p_zone, p_ring, nx, ny) in &positions {
            assert_eq!(p_zone, p_ring);
            assert!((0.0..=1.0).contains(&p_zone));
            assert!((0.0..=1.0).contains(&nx));
            assert!((0.0..=1.0).contains(&ny));
        }
        for w in positions.windows(2) {
            assert!(w[1].0 > w[0].0, "chain position p must be monotonic");
        }
    }

    #[test]
    fn multi_ring_positions_p_ring_restarts_each_ring_while_p_zone_is_monotonic() {
        let positions = multi_ring_positions(3, 12);
        assert_eq!(positions.len(), 36);
        for w in positions.windows(2) {
            assert!(
                w[1].0 > w[0].0,
                "p_zone must be monotonic across the whole hub"
            );
        }
        assert_eq!(positions[0].1, 0.0);
        assert_eq!(positions[12].1, 0.0);
        assert_eq!(positions[24].1, 0.0);
        for &(_, _, nx, ny) in &positions {
            assert!((0.0..=1.0).contains(&nx));
            assert!((0.0..=1.0).contains(&ny));
        }
    }

    #[test]
    fn generator_shape_samples_has_len_and_stays_in_unit_range() {
        let base = DesignerParams::default();
        for &g in Generator::ALL.iter() {
            let samples = generator_shape_samples(&base, g, 24);
            assert_eq!(samples.len(), 24);
            for &v in &samples {
                assert!((0.0..=1.0).contains(&v), "generator={g:?} v={v}");
            }
        }
    }

    #[test]
    fn generator_shape_samples_reflects_the_requested_generator_not_base() {
        let base = DesignerParams {
            generator: Generator::Sine,
            ..Default::default()
        };
        let sine = generator_shape_samples(&base, Generator::Sine, 16);
        let sawtooth = generator_shape_samples(&base, Generator::Sawtooth, 16);
        assert_ne!(sine, sawtooth);
    }

    #[test]
    fn slider_relevance_matches_generator_table() {
        assert!(!slider_relevance(Generator::Sine).decay);
        assert!(!slider_relevance(Generator::Sine).width);
        assert!(slider_relevance(Generator::Pulse).decay);
        assert!(slider_relevance(Generator::Pulse).width);
        assert!(slider_relevance(Generator::Comet).decay);
        assert!(slider_relevance(Generator::Comet).width);
        assert!(slider_relevance(Generator::Twinkle).decay);
        assert!(!slider_relevance(Generator::Twinkle).width);
        assert!(!slider_relevance(Generator::Noise).decay);
        assert!(!slider_relevance(Generator::Noise).width);
        assert!(slider_relevance(Generator::Rain).decay);
        assert!(slider_relevance(Generator::Rain).width);
    }

    #[test]
    fn parse_import_round_trips_export() {
        let params = DesignerParams {
            generator: Generator::Comet,
            ..Default::default()
        };
        let value = serde_json::json!({
            "halofx": EXPORT_VERSION,
            "name": "My Comet",
            "params": params.to_params(),
        });
        let bytes = serde_json::to_vec(&value).unwrap();
        let (name, back) = parse_import(&bytes).unwrap();
        assert_eq!(name, "My Comet");
        assert_eq!(back, params);
    }

    #[test]
    fn parse_import_rejects_bad_version_and_name() {
        assert!(parse_import(b"not json").is_err());

        let bad_version = serde_json::json!({
            "halofx": 99,
            "name": "ok",
            "params": {},
        });
        assert!(parse_import(&serde_json::to_vec(&bad_version).unwrap()).is_err());

        let bad_name = serde_json::json!({
            "halofx": EXPORT_VERSION,
            "name": "../escape",
            "params": {},
        });
        assert!(parse_import(&serde_json::to_vec(&bad_name).unwrap()).is_err());
    }

    #[test]
    fn parse_import_clamps_out_of_range_params() {
        let mut params = std::collections::HashMap::new();
        params.insert("speed".to_string(), EffectParamValue::Float(9999.0));
        let value = serde_json::json!({
            "halofx": EXPORT_VERSION,
            "name": "Wild",
            "params": params,
        });
        let (_, back) = parse_import(&serde_json::to_vec(&value).unwrap()).unwrap();
        assert_eq!(back.speed, 100.0);
    }

    #[test]
    fn edit_seeds_params_and_name_from_effect_def() {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "generator".to_string(),
            EffectParamValue::Str("noise".into()),
        );
        let def = EffectDef {
            effect_id: halod_shared::effect_designer::DESIGNER_EFFECT_ID.to_string(),
            name: Some("Saved One".to_string()),
            params,
        };
        let ui = DesignerUi::edit(&def);
        assert_eq!(ui.name, "Saved One");
        assert_eq!(ui.editing.as_deref(), Some("Saved One"));
        assert_eq!(ui.params.generator, Generator::Noise);
    }

    #[test]
    fn edit_for_canvas_instance_seeds_params_and_targets_the_canvas() {
        let mut params = std::collections::HashMap::new();
        params.insert(
            "generator".to_string(),
            EffectParamValue::Str("comet".into()),
        );
        let def = EffectDef {
            effect_id: halod_shared::effect_designer::DESIGNER_PIXMAP_EFFECT_ID.to_string(),
            name: None,
            params,
        };
        let ui = DesignerUi::edit_for_canvas_instance(
            &def,
            "inst-1".to_string(),
            "My Comet".to_string(),
        );
        assert_eq!(ui.name, "My Comet");
        assert_eq!(ui.params.generator, Generator::Comet);
        // Returns to the unified RGB Lighting page; the Effects Canvas tab is
        // distinguished by `canvas_instance`, not a dedicated page.
        assert_eq!(ui.back_page, Page::Lighting);
        // No pre-existing library preset is tracked — Save must not delete one.
        assert_eq!(ui.editing, None);
        assert_eq!(
            ui.canvas_instance,
            Some((
                "inst-1".to_string(),
                halod_shared::effect_designer::DESIGNER_PIXMAP_EFFECT_ID.to_string()
            ))
        );
    }

    #[test]
    fn only_non_canvas_edits_carry_a_result_to_the_direct_effects_tab() {
        // Save gates `result` (and the back label / return tab) on
        // `canvas_instance`: a canvas-instance edit updates that instance and
        // must NOT leave a pending direct-apply, while New/Edit target the
        // Direct Effects tab and must.
        assert!(DesignerUi::new_effect().canvas_instance.is_none());

        let def = EffectDef {
            effect_id: "custom".into(),
            name: Some("Comet".into()),
            params: HashMap::new(),
        };
        assert!(DesignerUi::edit(&def).canvas_instance.is_none());
        assert!(
            DesignerUi::edit_for_canvas_instance(&def, "inst-1".into(), "Comet".into())
                .canvas_instance
                .is_some()
        );
    }
}
