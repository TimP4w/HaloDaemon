//! Widget sizing shared by the daemon's LCD renderer and the GUI editor stage.

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
}
