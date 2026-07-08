// SPDX-License-Identifier: GPL-3.0-or-later
//! Daemon-rendered widget sprites for the LCD editor stage: requesting a
//! render, uploading/caching the resulting textures, and the pure geometry
//! that maps a sprite (or its fallback nominal box) onto the stage.

use egui::{Rect, Vec2};
use halod_shared::lcd_geometry::MAX_SCALE;

use super::{DeviceUi, TabCtx};

const EDITOR_PREVIEW_FPS: f64 = 5.0;
const EDITOR_PREVIEW_INTERVAL: f64 = 1.0 / EDITOR_PREVIEW_FPS;

/// Ask the daemon to render the current def to per-widget sprites, rate-limited
/// to [`EDITOR_PREVIEW_FPS`]. Schedules a repaint one interval out so the
/// periodic re-request keeps firing while the tab is idle. Sends `known`
/// (every cached texture's id → signature) so the daemon can reply with only
/// the widgets that changed.
pub(super) fn request_editor_render(ui: &egui::Ui, ctx: &TabCtx, st: &mut DeviceUi, id: &str) {
    ui.ctx()
        .request_repaint_after(std::time::Duration::from_secs_f64(EDITOR_PREVIEW_INTERVAL));
    let ed = &mut st.lcd.editor;
    let since = ctx.time - ed.last_render_req;
    if since < EDITOR_PREVIEW_INTERVAL {
        return;
    }
    ed.last_render_req = ctx.time;
    let known = known_signatures(&ed.sprite_tex);
    crate::domain::actions::lcd::render_lcd_editor(ctx.cmd, id, ed.def.clone(), known);
}

/// Build the `known` map to send with a render request: every cached
/// texture's id → signature. Pure so it's unit-tested independent of egui.
pub(super) fn known_signatures(
    sprite_tex: &std::collections::HashMap<String, (u64, egui::TextureHandle)>,
) -> std::collections::HashMap<String, u64> {
    sprite_tex
        .iter()
        .map(|(id, (sig, _))| (id.clone(), *sig))
        .collect()
}

/// Which cached texture ids survive a delta render: an id in `signatures`
/// stays (its texture is either freshly uploaded this frame or still valid);
/// anything else was dropped from the def and is evicted. When `signatures`
/// is empty (a legacy full-replace reply), fall back to the ids present in
/// `sprites` — the reply IS the complete set. Pure so it's unit-tested.
pub(super) fn retained_sprite_ids<'a>(
    sprites: &'a [crate::runtime::ipc::DecodedSprite],
    signatures: &'a [(String, u64)],
) -> std::collections::HashSet<&'a str> {
    if signatures.is_empty() {
        sprites.iter().map(|s| s.id.as_str()).collect()
    } else {
        signatures.iter().map(|(id, _)| id.as_str()).collect()
    }
}

/// Upload or refresh a texture for each changed widget sprite, cached by
/// `(id, signature)` so unchanged widgets aren't re-uploaded, then merge:
/// drop cached textures for widgets no longer present, keep the rest
/// (a delta reply doesn't re-send them, so a plain retain-by-`sprites`
/// would evict everything not in this tick's changed set).
pub(super) fn ensure_sprite_textures(
    ui: &egui::Ui,
    st: &mut DeviceUi,
    render: &crate::runtime::ipc::DecodedEditorRender,
) {
    for s in &render.sprites {
        let fresh = match st.lcd.editor.sprite_tex.get(&s.id) {
            Some((sig, _)) => *sig != s.signature,
            None => true,
        };
        if fresh && s.w > 0 && s.h > 0 && s.rgba.len() == s.w * s.h * 4 {
            let img = egui::ColorImage::from_rgba_unmultiplied([s.w, s.h], &s.rgba);
            let tex = ui.ctx().load_texture(
                format!("lcd_sprite_{}", s.id),
                img,
                egui::TextureOptions::LINEAR,
            );
            st.lcd
                .editor
                .sprite_tex
                .insert(s.id.clone(), (s.signature, tex));
        }
    }
    let keep = retained_sprite_ids(&render.sprites, &render.signatures);
    st.lcd
        .editor
        .sprite_tex
        .retain(|k, _| keep.contains(k.as_str()));
}

/// Map a daemon sprite (device px `w`x`h`, centered on the widget point) onto the
/// editor stage: the unrotated destination rect centered at `pos`, scaled from
/// the device `canvas` to the `stage` rect. Pure geometry — unit-tested.
pub(super) fn sprite_dest_rect(
    pos: egui::Pos2,
    w: u32,
    h: u32,
    canvas: (u32, u32),
    stage: Rect,
) -> Rect {
    let sx = stage.width() / canvas.0.max(1) as f32;
    let sy = stage.height() / canvas.1.max(1) as f32;
    Rect::from_center_size(pos, Vec2::new(w as f32 * sx, h as f32 * sy))
}

/// The on-stage content box for a widget: the daemon sprite's rect when one has
/// rendered, else the nominal square `size` box.
pub(super) fn content_bounds(
    pos: egui::Pos2,
    sprite: Option<(u32, u32)>,
    canvas: (u32, u32),
    stage: Rect,
    size: f32,
) -> Rect {
    match sprite {
        Some((w, h)) => sprite_dest_rect(pos, w, h, canvas, stage),
        None => Rect::from_center_size(pos, Vec2::splat(size)),
    }
}

/// New `(scale, scale_y)` for a resize drag that places the widget's local
/// bottom-right content corner at the pointer. `local` is the pointer offset from
/// the widget center in its unrotated frame; `start_*` are captured at gesture
/// start. Box widgets stretch each axis independently; others stay uniform (the
/// two axis factors are averaged). Pure so the scale math is unit-tested.
pub(super) fn resize_scales(
    local: Vec2,
    start_size: Vec2,
    start_scale: f32,
    start_scale_y: f32,
    box_widget: bool,
) -> (f32, f32) {
    let factor = |half: f32, start: f32| {
        if start > 0.0 {
            (2.0 * half.max(1.0)) / start
        } else {
            1.0
        }
    };
    let fx = factor(local.x, start_size.x);
    let fy = factor(local.y, start_size.y);
    if box_widget {
        (
            (start_scale * fx).clamp(0.6, MAX_SCALE),
            (start_scale_y * fy).clamp(0.6, MAX_SCALE),
        )
    } else {
        let s = (start_scale * (fx + fy) / 2.0).clamp(0.6, MAX_SCALE);
        (s, s)
    }
}

/// The previewed content rect during a resize: the box captured at gesture start
/// scaled by how far each axis's scale has moved since. Pure so the ratio math is
/// unit-tested; a non-positive start scale is treated as identity.
pub(super) fn resize_preview_rect(
    pos: egui::Pos2,
    start_size: Vec2,
    start_scale: f32,
    start_scale_y: f32,
    scale: f32,
    scale_y: f32,
) -> Rect {
    let sx = if start_scale > 0.0 {
        scale / start_scale
    } else {
        1.0
    };
    let sy = if start_scale_y > 0.0 {
        scale_y / start_scale_y
    } else {
        1.0
    };
    Rect::from_center_size(pos, Vec2::new(start_size.x * sx, start_size.y * sy))
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fake_tex(egui_ctx: &egui::Context, name: &str) -> egui::TextureHandle {
        let img = egui::ColorImage::new([1, 1], vec![egui::Color32::WHITE]);
        egui_ctx.load_texture(name, img, egui::TextureOptions::LINEAR)
    }

    fn decoded_sprite(id: &str, signature: u64) -> crate::runtime::ipc::DecodedSprite {
        crate::runtime::ipc::DecodedSprite {
            id: id.to_string(),
            signature,
            w: 1,
            h: 1,
            rgba: vec![255, 255, 255, 255],
        }
    }

    #[test]
    fn known_signatures_reflects_cached_texture_ids_and_sigs() {
        let egui_ctx = egui::Context::default();
        let mut cache = std::collections::HashMap::new();
        cache.insert("a".to_string(), (1u64, fake_tex(&egui_ctx, "a")));
        cache.insert("b".to_string(), (2u64, fake_tex(&egui_ctx, "b")));
        let known = known_signatures(&cache);
        assert_eq!(known.get("a"), Some(&1));
        assert_eq!(known.get("b"), Some(&2));
        assert_eq!(known.len(), 2);
    }

    #[test]
    fn retained_sprite_ids_uses_signatures_when_present() {
        let sprites = vec![decoded_sprite("a", 5)];
        let signatures = vec![("a".to_string(), 5), ("b".to_string(), 9)];
        // "b" wasn't in `sprites` (unchanged this tick) but is still current —
        // its cached texture must be retained.
        let keep = retained_sprite_ids(&sprites, &signatures);
        assert!(keep.contains("a"));
        assert!(keep.contains("b"));
        assert_eq!(keep.len(), 2);
    }

    #[test]
    fn retained_sprite_ids_falls_back_to_sprites_when_signatures_empty() {
        let sprites = vec![decoded_sprite("a", 5), decoded_sprite("b", 9)];
        let keep = retained_sprite_ids(&sprites, &[]);
        assert!(keep.contains("a"));
        assert!(keep.contains("b"));
        assert_eq!(keep.len(), 2);
    }

    #[test]
    fn sprite_dest_rect_scales_device_pixels_onto_the_stage() {
        // 240px device canvas mapped onto a 480px stage → 2× scale; a 100×50
        // sprite centered at `pos` becomes a 200×100 rect centered on `pos`.
        let stage = Rect::from_min_size(egui::pos2(10.0, 20.0), Vec2::new(480.0, 480.0));
        let pos = egui::pos2(100.0, 200.0);
        let r = sprite_dest_rect(pos, 100, 50, (240, 240), stage);
        assert!((r.width() - 200.0).abs() < 1e-3);
        assert!((r.height() - 100.0).abs() < 1e-3);
        assert!((r.center() - pos).length() < 1e-3);
    }

    #[test]
    fn content_bounds_uses_sprite_when_present_else_nominal_box() {
        let stage = Rect::from_min_size(egui::pos2(0.0, 0.0), Vec2::new(240.0, 240.0));
        let pos = egui::pos2(50.0, 50.0);
        // Same-size canvas ⇒ 1× scale, so the sprite rect equals its pixel dims.
        let with_sprite = content_bounds(pos, Some((80, 40)), (240, 240), stage, 30.0);
        assert!((with_sprite.width() - 80.0).abs() < 1e-3);
        assert!((with_sprite.height() - 40.0).abs() < 1e-3);
        // No sprite yet ⇒ fall back to the nominal square `size` box.
        let fallback = content_bounds(pos, None, (240, 240), stage, 30.0);
        assert!((fallback.width() - 30.0).abs() < 1e-3);
        assert!((fallback.height() - 30.0).abs() < 1e-3);
    }

    #[test]
    fn resize_preview_scales_start_box_by_scale_ratio() {
        let pos = egui::pos2(50.0, 60.0);
        let start = Vec2::new(80.0, 40.0);
        // Doubling the horizontal scale doubles width; height tracks scale_y.
        let r = resize_preview_rect(pos, start, 1.0, 1.0, 2.0, 1.5);
        assert!((r.width() - 160.0).abs() < 1e-3);
        assert!((r.height() - 60.0).abs() < 1e-3);
        assert!((r.center() - pos).length() < 1e-3);
        // A non-positive start scale is identity — no divide-by-zero blowup.
        let r0 = resize_preview_rect(pos, start, 0.0, 0.0, 2.0, 2.0);
        assert!((r0.width() - 80.0).abs() < 1e-3);
        assert!((r0.height() - 40.0).abs() < 1e-3);
    }

    #[test]
    fn resize_scales_track_the_pointer_corner_and_stay_bounded() {
        let start = Vec2::new(80.0, 40.0);
        // Pointer at the exact start corner (half-extents 40×20) reproduces the
        // start scale — the box is unchanged.
        let (sx, sy) = resize_scales(Vec2::new(40.0, 20.0), start, 1.0, 1.0, true);
        assert!((sx - 1.0).abs() < 1e-3 && (sy - 1.0).abs() < 1e-3);
        // Dragging the corner twice as far doubles each axis's scale (box widget).
        let (sx, sy) = resize_scales(Vec2::new(80.0, 40.0), start, 1.0, 1.0, true);
        assert!((sx - 2.0).abs() < 1e-3 && (sy - 2.0).abs() < 1e-3);
        // Uniform widgets average the two axis factors and keep both equal.
        let (sx, sy) = resize_scales(Vec2::new(80.0, 20.0), start, 1.0, 1.0, false);
        assert_eq!(sx, sy);
        assert!((sx - 1.5).abs() < 1e-3); // (2.0 + 1.0) / 2
                                          // Extremes clamp into range and never invert.
        for local in [Vec2::new(-500.0, -500.0), Vec2::new(9999.0, 9999.0)] {
            let (sx, sy) = resize_scales(local, start, 1.0, 1.0, true);
            assert!((0.6..=MAX_SCALE).contains(&sx));
            assert!((0.6..=MAX_SCALE).contains(&sy));
        }
    }
}
