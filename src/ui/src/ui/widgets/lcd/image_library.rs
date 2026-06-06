use gtk4::{self as gtk, prelude::GdkCairoContextExt};

// ── Preview source ────────────────────────────────────────────────────────────

/// Holds the current renderable state for the preview drawing area.
pub(super) enum PreviewSource {
    Static(gtk::gdk_pixbuf::Pixbuf),
    Animated(gtk::gdk_pixbuf::PixbufAnimationIter),
}

// ── Pending upload/apply tracking ─────────────────────────────────────────────

pub(super) enum PendingPayload {
    /// New upload: apply preview from in-memory bytes on ACK.
    Bytes(Vec<u8>),
    /// Library apply: load preview from file on ACK (None = let state broadcast handle it).
    File(Option<std::path::PathBuf>),
}

pub(super) struct PendingItem {
    pub(super) payload: PendingPayload,
    pub(super) timeout_id: gtk::glib::SourceId,
}

// ── Drawing helpers ───────────────────────────────────────────────────────────

pub(super) fn draw_preview(
    cr: &gtk::cairo::Context,
    w: i32,
    h: i32,
    shape: &halod_protocol::types::ScreenShape,
    source: &Option<PreviewSource>,
    rotation_degrees: u32,
) {
    let (fw, fh) = (w as f64, h as f64);
    let cx = fw / 2.0;
    let cy = fh / 2.0;
    let r = fw.min(fh) / 2.0 - 4.0;

    // Clip to circle or rectangle.
    cr.save().ok();
    match shape {
        halod_protocol::types::ScreenShape::Circle => {
            cr.arc(cx, cy, r, 0.0, 2.0 * std::f64::consts::PI);
            cr.clip();
        }
        halod_protocol::types::ScreenShape::Square => {
            cr.rectangle(cx - r, cy - r, r * 2.0, r * 2.0);
            cr.clip();
        }
    }

    let pixbuf = match source {
        Some(PreviewSource::Static(pb)) => Some(pb.clone()),
        Some(PreviewSource::Animated(iter)) => Some(iter.pixbuf()),
        None => None,
    };

    if let Some(pixbuf) = pixbuf {
        let img_w = pixbuf.width() as f64;
        let img_h = pixbuf.height() as f64;
        let side = r * 2.0;
        let scale = side / img_w.max(img_h);

        cr.translate(cx, cy);
        cr.rotate(rotation_degrees as f64 * std::f64::consts::PI / 180.0);
        cr.scale(scale, scale);
        cr.translate(-img_w / 2.0, -img_h / 2.0);

        cr.set_source_pixbuf(&pixbuf, 0.0, 0.0);
        cr.paint().ok();
        cr.restore().ok();
        return;
    }

    // Placeholder: dark grey background.
    cr.set_source_rgb(0.15, 0.15, 0.15);
    cr.paint().ok();
    cr.restore().ok();

    cr.set_source_rgba(1.0, 1.0, 1.0, 0.2);
    cr.set_font_size(14.0);
    let _ = cr.move_to(cx - 40.0, cy + 5.0);
    let _ = cr.show_text("No image");

    // Draw shape border.
    cr.new_path();
    cr.set_source_rgba(1.0, 1.0, 1.0, 0.15);
    cr.set_line_width(1.5);
    match shape {
        halod_protocol::types::ScreenShape::Circle => {
            cr.arc(cx, cy, r, 0.0, 2.0 * std::f64::consts::PI);
        }
        halod_protocol::types::ScreenShape::Square => {
            cr.rectangle(cx - r, cy - r, r * 2.0, r * 2.0);
        }
    }
    cr.stroke().ok();
}

/// Draw a pixbuf center-cropped into a square w×h area with rounded corners.
pub(super) fn draw_thumb(cr: &gtk::cairo::Context, w: i32, h: i32, pixbuf: &gtk::gdk_pixbuf::Pixbuf) {
    let (fw, fh) = (w as f64, h as f64);
    let r = 8.0_f64; // border-radius matching CSS

    // Rounded-rectangle clip.
    cr.save().ok();
    cr.new_sub_path();
    cr.arc(fw - r, r,      r, -std::f64::consts::FRAC_PI_2, 0.0);
    cr.arc(fw - r, fh - r, r, 0.0,  std::f64::consts::FRAC_PI_2);
    cr.arc(r,      fh - r, r, std::f64::consts::FRAC_PI_2, std::f64::consts::PI);
    cr.arc(r,      r,      r, std::f64::consts::PI, 3.0 * std::f64::consts::FRAC_PI_2);
    cr.close_path();
    cr.clip();

    let img_w = pixbuf.width() as f64;
    let img_h = pixbuf.height() as f64;
    let scale = (fw / img_w).max(fh / img_h);
    let x = (fw - img_w * scale) / 2.0;
    let y = (fh - img_h * scale) / 2.0;

    cr.scale(scale, scale);
    cr.set_source_pixbuf(pixbuf, x / scale, y / scale);
    cr.paint().ok();
    cr.restore().ok();
}
