// SPDX-License-Identifier: GPL-3.0-or-later
//! The stage: a static schematic render of the template (not the daemon's
//! live render — see the module doc), widget hit-testing/drag/resize/rotate,
//! and the rotation-aware painter used to composite widget sprites.

use std::collections::HashMap;

use egui::{Color32, Rect, Sense, Stroke, Vec2};
use halod_shared::lcd_custom::{param_str, BgKind, WidgetDef, FONT_INTER, FONT_MONO, FONT_SANS};
use halod_shared::lcd_geometry::MAX_SCALE;
use halod_shared::types::{EffectParamValue, LcdStatus, ScreenShape};

use super::geometry::{rotate_about, rotated_corners, rotation_active, snap_rotation};
use super::sprites::{
    content_bounds, ensure_sprite_textures, request_editor_render, resize_preview_rect,
};
use super::{send_def, DeviceUi, ResizePreview, TabCtx};
use crate::ui::components as widgets;
use crate::ui::screens::device::lcd::gif::decode_next_thumb;
use crate::ui::screens::device::lcd::preview::{cover_uv, paint_image_circle, panel_size};
use crate::ui::theme;

/// Screen distance from a selected widget's top edge to its rotation handle.
#[cfg(test)]
const ROTATE_DIST: f32 = 20.0;

/// `egui` fonts render visually larger than the daemon's `ab_glyph` fonts
/// (NotoSans / monospace) at the same nominal pixel size — the inline text
/// editor scales by this so the edit box matches the daemon-rendered sprite.
const FONT_CAL: f32 = 0.78;

/// `ROTATE_DIST` out from the top-edge midpoint along the widget's local up axis.
/// Delegated to the shared widget helper.
use widgets::rotation_handle_pos;

pub(super) fn stage(ui: &mut egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str, lcd: &LcdStatus) {
    let avail = (ui.available_width() - 4.0).clamp(220.0, 520.0);
    let size_vec = panel_size(avail, lcd.descriptor.width, lcd.descriptor.height);
    // Background senses clicks before per-widget interacts below, so a widget
    // (registered later, "on top") consumes clicks over it — giving
    // click-to-deselect without stealing widget clicks.
    let (rect, bg_resp) = ui
        .vertical_centered(|ui| ui.allocate_exact_size(size_vec, Sense::click_and_drag()))
        .inner;
    st.lcd.editor.stage_rect = Some(rect);
    crate::domain::tour::anchor(
        ui.ctx(),
        crate::domain::tour::AnchorId::LcdEditorStage,
        rect,
    );
    let center = rect.center();
    let radius = rect.width().min(rect.height()) / 2.0;

    let bg_tex = match &st.lcd.editor.def.style.background {
        BgKind::Image { filename, .. } if !filename.is_empty() => {
            let filename = filename.clone();
            decode_next_thumb(ui, ctx, st, std::iter::once(&filename));
            st.lcd.image_cache.get(&filename).cloned()
        }
        _ => None,
    };
    // Decode Image-widget files so the stage can preview the real picture.
    let image_files: Vec<String> = st
        .lcd
        .editor
        .def
        .widgets
        .iter()
        .flat_map(|widget| {
            let catalog_id = widget.widget.clone();
            ctx.state
                .lcd
                .engine
                .available_widgets
                .iter()
                .find(|descriptor| descriptor.id == catalog_id)
                .into_iter()
                .flat_map(move |descriptor| {
                    descriptor
                        .params
                        .iter()
                        .filter(|param| matches!(param.kind, halod_shared::types::ParamKind::Image))
                        .map(move |param| param_str(widget, &param.id))
                })
        })
        .filter(|f| !f.is_empty())
        .collect();
    decode_next_thumb(ui, ctx, st, image_files.iter());
    paint_stage_background(
        ui.painter(),
        rect,
        &st.lcd.editor.def.style,
        lcd.descriptor.shape.clone(),
        bg_tex.as_ref(),
    );
    match lcd.descriptor.shape {
        ScreenShape::Circle => {
            ui.painter()
                .circle_stroke(center, radius, Stroke::new(6.0, theme::BODY));
        }
        ScreenShape::Square => {
            ui.painter().rect_stroke(
                rect,
                theme::RADIUS_XL,
                Stroke::new(6.0, theme::BODY),
                egui::StrokeKind::Outside,
            );
        }
    }

    // Matches the daemon's `widget_rect` formula: scale off min(panel_w, panel_h).
    let stage_min = rect.width().min(rect.height());

    // x/y normalised over the full panel, exactly like the daemon's
    // `widget_rect` (no inscribed-square inset), or placement disagrees.
    let (clamp_off_x, clamp_w) = (rect.min.x, rect.width());
    let (clamp_off_y, clamp_h) = (rect.min.y, rect.height());

    // Editor preview is authoritative-from-daemon: it renders each widget to a
    // sprite bitmap; the stage composites those instead of re-drawing content.
    // Request a fresh render on open, on edits, and ~1/s for live widgets.
    request_editor_render(ui, ctx, st, id);
    let render = ctx.lcd_editor_render.as_ref().filter(|r| r.device_id == id);
    if let Some(render) = render {
        st.lcd.editor.canvas = Some((render.canvas_w, render.canvas_h));
        ensure_sprite_textures(ui, st, render);
    }
    let canvas = st
        .lcd
        .editor
        .canvas
        .unwrap_or((lcd.descriptor.width.max(1), lcd.descriptor.height.max(1)));
    // Sprite dims come from the cached textures (which persist across the
    // many frames a delta reply carries no update for), not `render.sprites`
    // — a delta reply only lists widgets that changed this tick.
    // Owned keys (not `&str` into `sprite_tex`) so the map doesn't hold a borrow
    // of `st.lcd.editor` across the widget loop, which would block the selection
    // helpers (`select_only`, `clear_selection`) that take `&mut self`.
    let sprite_dims: HashMap<String, (u32, u32)> = st
        .lcd
        .editor
        .sprite_tex
        .iter()
        .map(|(id, (_, tex))| {
            let [w, h] = tex.size();
            (id.clone(), (w as u32, h as u32))
        })
        .collect();

    let ids: Vec<String> = st
        .lcd
        .editor
        .def
        .widgets
        .iter()
        .map(|w| w.id.clone())
        .collect();
    let mut moved = false;
    let mut resized = false;
    let mut rotated = false;
    let mut deleted = false;
    let mut edited = false;
    let mut widget_clicked = false;
    let mut widget_double_clicked = false;
    // Each widget's on-stage hit rect, collected for marquee intersection.
    let mut widget_rects: Vec<(String, Rect)> = Vec::new();
    let was_editing = st.lcd.editor.editing_text.is_some();
    // A resize preview can't outlive its gesture: with the pointer up there is
    // no active drag, so drop any lingering capture (e.g. released off-window).
    if !ui.input(|i| i.pointer.primary_down()) {
        st.lcd.editor.resize_preview = None;
    }
    for wid in &ids {
        let Some(idx) = st.lcd.editor.def.widgets.iter().position(|w| &w.id == wid) else {
            continue;
        };
        let snapshot = st.lcd.editor.def.widgets[idx].clone();
        // `size` is the daemon's exact `widget_rect` output (shared formula), so
        // the stage stays pixel-proportional to the device — the fallback box
        // when no sprite has rendered yet.
        let size = halod_shared::lcd_geometry::widget_size(snapshot.scale, stage_min);
        let pos = egui::pos2(
            clamp_off_x + snapshot.x.clamp(0.0, 1.0) * clamp_w,
            clamp_off_y + snapshot.y.clamp(0.0, 1.0) * clamp_h,
        );
        let selected = st.lcd.editor.is_selected(wid);
        // Per-widget handles (resize/rotate/remove) appear only for the sole
        // selection; a multi-selection just highlights and group-moves.
        let is_primary = st.lcd.editor.primary().is_some_and(|p| p == wid);
        let editing = st.lcd.editor.editing_text.as_deref() == Some(wid.as_str())
            && snapshot.params.contains_key("text");
        let default_font = st.lcd.editor.def.style.font.clone();
        // Hit-test against the daemon sprite's on-stage rect, not the (much
        // larger) nominal box. `corners` is that content box rotated about
        // `pos`; its axis-aligned bounds (`hit`) is what egui interacts with.
        let mut content = content_bounds(
            pos,
            sprite_dims.get(wid.as_str()).copied(),
            canvas,
            rect,
            size,
        );
        // While this widget is being resized, preview the new size by scaling
        // the captured start box by the live scale ratio — the daemon's sprite
        // for the new scale lags a render tick behind (unlike move/rotate, which
        // are applied locally). Cleared on release, falling back to the sprite.
        if let Some(rp) = &st.lcd.editor.resize_preview {
            if rp.id == *wid {
                content = resize_preview_rect(
                    pos,
                    rp.start_size,
                    rp.start_scale,
                    rp.start_scale_y,
                    snapshot.scale,
                    halod_shared::lcd_custom::scale_y(&snapshot),
                );
            }
        }
        let deg = snapshot.rotation;
        let corners = rotated_corners(content, pos, deg);
        let hit = Rect::from_points(&corners);
        widget_rects.push((wid.clone(), hit));

        let resp = ui.interact(
            hit,
            ui.id().with(("lcd_editor_widget", wid.as_str())),
            Sense::click_and_drag(),
        );
        if resp.dragged() && !editing && !is_primary {
            let delta = resp.drag_delta();
            // Dragging a widget that's part of a multi-selection moves the whole
            // group by the same normalized delta; otherwise just this one. Only
            // the center is kept on the panel; content may hang off the edge.
            let (dnx, dny) = (delta.x / clamp_w, delta.y / clamp_h);
            let group = selected && st.lcd.editor.selected.len() > 1;
            let targets: Vec<String> = if group {
                st.lcd.editor.selected.iter().cloned().collect()
            } else {
                vec![wid.clone()]
            };
            for tid in &targets {
                if let Some(w) = st.lcd.editor.def.widgets.iter_mut().find(|w| &w.id == tid) {
                    w.x = (w.x + dnx).clamp(0.0, 1.0);
                    w.y = (w.y + dny).clamp(0.0, 1.0);
                }
            }
            moved = true;
        }
        if resp.double_clicked() && snapshot.params.contains_key("text") {
            widget_double_clicked = true;
            st.lcd.editor.select_only(wid.clone());
            st.lcd.editor.editing_text = Some(wid.clone());
            st.lcd.editor.focus_editing = true;
        } else if resp.clicked() {
            widget_clicked = true;
        }
        if resp.hovered() {
            ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
        }

        let color =
            widgets::rgb_to_color32(snapshot.color.unwrap_or(st.lcd.editor.def.style.accent));
        let hovered = resp.hovered();

        if editing {
            let mut buf = param_str(&snapshot, "text");
            let edit_h = halod_shared::lcd_geometry::widget_size(
                halod_shared::lcd_custom::scale_y(&snapshot),
                stage_min,
            );
            let edit_rect = Rect::from_center_size(pos, Vec2::new(size, edit_h));
            let font_id = resolved_font(
                &snapshot,
                &default_font,
                edit_h * 0.64 / FONT_CAL,
                &st.lcd.editor.registered_fonts,
            );
            let text_edit = egui::TextEdit::singleline(&mut buf)
                .font(font_id)
                .text_color(color)
                .horizontal_align(egui::Align::Center)
                .vertical_align(egui::Align::Center)
                .clip_text(false)
                .frame(egui::Frame::NONE);
            // A child `Ui` rather than `ui.put`, which would advance the parent
            // cursor to below `edit_rect` and pull the stage footer on top of
            // the canvas.
            let mut edit_ui = ui.new_child(egui::UiBuilder::new().max_rect(edit_rect).layout(
                egui::Layout::centered_and_justified(egui::Direction::LeftToRight),
            ));
            let edit_resp = edit_ui.add(text_edit);
            if st.lcd.editor.focus_editing {
                st.lcd.editor.focus_editing = false;
                edit_resp.request_focus();
            }
            if edit_resp.lost_focus() {
                st.lcd.editor.editing_text = None;
            }
            if edit_resp.changed() {
                let descriptor = ctx
                    .state
                    .lcd
                    .engine
                    .available_widgets
                    .iter()
                    .find(|descriptor| descriptor.id == snapshot.widget);
                let widget = &mut st.lcd.editor.def.widgets[idx];
                widget
                    .params
                    .insert("text".to_string(), EffectParamValue::Str(buf.clone()));
                if let Some(descriptor) = descriptor
                    .filter(|descriptor| descriptor.auto_width_param.as_deref() == Some("text"))
                {
                    let height = halod_shared::lcd_custom::scale_y(widget);
                    let content_width = buf.chars().count() as f32 * height * 0.22;
                    widget.scale = content_width
                        .max(descriptor.default_scale)
                        .clamp(descriptor.min_scale, MAX_SCALE);
                }
                edited = true;
            }
            if ui.input(|i| i.key_pressed(egui::Key::Enter) || i.key_pressed(egui::Key::Escape)) {
                st.lcd.editor.editing_text = None;
            }
        }
        if selected || hovered {
            let p = ui.painter();
            let stroke = if selected {
                Stroke::new(1.5, theme::CYAN)
            } else {
                Stroke::new(1.0, theme::a(theme::TEXT_MUT, 0.6))
            };
            let fill = if selected {
                theme::a(theme::CYAN, 0.10)
            } else {
                Color32::TRANSPARENT
            };
            if rotation_active(deg) {
                p.add(egui::Shape::convex_polygon(corners.to_vec(), fill, stroke));
            } else {
                // Unrotated: keep the rounded-rect look of the original outline.
                if selected {
                    p.rect_filled(content, theme::RADIUS_XS, fill);
                }
                p.rect_stroke(content, theme::RADIUS_XS, stroke, egui::StrokeKind::Outside);
            }
        }
        if !editing {
            // Composite the daemon's per-widget sprite (opacity already baked
            // into its alpha), clipped to the stage so content hanging off the
            // panel edge is cut exactly like on the device. Rotation and scale
            // are applied here; the sprite itself is unrotated content.
            match st.lcd.editor.sprite_tex.get(wid.as_str()) {
                Some((_, tex)) => {
                    let base = ui.painter().with_clip_rect(rect);
                    let rp = RotPainter::new(base, pos, deg);
                    let full_uv = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0));
                    rp.image(content, tex.id(), full_uv, Color32::WHITE);
                }
                None => {
                    let painter = ui.painter().with_clip_rect(rect);
                    painter.rect_filled(content, theme::RADIUS_XS, theme::a(theme::TEXT_MUT, 0.10));
                    if st.lcd.editor.missing_widgets.contains(wid) {
                        painter.rect_stroke(
                            content,
                            theme::RADIUS_XS,
                            egui::Stroke::new(1.0, theme::OFFLINE),
                            egui::StrokeKind::Inside,
                        );
                        paint_missing_icon(&painter, content.center());
                    }
                }
            }
        }

        if is_primary {
            // Resize handle at the (rotated) bottom-right corner.
            let handle_c = corners[2];
            let handle_rect = Rect::from_center_size(handle_c, Vec2::splat(14.0));
            let hresp = ui.interact(
                handle_rect,
                ui.id().with(("lcd_editor_resize", wid.as_str())),
                Sense::drag(),
            );
            ui.painter().rect_filled(handle_rect, 3.0, theme::CYAN);
            if hresp.drag_started() {
                // Capture the true (daemon-sprite) start box before this frame's
                // drag; later frames scale it by the live scale ratio.
                st.lcd.editor.resize_preview = Some(ResizePreview {
                    id: wid.clone(),
                    start_size: content.size(),
                    start_scale: snapshot.scale.clamp(
                        ctx.state
                            .lcd
                            .engine
                            .available_widgets
                            .iter()
                            .find(|descriptor| descriptor.id == snapshot.widget)
                            .map_or(0.6, |descriptor| descriptor.min_scale),
                        MAX_SCALE,
                    ),
                    start_scale_y: halod_shared::lcd_custom::scale_y(&snapshot),
                });
            }
            if hresp.dragged() {
                if let (Some(cur), Some(rp)) = (
                    hresp.interact_pointer_pos(),
                    st.lcd.editor.resize_preview.clone(),
                ) {
                    // Put the widget's local bottom-right content corner under the
                    // pointer so the handle tracks the cursor 1:1 (no rubber-band),
                    // then back the scale(s) out of the resulting box. Unrotate the
                    // pointer into the widget's own frame first.
                    let (s, c) = (-deg).to_radians().sin_cos();
                    let local = rotate_about(cur, pos, s, c) - pos;
                    let catalog_id = snapshot.widget.clone();
                    let descriptor = ctx
                        .state
                        .lcd
                        .engine
                        .available_widgets
                        .iter()
                        .find(|descriptor| descriptor.id == catalog_id);
                    let box_widget = descriptor.is_some_and(|descriptor| {
                        descriptor.resize == halod_shared::types::LcdWidgetResize::Box
                    });
                    let min_scale = descriptor.map_or(0.6, |descriptor| descriptor.min_scale);
                    let (sx, sy) = super::sprites::resize_scales(
                        local,
                        rp.start_size,
                        rp.start_scale,
                        rp.start_scale_y,
                        box_widget,
                        min_scale,
                    );
                    let w = &mut st.lcd.editor.def.widgets[idx];
                    w.scale = sx;
                    if box_widget {
                        w.params.insert(
                            "scale_y".to_string(),
                            halod_shared::types::EffectParamValue::Float(sy as f64),
                        );
                    } else {
                        w.params.remove("scale_y"); // stay uniform
                    }
                    resized = true;
                }
            }
            if hresp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::ResizeSouthEast);
            }

            // Rotation handle, out beyond the (rotated) top edge — mirrors the
            // effects canvas. Dragging spins the widget about its centre; the
            // angle change is derived from pointer movement around `pos`.
            let top_mid = egui::pos2(
                (corners[0].x + corners[1].x) / 2.0,
                (corners[0].y + corners[1].y) / 2.0,
            );
            let rot_c = rotation_handle_pos(top_mid, deg);
            let rot_rect = Rect::from_center_size(rot_c, Vec2::splat(16.0));
            let rresp = ui.interact(
                rot_rect,
                ui.id().with(("lcd_editor_rotate", wid.as_str())),
                Sense::drag(),
            );
            {
                let p = ui.painter();
                p.line_segment(
                    [top_mid, rot_c],
                    Stroke::new(1.0, theme::a(theme::TEXT_MUT, 0.8)),
                );
                p.circle_filled(rot_c, 6.0, theme::CYAN);
            }
            if rresp.dragged() {
                if let Some(cur) = rresp.interact_pointer_pos() {
                    let prev = cur - rresp.drag_delta();
                    let a_now = (cur.y - pos.y).atan2(cur.x - pos.x);
                    let a_prev = (prev.y - pos.y).atan2(prev.x - pos.x);
                    let next = snap_rotation(
                        st.lcd.editor.def.widgets[idx].rotation + (a_now - a_prev).to_degrees(),
                    );
                    st.lcd.editor.def.widgets[idx].rotation = next;
                    rotated = true;
                }
            }
            if rresp.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::Grabbing);
            }

            // Delete badge at the (rotated) top-right corner.
            let badge_c = corners[1];
            let badge_rect = Rect::from_center_size(badge_c, Vec2::splat(18.0));
            let badge = ui.interact(
                badge_rect,
                ui.id().with(("lcd_editor_del", wid.as_str())),
                Sense::click(),
            );
            let p = ui.painter();
            p.circle_filled(badge_c, 9.0, theme::OFFLINE);
            p.text(
                badge_c,
                egui::Align2::CENTER_CENTER,
                "×",
                theme::body_md(),
                Color32::WHITE,
            );
            if badge.hovered() {
                ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
            }
            if badge.clicked() {
                st.lcd.editor.def.widgets.remove(idx);
                st.lcd.editor.clear_selection();
                st.lcd.editor.editing_text = None;
                deleted = true;
            }
        }
    }

    // A sole selection gets a final, inset interaction surface. It is
    // registered after every normal widget hit target, so a selected widget
    // remains draggable even when a later (visually higher) widget covers it.
    // Insets keep the edge handles authoritative.
    if let Some(selected_id) = st.lcd.editor.primary().cloned() {
        if st.lcd.editor.editing_text.as_deref() != Some(selected_id.as_str()) {
            if let Some((_, selected_hit)) = widget_rects
                .iter()
                .find(|(widget_id, _)| widget_id == &selected_id)
            {
                let inset = (selected_hit.width().min(selected_hit.height()) * 0.2).min(9.0);
                let drag_hit = selected_hit.shrink(inset);
                let selected_resp = ui.interact(
                    drag_hit,
                    ui.id()
                        .with(("lcd_editor_selected_overlay", selected_id.as_str())),
                    Sense::click_and_drag(),
                );
                if selected_resp.dragged() {
                    let delta = selected_resp.drag_delta();
                    if let Some(widget) = st
                        .lcd
                        .editor
                        .def
                        .widgets
                        .iter_mut()
                        .find(|widget| widget.id == selected_id)
                    {
                        widget.x = (widget.x + delta.x / clamp_w).clamp(0.0, 1.0);
                        widget.y = (widget.y + delta.y / clamp_h).clamp(0.0, 1.0);
                        moved = true;
                    }
                }
                let double_clicked = selected_resp.clicked()
                    && ui.input(|input| {
                        input
                            .pointer
                            .button_double_clicked(egui::PointerButton::Primary)
                    })
                    && st
                        .lcd
                        .editor
                        .def
                        .widgets
                        .iter()
                        .find(|widget| widget.id == selected_id)
                        .is_some_and(|widget| widget.params.contains_key("text"));
                if double_clicked {
                    widget_double_clicked = true;
                    st.lcd.editor.editing_text = Some(selected_id.clone());
                    st.lcd.editor.focus_editing = true;
                } else {
                    widget_clicked |= selected_resp.clicked();
                }
                if selected_resp.hovered() {
                    ui.ctx().set_cursor_icon(egui::CursorIcon::Grab);
                }
            }
        }
    }

    // Repeated clicks at an overlap cycle from the visually top widget down
    // through every widget under the pointer. Once selected, the interaction
    // surface above makes that lower widget directly draggable.
    if widget_clicked && !widget_double_clicked {
        if let Some(pointer) = ui.input(|input| input.pointer.latest_pos()) {
            let current = st.lcd.editor.primary().map(String::as_str);
            if let Some(target) = cycle_widget_at(pointer, &widget_rects, current) {
                let additive = ui.input(|input| input.modifiers.ctrl || input.modifiers.shift);
                widgets::click_select(&mut st.lcd.editor.selected, target.clone(), additive);
                if st.lcd.editor.editing_text.as_deref() != Some(target.as_str()) {
                    st.lcd.editor.editing_text = None;
                }
            }
        }
    }

    // Rubber-band marquee: a drag that begins on empty stage space (widgets, on
    // top, consume their own drags) selects everything it touches — mirroring the
    // Effects Canvas. A modifier held at start unions with the prior selection.
    let multi = ui.input(|i| i.modifiers.ctrl || i.modifiers.shift);
    if bg_resp.drag_started() {
        if let Some(p) = bg_resp.interact_pointer_pos() {
            let base = if multi {
                st.lcd.editor.selected.clone()
            } else {
                st.lcd.editor.clear_selection();
                std::collections::HashSet::new()
            };
            st.lcd.editor.marquee = Some(super::Marquee {
                start: p,
                cur: p,
                additive: multi,
                base,
            });
        }
    }
    if bg_resp.dragged() {
        if let Some(p) = bg_resp.interact_pointer_pos() {
            if let Some(mq) = st.lcd.editor.marquee.as_mut() {
                mq.cur = p;
            }
            if let Some(mq) = st.lcd.editor.marquee.as_ref() {
                let m = Rect::from_two_pos(mq.start, mq.cur);
                let hits = widget_rects
                    .iter()
                    .filter(|(_, r)| m.intersects(*r))
                    .map(|(wid, _)| wid.clone());
                st.lcd.editor.selected = widgets::marquee_result(&mq.base, hits, mq.additive);
            }
        }
    }
    if bg_resp.drag_stopped() {
        st.lcd.editor.marquee = None;
    }
    if let Some(mq) = &st.lcd.editor.marquee {
        let r = Rect::from_two_pos(mq.start, mq.cur);
        ui.painter()
            .rect_filled(r, 2.0, theme::a(theme::CYAN, 0.07));
        ui.painter().rect_stroke(
            r,
            2.0,
            Stroke::new(1.0, theme::a(theme::CYAN, 0.5)),
            egui::StrokeKind::Middle,
        );
    }

    // Click on empty stage space (widgets, on top, consume their own clicks)
    // deselects — but not while inline-editing text, so clicking into the text
    // field doesn't drop the selection out from under the editor.
    if bg_resp.clicked() && st.lcd.editor.editing_text.is_none() {
        st.lcd.editor.clear_selection();
    }

    // Keyboard shortcuts — suppressed while any text field has focus (the
    // inline stage editor, but also the inspector's label/text inputs, where
    // Delete/letters must edit text, not delete or move a widget).
    if st.lcd.editor.editing_text.is_none() && !ui.ctx().egui_wants_keyboard_input() {
        // Escape deselects — unless it was consumed this frame to leave
        // inline text editing.
        if !was_editing && ui.input(|i| i.key_pressed(egui::Key::Escape)) {
            st.lcd.editor.clear_selection();
        }
        if ui.input(|i| i.key_pressed(egui::Key::Delete)) && !st.lcd.editor.selected.is_empty() {
            let sel = &st.lcd.editor.selected;
            st.lcd.editor.def.widgets.retain(|w| !sel.contains(&w.id));
            st.lcd.editor.clear_selection();
            deleted = true;
        }
        // Single-selection axis centering, Figma-style.
        let sel = st.lcd.editor.primary().cloned();
        // Center the selected widget on one axis, Figma-style: Alt+H snaps to
        // the horizontal center, Alt+V to the vertical center.
        let (center_h, center_v) = ui.input(|i| {
            (
                i.modifiers.alt && i.key_pressed(egui::Key::H),
                i.modifiers.alt && i.key_pressed(egui::Key::V),
            )
        });
        if center_h || center_v {
            if let Some(ref sid) = sel {
                if let Some(w) = st.lcd.editor.def.widgets.iter_mut().find(|w| &w.id == sid) {
                    if center_h {
                        w.x = 0.5;
                    }
                    if center_v {
                        w.y = 0.5;
                    }
                    moved = true;
                }
            }
        }
    }

    if deleted {
        send_def(ctx, st, id, true);
    } else if moved || resized || rotated || edited {
        send_def(ctx, st, id, false);
    }

    ui.add_space(theme::SPACE_3);
    ui.horizontal(|ui| {
        ui.label(
            egui::RichText::new(t!("lcd.widget_count", n = st.lcd.editor.def.widgets.len()))
                .font(theme::mono(10.5))
                .color(theme::TEXT_FAINT),
        );
        if !st.lcd.editor.def.widgets.is_empty()
            && widgets::button(
                ui,
                &t!("lcd.clear_all"),
                widgets::ButtonKind::Ghost,
                egui::vec2(80.0, 24.0),
            )
            .clicked()
        {
            st.lcd.editor.def.widgets.clear();
            st.lcd.editor.clear_selection();
            send_def(ctx, st, id, true);
        }
    });
    if st.lcd.editor.def.widgets.is_empty() {
        ui.label(
            egui::RichText::new(t!("lcd.empty_screen_hint"))
                .font(theme::caption())
                .color(theme::TEXT_FAINT),
        );
    } else {
        ui.label(
            egui::RichText::new(t!("lcd.editor_shortcuts_hint"))
                .font(theme::body(9.5))
                .color(theme::TEXT_FAINT2),
        );
    }
}

fn cycle_widget_at(
    point: egui::Pos2,
    widget_rects: &[(String, Rect)],
    current: Option<&str>,
) -> Option<String> {
    let hits: Vec<&str> = widget_rects
        .iter()
        .rev()
        .filter(|(_, rect)| rect.contains(point))
        .map(|(id, _)| id.as_str())
        .collect();
    if hits.is_empty() {
        return None;
    }
    let next = current
        .and_then(|current| hits.iter().position(|id| *id == current))
        .map_or(0, |index| (index + 1) % hits.len());
    Some(hits[next].to_owned())
}

/// Local, asset-independent "missing content" glyph. Missing plugin assets
/// cannot be trusted to supply their own icon, so the editor always has this
/// small broken-file mark available.
fn paint_missing_icon(painter: &egui::Painter, center: egui::Pos2) {
    let icon = Rect::from_center_size(center, Vec2::new(18.0, 22.0));
    let fold = 5.0;
    let stroke = Stroke::new(1.5, theme::OFFLINE);
    painter.line_segment(
        [icon.left_top(), egui::pos2(icon.right() - fold, icon.top())],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(icon.right() - fold, icon.top()),
            egui::pos2(icon.right(), icon.top() + fold),
        ],
        stroke,
    );
    painter.line_segment(
        [
            egui::pos2(icon.right(), icon.top() + fold),
            icon.right_bottom(),
        ],
        stroke,
    );
    painter.line_segment([icon.right_bottom(), icon.left_bottom()], stroke);
    painter.line_segment([icon.left_bottom(), icon.left_top()], stroke);
    painter.line_segment(
        [
            egui::pos2(icon.left() + 4.0, icon.center().y + 4.0),
            egui::pos2(icon.right() - 4.0, icon.center().y - 4.0),
        ],
        stroke,
    );
}

/// Procedural, non-live approximation of the daemon's four background kinds.
/// `Image` paints the cached first frame (`bg_tex`, from the shared library
/// thumbnail cache — a GIF stays static here), falling back to a labeled
/// placeholder while it decodes.
fn paint_stage_background(
    p: &egui::Painter,
    rect: Rect,
    style: &halod_shared::lcd_custom::ScreenStyle,
    shape: ScreenShape,
    bg_tex: Option<&egui::TextureHandle>,
) {
    let accent = widgets::rgb_to_color32(style.accent);
    // For circle screens, fill with card background so the area outside the
    // circle blends in — no "black square behind the circle".
    let screen_bg = match shape {
        ScreenShape::Circle => theme::CARD_BG,
        ScreenShape::Square => theme::BODY,
    };
    p.rect_filled(rect, theme::RADIUS_LG, screen_bg);
    match &style.background {
        BgKind::Flow => theme::glow(p, rect.center(), rect.width() * 0.45, accent, 0.45),
        BgKind::Glow => theme::glow(p, rect.center(), rect.width() * 0.6, accent, 0.60),
        BgKind::Solid => {
            let fill = theme::lerp_color(theme::BODY, accent, 0.14);
            match shape {
                // Clip the fill to the ring so it doesn't bleed into the corners
                // outside a circular panel.
                ScreenShape::Circle => {
                    let r = rect.width().min(rect.height()) / 2.0;
                    p.circle_filled(rect.center(), r, fill);
                }
                ScreenShape::Square => {
                    p.rect_filled(rect, theme::RADIUS_LG, fill);
                }
            }
        }
        BgKind::Grid => {
            let step = (rect.width() / 8.0).max(8.0);
            let line = theme::a(accent, 0.18);
            let mut x = rect.left();
            while x < rect.right() {
                p.line_segment(
                    [egui::pos2(x, rect.top()), egui::pos2(x, rect.bottom())],
                    Stroke::new(1.0, line),
                );
                x += step;
            }
            let mut y = rect.top();
            while y < rect.bottom() {
                p.line_segment(
                    [egui::pos2(rect.left(), y), egui::pos2(rect.right(), y)],
                    Stroke::new(1.0, line),
                );
                y += step;
            }
        }
        BgKind::Image { filename, dim } => match bg_tex {
            Some(tex) => {
                let uv = cover_uv(tex);
                let dim_a = ((dim.clamp(0.0, 100.0) / 100.0) * 255.0).round() as u8;
                match shape {
                    ScreenShape::Circle => {
                        let radius = rect.width().min(rect.height()) / 2.0;
                        paint_image_circle(p, tex, rect.center(), radius, uv);
                        if dim_a > 0 {
                            p.circle_filled(
                                rect.center(),
                                radius,
                                Color32::from_black_alpha(dim_a),
                            );
                        }
                    }
                    ScreenShape::Square => {
                        p.image(tex.id(), rect, uv, Color32::WHITE);
                        if dim_a > 0 {
                            p.rect_filled(rect, theme::RADIUS_LG, Color32::from_black_alpha(dim_a));
                        }
                    }
                }
            }
            None => {
                theme::glow(p, rect.center(), rect.width() * 0.5, accent, 0.30);
                let label = if filename.is_empty() {
                    t!("lcd.no_image_selected").to_string()
                } else {
                    filename.clone()
                };
                p.text(
                    rect.center(),
                    egui::Align2::CENTER_CENTER,
                    label,
                    theme::value_sm(),
                    theme::TEXT_DIM,
                );
            }
        },
    }
}

fn resolved_font(
    w: &WidgetDef,
    default_font: &str,
    sz: f32,
    registered_fonts: &std::collections::HashSet<String>,
) -> egui::FontId {
    let cal = sz * FONT_CAL;
    let selected = w.font.as_deref().unwrap_or(default_font);
    let family = match selected {
        FONT_SANS => "lcd_sans",
        FONT_MONO => "lcd_mono",
        FONT_INTER => "lcd_inter",
        other if registered_fonts.contains(other) => other,
        _ => "lcd_sans",
    };
    egui::FontId::new(cal, egui::FontFamily::Name(family.into()))
}

/// A painter wrapper that rotates every primitive about `origin`, forwarding
/// verbatim (pixel-identical) at zero rotation. Clipping only applies unrotated.
struct RotPainter {
    p: egui::Painter,
    origin: egui::Pos2,
    sin: f32,
    cos: f32,
    rotating: bool,
}

impl RotPainter {
    fn new(p: egui::Painter, origin: egui::Pos2, deg: f32) -> Self {
        let (sin, cos) = deg.to_radians().sin_cos();
        Self {
            p,
            origin,
            sin,
            cos,
            rotating: rotation_active(deg),
        }
    }

    fn rot(&self, pt: egui::Pos2) -> egui::Pos2 {
        if !self.rotating {
            return pt;
        }
        rotate_about(pt, self.origin, self.sin, self.cos)
    }

    fn quad(&self, rect: Rect) -> Vec<egui::Pos2> {
        [
            rect.left_top(),
            rect.right_top(),
            rect.right_bottom(),
            rect.left_bottom(),
        ]
        .into_iter()
        .map(|c| self.rot(c))
        .collect()
    }

    fn image(&self, rect: Rect, texture: egui::TextureId, uv: Rect, tint: Color32) {
        if !self.rotating {
            self.p.image(texture, rect, uv, tint);
            return;
        }
        let corners = self.quad(rect);
        let uvs = [
            uv.left_top(),
            uv.right_top(),
            uv.right_bottom(),
            uv.left_bottom(),
        ];
        let mut mesh = egui::Mesh::with_texture(texture);
        for (pos, uv) in corners.iter().zip(uvs) {
            mesh.vertices.push(egui::epaint::Vertex {
                pos: *pos,
                uv,
                color: tint,
            });
        }
        mesh.add_triangle(0, 1, 2);
        mesh.add_triangle(0, 2, 3);
        self.p.add(egui::Shape::mesh(mesh));
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn rotation_handle_sits_above_at_zero_and_right_at_90() {
        let up = rotation_handle_pos(egui::pos2(100.0, 50.0), 0.0);
        assert!((up.x - 100.0).abs() < 1e-4);
        assert!((up.y - (50.0 - ROTATE_DIST)).abs() < 1e-4);
        // +90° clockwise: the top edge faces right, so the handle moves right.
        let right = rotation_handle_pos(egui::pos2(100.0, 50.0), 90.0);
        assert!(right.x > 100.0 + ROTATE_DIST - 1e-3);
        assert!((right.y - 50.0).abs() < 1e-3);
    }

    #[test]
    fn repeated_overlap_clicks_cycle_from_top_to_bottom() {
        let shared = Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(20.0, 20.0));
        let widgets = vec![
            ("bottom".to_owned(), shared),
            ("middle".to_owned(), shared),
            ("top".to_owned(), shared),
        ];
        let point = egui::pos2(10.0, 10.0);

        assert_eq!(
            cycle_widget_at(point, &widgets, None).as_deref(),
            Some("top")
        );
        assert_eq!(
            cycle_widget_at(point, &widgets, Some("top")).as_deref(),
            Some("middle")
        );
        assert_eq!(
            cycle_widget_at(point, &widgets, Some("middle")).as_deref(),
            Some("bottom")
        );
        assert_eq!(
            cycle_widget_at(point, &widgets, Some("bottom")).as_deref(),
            Some("top")
        );
    }
}
