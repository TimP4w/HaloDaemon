// SPDX-License-Identifier: GPL-3.0-or-later
//! Display card: shape-clipped preview (image/GIF/engine frame), brightness,
//! rotation, and raw-streaming toggle.

use egui::{Color32, Rect, Sense, Stroke, Vec2};
use halod_shared::commands::DaemonCommand;
use halod_shared::types::{LcdStatus, LcdUploadProgress, ScreenRotation, ScreenShape};

use super::{drive_preview, preview_key, rot_label, DeviceUi, TabCtx};
use crate::ui::components as widgets;
use crate::ui::theme;

/// UV rect that center-crops `tex` to a square, so painting it into the square
/// or circular preview fills the area without distorting the source aspect
/// ratio. A square texture (engine/video frames are device-sized) maps to the
/// full `0..1` rect, leaving those paths unchanged.
pub(super) fn cover_uv(tex: &egui::TextureHandle) -> Rect {
    let [w, h] = tex.size();
    let (w, h) = (w as f32, h as f32);
    if w > h {
        let frac = h / w; // crop the sides
        Rect::from_min_max(
            egui::pos2(0.5 - frac / 2.0, 0.0),
            egui::pos2(0.5 + frac / 2.0, 1.0),
        )
    } else if h > w {
        let frac = w / h; // crop top and bottom
        Rect::from_min_max(
            egui::pos2(0.0, 0.5 - frac / 2.0),
            egui::pos2(1.0, 0.5 + frac / 2.0),
        )
    } else {
        Rect::from_min_max(egui::pos2(0.0, 0.0), egui::pos2(1.0, 1.0))
    }
}

/// Fit a `panel_w × panel_h` display into a square `avail`-max box, preserving
/// the device's real aspect ratio. Panels are usually square, but this keeps a
/// non-square one correct instead of forcing a square preview. Shared by the
/// Display card preview and the editor stage so both honour the reported size.
pub(super) fn panel_size(avail: f32, panel_w: u32, panel_h: u32) -> Vec2 {
    let (w, h) = (panel_w.max(1) as f32, panel_h.max(1) as f32);
    if w >= h {
        Vec2::new(avail, avail * h / w)
    } else {
        Vec2::new(avail * w / h, avail)
    }
}

/// Draw a texture clipped to a circle via a triangle-fan mesh (a plain
/// `Painter::image` would bleed the corners outside the circle boundary).
/// `uv` selects the source region (see [`cover_uv`]).
pub(super) fn paint_image_circle(
    p: &egui::Painter,
    tex: &egui::TextureHandle,
    center: egui::Pos2,
    radius: f32,
    uv: Rect,
) {
    use egui::epaint::{Mesh, Vertex};
    const SEGMENTS: u32 = 64;
    let mut mesh = Mesh::with_texture(tex.id());
    mesh.vertices.push(Vertex {
        pos: center,
        uv: uv.center(),
        color: Color32::WHITE,
    });
    for i in 0..=SEGMENTS {
        let angle = i as f32 * std::f32::consts::TAU / SEGMENTS as f32;
        let (sin, cos) = angle.sin_cos();
        mesh.vertices.push(Vertex {
            pos: center + egui::vec2(cos * radius, sin * radius),
            uv: egui::pos2(
                uv.min.x + (0.5 + cos * 0.5) * uv.width(),
                uv.min.y + (0.5 + sin * 0.5) * uv.height(),
            ),
            color: Color32::WHITE,
        });
    }
    for i in 0..SEGMENTS {
        mesh.indices.extend_from_slice(&[0, i + 1, i + 2]);
    }
    p.add(egui::Shape::mesh(mesh));
}

/// Paint an animated spinner arc centred at `center`, driving its own repaint.
fn paint_spinner(ui: &egui::Ui, center: egui::Pos2, time: f64) {
    use egui::epaint::PathShape;
    const SEG: usize = 16;
    let radius = 13.0;
    let start = (time * 4.0) as f32 % std::f32::consts::TAU;
    let pts: Vec<egui::Pos2> = (0..=SEG)
        .map(|i| {
            let a = start + i as f32 / SEG as f32 * std::f32::consts::PI * 1.5;
            center + radius * egui::vec2(a.cos(), a.sin())
        })
        .collect();
    ui.painter()
        .add(PathShape::line(pts, Stroke::new(2.5, theme::CYAN)));
    ui.ctx().request_repaint();
}

/// Whether the "setting image" preview spinner should clear: either the daemon
/// confirmed `pending` as the active image and its texture is ready, or a fresh
/// terminal (`Done`/`Failed`) upload signal arrived for this device.
fn preview_pending_cleared(
    pending: &str,
    active_image: Option<&str>,
    tex_ready: bool,
    terminal: Option<&LcdUploadProgress>,
    device_id: &str,
) -> bool {
    (active_image == Some(pending) && tex_ready)
        || crate::ui::screens::device::is_terminal_upload_for(terminal, device_id)
}

pub(super) fn display_card(
    ui: &mut egui::Ui,
    ctx: &TabCtx,
    st: &mut DeviceUi,
    id: &str,
    lcd: &LcdStatus,
) {
    widgets::card_titled(
        ui,
        &t!("lcd.display"),
        |_| {},
        |ui| {
            drive_preview(ui, ctx, st, lcd);

            // Clear the "setting image" spinner once the daemon confirms the
            // image *and* its texture is ready (a GIF's first frame may lag), or
            // when a terminal upload signal (`Done`/`Failed`) lands — so a failed
            // device write can't leave it spinning forever.
            if let Some(pending) = st.lcd.preview_pending.clone() {
                let tex_ready = st.lcd.preview_tex.is_some()
                    && st.lcd.preview_key == preview_key(&lcd.mode, Some(&pending));
                if preview_pending_cleared(
                    &pending,
                    lcd.active_image.as_deref(),
                    tex_ready,
                    ctx.lcd_upload_terminal.as_ref(),
                    id,
                ) {
                    st.lcd.preview_pending = None;
                }
            }

            // Preview area — sized to the device's real aspect ratio so a
            // non-square panel isn't forced into a square box.
            let size = panel_size(240.0, lcd.descriptor.width, lcd.descriptor.height);
            let (rect, _) = ui
                .vertical_centered(|ui| ui.allocate_exact_size(size, Sense::hover()))
                .inner;
            let center = rect.center();
            let radius = rect.width().min(rect.height()) / 2.0;
            let loading = st.lcd.preview_pending.is_some();
            let no_preview = st.lcd.preview_tex.is_none();
            {
                let p = ui.painter();
                match lcd.descriptor.shape {
                    ScreenShape::Circle => {
                        p.circle_filled(center, radius, theme::INNER_BG);
                        if let Some(tex) = &st.lcd.preview_tex {
                            paint_image_circle(p, tex, center, radius, cover_uv(tex));
                        }
                        p.circle_stroke(center, radius, Stroke::new(1.0, theme::BORDER));
                    }
                    ScreenShape::Square => {
                        p.rect_filled(rect, 12.0, theme::INNER_BG);
                        if let Some(tex) = &st.lcd.preview_tex {
                            p.image(tex.id(), rect, cover_uv(tex), Color32::WHITE);
                        }
                        p.rect_stroke(
                            rect,
                            12.0,
                            Stroke::new(1.0, theme::BORDER),
                            egui::StrokeKind::Middle,
                        );
                    }
                }
                if loading {
                    paint_spinner(ui, center, ctx.time);
                } else if no_preview {
                    p.text(
                        center,
                        egui::Align2::CENTER_CENTER,
                        format!("{}×{}", lcd.descriptor.width, lcd.descriptor.height),
                        theme::mono(11.0),
                        theme::TEXT_MUT,
                    );
                }
            }
            ui.add_space(16.0);

            // Brightness.
            let key = "lcd_bright";
            let mut b = st.guarded(key, lcd.brightness as f32, ctx.time);
            let readout = format!("{}%", b.round() as i32);
            if widgets::slider_row(ui, &t!("lcd.brightness"), &mut b, 0.0..=100.0, &readout) {
                st.set(key, b, ctx.time);
                st.queue(
                    key,
                    DaemonCommand::SetScreenBrightness {
                        id: id.to_string(),
                        brightness: b.round() as u8,
                    },
                    ctx.time,
                );
            }
            ui.add_space(16.0);

            // Rotation.
            widgets::caps_label(ui, &t!("lcd.rotation"));
            ui.add_space(6.0);
            ui.horizontal(|ui| {
                ui.spacing_mut().item_spacing.x = 7.0;
                let rots = if lcd.descriptor.supported_rotations.is_empty() {
                    vec![
                        ScreenRotation::R0,
                        ScreenRotation::R90,
                        ScreenRotation::R180,
                        ScreenRotation::R270,
                    ]
                } else {
                    lcd.descriptor.supported_rotations.clone()
                };
                for r in rots {
                    if widgets::pill(ui, rot_label(r), r == lcd.rotation) && r != lcd.rotation {
                        crate::domain::actions::lcd::set_screen_rotation(ctx.cmd, id, r);
                    }
                }
            });
            ui.add_space(16.0);

            raw_streaming_row(ui, ctx, id, lcd);
            ui.add_space(16.0);

            if widgets::button(
                ui,
                &t!("lcd.reset_to_default"),
                widgets::ButtonKind::Ghost,
                egui::vec2(150.0, 30.0),
            )
            .clicked()
            {
                crate::domain::actions::lcd::set_screen_default(ctx.cmd, id);
            }
        },
    );
}

/// Raw streaming: bypass the Q565 encode and push uncompressed frames.
/// Shared by the Display card and the editor's Screen style card.
pub(super) fn raw_streaming_row(ui: &mut egui::Ui, ctx: &TabCtx, id: &str, lcd: &LcdStatus) {
    let on = lcd.raw_streaming;
    let mut next = on;
    egui::Sides::new().show(
        ui,
        |ui| {
            ui.label(
                egui::RichText::new(t!("lcd.raw_streaming"))
                    .font(theme::body(12.0))
                    .color(theme::TEXT_DIM),
            )
            .on_hover_text(t!("lcd.raw_streaming_hint"));
        },
        |ui| next = widgets::toggle(ui, on),
    );
    if next != on {
        crate::domain::actions::lcd::set_screen_raw_streaming(ctx.cmd, id, next);
    }
}

#[cfg(test)]
mod tests {
    use super::super::gif::rgba_texture;
    use super::*;
    use halod_shared::types::LcdUploadStage;

    fn terminal(stage: LcdUploadStage, device_id: &str) -> LcdUploadProgress {
        LcdUploadProgress {
            device_id: device_id.into(),
            stage,
            percent: None,
        }
    }

    #[test]
    fn preview_spinner_clears_on_confirm_or_terminal() {
        // Confirmed active image + texture ready → clears.
        assert!(preview_pending_cleared(
            "a.png",
            Some("a.png"),
            true,
            None,
            "lcd"
        ));
        // Confirmed but texture not yet ready, no terminal → keep spinning.
        assert!(!preview_pending_cleared(
            "a.png",
            Some("a.png"),
            false,
            None,
            "lcd"
        ));
        // A device-write failure clears even though the image never became active.
        let failed = terminal(LcdUploadStage::Failed, "lcd");
        assert!(preview_pending_cleared(
            "a.png",
            None,
            false,
            Some(&failed),
            "lcd"
        ));
        // Staleness: no fresh terminal (delivered as `None`) → keep spinning.
        assert!(!preview_pending_cleared("a.png", None, false, None, "lcd"));
        // A terminal for another device is ignored.
        let other = terminal(LcdUploadStage::Failed, "other");
        assert!(!preview_pending_cleared(
            "a.png",
            None,
            false,
            Some(&other),
            "lcd"
        ));
    }

    #[test]
    fn cover_uv_center_crops_to_a_square_without_distortion() {
        let ctx = egui::Context::default();
        // 4×2 (wide): sides cropped to a centered half-width, full height.
        let wide = rgba_texture(&ctx, "w", &[0u8; 4 * 2 * 4], 4, 2);
        let uv = cover_uv(&wide);
        assert_eq!((uv.min.x, uv.max.x), (0.25, 0.75));
        assert_eq!((uv.min.y, uv.max.y), (0.0, 1.0));
        // 2×4 (tall): top/bottom cropped, full width.
        let tall = rgba_texture(&ctx, "t", &[0u8; 2 * 4 * 4], 2, 4);
        let uv = cover_uv(&tall);
        assert_eq!((uv.min.y, uv.max.y), (0.25, 0.75));
        assert_eq!((uv.min.x, uv.max.x), (0.0, 1.0));
        // Square textures (engine/video frames) map to the full UV — unchanged.
        let sq = rgba_texture(&ctx, "s", &[0u8; 2 * 2 * 4], 2, 2);
        let uv = cover_uv(&sq);
        assert_eq!(
            (uv.min.x, uv.min.y, uv.max.x, uv.max.y),
            (0.0, 0.0, 1.0, 1.0)
        );
    }

    #[test]
    fn panel_size_preserves_aspect_ratio_within_the_box() {
        // Square panel fills the box exactly.
        assert_eq!(panel_size(140.0, 240, 240), Vec2::new(140.0, 140.0));
        // Wide panel: full width, shorter height.
        assert_eq!(panel_size(140.0, 320, 240), Vec2::new(140.0, 105.0));
        // Tall panel: full height, narrower width.
        assert_eq!(panel_size(140.0, 240, 320), Vec2::new(105.0, 140.0));
        // Neither side ever exceeds the box, and a zero dimension can't panic.
        let s = panel_size(140.0, 0, 0);
        assert!(s.x <= 140.0 && s.y <= 140.0);
    }
}
