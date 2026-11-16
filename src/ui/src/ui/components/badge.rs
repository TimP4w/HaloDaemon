// SPDX-License-Identifier: GPL-3.0-or-later
//! Device/battery glyphs and the effect-picker cell.

use egui::{Align2, Color32, Pos2, Rect, Sense, Vec2};
use halod_shared::types::DeviceType;

use crate::ui::theme;

/// The shared device chip: the device type's short code centred in `rect`,
/// drawn over the caller's identity tile.
pub fn device_badge(p: &egui::Painter, rect: Rect, ty: DeviceType) {
    p.text(
        rect.center(),
        Align2::CENTER_CENTER,
        device_code(ty),
        theme::semibold(12.0),
        theme::NAV_TEXT,
    );
}

/// The 2–3 letter identity code for a device type.
pub fn device_code(ty: DeviceType) -> &'static str {
    match ty {
        DeviceType::Other => "DEV",
        DeviceType::Fan => "FAN",
        DeviceType::Hub => "HUB",
        DeviceType::Dongle => "RX",
        DeviceType::Keyboard => "KB",
        DeviceType::Mouse => "MS",
        DeviceType::Headset => "HS",
        DeviceType::Monitor => "MON",
        DeviceType::Gpu => "GPU",
        DeviceType::LedStrip => "LED",
        DeviceType::Motherboard => "MB",
        DeviceType::Ram => "RAM",
        DeviceType::Sensor => "SEN",
        DeviceType::AIO => "AIO",
        DeviceType::Speaker => "SPK",
        DeviceType::Computer => "PC",
    }
}

/// Preview swatch shown at the top of an [`effect_cell`].
pub enum CellPreview {
    /// A solid representative color.
    Solid(Color32),
    /// The logo RGB spectrum, for effects with no single color.
    Spectrum,
    /// Equal hard-edged color bands (e.g. per-LED paint).
    Segments(Vec<Color32>),
}

/// An effect-picker cell: an inset tile with a top preview strip and a bottom
/// label. `strip_h` sizes the preview. Returns whether it was clicked.
pub fn effect_cell(
    ui: &mut egui::Ui,
    label: &str,
    active: bool,
    preview: CellPreview,
    w: f32,
    height: f32,
    strip_h: f32,
) -> bool {
    effect_cell_noted(ui, label, None, active, preview, w, height, strip_h)
}

/// [`effect_cell`] with a muted `note` (e.g. the effect's source) right of the label.
#[allow(clippy::too_many_arguments)]
pub fn effect_cell_noted(
    ui: &mut egui::Ui,
    label: &str,
    note: Option<&str>,
    active: bool,
    preview: CellPreview,
    w: f32,
    height: f32,
    strip_h: f32,
) -> bool {
    let (rect, resp) = ui.allocate_exact_size(Vec2::new(w, height), Sense::click());
    let p = ui.painter();
    let border = if active {
        theme::CYAN
    } else if resp.hovered() {
        theme::BORDER_INNER
    } else {
        theme::BORDER
    };
    theme::paint_surface(p, rect, theme::RADIUS_SM, theme::INNER_BG, border);
    let strip = Rect::from_min_size(
        Pos2::new(rect.left() + 10.0, rect.top() + 10.0),
        Vec2::new(rect.width() - 20.0, strip_h),
    );
    match preview {
        CellPreview::Solid(c) => {
            p.rect_filled(strip, theme::RADIUS_XS, c);
        }
        CellPreview::Spectrum => theme::h_gradient(p, strip, &theme::LOGO_STOPS),
        CellPreview::Segments(colors) => {
            let w = strip.width() / colors.len().max(1) as f32;
            let clip = p.with_clip_rect(strip);
            for (i, c) in colors.iter().enumerate() {
                let band = Rect::from_min_size(
                    Pos2::new(strip.left() + i as f32 * w, strip.top()),
                    Vec2::new(w, strip.height()),
                );
                clip.rect_filled(band, 0.0, *c);
            }
        }
    }
    p.text(
        Pos2::new(rect.left() + 10.0, rect.bottom() - 12.0),
        Align2::LEFT_CENTER,
        label,
        theme::medium(13.0),
        theme::TEXT,
    );
    if let Some(note) = note {
        p.text(
            Pos2::new(rect.right() - 10.0, rect.bottom() - 12.0),
            Align2::RIGHT_CENTER,
            note,
            theme::body_sm(),
            theme::TEXT_MUT,
        );
    }
    if resp.hovered() {
        ui.ctx().set_cursor_icon(egui::CursorIcon::PointingHand);
    }
    resp.clicked()
}
