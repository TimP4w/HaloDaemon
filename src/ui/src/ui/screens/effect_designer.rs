// SPDX-License-Identifier: GPL-3.0-or-later
//! The Effect Designer page: create/edit a procedural Designer effect with live preview.

use crate::ui::components as widgets;
use std::collections::HashMap;

use egui::{Color32, Pos2, Sense, Stroke, Vec2};
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
    /// Preview rig index: strip, ring, matrix, triple ring.
    rig: usize,
    /// Color slot the picker edits (A, B, ambient); `usize::MAX` = closed.
    color_slot: usize,
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
            rig: 0,
            color_slot: 0,
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
            rig: 0,
            color_slot: 0,
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
            rig: 0,
            color_slot: 0,
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
                    if widgets::back_link(ui, &back_label, None) {
                        *page = self.back_page.clone();
                    }
                    self.title_row(ui, cmd, page);

                    let t = ui.input(|i| i.time) as f32;
                    let gap = theme::SPACE_8;
                    let aside_w = (ui.available_width() * 0.32).clamp(360.0, 420.0);
                    let left_w = ui.available_width() - aside_w - gap;
                    let ctx_ref = ui.ctx().clone();
                    widgets::split_columns(ui, left_w, gap, |left, right| {
                        let preview = left.scope(|ui| self.preview(ui, t)).response.rect;
                        crate::domain::tour::anchor(
                            &ctx_ref,
                            crate::domain::tour::AnchorId::EffectDesignerPreview,
                            preview,
                        );
                        let controls = right.scope(|ui| self.controls(ui)).response.rect;
                        crate::domain::tour::anchor(
                            &ctx_ref,
                            crate::domain::tour::AnchorId::EffectDesignerControls,
                            controls,
                        );
                    });
                });
            });
    }

    /// Title + name field (left), Reset + Save (right).
    fn title_row(&mut self, ui: &mut egui::Ui, cmd: &CommandTx, page: &mut Page) {
        let mut save = false;
        let mut reset = false;
        widgets::page_title_with(
            ui,
            &t!("misc.designer_title"),
            |ui| {
                ui.add_space(theme::SPACE_1);
                widgets::text_field(ui, &mut self.name, &t!("misc.designer_name_hint"), 240.0);
                if let Some(err) = &self.error {
                    ui.label(
                        egui::RichText::new(err)
                            .font(theme::body_sm())
                            .color(theme::OFFLINE),
                    );
                }
            },
            |ui| {
                let save_btn = widgets::button(
                    ui,
                    &t!("misc.designer_save"),
                    widgets::ButtonKind::Primary,
                    egui::vec2(0.0, 36.0),
                );
                crate::domain::tour::anchor(
                    ui.ctx(),
                    crate::domain::tour::AnchorId::EffectDesignerSave,
                    save_btn.rect,
                );
                save = save_btn.clicked();
                reset = widgets::button(
                    ui,
                    &t!("misc.designer_reset"),
                    widgets::ButtonKind::Secondary,
                    egui::vec2(0.0, 36.0),
                )
                .clicked();
            },
        );
        if reset {
            self.params = DesignerParams::default();
        }
        if save {
            self.save(cmd, page);
        }
    }

    fn save(&mut self, cmd: &CommandTx, page: &mut Page) {
        let name = self.name.trim().to_string();
        if !validate_effect_name(&name) {
            self.error = Some(t!("misc.designer_invalid_name").to_string());
            return;
        }
        let params = self.params.to_params();
        crate::runtime::ipc::send(
            cmd,
            halod_shared::commands::DaemonCommand::SaveCustomEffect {
                name: name.clone(),
                params: params.clone(),
            },
        );
        if let Some(old) = &self.editing {
            if old != &name {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::DeleteCustomEffect { name: old.clone() },
                );
            }
        }
        if let Some((instance_id, effect_id)) = &self.canvas_instance {
            crate::runtime::ipc::send(
                cmd,
                halod_shared::commands::DaemonCommand::CanvasUpsertEffect {
                    instance_id: instance_id.clone(),
                    def: EffectDef {
                        effect_id: effect_id.clone(),
                        name: Some(name.clone()),
                        params: params.clone(),
                    },
                },
            );
        }
        // Only the Direct Effects tab drains `result` (on the frame after
        // navigating back to it) — leaving it set when returning to a canvas
        // instance would misapply it whenever the user later opens Direct Effects.
        if self.canvas_instance.is_none() {
            self.result = Some((name, params));
        }
        *page = self.back_page.clone();
    }

    // ── Preview ───────────────────────────────────────────────────────────

    fn preview(&mut self, ui: &mut egui::Ui, t: f32) {
        widgets::card_with_margin(ui, PANEL_PAD, |ui| {
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(t!("misc.designer_live_preview"))
                            .font(theme::heading())
                            .color(theme::TEXT),
                    );
                },
                |ui| {
                    let labels = [
                        t!("misc.designer_rig_strip"),
                        t!("misc.designer_rig_ring"),
                        t!("misc.designer_rig_matrix"),
                        t!("misc.designer_rig_triple"),
                    ];
                    let refs: Vec<&str> = labels.iter().map(|l| l.as_ref()).collect();
                    widgets::segmented(ui, &refs, &mut self.rig, false);
                },
            );
            ui.add_space(theme::SPACE_8);
            let (rect, _) =
                ui.allocate_exact_size(Vec2::new(ui.available_width(), 180.0), Sense::hover());
            ui.painter().rect_filled(rect, 8.0, theme::STAGE_BG);
            let inner = rect.shrink2(Vec2::new(20.0, 16.0));
            match self.rig {
                0 => self.strip(ui.painter(), inner, 40, t),
                1 => self.rig(
                    ui.painter(),
                    inner,
                    &ring_positions(32),
                    Some((1.0, 1.0)),
                    t,
                ),
                2 => self.rig(
                    ui.painter(),
                    inner,
                    &matrix_positions(8, 4),
                    Some((8.0, 4.0)),
                    t,
                ),
                _ => self.rig(
                    ui.painter(),
                    inner,
                    &multi_ring_positions(3, 12),
                    Some((3.0, 1.0)),
                    t,
                ),
            }
        });
        ui.add_space(theme::SPACE_8);
        widgets::card_with_margin(ui, PANEL_PAD, |ui| {
            egui::Sides::new().show(
                ui,
                |ui| {
                    ui.label(
                        egui::RichText::new(t!("misc.designer_brightness_formula"))
                            .font(theme::heading())
                            .color(theme::TEXT),
                    );
                },
                |ui| {
                    ui.label(
                        egui::RichText::new(t!("misc.designer_curve_caption"))
                            .font(theme::body_sm())
                            .color(theme::TEXT_MUT),
                    );
                },
            );
            ui.add_space(theme::SPACE_6);
            self.brightness_graph(ui, t);
        });
    }

    /// The strip rig: `n` round LEDs filling the row, `gap` px apart.
    fn strip(&self, painter: &egui::Painter, inner: egui::Rect, n: usize, t: f32) {
        let gap = 6.0;
        let d = ((inner.width() - gap * (n - 1) as f32) / n as f32).max(2.0);
        for (i, &(p_zone, p_ring, nx, ny)) in strip_positions(n).iter().enumerate() {
            let c = Pos2::new(
                inner.left() + d / 2.0 + i as f32 * (d + gap),
                inner.center().y,
            );
            let p = self.effective_p(p_zone, p_ring);
            painter.circle_filled(c, d / 2.0, self.led_color(p, nx, ny, t));
        }
    }

    fn led_color(&self, p: f32, nx: f32, ny: f32, t: f32) -> Color32 {
        let (r, g, b) = self.params.color(p, nx, ny, t);
        let ch = |v: f32| (v.clamp(0.0, 1.0) * 255.0) as u8;
        Color32::from_rgb(ch(r), ch(g), ch(b))
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
    /// hash) — inside `inner`. `aspect`, when set, keeps the plotted region
    /// at that width:height ratio (e.g. `(1,1)` so a ring stays a circle).
    fn rig(
        &self,
        painter: &egui::Painter,
        inner: egui::Rect,
        positions: &[(f32, f32, f32, f32)],
        aspect: Option<(f32, f32)>,
        t: f32,
    ) {
        let plot = match aspect {
            Some((aw, ah)) => widgets::letterbox(inner, aw / ah),
            None => inner,
        };
        for &(p_zone, p_ring, nx, ny) in positions {
            let pos = Pos2::new(
                plot.left() + nx * plot.width(),
                plot.top() + ny * plot.height(),
            );
            let p = self.effective_p(p_zone, p_ring);
            painter.circle_filled(pos, 5.0, self.led_color(p, nx, ny, t));
        }
    }

    fn brightness_graph(&self, ui: &mut egui::Ui, t: f32) {
        let w = ui.available_width();
        let (rect, _) = ui.allocate_exact_size(Vec2::new(w, w / 4.0), Sense::hover());
        let p = ui.painter();
        p.rect_filled(rect, 8.0, theme::INNER_BG);
        let grid = Stroke::new(1.0, theme::STAGE_GRID);
        p.hline(rect.x_range(), rect.center().y, grid);
        for q in 1..4 {
            p.vline(
                rect.left() + rect.width() * q as f32 / 4.0,
                rect.y_range(),
                grid,
            );
        }
        const SAMPLES: usize = 61;
        let pts: Vec<Pos2> = (0..SAMPLES)
            .map(|i| {
                let x = i as f32 / (SAMPLES - 1) as f32;
                let b = self.params.brightness(x, x, 0.5, t);
                Pos2::new(
                    rect.left() + x * rect.width(),
                    rect.bottom() - rect.height() * (10.0 + b * 125.0) / 150.0,
                )
            })
            .collect();
        p.add(egui::Shape::line(pts, Stroke::new(2.0, theme::CYAN)));
    }

    // ── Controls ──────────────────────────────────────────────────────────

    fn controls(&mut self, ui: &mut egui::Ui) {
        widgets::card_with_margin(ui, PANEL_PAD, |ui| {
            ui.spacing_mut().item_spacing.y = theme::SPACE_5;
            ui.vertical(|ui| {
                field(ui, &t!("misc.designer_generator"));
                let options: Vec<_> = Generator::ALL
                    .iter()
                    .map(|&g| (generator_label(g), g))
                    .collect();
                if let Some(g) = widgets::option_grid_value(ui, &options, &self.params.generator, 4)
                {
                    self.params.generator = g;
                }
            });
            let rel = slider_relevance(self.params.generator);
            let p = &mut self.params;
            pct_slider(ui, &t!("misc.designer_speed"), &mut p.speed, "", true);
            let mut density = p.density;
            let readout = format!("{}×", density.round() as i32);
            if widgets::slider_row(
                ui,
                &t!("misc.designer_repeats"),
                &mut density,
                1.0..=8.0,
                &readout,
            ) {
                p.density = density.round();
            }
            pct_slider(ui, &t!("misc.designer_decay"), &mut p.decay, "%", rel.decay);
            pct_slider(ui, &t!("misc.designer_width"), &mut p.width, "%", rel.width);
            pct_slider(
                ui,
                &t!("misc.designer_brightness_floor"),
                &mut p.floor,
                "%",
                true,
            );
            let gradient = p.color_mode == ColorMode::Gradient;
            ui.add_enabled_ui(!gradient, |ui| {
                let mut hue = p.hue_drift;
                let readout = hue_readout(hue);
                if widgets::slider_row(
                    ui,
                    &t!("misc.designer_hue_drift"),
                    &mut hue,
                    -100.0..=100.0,
                    &readout,
                ) {
                    p.hue_drift = hue;
                }
            });
            self.color_section(ui);
            self.motion_section(ui);
        });
    }

    /// Color mode, the color slots sharing one picker, and the color sliders.
    fn color_section(&mut self, ui: &mut egui::Ui) {
        ui.vertical(|ui| {
            field(ui, &t!("misc.designer_color"));
            let labels: Vec<_> = ColorMode::ALL
                .iter()
                .map(|&m| color_mode_label(m))
                .collect();
            let refs: Vec<&str> = labels.iter().map(|l| l.as_ref()).collect();
            let mut idx = ColorMode::ALL
                .iter()
                .position(|&m| m == self.params.color_mode)
                .unwrap_or(0);
            if widgets::segmented(ui, &refs, &mut idx, false) {
                self.params.color_mode = ColorMode::ALL[idx];
            }
            ui.add_space(theme::SPACE_4);
            let gradient = self.params.color_mode == ColorMode::Gradient;
            let slots = [
                (t!("misc.designer_color_a"), self.params.color_a, true),
                (t!("misc.designer_color_b"), self.params.color_b, gradient),
                (
                    t!("misc.designer_ambient_color"),
                    self.params.ambient_color,
                    true,
                ),
            ];
            let shown: Vec<usize> = (0..slots.len()).filter(|&i| slots[i].2).collect();
            if !shown.contains(&self.color_slot) {
                self.color_slot = 0;
            }
            let gap = theme::SPACE_5;
            let w = (ui.available_width() - gap * (shown.len() - 1) as f32) / shown.len() as f32;
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = gap;
                for &i in &shown {
                    let (label, c, _) = &slots[i];
                    let color = widgets::rgb_to_color32(*c);
                    let active = self.color_slot == i;
                    if widgets::color_slot_button(ui, label, color, active, w).clicked() {
                        self.color_slot = if active { usize::MAX } else { i };
                    }
                }
            });
            let target = match self.color_slot {
                0 => Some(&mut self.params.color_a),
                1 => Some(&mut self.params.color_b),
                2 => Some(&mut self.params.ambient_color),
                _ => None,
            };
            if let Some(target) = target {
                ui.add_space(theme::SPACE_4);
                if let Some(c) = widgets::color_picker(ui, *target) {
                    *target = c;
                }
            }
        });
        let p = &mut self.params;
        let gradient = p.color_mode == ColorMode::Gradient;
        ui.add_enabled_ui(!gradient, |ui| {
            pct_slider(
                ui,
                &t!("misc.designer_saturation"),
                &mut p.saturation,
                "%",
                true,
            );
        });
        pct_slider(
            ui,
            &t!("misc.designer_color_cycle_speed"),
            &mut p.color_cycle_speed,
            "%",
            true,
        );
    }

    /// Direction, multi-ring scope, phase spread and sharpness.
    fn motion_section(&mut self, ui: &mut egui::Ui) {
        let p = &mut self.params;
        ui.vertical(|ui| {
            field(ui, &t!("misc.designer_direction"));
            let labels: Vec<_> = Direction::ALL.iter().map(|&d| direction_label(d)).collect();
            let refs: Vec<&str> = labels.iter().map(|l| l.as_ref()).collect();
            let mut idx = Direction::ALL
                .iter()
                .position(|&d| d == p.direction)
                .unwrap_or(0);
            if widgets::segmented(ui, &refs, &mut idx, false) {
                p.direction = Direction::ALL[idx];
            }
        });
        ui.vertical(|ui| {
            field(ui, &t!("misc.designer_ring_scope"));
            let labels: Vec<_> = RingScope::ALL
                .iter()
                .map(|&s| ring_scope_label(s))
                .collect();
            let refs: Vec<&str> = labels.iter().map(|l| l.as_ref()).collect();
            let mut idx = RingScope::ALL
                .iter()
                .position(|&s| s == p.ring_scope)
                .unwrap_or(0);
            if widgets::segmented(ui, &refs, &mut idx, false) {
                p.ring_scope = RingScope::ALL[idx];
            }
        });
        pct_slider(
            ui,
            &t!("misc.designer_phase_spread"),
            &mut p.phase_spread,
            "%",
            true,
        );
        pct_slider(
            ui,
            &t!("misc.designer_sharpness"),
            &mut p.sharpness,
            "%",
            true,
        );
    }
}

/// Inner padding of the designer's cards.
const PANEL_PAD: egui::Margin = egui::Margin::same(18);

/// A muted 12 px label above a control group.
fn field(ui: &mut egui::Ui, label: &str) {
    ui.label(
        egui::RichText::new(label)
            .font(theme::body_sm())
            .color(theme::TEXT_MUT),
    );
    ui.add_space(theme::SPACE_4);
}

/// A 0–100 slider row reading `"{v}{unit}"`, greyed out when not `enabled`.
fn pct_slider(ui: &mut egui::Ui, label: &str, v: &mut f32, unit: &str, enabled: bool) {
    ui.add_enabled_ui(enabled, |ui| {
        let mut x = *v;
        let readout = format!("{}{unit}", x.round() as i32);
        if widgets::slider_row(ui, label, &mut x, 0.0..=100.0, &readout) {
            *v = x;
        }
    });
}

fn hue_readout(hue: f32) -> String {
    let h = hue.round() as i32;
    if h == 0 {
        t!("misc.designer_off").to_string()
    } else {
        format!("{h:+}")
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
            .add_filter(
                format!("{} effect", halod_shared::app::APP_DISPLAY_NAME),
                &["json"],
            )
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
            .add_filter(
                format!("{} effect", halod_shared::app::APP_DISPLAY_NAME),
                &["json"],
            )
            .pick_file()
        else {
            return;
        };
        if let Ok(bytes) = std::fs::read(&path) {
            match parse_import(&bytes) {
                Ok((name, params)) => {
                    crate::runtime::ipc::send(
                        &cmd,
                        halod_shared::commands::DaemonCommand::SaveCustomEffect {
                            name,
                            params: params.to_params(),
                        },
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

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::EffectParamValue;

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
