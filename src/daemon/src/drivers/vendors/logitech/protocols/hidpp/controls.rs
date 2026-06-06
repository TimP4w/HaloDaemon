// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// HID++ 2.0 controls / remap codecs (features 0x1b04 REPROG_CONTROLS_V4,
/// 0x8010 GKEY and 0x8110 MOUSE_BUTTON_SPY).
///
/// Pure byte encoders/decoders for the Logitech control-remap features: the
/// generic task-ID → label mapping, the held-controls bitmap decoder, the
/// REPROG_CONTROLS_V4 divertedButtons event parser, the getCidInfo reply
/// parser, and the setCidReporting parameter-block builder. Kept device-
/// agnostic so any HID++ device with these features can reuse them.
///
/// Reference: Solaar (GPL-2.0-or-later) — hidpp20.py

/// One control parsed from a REPROG_CONTROLS_V4 getCidInfo (func 0x10) reply.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct CidInfo {
    /// Control ID (big-endian u16 in the reply).
    pub cid: u16,
    /// Task ID the control is currently bound to (big-endian u16).
    pub task_id: u16,
    /// Control flags byte; bit 3 (`0x08`) marks the control as divertable.
    pub flags: u8,
    /// Control group, used for grouped remap constraints.
    pub group: u8,
}

/// Map a REPROG_CONTROLS_V4 CID / task ID pair to a human-readable label.
/// Falls back to the hex CID string for unknown controls.
pub fn cid_label(cid: u16, task_id: u16) -> String {
    match task_id {
        0x0038 => "Left Button".to_string(),
        0x0039 => "Right Button".to_string(),
        0x003A => "Middle Button".to_string(),
        0x003B => "Back".to_string(),
        0x003C => "Forward".to_string(),
        0x00C7 => "DPI Down".to_string(),
        0x00C8 => "DPI Up".to_string(),
        0x00C9 => "DPI Cycle".to_string(),
        0x00D0 => "DPI Shift".to_string(),
        0x00D7 => "Smart Shift".to_string(),
        0x0050 => "Volume Mute".to_string(),
        0x0051 => "Volume Down".to_string(),
        0x0052 => "Volume Up".to_string(),
        _ => {
            // Try CID-based known values
            match cid {
                0x0056 => "Left Scroll".to_string(),
                0x005D => "Right Scroll".to_string(),
                _ => format!("Button {cid:#06x}"),
            }
        }
    }
}

/// Decode a held-controls bitmap (GKEY or MOUSE_BUTTON_SPY) into synthetic
/// CIDs. Control N is bit N-1, so CID = bit index + 1 — matching the
/// `ButtonDescriptor`s built by `init_gkey` / `init_mouse_button_spy`.
pub fn button_bitmap_to_cids(bitmap: u16) -> Vec<u16> {
    (0..16u16)
        .filter(|i| bitmap & (1u16 << i) != 0)
        .map(|i| i + 1)
        .collect()
}

/// Decode a REPROG_CONTROLS_V4 divertedButtonsEvent payload — up to 4 × u16
/// currently-pressed CIDs (zero-padded) — into a CID list. Big-endian u16s are
/// read in chunks; the zero padding is dropped.
pub fn parse_diverted_buttons_event(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .filter(|&cid| cid != 0)
        .collect()
}

/// Parse a REPROG_CONTROLS_V4 getCidInfo (func 0x10) reply.
///
/// Reply layout: `[cid_hi, cid_lo, task_hi, task_lo, flags, pos, group, gmask, ...]`.
/// Returns `None` when the reply is shorter than 8 bytes.
pub fn parse_cid_info(reply: &[u8]) -> Option<CidInfo> {
    if reply.len() < 8 {
        return None;
    }
    Some(CidInfo {
        cid: u16::from_be_bytes([reply[0], reply[1]]),
        task_id: u16::from_be_bytes([reply[2], reply[3]]),
        flags: reply[4],
        group: reply[6],
    })
}

/// Build the REPROG_CONTROLS_V4 setCidReporting (func 0x30) parameter block.
///
/// Layout: `[cid_hi, cid_lo, flags, 0, remap_hi, remap_lo, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0]`.
/// `flags` bit 0 is the divert flag; the remap target stays zero (native).
pub fn encode_set_cid_reporting(cid: u16, diverted: bool) -> [u8; 16] {
    let flags: u8 = if diverted { 0x01 } else { 0x00 };
    let cid_bytes = cid.to_be_bytes();
    [
        cid_bytes[0], cid_bytes[1], flags, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0, 0,
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn cid_label_known_tasks() {
        assert_eq!(cid_label(0x0050, 0x0038), "Left Button");
        assert_eq!(cid_label(0x0051, 0x0039), "Right Button");
        assert_eq!(cid_label(0x0052, 0x00C8), "DPI Up");
        assert_eq!(cid_label(0x0053, 0x00C7), "DPI Down");
    }

    #[test]
    fn cid_label_unknown_falls_back_to_hex() {
        let label = cid_label(0x1234, 0x9999);
        assert!(label.contains("0x1234"), "label should contain the CID hex: {label}");
    }

    #[test]
    fn button_bitmap_decodes_to_cids() {
        // Empty bitmap → no controls held.
        assert_eq!(button_bitmap_to_cids(0x0000), Vec::<u16>::new());
        // Single bits: control 1 is bit 0, control 12 is bit 11.
        assert_eq!(button_bitmap_to_cids(0x0001), vec![1]);
        assert_eq!(button_bitmap_to_cids(0x0080), vec![8]);
        assert_eq!(button_bitmap_to_cids(0x0800), vec![12]);
        // Multiple controls held at once, ascending CID order.
        assert_eq!(button_bitmap_to_cids(0x0005), vec![1, 3]);
        assert_eq!(button_bitmap_to_cids(0x0803), vec![1, 2, 12]);
    }

    #[test]
    fn diverted_buttons_event_reads_be_u16_chunks() {
        // Two CIDs followed by zero padding.
        let data: &[u8] = &[0x00, 0x50, 0x00, 0x51, 0x00, 0x00, 0x00, 0x00];
        assert_eq!(parse_diverted_buttons_event(data), vec![0x0050, 0x0051]);
    }

    #[test]
    fn diverted_buttons_event_empty_and_all_zero() {
        assert_eq!(parse_diverted_buttons_event(&[]), Vec::<u16>::new());
        assert_eq!(
            parse_diverted_buttons_event(&[0x00, 0x00, 0x00, 0x00]),
            Vec::<u16>::new()
        );
    }

    #[test]
    fn diverted_buttons_event_ignores_trailing_odd_byte() {
        // chunks_exact drops the trailing unpaired byte.
        let data: &[u8] = &[0x01, 0xB4, 0x07];
        assert_eq!(parse_diverted_buttons_event(data), vec![0x01B4]);
    }

    #[test]
    fn cid_info_parses_fields() {
        // [cid_hi, cid_lo, task_hi, task_lo, flags, pos, group, gmask]
        let reply: &[u8] = &[0x00, 0x50, 0x00, 0x38, 0x08, 0x01, 0x02, 0xFF];
        let info = parse_cid_info(reply).unwrap();
        assert_eq!(info.cid, 0x0050);
        assert_eq!(info.task_id, 0x0038);
        assert_eq!(info.flags, 0x08);
        assert_eq!(info.group, 0x02);
        // bit 3 marks divertable
        assert!(info.flags & 0x08 != 0);
    }

    #[test]
    fn cid_info_rejects_short_reply() {
        assert_eq!(parse_cid_info(&[]), None);
        assert_eq!(parse_cid_info(&[0x00, 0x50, 0x00, 0x38, 0x08, 0x01, 0x02]), None);
    }

    #[test]
    fn set_cid_reporting_encodes_divert_flag() {
        // Diverted: flags byte (index 2) = 0x01.
        let diverted = encode_set_cid_reporting(0x0050, true);
        assert_eq!(diverted.len(), 16);
        assert_eq!(diverted[0], 0x00);
        assert_eq!(diverted[1], 0x50);
        assert_eq!(diverted[2], 0x01);
        assert!(diverted[3..].iter().all(|&b| b == 0));

        // Not diverted: flags byte = 0x00.
        let native = encode_set_cid_reporting(0x01B4, false);
        assert_eq!(native[0], 0x01);
        assert_eq!(native[1], 0xB4);
        assert_eq!(native[2], 0x00);
        assert!(native[3..].iter().all(|&b| b == 0));
    }
}
