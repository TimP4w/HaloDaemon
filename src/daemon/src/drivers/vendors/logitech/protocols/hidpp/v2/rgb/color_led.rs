// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! COLOR_LED_EFFECTS (`0x8070`) codecs: the older LED effect type system used
//! by devices that predate `0x8071` RGB_EFFECTS (e.g. G502 Hero).
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp20.py (LEDEffects, LEDEffectSetting,
//! LEDZoneInfo, LEDEffectsInfo)

use halod_shared::types::RgbColor;

// ── Zone location → name ────────────────────────────────────────────────────

/// Map a COLOR_LED_EFFECTS zone location (u16) to a human-readable name.
/// Reference: Solaar LEDZoneLocations.
pub fn color_led_location_name(location: u16) -> &'static str {
    match location {
        0x0001 => "Primary",
        0x0002 => "Logo",
        0x0003 => "Left Side",
        0x0004 => "Right Side",
        0x0005 => "Combined",
        0x0006 => "Primary 1",
        0x0007 => "Primary 2",
        0x0008 => "Primary 3",
        0x0009 => "Primary 4",
        0x000A => "Primary 5",
        0x000B => "Primary 6",
        _ => "Unknown",
    }
}

// ── Effect parameter slots ────────────────────────────────────────────────────

/// Which parameters an effect ID uses, and at what byte offset in the 10-byte
/// payload (after the lead effect-ID byte).
#[derive(Clone, Debug)]
pub struct LedEffectLayout {
    /// Per-param `(name, offset, size)` triples.
    pub params: &'static [(&'static str, usize, usize)],
}

/// Colour at `off` (3 bytes), or a byte param with `min..=max` and optional
/// per-effect period range.
#[derive(Clone)]
pub struct LedEffectDef {
    pub name: &'static str,
    pub layout: LedEffectLayout,
}

// ── Effect table ──────────────────────────────────────────────────────────────

pub static LED_EFFECTS: &[(&str, u8, LedEffectLayout)] = &[
    ("Disabled", 0x00, LedEffectLayout { params: &[] }),
    (
        "Static",
        0x01,
        LedEffectLayout {
            params: &[("color", 0, 3), ("ramp", 3, 1)],
        },
    ),
    (
        "Pulse",
        0x02,
        LedEffectLayout {
            params: &[("color", 0, 3), ("speed", 3, 1)],
        },
    ),
    (
        "Cycle",
        0x03,
        LedEffectLayout {
            params: &[("period", 5, 2), ("intensity", 7, 1)],
        },
    ),
    (
        "Wave",
        0x04,
        LedEffectLayout {
            params: &[("period", 6, 2), ("direction", 9, 1)],
        },
    ),
    ("Boot", 0x08, LedEffectLayout { params: &[] }),
    ("Demo", 0x09, LedEffectLayout { params: &[] }),
    (
        "Breathe",
        0x0A,
        LedEffectLayout {
            params: &[
                ("color", 0, 3),
                ("period", 3, 2),
                ("form", 5, 1),
                ("intensity", 6, 1),
            ],
        },
    ),
    (
        "Ripple",
        0x0B,
        LedEffectLayout {
            params: &[("color", 0, 3), ("period", 4, 2)],
        },
    ),
    (
        "Decomposition",
        0x0E,
        LedEffectLayout {
            params: &[("period", 6, 2), ("intensity", 8, 1)],
        },
    ),
    (
        "Signature1",
        0x0F,
        LedEffectLayout {
            params: &[("period", 5, 2), ("intensity", 7, 1)],
        },
    ),
    (
        "Signature2",
        0x10,
        LedEffectLayout {
            params: &[("period", 5, 2), ("intensity", 7, 1)],
        },
    ),
];

/// Find an effect definition by numeric ID.
pub fn find_led_effect(id: u8) -> Option<&'static (&'static str, u8, LedEffectLayout)> {
    LED_EFFECTS.iter().find(|(_, eid, _)| *eid == id)
}

/// Find an effect definition by string name.
pub fn find_led_effect_by_name(name: &str) -> Option<&'static (&'static str, u8, LedEffectLayout)> {
    LED_EFFECTS.iter().find(|(n, _, _)| *n == name)
}

// ── Reply parsers ─────────────────────────────────────────────────────────────

/// Parse the zone count from a COLOR_LED_EFFECTS GetInfo reply (fn 0x00).
/// Reply: `[count, ???, capabilities_hi, capabilities_lo]` — at least 5 bytes.
pub fn parse_color_led_zone_count(reply: &[u8]) -> Option<u8> {
    if reply.len() < 5 {
        return None;
    }
    Some(reply[0])
}

/// Parse zone index, location, and effect count from GetZoneInfo (fn 0x10).
/// Reply layout: `[index, location_hi, location_lo, count, caps_hi, caps_lo]`
pub fn parse_color_led_zone_info(reply: &[u8]) -> Option<(u8, u16, u8)> {
    if reply.len() < 4 {
        return None;
    }
    let zone_index = reply[0];
    let location = u16::from_be_bytes([reply[1], reply[2]]);
    let count = reply[3];
    Some((zone_index, location, count))
}

/// Parse a single effect-table entry from GetZoneEffectInfo (fn 0x20).
/// Reply: `[zindex, eindex, id_hi, id_lo, caps_hi, caps_lo, period_hi, period_lo]`
pub fn parse_color_led_effect_entry(reply: &[u8]) -> Option<u16> {
    if reply.len() < 8 {
        return None;
    }
    Some(u16::from_be_bytes([reply[2], reply[3]]))
}

// ── SetEffect builder ─────────────────────────────────────────────────────────

/// Build the 12-byte SetEffect payload matching Solaar's `to_command`:
/// `[zone_idx, effect_slot, 10 param bytes]`. The 10 param bytes are the
/// LEDEffectSetting body (ID byte stripped).
pub fn encode_color_led_set_effect_static(zone_idx: u8, slot: u8, color: RgbColor) -> [u8; 12] {
    let mut buf = [0u8; 12];
    // to_command format: [zone_index, effect_slot, 10 param bytes]
    buf[0] = zone_idx;
    buf[1] = slot;
    // LEDEffectSetting body (without lead ID byte): Static = ID 0x01,
    // color at offset 0 (3 bytes), ramp at offset 3 (1 byte).
    // Static params: color@0, ramp@3 → ramp=0 (Default).
    buf[2] = color.r;
    buf[3] = color.g;
    buf[4] = color.b;
    buf
}

/// The effect slot for the given zone index and effect-table slot.
pub fn color_led_effect_command(_zone_idx: u8, effect_slot: u8, payload: &[u8]) -> Vec<u8> {
    let mut cmd = Vec::with_capacity(1 + payload.len());
    cmd.push(effect_slot);
    cmd.extend_from_slice(payload);
    cmd
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_zone_count_valid() {
        let reply = [3u8, 0, 0, 0, 0];
        assert_eq!(parse_color_led_zone_count(&reply), Some(3));
    }

    #[test]
    fn parse_zone_count_short() {
        assert_eq!(parse_color_led_zone_count(&[1, 2, 3]), None);
    }

    #[test]
    fn parse_zone_info_valid() {
        let reply = [0u8, 0x00, 0x02, 5u8]; // zone_index=0, location=2 (Logo), count=5
        assert_eq!(parse_color_led_zone_info(&reply), Some((0, 2, 5)));
    }

    #[test]
    fn parse_effect_entry_valid() {
        let reply = [0u8, 0, 0x00, 0x01, 0, 0, 0, 0]; // effect_id=0x0001
        assert_eq!(parse_color_led_effect_entry(&reply), Some(0x0001));
    }

    #[test]
    fn encode_static_effect() {
        let buf = encode_color_led_set_effect_static(
            1,
            2,
            RgbColor {
                r: 0xFF,
                g: 0x80,
                b: 0x40,
            },
        );
        assert_eq!(buf[0], 1); // zone_idx
        assert_eq!(buf[1], 2); // effect_slot
        assert_eq!(buf[2], 0xFF); // R
        assert_eq!(buf[3], 0x80); // G
        assert_eq!(buf[4], 0x40); // B
                                  // bytes 5-11 are zero padding
    }

    #[test]
    fn find_known_effects() {
        assert!(find_led_effect(0x01).is_some());
        assert!(find_led_effect(0x03).is_some());
        assert!(find_led_effect(0x0B).is_some());
    }

    #[test]
    fn find_unknown_effect() {
        assert!(find_led_effect(0xFF).is_none());
    }
}
