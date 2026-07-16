//! Shared sizing/geometry constants for the "custom" LCD template's widgets.
//!
//! The daemon (`daemon/src/engines/lcd/custom.rs`) renders the real frame; the
//! GUI editor (`ui/src/device/lcd_editor.rs`) paints a schematic stage
//! placeholder that must stay pixel-proportional to it. Both crates read
//! these constants so the two renderers cannot drift apart.

/// Nominal on-screen fraction of `min(w, h)` a widget occupies at `scale == 1.0`.
pub const WIDGET_BASE: f32 = 0.34;
/// Absolute lower bound accepted by the renderer; individual widget
/// descriptors may expose a higher editor minimum.
pub const MIN_SCALE: f32 = 0.1;
/// Upper bound of a widget's `scale` — at 3.0 a ring gauge can fill the whole
/// panel (`WIDGET_BASE * 3 ≈ 1.0`).
pub const MAX_SCALE: f32 = 3.0;

/// A widget's base pixel `size` for a given `scale` and panel `min(w, h)`,
/// capped at the panel so content never exceeds it. The single source of truth
/// for both the daemon renderer and the GUI editor stage.
pub fn widget_size(scale: f32, panel_min: f32) -> f32 {
    (WIDGET_BASE * scale.clamp(MIN_SCALE, MAX_SCALE) * panel_min)
        .max(0.0)
        .min(panel_min)
}

/// Number of `AudioSpectrum` bars for a `bands` param value — rounded and
/// clamped to `1..=SPECTRUM_MAX_BANDS`, shared by both renderers.
pub fn spectrum_bands(bands_param: f64) -> usize {
    (bands_param.round() as usize).clamp(1, SPECTRUM_MAX_BANDS)
}

/// Maximum `AudioSpectrum` bars; must equal the audio DSP's band count.
pub const SPECTRUM_MAX_BANDS: usize = 64;

/// Plain/pill text size as a multiple of the widget's base `size`.
pub const TEXT_FONT: f32 = 0.22;

/// Fraction of a `Shape` widget's box the primitive occupies, shared so the
/// daemon renderer and the GUI editor draw shapes at the same size.
pub const SHAPE_SIZE: f32 = 0.82;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_size_caps_at_panel_and_scales() {
        assert!((widget_size(1.0, 240.0) - WIDGET_BASE * 240.0).abs() < 1e-3);
        // Large scale is capped at the panel min.
        assert_eq!(widget_size(MAX_SCALE, 240.0), 240.0);
        // Scale is clamped to [MIN_SCALE, MAX_SCALE].
        assert_eq!(widget_size(0.0, 100.0), widget_size(MIN_SCALE, 100.0));
        assert_eq!(widget_size(99.0, 100.0), widget_size(MAX_SCALE, 100.0));
    }

    #[test]
    fn shared_size_constants_have_expected_values() {
        // Pinned on both sides (daemon + editor import these) so the two
        // renderers cannot silently drift; see the mirrored tests in
        // `daemon/src/engines/lcd/custom.rs` and `ui/src/device/lcd_editor.rs`.
        assert_eq!(SHAPE_SIZE, 0.82);
        assert_eq!(TEXT_FONT, 0.22);
    }

    #[test]
    fn spectrum_bands_rounds_and_clamps() {
        assert_eq!(spectrum_bands(32.0), 32);
        assert_eq!(spectrum_bands(32.4), 32);
        assert_eq!(spectrum_bands(0.0), 1);
        assert_eq!(spectrum_bands(1000.0), SPECTRUM_MAX_BANDS);
    }
}
