//! Shared sizing/geometry constants for the "custom" LCD template's widgets.
//!
//! The daemon (`daemon/src/engines/lcd/custom.rs`) renders the real frame; the
//! GUI editor (`ui/src/device/lcd_editor.rs`) paints a schematic stage
//! placeholder that must stay pixel-proportional to it. Both crates read
//! these constants so the two renderers cannot drift apart.

/// Nominal on-screen fraction of `min(w, h)` a widget occupies at `scale == 1.0`.
pub const WIDGET_BASE: f32 = 0.34;
/// Upper bound of a widget's `scale` — at 3.0 a ring gauge can fill the whole
/// panel (`WIDGET_BASE * 3 ≈ 1.0`).
pub const MAX_SCALE: f32 = 3.0;

/// A widget's base pixel `size` for a given `scale` and panel `min(w, h)`,
/// capped at the panel so content never exceeds it. The single source of truth
/// for both the daemon renderer and the GUI editor stage.
pub fn widget_size(scale: f32, panel_min: f32) -> f32 {
    (WIDGET_BASE * scale.clamp(0.6, MAX_SCALE) * panel_min)
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

/// Clock text size as a multiple of the widget's base `size`.
pub const CLOCK_FONT: f32 = 0.36;
/// Date text size as a multiple of the widget's base `size`.
pub const DATE_FONT: f32 = 0.20;
/// Sensor value text size as a multiple of the widget's base `size`.
pub const SENSOR_VALUE_FONT: f32 = 0.26;
/// Sensor label text size as a multiple of the widget's base `size`.
pub const SENSOR_LABEL_FONT: f32 = 0.12;
/// Sensor label vertical offset below center, as a multiple of `size`.
pub const SENSOR_LABEL_OFFSET: f32 = 0.34;
/// Sensor value vertical offset *above* center for the `bar` variant (the bar
/// itself sits at the widget centre, so the value reads above it), as a
/// multiple of `size`.
pub const SENSOR_BAR_VALUE_OFFSET: f32 = 0.20;
/// Plain/pill text size as a multiple of the widget's base `size`.
pub const TEXT_FONT: f32 = 0.22;
/// Debug frame-counter text size as a multiple of the widget's base `size`.
pub const DEBUG_FONT: f32 = 0.20;

/// Fraction of a `Shape` widget's box the primitive occupies, shared so the
/// daemon renderer and the GUI editor draw shapes at the same size.
pub const SHAPE_SIZE: f32 = 0.82;

/// `AudioSpectrum` bar-strip width as a multiple of the widget's base `size`.
pub const SPECTRUM_WIDTH: f32 = 1.6;
/// `AudioSpectrum` bar-strip height as a multiple of the widget's base `size`.
pub const SPECTRUM_HEIGHT: f32 = 0.8;

/// `NowPlaying` title text size as a multiple of the widget's base `size`.
pub const NOW_PLAYING_TITLE: f32 = 0.22;
/// `NowPlaying` artist text size as a multiple of the widget's base `size`.
pub const NOW_PLAYING_ARTIST: f32 = 0.14;
/// Gap between the `NowPlaying` art square and the text block, as a multiple
/// of `size`.
pub const NOW_PLAYING_ART_GAP: f32 = 0.25;
/// `NowPlaying` text-area width as a multiple of `size` (the title
/// scrolls/clips within it when it overflows).
pub const NOW_PLAYING_TEXT_WIDTH: f32 = 1.0;

/// Ring-gauge start angle, degrees clockwise from 12 o'clock.
pub const RING_START_DEG: f32 = 225.0;
/// Ring-gauge arc sweep, degrees (leaves a 90° gap centered on the bottom).
pub const RING_SWEEP_DEG: f32 = 270.0;
/// Ring-gauge track thickness as a multiple of the outer radius.
pub const RING_THICKNESS: f32 = 0.14;
/// Ring-gauge value dot radius as a multiple of the track thickness.
pub const RING_DOT: f32 = 0.9;

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn widget_size_caps_at_panel_and_scales() {
        assert!((widget_size(1.0, 240.0) - WIDGET_BASE * 240.0).abs() < 1e-3);
        // Large scale is capped at the panel min.
        assert_eq!(widget_size(MAX_SCALE, 240.0), 240.0);
        // Scale is clamped to [0.6, MAX_SCALE].
        assert_eq!(widget_size(0.0, 100.0), widget_size(0.6, 100.0));
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
