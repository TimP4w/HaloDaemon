//! Single source of truth for capability state keys — the top-level keys under
//! which a capability persists into a profile's `device_states`. The daemon
//! returns these from `Capability::state_key()` and the UI uses them for
//! override badges; both reference the constants here so they can't drift.

/// Every capability's state key. Adding a capability means adding its constant
/// here; the GUI translates each key to a human label (`capability_label`), so
/// no English lives in this crate.
pub const CAPABILITIES: &[&str] = &[
    RGB, DPI, FAN_CURVE, LCD, KEY_REMAP, EQUALIZER, CHOICE, RANGE, BOOLEAN,
];

pub const RGB: &str = "rgb";
pub const DPI: &str = "dpi";
pub const FAN_CURVE: &str = "fan_curve";
pub const LCD: &str = "lcd";
pub const KEY_REMAP: &str = "keyremap";
pub const EQUALIZER: &str = "equalizer";
pub const CHOICE: &str = "choice";
pub const RANGE: &str = "range";
pub const BOOLEAN: &str = "boolean";

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn every_capability_constant_is_listed() {
        for key in [
            RGB, DPI, FAN_CURVE, LCD, KEY_REMAP, EQUALIZER, CHOICE, RANGE, BOOLEAN,
        ] {
            assert!(
                CAPABILITIES.contains(&key),
                "capability constant {key:?} missing from CAPABILITIES table"
            );
        }
    }
}
