// SPDX-License-Identifier: GPL-3.0-or-later
//! Lighting tab — effect grid, zone selector, per-LED paint canvas (laid out
//! from the real zone topology) and a color picker. Applies via `RgbApply`.
//!
//! Colour/params are shown per effect: paint and solid expose a colour picker;
//! native effects expose their declared params (and a colour only if they take
//! one); "off" exposes nothing. The preview LEDs reflect the chosen mode —
//! solid → that colour, off → black, other effects → a rainbow, paint → the
//! per-LED buffer. Continuous edits are debounced via [`DeviceUi::queue`].

use crate::ui::components as widgets;
use std::collections::HashMap;

use egui::{Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::effect_designer::DESIGNER_EFFECT_ID;
use halod_shared::types::{
    Animation, ColorStep, DeviceCapability, EffectParamDescriptor, EffectParamValue, NativeEffect,
    ParamKind, RgbColor, RgbState, RgbStatus, RgbZone, ZoneTopology,
};
use halod_shared::zone_transform::ZoneContentTransform;

use super::{DeviceUi, TabCtx};
use crate::ui::components::rgb_to_color32;
use crate::ui::theme;

/// Brand default paint/solid colour when the device reports none.
const DEFAULT_PAINT_COLOR: RgbColor = RgbColor {
    r: 0x5a,
    g: 0xd1,
    b: 0xe8,
};

/// The active lighting mode, derived from the device's `RgbState`.
#[derive(Clone, Debug, PartialEq)]
enum Mode {
    Solid,
    Paint,
    Effect(String),
    Direct(String),
    Engine,
    None,
}

fn current_mode(state: &Option<RgbState>) -> Mode {
    match state {
        Some(RgbState::Static { .. }) => Mode::Solid,
        Some(RgbState::PerLed { .. }) => Mode::Paint,
        Some(RgbState::NativeEffect { id, .. }) => Mode::Effect(id.clone()),
        Some(RgbState::DirectEffect { id, .. }) => Mode::Direct(id.clone()),
        Some(RgbState::Engine) => Mode::Engine,
        None => Mode::None,
    }
}

/// An "off"-style effect shows black LEDs and needs no colour/params.
fn is_off(id: &str, effects: &[NativeEffect]) -> bool {
    let off = |s: &str| {
        let s = s.to_ascii_lowercase();
        s == "off" || s == "none"
    };
    off(id) || effects.iter().any(|e| e.id == id && off(&e.name))
}

/// How the preview LEDs are coloured for the current mode.
#[derive(Debug, PartialEq)]
enum LedFill<'a> {
    Buffer,
    Solid(RgbColor),
    Off,
    Rainbow,
    /// Live per-LED colors from the daemon's canvas frame (engine / direct effects).
    Live(&'a HashMap<u32, RgbColor>),
}

/// Pick the preview fill for the current mode.
fn fill_for<'a>(
    mode: &Mode,
    effects: &[NativeEffect],
    paint_color: Option<RgbColor>,
    live: Option<&'a HashMap<u32, RgbColor>>,
) -> LedFill<'a> {
    match mode {
        Mode::Paint => LedFill::Buffer,
        Mode::Solid => LedFill::Solid(paint_color.unwrap_or(DEFAULT_PAINT_COLOR)),
        Mode::Effect(id) if is_off(id, effects) => LedFill::Off,
        Mode::Direct(_) | Mode::Engine => match live {
            Some(m) => LedFill::Live(m),
            None => LedFill::Rainbow,
        },
        Mode::Effect(_) => LedFill::Rainbow,
        Mode::None => LedFill::Off,
    }
}

pub fn show(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::TabLighting,
        ui.max_rect(),
    );
    let Some(rgb) = ctx.dev.capabilities.iter().find_map(|c| match c {
        DeviceCapability::Rgb(r) => Some(r),
        _ => None,
    }) else {
        return;
    };
    if rgb.descriptor.zones.is_empty() {
        widgets::empty_state(
            ui,
            &t!("lighting.no_zones_title"),
            Some(t!("lighting.no_zones_subtitle").as_ref()),
        );
        return;
    }

    if st.lighting.zone.is_empty()
        || !rgb
            .descriptor
            .zones
            .iter()
            .any(|z| z.id == st.lighting.zone)
    {
        st.lighting.zone = rgb.descriptor.zones[0].id.clone();
    }
    if st.lighting.paint_color.is_none() {
        st.lighting.paint_color = Some(current_color(rgb).unwrap_or(DEFAULT_PAINT_COLOR));
    }
    if !st.lighting.paint_seeded {
        if let Some(RgbState::PerLed { zones }) = &rgb.state {
            st.lighting.paint_buf = zones
                .iter()
                .map(|(z, leds)| {
                    let m = leds
                        .iter()
                        .filter_map(|(k, c)| k.parse::<u32>().ok().map(|id| (id, *c)))
                        .collect();
                    (z.clone(), m)
                })
                .collect();
        }
        st.lighting.paint_seeded = true;
    }

    let mode = current_mode(&rgb.state);

    // 60/40 preview-vs-panel split (design), not the even `ui.columns` split.
    let gap = 18.0;
    let left_w = (ui.available_width() - gap) * 1.5 / 2.5;
    widgets::split_columns(ui, left_w, gap, |left, right| {
        preview(left, ctx, st, rgb, &mode);
        right_panel(right, ctx, st, rgb, &mode);
    });

    leave_canvas_modal(ui, ctx, st);
}

fn leave_canvas_modal(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi) {
    if st.lighting.confirm_leave_canvas.is_none() {
        return;
    }
    let (mut confirm, mut cancel) = (false, false);
    let dismissed = widgets::dialog(
        ui.ctx(),
        "lighting_leave_canvas",
        &t!("lighting.remove_from_canvas_title"),
        420.0,
        |ui| {
            ui.label(
                egui::RichText::new(t!("lighting.remove_from_canvas_body"))
                    .font(theme::body_md())
                    .color(theme::TEXT_MUT),
            );
        },
        |ui| {
            if widgets::button(
                ui,
                &t!("lighting.remove_and_apply"),
                widgets::ButtonKind::Primary,
                egui::vec2(150.0, 32.0),
            )
            .clicked()
            {
                confirm = true;
            }
            ui.add_space(theme::SPACE_4);
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
    if let Some(cmd) = widgets::resolve_delete_confirm(
        &mut st.lighting.confirm_leave_canvas,
        confirm,
        cancel || dismissed,
    ) {
        crate::runtime::ipc::send(ctx.cmd, cmd);
    }
}

fn preview(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, rgb: &RgbStatus, mode: &Mode) {
    widgets::card_titled(
        ui,
        &t!("lighting.lighting_preview"),
        |ui| {
            ui.label(
                egui::RichText::new(match mode {
                    Mode::Solid => t!("lighting.mode_static"),
                    Mode::Paint => t!("lighting.mode_paint"),
                    Mode::Effect(_) => t!("lighting.mode_effect"),
                    Mode::Direct(_) => t!("lighting.mode_software"),
                    Mode::Engine => t!("lighting.mode_canvas_engine"),
                    Mode::None => "\u{2014}".into(),
                })
                .font(theme::mono_semibold(11.0))
                .color(theme::CYAN),
            );
        },
        |ui| {
            // Zone selector
            ui.horizontal_wrapped(|ui| {
                ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
                widgets::caps_label_inline(ui, &t!("lighting.zone_caps"));
                if matches!(mode, Mode::Solid | Mode::Direct(_)) {
                    let _ = widgets::pill(ui, &t!("lighting.all_zones"), true);
                } else {
                    for z in &rgb.descriptor.zones {
                        if widgets::pill(ui, &z.name, z.id == st.lighting.zone) {
                            st.lighting.zone = z.id.clone();
                        }
                    }
                }
            });
            ui.add_space(theme::SPACE_6);

            let kbd_status = ctx.dev.keyboard_layout();
            if let Some(status) = kbd_status {
                layout_selector(ui, ctx, status);
                ui.add_space(theme::SPACE_6);
            }

            let zone = rgb
                .descriptor
                .zones
                .iter()
                .find(|z| z.id == st.lighting.zone)
                .unwrap_or(&rgb.descriptor.zones[0]);
            let painting = matches!(mode, Mode::Paint);
            let live = ctx.led_colors.get(&(ctx.dev.id.clone(), zone.id.clone()));
            let fill = fill_for(
                mode,
                &rgb.descriptor.native_effects,
                st.lighting.paint_color,
                live,
            );
            // A keyboard zone with a resolved key grid renders as real key caps;
            // everything else (rings, strips, keyless boards) keeps the LED dots.
            let as_keyboard = matches!(zone.topology, ZoneTopology::Keyboard { .. })
                && kbd_status.is_some_and(|s| !s.keys.is_empty());
            let dirty = if as_keyboard {
                keyboard_canvas(ui, st, zone, painting, &fill, kbd_status.unwrap())
            } else {
                led_canvas(ui, st, zone, painting, &fill)
            };
            if dirty {
                let cmd = paint_cmd(ctx, st);
                st.queue("rgb", cmd, ctx.time);
            }

            // Zone transform controls
            ui.add_space(theme::SPACE_5);
            transform_controls(ui, ctx, st, rgb);

            if painting {
                ui.add_space(theme::SPACE_6);
                egui::Sides::new().show(
                    ui,
                    |ui| {
                        ui.label(
                            egui::RichText::new(t!("lighting.paint_hint"))
                                .font(theme::body_sm())
                                .color(theme::TEXT_MUT),
                        );
                    },
                    |ui| {
                        if widgets::button(
                            ui,
                            &t!("lighting.clear"),
                            widgets::ButtonKind::Ghost,
                            egui::vec2(64.0, 28.0),
                        )
                        .clicked()
                        {
                            st.lighting.paint_buf.remove(&zone.id);
                            crate::runtime::ipc::send(ctx.cmd, paint_cmd(ctx, st));
                        }
                        if widgets::button(
                            ui,
                            &t!("lighting.fill_zone"),
                            widgets::ButtonKind::Ghost,
                            egui::vec2(82.0, 28.0),
                        )
                        .clicked()
                        {
                            let c = st.lighting.paint_color.unwrap_or_default();
                            let m: HashMap<u32, RgbColor> =
                                zone.leds.iter().map(|l| (l.id, c)).collect();
                            st.lighting.paint_buf.insert(zone.id.clone(), m);
                            crate::runtime::ipc::send(ctx.cmd, paint_cmd(ctx, st));
                        }
                    },
                );
            }

            // Effect Range params (brightness/speed/…) when an effect is active.
            if let Mode::Effect(eid) = mode {
                if let Some(eff) = rgb.descriptor.native_effects.iter().find(|e| &e.id == eid) {
                    if param_sliders(ui, ctx, st, rgb, EffectKind::Native, &eff.id, &eff.params) {
                        let cmd = effect_cmd(ctx, st, rgb, eff);
                        st.queue("rgb", cmd, ctx.time);
                    }
                }
            }
            if let Mode::Direct(eid) = mode {
                if let Some(anim) = ctx
                    .state
                    .lighting
                    .canvas
                    .available_direct_effects
                    .iter()
                    .find(|a| &a.id == eid)
                {
                    if param_sliders(ui, ctx, st, rgb, EffectKind::Direct, &anim.id, &anim.params) {
                        let cmd = direct_cmd(ctx, st, rgb, anim);
                        st.queue("rgb", cmd, ctx.time);
                    }
                }
            }
        },
    );
}

/// Zone transform controls: flip H/V for flat layouts, reverse + rotate for rings.
fn transform_controls(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, rgb: &RgbStatus) {
    let Some(zone) = rgb
        .descriptor
        .zones
        .iter()
        .find(|z| z.id == st.lighting.zone)
    else {
        return;
    };
    if zone.leds.is_empty() {
        return;
    }

    let live_tx = rgb
        .zone_transforms
        .get(&zone.id)
        .copied()
        .unwrap_or_default();
    let is_ring = matches!(
        zone.topology,
        ZoneTopology::Ring | ZoneTopology::Rings { .. }
    );
    let is_rings = matches!(zone.topology, ZoneTopology::Rings { .. });

    let mut changed_tx: Option<ZoneContentTransform> = None;

    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::LightingTransform,
        Rect::from_min_size(ui.cursor().min, Vec2::new(ui.available_width(), 32.0)),
    );
    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(6.0, 6.0);
        widgets::caps_label_inline(ui, &t!("lighting.transform_caps"));

        if is_ring {
            if widgets::pill(ui, &t!("lighting.reverse"), live_tx.reverse) {
                changed_tx = Some(ZoneContentTransform {
                    reverse: !live_tx.reverse,
                    ..live_tx
                });
            }
            if is_rings && widgets::pill(ui, &t!("lighting.swap"), live_tx.swap_rings) {
                changed_tx = Some(ZoneContentTransform {
                    swap_rings: !live_tx.swap_rings,
                    ..live_tx
                });
            }
        } else {
            if widgets::pill(ui, &t!("lighting.flip_h"), live_tx.flip_h) {
                changed_tx = Some(ZoneContentTransform {
                    flip_h: !live_tx.flip_h,
                    ..live_tx
                });
            }
            if widgets::pill(ui, &t!("lighting.flip_v"), live_tx.flip_v) {
                changed_tx = Some(ZoneContentTransform {
                    flip_v: !live_tx.flip_v,
                    ..live_tx
                });
            }
        }

        if !live_tx.is_identity() && widgets::pill(ui, &t!("lighting.reset"), false) {
            changed_tx = Some(ZoneContentTransform::default());
        }
    });

    if let Some(new_tx) = changed_tx {
        crate::runtime::ipc::send(ctx.cmd, tx_cmd(ctx, &zone.id, new_tx));
    }

    // Rotation stepper for ring topologies — steps the content by whole LEDs.
    if is_ring {
        let n = zone.leds.len() as i32;
        let offset = live_tx.led_offset.rem_euclid(n);
        ui.add_space(theme::SPACE_4);
        let readout = format!("{offset} / {n}");
        let delta = widgets::stepper_row(ui, &t!("lighting.rotate"), &readout);
        if delta != 0 {
            let new_tx = ZoneContentTransform {
                led_offset: wrap_offset(live_tx.led_offset, delta, n),
                ..live_tx
            };
            crate::runtime::ipc::send(ctx.cmd, tx_cmd(ctx, &zone.id, new_tx));
        }
    }
}

/// Steps a ring LED offset by `delta` and normalizes it into `0..n`.
fn wrap_offset(offset: i32, delta: i32, n: i32) -> i32 {
    if n <= 0 {
        return 0;
    }
    (offset + delta).rem_euclid(n)
}

fn right_panel(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, rgb: &RgbStatus, mode: &Mode) {
    // Effect grid — uniform cells, 2 per row.
    widgets::card_titled(
        ui,
        &t!("lighting.effect"),
        |_| {},
        |ui| {
            let gap = 9.0;
            let cell_w = ((ui.available_width() - gap) / 2.0).max(60.0);
            let solid_color = st.lighting.paint_color.map(rgb_to_color32);
            let mut entries: Vec<(String, EffectPick, Option<Color32>)> = vec![
                (
                    t!("lighting.static").to_string(),
                    EffectPick::Solid,
                    solid_color,
                ),
                (t!("lighting.paint").to_string(), EffectPick::Paint, None),
            ];
            for e in &rgb.descriptor.native_effects {
                entries.push((
                    format!("\u{2699} {}", e.name),
                    EffectPick::Effect(e.id.clone()),
                    None,
                ));
            }
            // The raw `designer` id is a parameter bag whose generic Range/Enum
            // controls make a poor stand-in for the actual Effect Designer page
            // — only its saved custom instances (below) belong in the grid.
            for a in &ctx.state.lighting.canvas.available_direct_effects {
                if !is_selectable_direct_effect(&a.id) {
                    continue;
                }
                entries.push((
                    format!("\u{2605} {}", a.name),
                    EffectPick::Direct(a.id.clone()),
                    None,
                ));
            }
            for (i, chunk) in entries.chunks(2).enumerate() {
                if i > 0 {
                    ui.add_space(gap);
                }
                let row_cursor = ui.cursor().min;
                if i == 0 {
                    // Anchor the first effect row (Static + Paint) for the tour.
                    let row_rect =
                        Rect::from_min_size(row_cursor, Vec2::new(ui.available_width(), 56.0));
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::LightingEffectsGrid,
                        row_rect,
                    );
                    // The paint cell is the second cell in the first row.
                    let paint_rect = Rect::from_min_size(
                        egui::pos2(row_cursor.x + cell_w + gap, row_cursor.y),
                        Vec2::new(cell_w, 56.0),
                    );
                    crate::domain::tour::anchor(
                        ui.ctx(),
                        crate::domain::tour::AnchorId::LightingPaintCell,
                        paint_rect,
                    );
                }
                ui.horizontal(|ui| {
                    ui.spacing_mut().item_spacing.x = gap;
                    for (label, pick, preview) in chunk {
                        let active = pick.is_active(mode);
                        let cell_preview = match preview {
                            Some(c) => widgets::CellPreview::Solid(*c),
                            None => widgets::CellPreview::Spectrum,
                        };
                        if widgets::effect_cell(
                            ui,
                            label,
                            active,
                            cell_preview,
                            cell_w,
                            56.0,
                            18.0,
                            false,
                        ) && !active
                        {
                            let built = match pick {
                                EffectPick::Solid => Some(solid_cmd(ctx, st)),
                                EffectPick::Paint => Some(paint_cmd(ctx, st)),
                                EffectPick::Effect(id) => rgb
                                    .descriptor
                                    .native_effects
                                    .iter()
                                    .find(|e| &e.id == id)
                                    .map(|eff| effect_cmd(ctx, st, rgb, eff)),
                                EffectPick::Direct(id) => ctx
                                    .state
                                    .lighting
                                    .canvas
                                    .available_direct_effects
                                    .iter()
                                    .find(|a| &a.id == id)
                                    .map(|anim| direct_cmd(ctx, st, rgb, anim)),
                            };
                            if let Some(cmd) = built {
                                // On-canvas devices are mutually exclusive with
                                // per-device effects: confirm before yanking one off the canvas.
                                if matches!(mode, Mode::Engine) {
                                    st.lighting.confirm_leave_canvas = Some(cmd);
                                } else {
                                    crate::runtime::ipc::send(ctx.cmd, cmd);
                                }
                            }
                        }
                    }
                });
            }

            ui.add_space(gap);
            let canvas_btn = widgets::button(
                ui,
                &t!("lighting.place_on_canvas"),
                widgets::ButtonKind::Ghost,
                egui::vec2(ui.available_width(), 30.0),
            );
            crate::domain::tour::anchor(
                ui.ctx(),
                crate::domain::tour::AnchorId::LightingPlaceCanvas,
                canvas_btn.rect,
            );
            if canvas_btn.clicked() {
                crate::runtime::ipc::send(ctx.cmd, place_on_canvas_cmd(ctx, st));
            }
        },
    );

    // Colour picker — only when the mode actually uses a colour.
    if show_color(
        mode,
        rgb,
        &ctx.state.lighting.canvas.available_direct_effects,
    ) {
        let title = if matches!(mode, Mode::Paint) {
            t!("lighting.brush_color")
        } else {
            t!("lighting.color")
        };
        let cur = st.lighting.paint_color.unwrap_or_default();
        widgets::card_titled(
            ui,
            &title,
            |_ui| {},
            |ui| {
                let mut color_changed = false;
                if let Some(new_c) = widgets::color_picker(ui, cur) {
                    st.lighting.paint_color = Some(new_c);
                    color_changed = true;
                }

                if color_changed {
                    match mode {
                        Mode::Solid => crate::runtime::ipc::send(ctx.cmd, solid_cmd(ctx, st)),
                        Mode::Effect(id) => {
                            if let Some(eff) =
                                rgb.descriptor.native_effects.iter().find(|e| &e.id == id)
                            {
                                crate::runtime::ipc::send(ctx.cmd, effect_cmd(ctx, st, rgb, eff));
                            }
                        }
                        Mode::Direct(id) => {
                            if let Some(anim) = ctx
                                .state
                                .lighting
                                .canvas
                                .available_direct_effects
                                .iter()
                                .find(|a| &a.id == id)
                            {
                                crate::runtime::ipc::send(ctx.cmd, direct_cmd(ctx, st, rgb, anim));
                            }
                        }
                        _ => {}
                    }
                }
            },
        );
    }
}

#[derive(Clone, Copy)]
enum EffectKind {
    Native,
    Direct,
}

impl EffectKind {
    fn tag(self) -> &'static str {
        match self {
            EffectKind::Native => "native",
            EffectKind::Direct => "direct",
        }
    }
}

enum EffectPick {
    Solid,
    Paint,
    Effect(String),
    Direct(String),
}

impl EffectPick {
    fn is_active(&self, mode: &Mode) -> bool {
        match (self, mode) {
            (EffectPick::Solid, Mode::Solid) => true,
            (EffectPick::Paint, Mode::Paint) => true,
            (EffectPick::Effect(id), Mode::Effect(m)) => id == m,
            (EffectPick::Direct(id), Mode::Direct(m)) => id == m,
            _ => false,
        }
    }
}

/// Whether a direct-effect id belongs in the per-device effect grid. The raw
/// designer id is a parameter bag, not a user-facing entry — only saved custom
/// instances (which carry their own id) are selectable here; those are edited
/// exclusively on the Effect Designer page.
fn is_selectable_direct_effect(id: &str) -> bool {
    id != DESIGNER_EFFECT_ID
}

/// Whether the colour picker applies to the current mode: paint and solid
/// always, native/direct effects only when they declare a `Color` param named
/// `"color"`. Effects with more than one colour (e.g. a two-stop gradient)
/// render each individually in [`param_sliders`] instead.
fn show_color(mode: &Mode, rgb: &RgbStatus, direct: &[Animation]) -> bool {
    let has_color = |params: &[EffectParamDescriptor]| {
        params
            .iter()
            .any(|p| p.id == "color" && matches!(p.kind, ParamKind::Color))
    };
    match mode {
        Mode::Paint | Mode::Solid => true,
        Mode::Effect(id) => rgb
            .descriptor
            .native_effects
            .iter()
            .find(|e| &e.id == id)
            .is_some_and(|e| has_color(&e.params)),
        Mode::Direct(id) => direct
            .iter()
            .find(|a| &a.id == id)
            .is_some_and(|a| has_color(&a.params)),
        _ => false,
    }
}

/// Lay out a zone's LEDs by topology and (when painting) paint them. Returns
/// `true` when the paint buffer changed this frame.
fn led_canvas(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    zone: &RgbZone,
    painting: bool,
    fill: &LedFill,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(
        Vec2::new(ui.available_width(), 220.0),
        Sense::click_and_drag(),
    );
    let p = ui.painter();
    p.rect_filled(rect, theme::RADIUS_MD, theme::hex(0x0a0d13));
    p.rect_stroke(
        rect,
        theme::RADIUS_MD,
        Stroke::new(1.0, theme::BORDER_INNER),
        egui::StrokeKind::Middle,
    );
    let inner = rect.shrink(18.0);

    let pts = led_layout(zone, inner);
    let n = pts.len().max(1);

    let mut changed = false;
    if painting && (resp.is_pointer_button_down_on() || resp.dragged() || resp.clicked()) {
        if let Some(pos) = resp.interact_pointer_pos() {
            let color = st.lighting.paint_color.unwrap_or_default();
            for (led, c, r) in &pts {
                if c.distance(pos) <= r + 2.0 {
                    st.lighting
                        .paint_buf
                        .entry(zone.id.clone())
                        .or_default()
                        .insert(*led, color);
                    changed = true;
                }
            }
        }
    }
    if painting && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }

    let buf = st.lighting.paint_buf.get(&zone.id);
    let painter = ui.painter();
    for (i, (led, c, r)) in pts.iter().enumerate() {
        let col = key_fill(fill, buf, *led, i as f32 / n as f32);
        let dark = col == theme::hex(0x1b2230) || col == theme::hex(0x14181f);
        if !dark {
            theme::glow(painter, *c, r * 2.4, col, 0.09);
        }
        painter.circle_filled(*c, *r, col);
        if dark {
            painter.circle_stroke(*c, *r, Stroke::new(1.0, theme::hex(0x2a3446)));
        }
    }
    changed
}

/// A live LED's preview colour: black (LED off) and missing LEDs both render
/// as the same unlit disc the `Off` fill uses.
fn live_led_color32(m: &HashMap<u32, RgbColor>, led: u32) -> Color32 {
    match m.get(&led) {
        Some(c) if *c != (RgbColor { r: 0, g: 0, b: 0 }) => rgb_to_color32(*c),
        _ => theme::hex(0x14181f),
    }
}

/// Resolve a single key's/LED's fill colour, shared by the LED-dot canvas and
/// the keyboard-cap canvas. `frac` positions the LED in the rainbow gradient.
fn key_fill(
    fill: &LedFill,
    buf: Option<&HashMap<u32, RgbColor>>,
    led_id: u32,
    frac: f32,
) -> Color32 {
    match fill {
        LedFill::Buffer => buf
            .and_then(|m| m.get(&led_id))
            .map(|rc| rgb_to_color32(*rc))
            .unwrap_or(theme::hex(0x1b2230)),
        LedFill::Solid(rc) => rgb_to_color32(*rc),
        LedFill::Off => theme::hex(0x14181f),
        LedFill::Rainbow => rainbow(frac),
        LedFill::Live(m) => live_led_color32(m, led_id),
    }
}

/// Draw a keyboard zone as real key caps (from the device's `KeyboardLayout`
/// capability). Paint-mode hit-tests key rects. Returns whether the paint
/// buffer changed. Mirrors [`led_canvas`] but keyed on `VisualKey`.
fn keyboard_canvas(
    ui: &mut egui::Ui,
    st: &mut DeviceUi,
    zone: &RgbZone,
    painting: bool,
    fill: &LedFill,
    status: &halod_shared::keyboard::KeyboardLayoutStatus,
) -> bool {
    use super::keyboard_visual as kbv;

    let (resp, inner) = kbv::panel(ui, 260.0, Sense::click_and_drag());
    let keys = &status.keys;
    let rects = kbv::key_rects(keys, inner, 3.0);
    let unit = kbv::unit_for(keys, inner);

    let mut changed = false;
    if painting && (resp.is_pointer_button_down_on() || resp.dragged() || resp.clicked()) {
        if let Some(pos) = resp.interact_pointer_pos() {
            if let Some(i) = kbv::hit_key(keys, &rects, pos, unit) {
                let color = st.lighting.paint_color.unwrap_or_default();
                st.lighting
                    .paint_buf
                    .entry(zone.id.clone())
                    .or_default()
                    .insert(keys[i].led_id, color);
                changed = true;
            }
        }
    }
    if painting && resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::Crosshair);
    }

    let buf = st.lighting.paint_buf.get(&zone.id);
    let n = keys.len().max(1) as f32;
    let fills: HashMap<u32, Color32> = keys
        .iter()
        .enumerate()
        .map(|(i, k)| (k.led_id, key_fill(fill, buf, k.led_id, i as f32 / n)))
        .collect();
    let off = theme::hex(0x14181f);
    kbv::draw_keyboard(
        ui,
        keys,
        &rects,
        &|id| fills.get(&id).copied().unwrap_or(off),
        None,
        status.language,
        unit,
    );
    changed
}

/// The layout-selector combo boxes for a keyboard, emitting `SetKeyboardLayout`.
fn layout_selector(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    status: &halod_shared::keyboard::KeyboardLayoutStatus,
) {
    use super::keyboard_visual as kbv;
    use halod_shared::keyboard::KeyboardLayoutSelection;

    let opts = kbv::layout_options(status);
    let emit = |sel: KeyboardLayoutSelection| {
        let _ = ctx.cmd.send(DaemonCommand::SetKeyboardLayout {
            id: ctx.dev.id.clone(),
            selection: sel,
        });
    };
    // Render one combo box; return the newly picked index only when it changed.
    let combo = |ui: &mut egui::Ui, salt: &str, labels: &[String], sel: usize| -> Option<usize> {
        let mut idx = sel;
        egui::ComboBox::from_id_salt((salt, &ctx.dev.id))
            .selected_text(labels[idx].clone())
            .show_ui(ui, |ui| {
                for (i, label) in labels.iter().enumerate() {
                    ui.selectable_value(&mut idx, i, label);
                }
            });
        (idx != sel).then_some(idx)
    };

    ui.horizontal_wrapped(|ui| {
        ui.spacing_mut().item_spacing = egui::vec2(8.0, 6.0);
        widgets::caps_label_inline(ui, &t!("lighting.layout_caps"));

        if opts.show_variant {
            if let Some(idx) = combo(
                ui,
                "kbd_variant",
                &opts.variant_labels,
                opts.variant_selected,
            ) {
                emit(KeyboardLayoutSelection {
                    variant: kbv::variant_from_index(idx),
                    language: status.selection.language,
                });
            }
        }
        if let Some(idx) = combo(
            ui,
            "kbd_lang",
            &opts.language_labels,
            opts.language_selected,
        ) {
            emit(KeyboardLayoutSelection {
                variant: status.selection.variant,
                language: kbv::language_from_index(status, idx),
            });
        }
    });
}

/// Screen positions + radius for each LED, per zone topology.
fn led_layout(zone: &RgbZone, rect: Rect) -> Vec<(u32, Pos2, f32)> {
    let n = zone.leds.len();
    if n == 0 {
        return Vec::new();
    }
    let center = rect.center();
    match &zone.topology {
        ZoneTopology::Ring => {
            let rad = rect.width().min(rect.height()) * 0.42;
            zone.leds
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let a =
                        i as f32 / n as f32 * std::f32::consts::TAU - std::f32::consts::FRAC_PI_2;
                    (l.id, center + Vec2::new(a.cos() * rad, a.sin() * rad), 9.0)
                })
                .collect()
        }
        ZoneTopology::Rings { count } => {
            // `count` separate rings laid out side-by-side across the canvas
            // (e.g. a 3-zone mouse), not concentric.
            let rings = (*count).max(1) as usize;
            let per = n.div_ceil(rings);
            let slot_w = rect.width() / rings as f32;
            let rad = (slot_w * 0.36).min(rect.height() * 0.4);
            zone.leds
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let ring = i / per;
                    let k = i % per;
                    let cx = rect.left() + slot_w * (ring as f32 + 0.5);
                    let a = k as f32 / per.max(1) as f32 * std::f32::consts::TAU
                        - std::f32::consts::FRAC_PI_2;
                    (
                        l.id,
                        Pos2::new(cx + a.cos() * rad, center.y + a.sin() * rad),
                        7.5,
                    )
                })
                .collect()
        }
        ZoneTopology::Linear => {
            let cols = n.min(((rect.width() / 22.0) as usize).max(1));
            let rows = n.div_ceil(cols);
            let dx = rect.width() / cols as f32;
            let dy = (rect.height() / rows as f32).min(40.0);
            let v_off = (rect.height() - dy * rows as f32) / 2.0;
            zone.leds
                .iter()
                .enumerate()
                .map(|(i, l)| {
                    let cx = rect.left() + dx * (i % cols) as f32 + dx / 2.0;
                    let cy = rect.top() + v_off + dy * (i / cols) as f32 + dy / 2.0;
                    (
                        l.id,
                        Pos2::new(cx, cy),
                        (dx.min(dy) * 0.32).clamp(6.0, 12.0),
                    )
                })
                .collect()
        }
        ZoneTopology::Grid | ZoneTopology::Keyboard { .. } => {
            let (mut minx, mut maxx, mut miny, mut maxy) = (f32::MAX, f32::MIN, f32::MAX, f32::MIN);
            for l in &zone.leds {
                minx = minx.min(l.x);
                maxx = maxx.max(l.x);
                miny = miny.min(l.y);
                maxy = maxy.max(l.y);
            }
            let sx = (maxx - minx).max(1.0);
            let sy = (maxy - miny).max(1.0);
            let single_row = (maxy - miny).abs() < 0.01;
            zone.leds
                .iter()
                .map(|l| {
                    let x = rect.left() + (l.x - minx) / sx * rect.width();
                    let y = if single_row {
                        rect.center().y
                    } else {
                        rect.top() + (l.y - miny) / sy * rect.height()
                    };
                    (l.id, Pos2::new(x, y), 7.5)
                })
                .collect()
        }
    }
}

fn param_sliders(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    rgb: &RgbStatus,
    kind: EffectKind,
    effect_id: &str,
    params: &[EffectParamDescriptor],
) -> bool {
    let mut changed = false;
    for d in params {
        let key = format!("fx:{}:{effect_id}:{}", kind.tag(), d.id);
        match &d.kind {
            ParamKind::Range { min, max, step } => {
                let (min, max, step) = (*min, *max, *step);
                let live =
                    current_param_f32(rgb, kind, effect_id, &d.id).unwrap_or(match d.default {
                        EffectParamValue::Float(f) => f as f32,
                        _ => min as f32,
                    });
                let mut v = st.guarded(&key, live, ctx.time);
                ui.add_space(theme::SPACE_7);
                let readout = widgets::range_readout(v, step);
                if widgets::slider_row(ui, &d.label, &mut v, min as f32..=max as f32, &readout) {
                    let snapped = widgets::snap_to_step(v, min as f32, max as f32, step as f32);
                    st.set(&key, snapped, ctx.time);
                    changed = true;
                }
            }
            ParamKind::Number { min, max } => {
                let live =
                    current_param_f32(rgb, kind, effect_id, &d.id).unwrap_or(match d.default {
                        EffectParamValue::Float(f) => f as f32,
                        _ => 0.0,
                    });
                let mut v = st.guarded(&key, live, ctx.time);
                ui.add_space(theme::SPACE_7);
                let (edited, committed) =
                    widgets::num_input_row(ui, &d.label, &mut v, *min as f32..=*max as f32);
                if edited || committed {
                    st.set(&key, v, ctx.time);
                }
                changed |= committed;
            }
            // Master swatch handles the `"color"`-id param; any other Color
            // param (e.g. a two-stop gradient's second colour) gets its own row.
            ParamKind::Color if d.id != "color" => {
                let default = match d.default {
                    EffectParamValue::Color(c) => c,
                    _ => RgbColor::default(),
                };
                let live = current_param_color(rgb, kind, effect_id, &d.id).unwrap_or(default);
                let current = st.lighting.param_colors.get(&key).copied().unwrap_or(live);
                ui.add_space(theme::SPACE_7);
                ui.label(
                    egui::RichText::new(&d.label)
                        .font(theme::body_sm())
                        .color(theme::TEXT_MUT),
                );
                if let Some(new_c) = widgets::color_picker(ui, current) {
                    st.lighting.param_colors.insert(key, new_c);
                    changed = true;
                }
            }
            ParamKind::Steps => {
                let live = current_param_steps(rgb, kind, effect_id, &d.id)
                    .unwrap_or_else(|| widgets::steps_default(d));
                let steps = st.lighting.param_steps.entry(key).or_insert(live);
                ui.add_space(theme::SPACE_7);
                changed |= widgets::steps_editor(ui, &d.label, steps);
            }
            ParamKind::Sensor => {
                let sensors = crate::ui::screens::lighting::sensor_options(ctx.state);
                let default = current_param_str(rgb, kind, effect_id, &d.id).unwrap_or_default();
                let current = st.lighting.param_strs.get(&key).cloned().unwrap_or(default);
                ui.add_space(theme::SPACE_7);
                changed |= widgets::combo_param_row(
                    ui,
                    &d.label,
                    key,
                    &mut st.lighting.param_strs,
                    current,
                    &sensors,
                    Some(t!("lighting.none").as_ref()),
                );
            }
            ParamKind::Enum { options } => {
                let default = current_param_str(rgb, kind, effect_id, &d.id).unwrap_or_else(|| {
                    match &d.default {
                        EffectParamValue::Str(s) => s.clone(),
                        _ => options.first().cloned().unwrap_or_default(),
                    }
                });
                let current = st.lighting.param_strs.get(&key).cloned().unwrap_or(default);
                let opts: Vec<(String, String)> =
                    options.iter().map(|o| (o.clone(), o.clone())).collect();
                ui.add_space(theme::SPACE_7);
                changed |= widgets::combo_param_row(
                    ui,
                    &d.label,
                    key,
                    &mut st.lighting.param_strs,
                    current,
                    &opts,
                    None,
                );
            }
            _ => {}
        }
    }
    changed
}

/// The live param map for the effect currently applied to the device, if
/// `eid`/`kind` match what the daemon reports (i.e. this is the active effect).
fn current_effect_params<'a>(
    rgb: &'a RgbStatus,
    kind: EffectKind,
    eid: &str,
) -> Option<&'a HashMap<String, EffectParamValue>> {
    match (kind, &rgb.state) {
        (EffectKind::Native, Some(RgbState::NativeEffect { id, params })) if id == eid => {
            Some(params)
        }
        (EffectKind::Direct, Some(RgbState::DirectEffect { id, params })) if id == eid => {
            Some(params)
        }
        _ => None,
    }
}

fn current_param_f32(rgb: &RgbStatus, kind: EffectKind, eid: &str, pid: &str) -> Option<f32> {
    match current_effect_params(rgb, kind, eid)?.get(pid) {
        Some(EffectParamValue::Float(f)) => Some(*f as f32),
        _ => None,
    }
}

fn current_param_str(rgb: &RgbStatus, kind: EffectKind, eid: &str, pid: &str) -> Option<String> {
    match current_effect_params(rgb, kind, eid)?.get(pid) {
        Some(EffectParamValue::Str(s)) => Some(s.clone()),
        _ => None,
    }
}

fn current_param_steps(
    rgb: &RgbStatus,
    kind: EffectKind,
    eid: &str,
    pid: &str,
) -> Option<Vec<ColorStep>> {
    match current_effect_params(rgb, kind, eid)?.get(pid) {
        Some(EffectParamValue::Steps(s)) if !s.is_empty() => Some(s.clone()),
        _ => None,
    }
}

fn current_param_color(
    rgb: &RgbStatus,
    kind: EffectKind,
    eid: &str,
    pid: &str,
) -> Option<RgbColor> {
    match current_effect_params(rgb, kind, eid)?.get(pid) {
        Some(EffectParamValue::Color(c)) => Some(*c),
        _ => None,
    }
}

fn current_color(rgb: &RgbStatus) -> Option<RgbColor> {
    match &rgb.state {
        Some(RgbState::Static { color }) => Some(*color),
        _ => None,
    }
}

// ── Command builders ────────────────────────────────────────────────────────────

fn solid_cmd(ctx: &TabCtx, st: &DeviceUi) -> DaemonCommand {
    DaemonCommand::RgbApply {
        id: ctx.dev.id.clone(),
        state: RgbState::Static {
            color: st.lighting.paint_color.unwrap_or_default(),
        },
    }
}

fn paint_cmd(ctx: &TabCtx, st: &DeviceUi) -> DaemonCommand {
    let zones: HashMap<String, HashMap<String, RgbColor>> = st
        .lighting
        .paint_buf
        .iter()
        .map(|(z, leds)| {
            let m = leds.iter().map(|(id, c)| (id.to_string(), *c)).collect();
            (z.clone(), m)
        })
        .collect();
    DaemonCommand::RgbApply {
        id: ctx.dev.id.clone(),
        state: RgbState::PerLed { zones },
    }
}

fn collect_effect_params(
    st: &DeviceUi,
    rgb: &RgbStatus,
    kind: EffectKind,
    effect_id: &str,
    descs: &[EffectParamDescriptor],
) -> HashMap<String, EffectParamValue> {
    let mut params: HashMap<String, EffectParamValue> = HashMap::new();
    for d in descs {
        let val = match &d.kind {
            ParamKind::Range { min, max, step } => {
                let key = format!("fx:{}:{effect_id}:{}", kind.tag(), d.id);
                let live =
                    current_param_f32(rgb, kind, effect_id, &d.id).unwrap_or(match d.default {
                        EffectParamValue::Float(f) => f as f32,
                        _ => *min as f32,
                    });
                let v = st.scratch.get(&key).copied().unwrap_or(live);
                EffectParamValue::Float(widgets::snap_to_step(
                    v,
                    *min as f32,
                    *max as f32,
                    *step as f32,
                ) as f64)
            }
            ParamKind::Number { min, max } => {
                let key = format!("fx:{}:{effect_id}:{}", kind.tag(), d.id);
                let live =
                    current_param_f32(rgb, kind, effect_id, &d.id).unwrap_or(match d.default {
                        EffectParamValue::Float(f) => f as f32,
                        _ => 0.0,
                    });
                let v = st.scratch.get(&key).copied().unwrap_or(live);
                EffectParamValue::Float(v.clamp(*min as f32, *max as f32) as f64)
            }
            ParamKind::Color if d.id == "color" => EffectParamValue::Color(
                st.lighting.paint_color.unwrap_or_else(|| match d.default {
                    EffectParamValue::Color(c) => c,
                    _ => RgbColor::default(),
                }),
            ),
            ParamKind::Color => {
                let key = format!("fx:{}:{effect_id}:{}", kind.tag(), d.id);
                let default = match d.default {
                    EffectParamValue::Color(c) => c,
                    _ => RgbColor::default(),
                };
                let live = current_param_color(rgb, kind, effect_id, &d.id).unwrap_or(default);
                EffectParamValue::Color(st.lighting.param_colors.get(&key).copied().unwrap_or(live))
            }
            ParamKind::Sensor => {
                let key = format!("fx:{}:{effect_id}:{}", kind.tag(), d.id);
                let live = current_param_str(rgb, kind, effect_id, &d.id).unwrap_or_default();
                EffectParamValue::Str(st.lighting.param_strs.get(&key).cloned().unwrap_or(live))
            }
            ParamKind::Enum { options } => {
                let key = format!("fx:{}:{effect_id}:{}", kind.tag(), d.id);
                let live =
                    current_param_str(rgb, kind, effect_id, &d.id).unwrap_or_else(|| {
                        match &d.default {
                            EffectParamValue::Str(s) => s.clone(),
                            _ => options.first().cloned().unwrap_or_default(),
                        }
                    });
                EffectParamValue::Str(st.lighting.param_strs.get(&key).cloned().unwrap_or(live))
            }
            ParamKind::Steps => {
                let key = format!("fx:{}:{effect_id}:{}", kind.tag(), d.id);
                let live = current_param_steps(rgb, kind, effect_id, &d.id)
                    .unwrap_or_else(|| widgets::steps_default(d));
                EffectParamValue::Steps(st.lighting.param_steps.get(&key).cloned().unwrap_or(live))
            }
            _ => d.default.clone(),
        };
        params.insert(d.id.clone(), val);
    }
    params
}

fn effect_cmd(ctx: &TabCtx, st: &DeviceUi, rgb: &RgbStatus, eff: &NativeEffect) -> DaemonCommand {
    DaemonCommand::RgbApply {
        id: ctx.dev.id.clone(),
        state: RgbState::NativeEffect {
            id: eff.id.clone(),
            params: collect_effect_params(st, rgb, EffectKind::Native, &eff.id, &eff.params),
        },
    }
}

fn direct_cmd(ctx: &TabCtx, st: &DeviceUi, rgb: &RgbStatus, anim: &Animation) -> DaemonCommand {
    DaemonCommand::RgbApply {
        id: ctx.dev.id.clone(),
        state: RgbState::DirectEffect {
            id: anim.id.clone(),
            params: collect_effect_params(st, rgb, EffectKind::Direct, &anim.id, &anim.params),
        },
    }
}

fn place_on_canvas_cmd(ctx: &TabCtx, st: &DeviceUi) -> DaemonCommand {
    DaemonCommand::CanvasPlaceZone {
        device_id: ctx.dev.id.clone(),
        zone_id: st.lighting.zone.clone(),
        x: None,
        y: None,
        w: None,
        h: None,
        rotation: None,
        effect: None,
        sampling_mode: None,
    }
}

fn tx_cmd(ctx: &TabCtx, zone_id: &str, transform: ZoneContentTransform) -> DaemonCommand {
    DaemonCommand::RgbSetZoneTransform {
        id: ctx.dev.id.clone(),
        zone_id: zone_id.to_string(),
        transform,
    }
}

/// A vivid rainbow colour for fractional hue `t` (0..1).
fn rainbow(t: f32) -> Color32 {
    let h = t.fract() * 6.0;
    let (s, v) = (0.85, 0.95);
    let c = v * s;
    let x = c * (1.0 - ((h % 2.0) - 1.0).abs());
    let m = v - c;
    let (r, g, b) = match h as u32 {
        0 => (c, x, 0.0),
        1 => (x, c, 0.0),
        2 => (0.0, c, x),
        3 => (0.0, x, c),
        4 => (x, 0.0, c),
        _ => (c, 0.0, x),
    };
    Color32::from_rgb(
        ((r + m) * 255.0) as u8,
        ((g + m) * 255.0) as u8,
        ((b + m) * 255.0) as u8,
    )
}

#[cfg(test)]
mod tests {
    use super::*;
    use halod_shared::types::LedPosition;

    fn zone(topology: ZoneTopology, n: u32) -> RgbZone {
        RgbZone {
            id: "z".into(),
            name: "Z".into(),
            topology,
            leds: (0..n)
                .map(|i| LedPosition {
                    id: i,
                    x: (i % 7) as f32,
                    y: (i / 7) as f32,
                })
                .collect(),
        }
    }

    #[test]
    fn key_fill_resolves_buffer_solid_off_and_live() {
        let red = RgbColor { r: 200, g: 0, b: 0 };
        let mut buf = HashMap::new();
        buf.insert(5u32, red);
        // Buffer: painted LED shows its colour; unpainted shows the empty cap.
        assert_eq!(
            key_fill(&LedFill::Buffer, Some(&buf), 5, 0.0),
            rgb_to_color32(red)
        );
        assert_eq!(
            key_fill(&LedFill::Buffer, Some(&buf), 6, 0.0),
            theme::hex(0x1b2230)
        );
        // Solid paints every key the same colour.
        assert_eq!(
            key_fill(&LedFill::Solid(red), None, 9, 0.0),
            rgb_to_color32(red)
        );
        // Off is the unlit cap.
        assert_eq!(key_fill(&LedFill::Off, None, 9, 0.5), theme::hex(0x14181f));
        // Live mirrors live_led_color32 (a black/missing LED is unlit).
        let mut live = HashMap::new();
        live.insert(9u32, red);
        assert_eq!(
            key_fill(&LedFill::Live(&live), None, 9, 0.0),
            rgb_to_color32(red)
        );
        assert_eq!(
            key_fill(&LedFill::Live(&live), None, 1, 0.0),
            theme::hex(0x14181f)
        );
    }

    #[test]
    fn every_topology_lays_leds_inside_the_canvas() {
        let rect = Rect::from_min_size(Pos2::new(10.0, 10.0), Vec2::new(300.0, 200.0));
        let topos = [
            ZoneTopology::Ring,
            ZoneTopology::Rings { count: 3 },
            ZoneTopology::Linear,
            ZoneTopology::Grid,
        ];
        for t in topos {
            let z = zone(t, 24);
            let pts = led_layout(&z, rect);
            assert_eq!(pts.len(), 24);
            for (_, c, _) in pts {
                assert!(c.x >= rect.left() - 24.0 && c.x <= rect.right() + 24.0);
                assert!(c.y >= rect.top() - 24.0 && c.y <= rect.bottom() + 24.0);
            }
        }
    }

    #[test]
    fn mode_is_derived_from_rgb_state() {
        assert_eq!(
            current_mode(&Some(RgbState::Static {
                color: RgbColor::default()
            })),
            Mode::Solid
        );
        assert_eq!(
            current_mode(&Some(RgbState::PerLed {
                zones: Default::default()
            })),
            Mode::Paint
        );
        assert_eq!(current_mode(&None), Mode::None);
    }

    fn empty_rgb_status() -> RgbStatus {
        RgbStatus {
            descriptor: halod_shared::types::RgbDescriptor {
                zones: vec![],
                native_effects: vec![],
            },
            state: None,
            zone_transforms: Default::default(),
            chainable_channels: vec![],
        }
    }

    #[test]
    fn is_selectable_direct_effect_excludes_only_the_raw_designer_id() {
        assert!(!is_selectable_direct_effect(DESIGNER_EFFECT_ID));
        assert!(is_selectable_direct_effect("breathing"));
        assert!(is_selectable_direct_effect("designer_pixmap"));
    }

    #[test]
    fn show_color_false_for_two_stop_gradient_direct_effect() {
        let two_stop = Animation {
            id: "sensor_gradient".into(),
            name: "Sensor Gradient".into(),
            params: vec![
                EffectParamDescriptor {
                    id: "color_a".into(),
                    label: "Color A".into(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor::default()),
                },
                EffectParamDescriptor {
                    id: "color_b".into(),
                    label: "Color B".into(),
                    kind: ParamKind::Color,
                    default: EffectParamValue::Color(RgbColor::default()),
                },
            ],
        };
        let rgb = empty_rgb_status();
        assert!(!show_color(
            &Mode::Direct("sensor_gradient".into()),
            &rgb,
            std::slice::from_ref(&two_stop),
        ));
    }

    #[test]
    fn collect_effect_params_second_color_sensor_and_enum() {
        let mut st = DeviceUi::default();
        st.lighting.param_colors.insert(
            "fx:direct:sensor_gradient:color_b".to_string(),
            RgbColor { r: 9, g: 9, b: 9 },
        );
        st.lighting.param_strs.insert(
            "fx:direct:sensor_gradient:sensor".to_string(),
            "temp1".to_string(),
        );
        let rgb = empty_rgb_status();
        let descs = vec![
            EffectParamDescriptor {
                id: "color_a".into(),
                label: "Color A".into(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 }),
            },
            EffectParamDescriptor {
                id: "color_b".into(),
                label: "Color B".into(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(RgbColor { r: 4, g: 5, b: 6 }),
            },
            EffectParamDescriptor {
                id: "sensor".into(),
                label: "Sensor".into(),
                kind: ParamKind::Sensor,
                default: EffectParamValue::Str(String::new()),
            },
            EffectParamDescriptor {
                id: "mode".into(),
                label: "Mode".into(),
                kind: ParamKind::Enum {
                    options: vec!["gradient".into(), "meter".into()],
                },
                default: EffectParamValue::Str("gradient".into()),
            },
        ];
        let params =
            collect_effect_params(&st, &rgb, EffectKind::Direct, "sensor_gradient", &descs);
        assert_eq!(
            params["color_a"],
            EffectParamValue::Color(RgbColor { r: 1, g: 2, b: 3 }),
            "untouched color param keeps its own default"
        );
        assert_eq!(
            params["color_b"],
            EffectParamValue::Color(RgbColor { r: 9, g: 9, b: 9 }),
            "live override wins for the second color"
        );
        assert_eq!(params["sensor"], EffectParamValue::Str("temp1".into()));
        assert_eq!(params["mode"], EffectParamValue::Str("gradient".into()));
    }

    #[test]
    fn engine_and_direct_modes_use_live_colors_when_available() {
        let live: HashMap<u32, RgbColor> = [(
            0,
            RgbColor {
                r: 10,
                g: 20,
                b: 30,
            },
        )]
        .into_iter()
        .collect();
        for mode in [Mode::Engine, Mode::Direct("breathing".into())] {
            assert_eq!(
                fill_for(&mode, &[], None, Some(&live)),
                LedFill::Live(&live)
            );
            // No canvas frame yet → rainbow placeholder.
            assert_eq!(fill_for(&mode, &[], None, None), LedFill::Rainbow);
        }
    }

    #[test]
    fn native_effects_stay_rainbow_even_with_live_colors() {
        // On-device effects run in hardware; the daemon has no colors for them.
        let live: HashMap<u32, RgbColor> = [(
            0,
            RgbColor {
                r: 10,
                g: 20,
                b: 30,
            },
        )]
        .into_iter()
        .collect();
        assert_eq!(
            fill_for(&Mode::Effect("wave".into()), &[], None, Some(&live)),
            LedFill::Rainbow
        );
        assert_eq!(
            fill_for(&Mode::Solid, &[], None, Some(&live)),
            LedFill::Solid(DEFAULT_PAINT_COLOR)
        );
    }

    #[test]
    fn live_led_color_maps_black_and_missing_to_unlit() {
        let c = RgbColor { r: 5, g: 6, b: 7 };
        let m: HashMap<u32, RgbColor> = [(0, c), (1, RgbColor { r: 0, g: 0, b: 0 })]
            .into_iter()
            .collect();
        assert_eq!(live_led_color32(&m, 0), rgb_to_color32(c));
        let unlit = live_led_color32(&m, 1);
        assert_eq!(live_led_color32(&m, 99), unlit, "missing LED renders unlit");
        assert_ne!(live_led_color32(&m, 0), unlit);
    }

    #[test]
    fn wrap_offset_steps_and_wraps_within_ring() {
        assert_eq!(wrap_offset(0, 1, 8), 1);
        assert_eq!(wrap_offset(7, 1, 8), 0, "increment wraps past the last LED");
        assert_eq!(wrap_offset(0, -1, 8), 7, "decrement wraps to the last LED");
        assert_eq!(
            wrap_offset(10, -1, 8),
            1,
            "normalizes an out-of-range offset"
        );
        assert_eq!(wrap_offset(3, 1, 0), 0, "empty ring is a no-op");
    }
}
