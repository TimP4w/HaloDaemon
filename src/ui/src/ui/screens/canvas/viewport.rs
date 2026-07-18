// SPDX-License-Identifier: GPL-3.0-or-later
//! Canvas viewport: the interactive zone stage — drag/marquee/rotate/resize
//! input handling, zone + LED rendering, and hit-testing.

use std::collections::HashSet;

use egui::{Align2, Color32, Pos2, Rect, Sense, Stroke, Vec2};
use halod_shared::types::{AppState, DeviceCapability, PlacedZone, SamplingMode, ZoneTopology};

use crate::runtime::ipc::CommandTx;
use crate::ui::components as widgets;
use crate::ui::theme::{self, a};

use super::geometry::{
    apply_drag, body_move, fill_coords, led_bounds, led_screen_pos, letterbox, norm_to_screen,
    point_in_zone, rounded_zone_outline_sc, screen_to_norm, zone_corners, zone_corners_sc,
    zone_key, zones_in_marquee,
};
use super::rack::{instance_color, instance_indices, rgb_zone_descriptor, ring_zone_context_menu};
use super::{CanvasUi, DragState, Handle, LedMode, MarqueeState, DEBOUNCE, HANDLE_HIT_R, HANDLE_R};

/// Screen position of the rotation handle — delegated to the shared widget helper.
use widgets::rotation_handle_pos;

// ── Canvas view ───────────────────────────────────────────────────────────────
pub(super) fn canvas_view(
    ui: &mut egui::Ui,
    state: &AppState,
    canvas_ui: &mut CanvasUi,
    cmd: &CommandTx,
    time: f64,
    outer: Rect,
) {
    crate::domain::tour::anchor(ui.ctx(), crate::domain::tour::AnchorId::CanvasStage, outer);
    let canvas_rect = letterbox(outer, canvas_ui.canvas_aspect);
    const ROUND: f32 = 16.0;
    let p = ui.painter();
    p.rect_filled(canvas_rect, ROUND, theme::hex(0x070a0f));

    let gp = p.with_clip_rect(canvas_rect);
    let grid_col = a(Color32::WHITE, 0.04);
    let step = 34.0_f32;
    let mut x = canvas_rect.left() + (canvas_rect.left() % step + step) % step;
    while x < canvas_rect.right() {
        gp.line_segment(
            [
                Pos2::new(x, canvas_rect.top()),
                Pos2::new(x, canvas_rect.bottom()),
            ],
            Stroke::new(1.0, grid_col),
        );
        x += step;
    }
    let mut y = canvas_rect.top() + (canvas_rect.top() % step + step) % step;
    while y < canvas_rect.bottom() {
        gp.line_segment(
            [
                Pos2::new(canvas_rect.left(), y),
                Pos2::new(canvas_rect.right(), y),
            ],
            Stroke::new(1.0, grid_col),
        );
        y += step;
    }

    if canvas_ui.led_mode == LedMode::Frame {
        if let Some(tex) = &canvas_ui.texture {
            p.image(
                tex.id(),
                canvas_rect,
                Rect::from_min_max(Pos2::ZERO, Pos2::new(1.0, 1.0)),
                Color32::WHITE,
            );
        }
    }
    theme::round_corners(p, canvas_rect, ROUND, theme::MAIN_BG);

    p.text(
        Pos2::new(canvas_rect.left() + 14.0, canvas_rect.bottom() - 12.0),
        Align2::LEFT_BOTTOM,
        t!("canvas.drag_hint"),
        theme::body(10.0),
        theme::TEXT_FAINT,
    );

    let resp = ui.interact(outer, egui::Id::new("canvas_area"), Sense::click_and_drag());
    let ptr_norm = resp
        .interact_pointer_pos()
        .map(|pp| screen_to_norm(pp, canvas_rect));

    let multi = ui.input(|i| i.modifiers.ctrl || i.modifiers.shift);

    if resp.drag_started() {
        if let Some(norm) = ptr_norm {
            let target = hit_test(norm, &state.lighting.canvas.placed_zones, canvas_rect);
            match target {
                Some((dev_id, channel_id, handle)) => {
                    // Dragging a zone that's already part of a multi-selection
                    // keeps the group (so a plain drag moves them all); pressing
                    // an unselected zone selects only it, unless a modifier adds.
                    let already = canvas_ui
                        .selected
                        .contains(&(dev_id.clone(), channel_id.clone()));
                    if !multi && !already {
                        canvas_ui.selected.clear();
                    }
                    canvas_ui
                        .selected
                        .insert((dev_id.clone(), channel_id.clone()));
                    let orig = state
                        .lighting
                        .canvas
                        .placed_zones
                        .iter()
                        .find(|z| z.device_id == dev_id && z.channel_id == channel_id)
                        .cloned()
                        .unwrap_or_default();
                    // Group move applies only to a Body drag; resize/rotate act
                    // on the pressed zone alone.
                    let group: Vec<PlacedZone> = if handle == Handle::Body {
                        state
                            .lighting
                            .canvas
                            .placed_zones
                            .iter()
                            .filter(|z| {
                                canvas_ui
                                    .selected
                                    .contains(&(z.device_id.clone(), z.channel_id.clone()))
                            })
                            .cloned()
                            .collect()
                    } else {
                        vec![orig.clone()]
                    };
                    let press_screen = resp
                        .interact_pointer_pos()
                        .unwrap_or_else(|| norm_to_screen(norm, canvas_rect));
                    canvas_ui.drag = Some(DragState {
                        handle,
                        orig,
                        group,
                        start_norm: norm,
                        press_screen,
                    });
                }
                None => {
                    // Start rubber-band marquee on empty canvas. A plain drag
                    // replaces the selection; a modifier+drag unions with it.
                    let base = if multi {
                        canvas_ui.selected.clone()
                    } else {
                        canvas_ui.selected.clear();
                        HashSet::new()
                    };
                    canvas_ui.marquee = Some(MarqueeState {
                        start_norm: norm,
                        cur_norm: norm,
                        additive: multi,
                        base,
                    });
                }
            }
        }
    }

    if resp.dragged() {
        if let Some(drag) = canvas_ui.drag.as_ref() {
            if let Some(norm) = ptr_norm {
                let delta = norm - drag.start_norm;
                let cur_screen = resp
                    .interact_pointer_pos()
                    .unwrap_or_else(|| norm_to_screen(norm, canvas_rect));
                // A Body drag moves the whole selection; resize/rotate act on the
                // pressed zone alone.
                let updated: Vec<PlacedZone> = if drag.handle == Handle::Body {
                    drag.group.iter().map(|z| body_move(z, delta)).collect()
                } else {
                    vec![apply_drag(drag, delta, cur_screen, canvas_rect)]
                };
                for u in updated {
                    canvas_ui
                        .drag_zones
                        .insert(zone_key(&u.device_id, &u.channel_id), u.clone());
                    canvas_ui.pending.queue_move(&u, time + DEBOUNCE);
                }
            }
        } else if let Some(norm) = ptr_norm {
            if let Some(mq) = canvas_ui.marquee.as_mut() {
                mq.cur_norm = norm;
            }
            if let Some(mq) = canvas_ui.marquee.as_ref() {
                let hits = zones_in_marquee(&state.lighting.canvas.placed_zones, mq, canvas_rect);
                canvas_ui.selected = widgets::marquee_result(&mq.base, hits, mq.additive);
            }
        }
    }

    if resp.drag_stopped() {
        if canvas_ui.drag.is_some() {
            for (_, (c, _)) in canvas_ui.pending.move_zones.drain() {
                crate::runtime::ipc::send(cmd, c);
            }
            // Keep drag_zones as an optimistic override; prune_drag_zones drops
            // it once the daemon broadcast reflects the new position.
            canvas_ui.drag = None;
        }
        canvas_ui.marquee = None;
    }

    if resp.clicked() && !resp.dragged() {
        if let Some(norm) = ptr_norm {
            // A click on the sole selection's top-right remove badge deletes it.
            if let Some((d, z)) = badge_hit(canvas_ui, state, norm, canvas_rect) {
                crate::runtime::ipc::send(
                    cmd,
                    halod_shared::commands::DaemonCommand::CanvasRemoveZone {
                        device_id: d,
                        channel_id: z,
                    },
                );
                canvas_ui.selected.clear();
            } else {
                let hit = hit_test(norm, &state.lighting.canvas.placed_zones, canvas_rect);
                match hit {
                    None if !multi => canvas_ui.selected.clear(),
                    Some((d, z, _)) => {
                        widgets::click_select(&mut canvas_ui.selected, (d, z), multi)
                    }
                    None => {}
                }
            }
        }
    }

    // Right-click on a ring zone → context menu.
    if resp.secondary_clicked() {
        if let Some(norm) = ptr_norm {
            if let Some((d, z, _)) =
                hit_test(norm, &state.lighting.canvas.placed_zones, canvas_rect)
            {
                let is_ring = rgb_zone_descriptor(state, &d, &z)
                    .map(|rz| {
                        matches!(rz.topology, ZoneTopology::Ring | ZoneTopology::Rings { .. })
                    })
                    .unwrap_or(false);
                if is_ring {
                    canvas_ui.selected.clear();
                    canvas_ui.selected.insert((d.clone(), z.clone()));
                    canvas_ui.context_menu_target = Some((d, z));
                    ui.ctx().request_repaint();
                }
            }
        }
    }

    draw_zones(ui.painter(), state, canvas_ui, canvas_rect);

    // Context menu popup for right-clicked ring zone.
    if let Some((ref dev_id, ref channel_id)) = canvas_ui.context_menu_target {
        let placed = state
            .lighting
            .canvas
            .placed_zones
            .iter()
            .find(|p| &p.device_id == dev_id && &p.channel_id == channel_id);
        if let Some(z) = placed {
            egui::Area::new("canvas_zone_context_menu".into())
                .order(egui::Order::Foreground)
                .pivot(egui::Align2::LEFT_TOP)
                .default_pos(ui.ctx().pointer_latest_pos().unwrap_or_default())
                .show(ui.ctx(), |ui| {
                    egui::Frame::popup(ui.style()).show(ui, |ui| {
                        ui.set_min_width(140.0);
                        ring_zone_context_menu(ui, cmd, z);
                        canvas_ui.context_menu_target = None;
                    });
                });
        } else {
            canvas_ui.context_menu_target = None;
        }
    }

    mode_toggle(ui, canvas_rect, canvas_ui);

    if let Some(mq) = &canvas_ui.marquee {
        let r = Rect::from_two_pos(
            norm_to_screen(mq.start_norm, canvas_rect),
            norm_to_screen(mq.cur_norm, canvas_rect),
        );
        ui.painter().rect_filled(r, 2.0, a(theme::CYAN, 0.07));
        ui.painter().rect_stroke(
            r,
            2.0,
            Stroke::new(1.0, a(theme::CYAN, 0.5)),
            egui::StrokeKind::Middle,
        );
    }

    if state.lighting.config.canvas_enabled {
        ui.ctx()
            .request_repaint_after(std::time::Duration::from_millis(50));
    }
}

// ── LED render-mode toggle ──────────────────────────────────────────────────────
/// Per-segment width for the mode toggle: at least `MODE_SEG_MIN`, but widened so
/// the widest label fits with padding (localized labels — "Fotogramma" — are far
/// wider than the English "Frame" the minimum was sized for).
fn mode_seg_width(max_label_w: f32) -> f32 {
    const MODE_SEG_MIN: f32 = 60.0;
    const MODE_SEG_PAD: f32 = 16.0;
    MODE_SEG_MIN.max(max_label_w + MODE_SEG_PAD * 2.0)
}

fn mode_toggle(ui: &mut egui::Ui, canvas_rect: Rect, canvas_ui: &mut CanvasUi) {
    const H: f32 = 26.0;
    let font = theme::body_sm();
    let segments = [
        (t!("canvas.mode_frame"), LedMode::Frame),
        (t!("canvas.mode_leds"), LedMode::Leds),
    ];
    let max_label_w = segments
        .iter()
        .map(|(label, _)| {
            ui.painter()
                .layout_no_wrap(label.to_string(), font.clone(), Color32::WHITE)
                .size()
                .x
        })
        .fold(0.0_f32, f32::max);
    let seg_w = mode_seg_width(max_label_w);

    let origin = Pos2::new(canvas_rect.left() + 12.0, canvas_rect.top() + 12.0);
    let bg = Rect::from_min_size(origin, Vec2::new(seg_w * 2.0, H));
    let p = ui.painter();
    p.rect_filled(bg, 8.0, a(theme::hex(0x0b0e14), 0.85));
    p.rect_stroke(
        bg,
        8.0,
        Stroke::new(1.0, theme::BORDER),
        egui::StrokeKind::Middle,
    );

    for (i, (label, mode)) in segments.into_iter().enumerate() {
        let seg = Rect::from_min_size(
            Pos2::new(origin.x + i as f32 * seg_w, origin.y),
            Vec2::new(seg_w, H),
        );
        let resp = ui.interact(seg, egui::Id::new(("led_mode", i)), Sense::click());
        let active = canvas_ui.led_mode == mode;
        if active {
            p.rect_filled(seg.shrink(3.0), 6.0, a(theme::CYAN, 0.18));
        } else if resp.hovered() {
            p.rect_filled(seg.shrink(3.0), 6.0, a(Color32::WHITE, 0.05));
            ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
        }
        p.text(
            seg.center(),
            Align2::CENTER_CENTER,
            label,
            font.clone(),
            if active { theme::CYAN } else { theme::TEXT_DIM },
        );
        if resp.clicked() {
            canvas_ui.led_mode = mode;
        }
    }
}

// ── Zone drawing ──────────────────────────────────────────────────────────────
fn draw_zones(p: &egui::Painter, state: &AppState, canvas_ui: &CanvasUi, canvas_rect: Rect) {
    let single_sel = if canvas_ui.selected.len() == 1 {
        canvas_ui.selected.iter().next().cloned()
    } else {
        None
    };
    let idx_map = instance_indices(state);

    for zone in &state.lighting.canvas.placed_zones {
        let key = zone_key(&zone.device_id, &zone.channel_id);
        let display = canvas_ui.drag_zones.get(&key).unwrap_or(zone);
        let selected = canvas_ui
            .selected
            .contains(&(zone.device_id.clone(), zone.channel_id.clone()));
        let angle = display.rotation.to_radians();
        let (sin, cos) = angle.sin_cos();
        let corners = zone_corners_sc(display, canvas_rect, sin, cos);

        let fill = if selected {
            a(theme::CYAN, 0.18)
        } else {
            a(Color32::WHITE, 0.07)
        };
        let border_col = if selected {
            theme::CYAN
        } else {
            theme::hex(0x2e3a50)
        };
        let outline = rounded_zone_outline_sc(display, canvas_rect, 7.0, sin, cos);
        p.add(egui::Shape::convex_polygon(
            outline,
            fill,
            Stroke::new(1.5, border_col),
        ));

        let device = state.devices.iter().find(|d| d.id == zone.device_id);
        if let Some(dev) = device {
            let led_key = (zone.device_id.clone(), zone.channel_id.clone());
            let zone_leds = canvas_ui.led_colors.get(&led_key);
            for cap in &dev.capabilities {
                if let DeviceCapability::Lighting(rgb) = cap {
                    if let Some(rz) = rgb
                        .descriptor
                        .channels
                        .iter()
                        .find(|z| z.id == zone.channel_id)
                    {
                        let unrolled = display.sampling_mode == SamplingMode::Unrolled
                            && matches!(
                                rz.topology,
                                ZoneTopology::Ring | ZoneTopology::Rings { .. }
                            );
                        let bounds = led_bounds(&rz.leds);
                        for (i, led) in rz.leds.iter().enumerate() {
                            let (fx, fy) = if unrolled {
                                halod_shared::zone_transform::unrolled_led_pos(i, rz.leds.len())
                            } else {
                                // Normalize each LED against the cloud's own extent so
                                // the outermost LEDs reach the box edge on every axis,
                                // instead of inheriting whatever margin the descriptor
                                // baked in.
                                fill_coords(led, bounds)
                            };
                            let pos = led_screen_pos(fx, fy, display, canvas_rect);
                            let col = match canvas_ui.led_mode {
                                LedMode::Frame => a(theme::TEXT_BRIGHT, 0.9),
                                LedMode::Leds => zone_leds
                                    .and_then(|m| m.get(&led.id))
                                    .map(|c| Color32::from_rgb(c.r, c.g, c.b))
                                    .unwrap_or(a(theme::TEXT_FAINT, 0.6)),
                            };
                            if canvas_ui.led_mode == LedMode::Leds {
                                // Glow halo so lit LEDs read clearly on the dark canvas.
                                theme::glow(p, pos, 5.0, col, 0.5);
                            }
                            p.circle_filled(pos, 2.5, col);
                        }
                    }
                }
            }
        }

        let dev_name = device.map(|d| d.name.as_str()).unwrap_or(&zone.device_id);
        let anchor = Pos2::new(
            corners[0].x + 5.0 * cos - 4.0 * sin,
            corners[0].y + 5.0 * sin + 4.0 * cos,
        );
        let galley = p.layout_no_wrap(
            format!("{dev_name} / {}", zone.channel_id),
            theme::semibold(8.5),
            theme::TEXT_DIM,
        );
        p.add(egui::epaint::TextShape::new(anchor, galley, theme::TEXT_DIM).with_angle(angle));

        // Badge showing which effect instance this zone belongs to.
        if let Some(inst) = zone
            .effect
            .as_deref()
            .or(state.lighting.canvas.default_effect.as_deref())
            .filter(|id| state.lighting.canvas.effects.contains_key(*id))
        {
            let col = idx_map
                .get(inst)
                .map(|&i| instance_color(i))
                .unwrap_or(theme::hex(0x39414f));
            let label = super::rack::instance_name(&state.lighting.canvas.effects, inst);
            let bx = corners.iter().map(|c| c.x).fold(f32::MAX, f32::min);
            let by = corners.iter().map(|c| c.y).fold(f32::MAX, f32::min);
            let g = p.layout_no_wrap(label.to_string(), theme::semibold(8.5), col);
            let rect =
                Rect::from_min_size(Pos2::new(bx, by - 17.0), Vec2::new(g.size().x + 22.0, 14.0));
            p.rect_filled(rect, theme::RADIUS_XS, a(theme::hex(0x0b0e14), 0.92));
            p.rect_stroke(
                rect,
                theme::RADIUS_XS,
                Stroke::new(1.0, a(col, 0.5)),
                egui::StrokeKind::Middle,
            );
            p.circle_filled(Pos2::new(rect.left() + 7.0, rect.center().y), 3.0, col);
            p.text(
                Pos2::new(rect.left() + 14.0, rect.center().y),
                Align2::LEFT_CENTER,
                label,
                theme::semibold(8.5),
                col,
            );
        }

        if single_sel
            .as_ref()
            .is_some_and(|(d, z)| *d == zone.device_id && *z == zone.channel_id)
        {
            // Same affordances as the LCD editor: one bottom-right resize handle,
            // a top-right remove badge, and the rotation nub above the top edge.
            let resize = widgets::resize_handle_rect(&corners);
            p.rect_filled(resize, 3.0, theme::CYAN);

            let top_mid = widgets::top_mid(&corners);
            let rot_h = rotation_handle_pos(top_mid, display.rotation);
            p.line_segment([top_mid, rot_h], Stroke::new(1.0, theme::TEXT_MUT));
            p.circle_filled(rot_h, HANDLE_R, theme::CYAN);

            let badge = widgets::remove_badge_rect(&corners).center();
            p.circle_filled(badge, widgets::REMOVE_BADGE_R, theme::OFFLINE);
            p.text(
                badge,
                Align2::CENTER_CENTER,
                "×",
                theme::body_md(),
                Color32::WHITE,
            );
        }
    }
}

// ── Hit testing ───────────────────────────────────────────────────────────────
fn hit_test(
    norm: Pos2,
    channels: &[PlacedZone],
    canvas_rect: Rect,
) -> Option<(String, String, Handle)> {
    let norm_screen = norm_to_screen(norm, canvas_rect);
    for zone in channels.iter().rev() {
        let corners = zone_corners(zone, canvas_rect);
        let top_mid = Pos2::new(
            (corners[0].x + corners[1].x) / 2.0,
            (corners[0].y + corners[1].y) / 2.0,
        );
        let rot_h = rotation_handle_pos(top_mid, zone.rotation);
        if (norm_screen - rot_h).length() < HANDLE_HIT_R {
            return Some((
                zone.device_id.clone(),
                zone.channel_id.clone(),
                Handle::Rotation,
            ));
        }
        // Single bottom-right (corner 2) resize handle, mirroring the LCD editor.
        if (norm_screen - corners[2]).length() < HANDLE_HIT_R {
            return Some((
                zone.device_id.clone(),
                zone.channel_id.clone(),
                Handle::Corner(2),
            ));
        }
        if point_in_zone(norm, zone) {
            return Some((
                zone.device_id.clone(),
                zone.channel_id.clone(),
                Handle::Body,
            ));
        }
    }
    None
}

/// If exactly one zone is selected and `norm` lands on its top-right remove
/// badge, return that zone's key. The badge only paints for the sole selection.
fn badge_hit(
    canvas_ui: &CanvasUi,
    state: &AppState,
    norm: Pos2,
    canvas_rect: Rect,
) -> Option<(String, String)> {
    if canvas_ui.selected.len() != 1 {
        return None;
    }
    let (d, z) = canvas_ui.selected.iter().next()?;
    let zone = state
        .lighting
        .canvas
        .placed_zones
        .iter()
        .find(|p| &p.device_id == d && &p.channel_id == z)?;
    let corners = zone_corners(zone, canvas_rect);
    let screen = norm_to_screen(norm, canvas_rect);
    widgets::remove_badge_rect(&corners)
        .contains(screen)
        .then(|| (d.clone(), z.clone()))
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::ui::screens::canvas::test_fixtures::{r, z};

    const ROTATE_DIST: f32 = 20.0;

    #[test]
    fn mode_seg_width_holds_minimum_for_short_labels() {
        // A short label ("Frame") stays at the 60px minimum.
        assert_eq!(mode_seg_width(24.0), 60.0);
    }

    #[test]
    fn mode_seg_width_grows_for_a_wide_label() {
        // A wide label ("Fotogramma") widens the segment to fit it plus padding.
        assert_eq!(mode_seg_width(70.0), 70.0 + 16.0 * 2.0);
    }

    #[test]
    fn rotation_handle_above_at_zero() {
        // Unrotated: handle sits directly above the top-edge midpoint.
        let h = rotation_handle_pos(Pos2::new(100.0, 50.0), 0.0);
        assert!((h.x - 100.0).abs() < 1e-4);
        assert!((h.y - (50.0 - ROTATE_DIST)).abs() < 1e-4);
    }

    #[test]
    fn rotation_handle_follows_clockwise() {
        // At +90° (clockwise on screen) the top edge faces right, so the handle
        // must move to the RIGHT of the midpoint (+x), not the left.
        let h = rotation_handle_pos(Pos2::new(100.0, 50.0), 90.0);
        assert!(
            h.x > 100.0 + ROTATE_DIST - 1e-3,
            "handle x={} not right",
            h.x
        );
        assert!((h.y - 50.0).abs() < 1e-3);
    }

    #[test]
    fn hit_test_resizes_only_from_the_bottom_right_corner() {
        // An axis-aligned zone: corner 2 (BR) resizes; the other corners fall
        // through to a body hit (only one resize handle, LCD-editor style).
        let zone = z(0.25, 0.25, 0.5, 0.5);
        let rect = r();
        let corners = zone_corners(&zone, rect);
        let at = |c: Pos2| screen_to_norm(c, rect);
        assert!(matches!(
            hit_test(at(corners[2]), std::slice::from_ref(&zone), rect),
            Some((_, _, Handle::Corner(2)))
        ));
        for i in [0usize, 1, 3] {
            assert!(
                matches!(
                    hit_test(at(corners[i]), std::slice::from_ref(&zone), rect),
                    Some((_, _, Handle::Body))
                ),
                "corner {i} should not be a resize handle"
            );
        }
    }
}
