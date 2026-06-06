// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// HID++ 2.0 RGB-effects codecs (feature 0x8071 RGB_EFFECTS and 0x808x
/// PER_KEY_LIGHTING).
///
/// Pure byte encoders/decoders for the Logitech RGB lighting features: the
/// native firmware-effect type system and its `NATIVE_EFFECTS` table, the
/// `SetEffect` static block builder, RGB feature-reply parsers, and the
/// per-key-lighting packet builders / LED-bitmap parser. Kept device-agnostic
/// so any HID++ device with these features can reuse them.
///
/// Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use std::collections::HashMap;

use halod_protocol::types::{
    EffectParamDescriptor, EffectParamValue, ParamKind, RgbColor,
};

use super::build_packet;

// ── Native effect type system ─────────────────────────────────────────────────

/// Where (and how) an effect parameter value lands in the SetEffect byte block.
#[derive(Clone, Copy)]
pub enum EffectByteSlot {
    /// RGB colour at bytes `[off, off+1, off+2]`.
    Color { off: usize },
    /// A `min..=max` (UI units) value stored in one byte. `scale` maps a UI
    /// unit to a byte value (1.0 = verbatim; 2.55 = 0-100 UI → 0-255 byte).
    Byte { off: usize, min: f64, max: f64, scale: f64 },
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
    // Color Wave — fixed preset; the param byte layout is not yet decoded.
    NativeEffectDef {
        id: "color_wave",
        name: "Color Wave",
        base: [0xff, 0x00, 0, 0, 0, 0, 0, 0, 0x88, 0x01, 0x64, 0x13, 0x01, 0, 0],
        params: &[],
    },
    // Ripple — offsets decoded from the G HUB capture
    // `ff 03 5e5e5e ff 00 00 00 14 00 00 01 ...` (background 0x5E5E5E, rate
    // 20 ms, saturation 100%).
    NativeEffectDef {
        id: "ripple",
        name: "Ripple",
        base: [0xff, 0x03, 0x5e, 0x5e, 0x5e, 0xff, 0, 0, 0, 0x14, 0, 0, 0x01, 0, 0],
        params: &[
            NativeEffectParam {
                id: "background",
                label: "Background Color",
                slot: EffectByteSlot::Color { off: 2 },
            },
            NativeEffectParam {
                id: "rate",
                label: "Effect Rate (ms)",
                slot: EffectByteSlot::Byte { off: 9, min: 2.0, max: 200.0, scale: 1.0 },
            },
            NativeEffectParam {
                id: "saturation",
                label: "Saturation",
                slot: EffectByteSlot::Byte { off: 5, min: 0.0, max: 100.0, scale: 2.55 },
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
                            r: self.base[off],
                            g: self.base[off + 1],
                            b: self.base[off + 2],
                        }),
                    ),
                    EffectByteSlot::Byte { off, min, max, scale } => (
                        ParamKind::Range { min, max, step: 1.0 },
                        EffectParamValue::Float((self.base[off] as f64 / scale).round()),
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
                    block[off] = c.r;
                    block[off + 1] = c.g;
                    block[off + 2] = c.b;
                }
                (EffectByteSlot::Byte { off, min, max, scale }, EffectParamValue::Float(f)) => {
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
/// plain static colour: `[zone, slot, r, g, b, period_hi, period_lo, ...]`.
/// The trailing constants (`0x64, 0x0B, 0xB8, 0x64, 0, 0, 0, 0x01, 0, 0`) are
/// the static-effect preset captured from G HUB.
pub fn encode_set_effect_static(zone_idx: u8, slot: u8, color: RgbColor) -> [u8; 15] {
    [
        zone_idx, slot, color.r, color.g, color.b, 0x64, 0x0B, 0xB8, 0x64, 0, 0,
        0, 0x01, 0, 0,
    ]
}

// ── RGB feature-reply parsers ──────────────────────────────────────────────────

/// Parse the zone count from an RGB_EFFECTS GetInfo(0xFF, 0xFF, 0x00) reply.
/// The zone count is byte 2; returns `None` when the reply is too short.
pub fn parse_rgb_zone_count(reply: &[u8]) -> Option<u8> {
    (reply.len() >= 3).then(|| reply[2])
}

/// Parse the effect_id from an RGB_EFFECTS GetInfo(zone, slot, 0x00) effect-table
/// entry reply. The effect_id is the big-endian u16 at bytes 2..4; returns
/// `None` when the reply is too short.
pub fn parse_rgb_effect_table_entry(reply: &[u8]) -> Option<u16> {
    (reply.len() >= 4).then(|| u16::from_be_bytes([reply[2], reply[3]]))
}

// ── PER_KEY_LIGHTING packet builders ───────────────────────────────────────────

/// Build one PER_KEY_LIGHTING SET_INDIVIDUAL (func 0x10) batch packet for up to
/// four `(key_id, r, g, b)` colour pairs. The 16-byte parameter buffer holds
/// four 4-byte entries; unused entries stay zero (callers pad short batches).
pub fn encode_per_key_batch(batch: &[(u8, u8, u8, u8)], devnum: u8, pk_idx: u8) -> Vec<u8> {
    let mut buf = [0u8; 16];
    for (i, &(k, r, g, bl)) in batch.iter().take(4).enumerate() {
        buf[i * 4] = k;
        buf[i * 4 + 1] = r;
        buf[i * 4 + 2] = g;
        buf[i * 4 + 3] = bl;
    }
    build_packet(devnum, pk_idx, 0x10 | 0x01, &buf, true)
}

/// Build the PER_KEY_LIGHTING COMMIT (func 0x70) packet that applies queued
/// per-key colour changes.
pub fn encode_per_key_commit(devnum: u8, pk_idx: u8) -> Vec<u8> {
    build_packet(devnum, pk_idx, 0x70 | 0x01, &[0x00], true)
}

/// Build the PER_KEY_LIGHTING SET_RANGE (func 0x50) parameter block that paints
/// the whole `[0x00, 0xFF]` key range a single colour.
pub fn encode_per_key_set_range(color: RgbColor) -> [u8; 5] {
    [0x00, 0xFF, color.r, color.g, color.b]
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

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn native_effect_base_blocks_match_ghub_captures() {
        // Base SetEffect (func 0x10) byte blocks captured from G HUB.
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
        // Background defaults to the captured 0x5E5E5E.
        match &by_id("background").default {
            EffectParamValue::Color(c) => assert_eq!((c.r, c.g, c.b), (0x5e, 0x5e, 0x5e)),
            other => panic!("background default not a color: {other:?}"),
        }
        // Rate 0x14 = 20 ms; saturation 0xFF → 100%.
        assert!(matches!(&by_id("rate").default, EffectParamValue::Float(v) if *v == 20.0));
        assert!(matches!(&by_id("saturation").default, EffectParamValue::Float(v) if *v == 100.0));
    }

    #[test]
    fn ripple_encode_overlays_param_values_and_clamps() {
        let ripple = find_native_effect("ripple").unwrap();
        let mut values = HashMap::new();
        values.insert(
            "background".to_string(),
            EffectParamValue::Color(RgbColor { r: 0x11, g: 0x22, b: 0x33 }),
        );
        values.insert("rate".to_string(), EffectParamValue::Float(100.0));
        values.insert("saturation".to_string(), EffectParamValue::Float(0.0));
        let block = ripple.encode(&values);
        assert_eq!(&block[2..5], &[0x11, 0x22, 0x33], "background color bytes");
        assert_eq!(block[9], 100, "effect rate byte");
        assert_eq!(block[5], 0, "saturation 0% → byte 0");

        // Rate clamps to 2..=200; an unset effect with no params is unchanged.
        values.insert("rate".to_string(), EffectParamValue::Float(999.0));
        assert_eq!(ripple.encode(&values)[9], 200);
        assert_eq!(find_native_effect("color_wave").unwrap().encode(&HashMap::new())[1], 0x00);
    }

    #[test]
    fn encode_set_effect_static_builds_ghub_static_block() {
        let block = encode_set_effect_static(0x03, 0x02, RgbColor { r: 0x11, g: 0x22, b: 0x33 });
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
        assert_eq!(parse_rgb_effect_table_entry(&[0, 0, 0x12, 0x34]), Some(0x1234));
        assert_eq!(parse_rgb_effect_table_entry(&[0, 0, 0x12]), None);
    }

    #[test]
    fn encode_per_key_batch_packs_four_byte_entries() {
        let pkt = encode_per_key_batch(&[(10, 1, 2, 3), (20, 4, 5, 6)], 0x01, 0x07);
        assert_eq!(pkt[2], 0x07, "sub_id = per-key feature index");
        assert_eq!(pkt[3], 0x11, "func 0x10 | SW id 0x01");
        assert_eq!(&pkt[4..12], &[10, 1, 2, 3, 20, 4, 5, 6]);
        assert!(pkt[12..].iter().all(|&b| b == 0), "unused entries stay zero");
    }

    #[test]
    fn encode_per_key_batch_caps_at_four_entries() {
        let batch = [(1, 0, 0, 0), (2, 0, 0, 0), (3, 0, 0, 0), (4, 0, 0, 0), (5, 0, 0, 0)];
        let pkt = encode_per_key_batch(&batch, 0x01, 0x07);
        assert_eq!(&pkt[4..8], &[1, 0, 0, 0]);
        assert_eq!(&pkt[16..20], &[4, 0, 0, 0], "fifth entry dropped");
    }

    #[test]
    fn encode_per_key_commit_targets_func_0x70() {
        let pkt = encode_per_key_commit(0x01, 0x07);
        assert_eq!(pkt[2], 0x07);
        assert_eq!(pkt[3], 0x71, "func 0x70 | SW id 0x01");
    }

    #[test]
    fn encode_per_key_set_range_paints_full_range() {
        assert_eq!(
            encode_per_key_set_range(RgbColor { r: 0x11, g: 0x22, b: 0x33 }),
            [0x00, 0xFF, 0x11, 0x22, 0x33],
        );
    }

    #[test]
    fn parse_pk_led_bitmap_decodes_set_bits() {
        // code 1 → byte 0 bit 1 (0x02); code 8 → byte 1 bit 0 (0x01);
        // code 31 → byte 3 bit 7 (0x80).
        let bitmap = [0x02, 0x01, 0x00, 0x80];
        assert_eq!(parse_pk_led_bitmap(&bitmap), vec![1, 8, 31]);
    }

    #[test]
    fn parse_pk_led_bitmap_handles_short_input_without_panic() {
        assert_eq!(parse_pk_led_bitmap(&[]), Vec::<u8>::new());
        assert_eq!(parse_pk_led_bitmap(&[0x02]), vec![1]);
    }
}
