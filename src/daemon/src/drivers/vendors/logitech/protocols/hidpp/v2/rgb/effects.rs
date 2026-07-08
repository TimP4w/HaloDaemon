// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! RGB_EFFECTS (`0x8071`) codecs: the native firmware-effect type system and
//! its `NATIVE_EFFECTS` table, the `SetEffect` static block builder, and the
//! feature-reply parsers (zone count, effect-table entry).
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use std::collections::HashMap;

use halod_shared::types::{EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor};

// ── Native effect type system ─────────────────────────────────────────────────

/// Where (and how) an effect parameter value lands in the SetEffect byte block.
#[derive(Clone, Copy)]
pub enum EffectByteSlot {
    /// RGB colour at bytes `[off, off+1, off+2]`.
    Color { off: usize },
    /// A `min..=max` (UI units) value stored in one byte. `scale` maps a UI
    /// unit to a byte value (1.0 = verbatim; 2.55 = 0-100 UI → 0-255 byte).
    Byte {
        off: usize,
        min: f64,
        max: f64,
        scale: f64,
    },
}

/// One editable parameter of a native effect.
pub struct NativeEffectParam {
    pub id: &'static str,
    pub label: &'static str,
    pub slot: EffectByteSlot,
}

/// A native RGB effect: wire id, display name, the base SetEffect (func 0x10)
/// byte block captured from G HUB, and its editable parameters. `base[0]` is
/// the zone (0xFF = all zones), `base[1]` the effect-table slot.
pub struct NativeEffectDef {
    pub id: &'static str,
    pub name: &'static str,
    pub base: [u8; 15],
    pub params: &'static [NativeEffectParam],
}

pub static NATIVE_EFFECTS: &[NativeEffectDef] = &[
    // Color Wave — fixed preset; param byte layout not yet decoded.
    NativeEffectDef {
        id: "color_wave",
        name: "Color Wave",
        base: [
            0xff, 0x00, 0, 0, 0, 0, 0, 0, 0x88, 0x01, 0x64, 0x13, 0x01, 0, 0,
        ],
        params: &[],
    },
    // Ripple — offsets decoded from the G HUB capture `ff 03 5e5e5e ff 00 00 00 14 00 00 01 ...`.
    NativeEffectDef {
        id: "ripple",
        name: "Ripple",
        base: [
            0xff, 0x03, 0x5e, 0x5e, 0x5e, 0xff, 0, 0, 0, 0x14, 0, 0, 0x01, 0, 0,
        ],
        params: &[
            NativeEffectParam {
                id: "background",
                label: "Background Color",
                slot: EffectByteSlot::Color { off: 2 },
            },
            NativeEffectParam {
                id: "rate",
                label: "Effect Rate (ms)",
                slot: EffectByteSlot::Byte {
                    off: 9,
                    min: 2.0,
                    max: 200.0,
                    scale: 1.0,
                },
            },
            NativeEffectParam {
                id: "saturation",
                label: "Saturation",
                slot: EffectByteSlot::Byte {
                    off: 5,
                    min: 0.0,
                    max: 100.0,
                    scale: 2.55,
                },
            },
        ],
    },
];

pub fn find_native_effect(id: &str) -> Option<&'static NativeEffectDef> {
    NATIVE_EFFECTS.iter().find(|e| e.id == id)
}

impl NativeEffectDef {
    /// UI parameter descriptors, with defaults decoded from `base`.
    pub fn param_descriptors(&self) -> Vec<EffectParamDescriptor> {
        self.params
            .iter()
            .map(|p| {
                let (kind, default) = match p.slot {
                    EffectByteSlot::Color { off } => (
                        ParamKind::Color,
                        EffectParamValue::Color(RgbColor {
                            r: self.base.get(off).copied().unwrap_or(0),
                            g: self.base.get(off + 1).copied().unwrap_or(0),
                            b: self.base.get(off + 2).copied().unwrap_or(0),
                        }),
                    ),
                    EffectByteSlot::Byte {
                        off,
                        min,
                        max,
                        scale,
                    } => (
                        ParamKind::Range {
                            min,
                            max,
                            step: 1.0,
                        },
                        EffectParamValue::Float(
                            (self.base.get(off).copied().unwrap_or(0) as f64 / scale).round(),
                        ),
                    ),
                };
                EffectParamDescriptor {
                    id: p.id.to_string(),
                    label: p.label.to_string(),
                    kind,
                    default,
                }
            })
            .collect()
    }

    /// Render the SetEffect byte block, overlaying `values` onto `base`.
    pub fn encode(&self, values: &HashMap<String, EffectParamValue>) -> [u8; 15] {
        let mut block = self.base;
        for p in self.params {
            let Some(v) = values.get(p.id) else { continue };
            match (p.slot, v) {
                (EffectByteSlot::Color { off }, EffectParamValue::Color(c)) => {
                    if let Some(slot) = block.get_mut(off..off + 3) {
                        slot.copy_from_slice(&[c.r, c.g, c.b]);
                    }
                }
                (
                    EffectByteSlot::Byte {
                        off,
                        min,
                        max,
                        scale,
                    },
                    EffectParamValue::Float(f),
                ) => {
                    block[off] = (f.clamp(min, max) * scale).round() as u8;
                }
                _ => {}
            }
        }
        block
    }
}

// ── SetEffect static block ────────────────────────────────────────────────────

/// Build the 15-byte RGB_EFFECTS `SetEffect` (func 0x10) parameter block for a
/// plain static colour. The trailing constants are the static-effect preset
/// captured from G HUB.
pub fn encode_set_effect_static(zone_idx: u8, slot: u8, color: RgbColor) -> [u8; 15] {
    [
        zone_idx, slot, color.r, color.g, color.b, 0x64, 0x0B, 0xB8, 0x64, 0, 0, 0, 0x01, 0, 0,
    ]
}

// ── RGB feature-reply parsers ──────────────────────────────────────────────────

/// Parse the zone count from an RGB_EFFECTS GetInfo(0xFF, 0xFF, 0x00) reply
/// (byte 2). `None` when the reply is too short.
pub fn parse_rgb_zone_count(reply: &[u8]) -> Option<u8> {
    (reply.len() >= 3).then(|| reply[2])
}

/// Parse the effect_id (big-endian u16 at bytes 2..4) from an RGB_EFFECTS
/// GetInfo(zone, slot, 0x00) effect-table entry reply.
pub fn parse_rgb_effect_table_entry(reply: &[u8]) -> Option<u16> {
    (reply.len() >= 4).then(|| u16::from_be_bytes([reply[2], reply[3]]))
}

// ── PER_KEY_LIGHTING LED-bitmap parser ─────────────────────────────────────────

/// Scan a PER_KEY_LIGHTING LED bitmap (3 pages × 13 bytes = 39 bytes) for the
/// low-range firmware LED IDs (codes 1..31). A code is present when its bit is
/// set: byte `code / 8`, bit `code % 8`.
pub fn parse_pk_led_bitmap(bitmap: &[u8]) -> Vec<u8> {
    (1u16..32)
        .filter(|&code| {
            let byte_idx = code as usize / 8;
            let bit = (code % 8) as u8;
            bitmap.get(byte_idx).copied().unwrap_or(0) & (1 << bit) != 0
        })
        .map(|code| code as u8)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_effect_base_blocks_match_ghub_captures() {
        assert_eq!(
            find_native_effect("color_wave").unwrap().base,
            [0xff, 0x00, 0, 0, 0, 0, 0, 0, 0x88, 0x01, 0x64, 0x13, 0x01, 0, 0],
        );
        assert_eq!(
            find_native_effect("ripple").unwrap().base,
            [0xff, 0x03, 0x5e, 0x5e, 0x5e, 0xff, 0, 0, 0, 0x14, 0, 0, 0x01, 0, 0],
        );
        assert!(find_native_effect("nonexistent").is_none());
    }

    #[test]
    fn ripple_param_descriptors_decode_defaults_from_base() {
        let descs = find_native_effect("ripple").unwrap().param_descriptors();
        let by_id = |id: &str| descs.iter().find(|d| d.id == id).expect("param present");
        match &by_id("background").default {
            EffectParamValue::Color(c) => assert_eq!((c.r, c.g, c.b), (0x5e, 0x5e, 0x5e)),
            other => panic!("background default not a color: {other:?}"),
        }
        assert!(matches!(&by_id("rate").default, EffectParamValue::Float(v) if *v == 20.0));
        assert!(matches!(&by_id("saturation").default, EffectParamValue::Float(v) if *v == 100.0));
    }

    #[test]
    fn ripple_encode_overlays_param_values_and_clamps() {
        let ripple = find_native_effect("ripple").unwrap();
        let mut values = HashMap::new();
        values.insert(
            "background".to_string(),
            EffectParamValue::Color(RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            }),
        );
        values.insert("rate".to_string(), EffectParamValue::Float(100.0));
        values.insert("saturation".to_string(), EffectParamValue::Float(0.0));
        let block = ripple.encode(&values);
        assert_eq!(&block[2..5], &[0x11, 0x22, 0x33], "background color bytes");
        assert_eq!(block[9], 100, "effect rate byte");
        assert_eq!(block[5], 0, "saturation 0% → byte 0");

        values.insert("rate".to_string(), EffectParamValue::Float(999.0));
        assert_eq!(ripple.encode(&values)[9], 200);
        assert_eq!(
            find_native_effect("color_wave")
                .unwrap()
                .encode(&HashMap::new())[1],
            0x00
        );
    }

    #[test]
    fn encode_set_effect_static_builds_ghub_static_block() {
        let block = encode_set_effect_static(
            0x03,
            0x02,
            RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            },
        );
        assert_eq!(
            block,
            [0x03, 0x02, 0x11, 0x22, 0x33, 0x64, 0x0B, 0xB8, 0x64, 0, 0, 0, 0x01, 0, 0],
        );
    }

    #[test]
    fn parse_rgb_zone_count_reads_byte_2_or_none() {
        assert_eq!(parse_rgb_zone_count(&[0x00, 0x00, 0x05]), Some(5));
        assert_eq!(parse_rgb_zone_count(&[0x00, 0x00]), None);
    }

    #[test]
    fn parse_rgb_effect_table_entry_reads_be_u16_or_none() {
        assert_eq!(
            parse_rgb_effect_table_entry(&[0, 0, 0x12, 0x34]),
            Some(0x1234)
        );
        assert_eq!(parse_rgb_effect_table_entry(&[0, 0, 0x12]), None);
    }

    #[test]
    fn parse_pk_led_bitmap_decodes_set_bits() {
        let bitmap = [0x02, 0x01, 0x00, 0x80];
        assert_eq!(parse_pk_led_bitmap(&bitmap), vec![1, 8, 31]);
    }

    #[test]
    fn parse_pk_led_bitmap_handles_short_input_without_panic() {
        assert_eq!(parse_pk_led_bitmap(&[]), Vec::<u8>::new());
        assert_eq!(parse_pk_led_bitmap(&[0x02]), vec![1]);
    }
}
