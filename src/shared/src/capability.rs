//! Top-level keys under which a capability persists into a profile's `device_states`.

/// The GUI translates each key via `capability_label`; no English lives here.
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
