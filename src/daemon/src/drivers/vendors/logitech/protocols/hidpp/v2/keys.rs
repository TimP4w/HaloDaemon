// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! Controls / remap features: REPROG_CONTROLS_V4 (`0x1b04`), GKEY (`0x8010`)
//! and MOUSE_BUTTON_SPY (`0x8110`).
//!
//! Codecs (task-id → label, held-controls bitmap, diverted-buttons event,
//! getCidInfo parser, setCidReporting builder) plus the typed [`Hidpp20`]
//! operations that drive them. Device-agnostic.
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use std::borrow::Cow;

use super::{feature, Hidpp20};

// ── Function codes ────────────────────────────────────────────────────────────
const REPROG_GET_COUNT: u8 = 0x00;
const REPROG_GET_CID_INFO: u8 = 0x10;
const REPROG_SET_CID_REPORTING: u8 = 0x30;
const GKEY_GET_COUNT: u8 = 0x00;
const GKEY_ENABLE_SW_CONTROL: u8 = 0x20;
const SPY_SET_STATE: u8 = 0x10;

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

impl CidInfo {
    /// Whether the control can be diverted (flags bit 3).
    pub fn divertable(&self) -> bool {
        self.flags & 0x08 != 0
    }
}

pub fn cid_label(cid: u16, task_id: u16) -> Cow<'static, str> {
    match task_id {
        0x0038 => Cow::Borrowed("Left Button"),
        0x0039 => Cow::Borrowed("Right Button"),
        0x003A => Cow::Borrowed("Middle Button"),
        0x003B => Cow::Borrowed("Back"),
        0x003C => Cow::Borrowed("Forward"),
        0x00C7 => Cow::Borrowed("DPI Down"),
        0x00C8 => Cow::Borrowed("DPI Up"),
        0x00C9 => Cow::Borrowed("DPI Cycle"),
        0x00D0 => Cow::Borrowed("DPI Shift"),
        0x00D7 => Cow::Borrowed("Smart Shift"),
        0x0050 => Cow::Borrowed("Volume Mute"),
        0x0051 => Cow::Borrowed("Volume Down"),
        0x0052 => Cow::Borrowed("Volume Up"),
        _ => match cid {
            0x0056 => Cow::Borrowed("Left Scroll"),
            0x005D => Cow::Borrowed("Right Scroll"),
            _ => Cow::Owned(format!("Button {cid:#06x}")),
        },
    }
}

/// Decode a held-controls bitmap (GKEY or MOUSE_BUTTON_SPY) into synthetic
/// CIDs. Control N is bit N-1, so CID = bit index + 1.
pub fn button_bitmap_to_cids(bitmap: u16) -> Vec<u16> {
    (0..16u16)
        .filter(|i| bitmap & (1u16 << i) != 0)
        .map(|i| i + 1)
        .collect()
}

/// Decode a REPROG_CONTROLS_V4 divertedButtonsEvent payload — up to 4 × u16
/// currently-pressed CIDs (zero-padded) — into a CID list.
pub fn parse_diverted_buttons_event(data: &[u8]) -> Vec<u16> {
    data.chunks_exact(2)
        .map(|b| u16::from_be_bytes([b[0], b[1]]))
        .filter(|&cid| cid != 0)
        .collect()
}

/// Decode a GKEY/SPY bitmap event payload (16-bit little-endian) into CIDs.
pub fn parse_button_bitmap_event(data: &[u8]) -> Vec<u16> {
    let bitmap = u16::from_le_bytes([
        data.first().copied().unwrap_or(0),
        data.get(1).copied().unwrap_or(0),
    ]);
    button_bitmap_to_cids(bitmap)
}

/// Parse a REPROG_CONTROLS_V4 getCidInfo (func 0x10) reply.
/// Reply layout: `[cid_hi, cid_lo, task_hi, task_lo, flags, pos, group, gmask, ...]`.
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
/// Layout: `[cid_hi, cid_lo, flags, 0, ...]`; `flags` bit 0 is the divert flag.
pub fn encode_set_cid_reporting(cid: u16, diverted: bool) -> [u8; 16] {
    let mut arr = [0u8; 16];
    let [hi, lo] = cid.to_be_bytes();
    arr[0] = hi;
    arr[1] = lo;
    arr[2] = if diverted { 0x01 } else { 0x00 };
    arr
}

// ── Typed operations ──────────────────────────────────────────────────────────

impl Hidpp20 {
    /// Number of REPROG_CONTROLS_V4 controls, or 0 when unavailable.
    pub async fn reprog_control_count(&self) -> u8 {
        let Some(idx) = self.idx(feature::REPROG_CONTROLS_V4) else {
            return 0;
        };
        match self.call(idx, REPROG_GET_COUNT, &[]).await {
            Ok(r) => r.first().copied().unwrap_or(0),
            Err(e) => {
                log::warn!("[HID++2.0] REPROG_CONTROLS_V4 getCount failed: {e}");
                0
            }
        }
    }

    /// Read getCidInfo for control index `i`.
    pub async fn reprog_control_info(&self, i: u8) -> Option<CidInfo> {
        let idx = self.idx(feature::REPROG_CONTROLS_V4)?;
        match self.call(idx, REPROG_GET_CID_INFO, &[i]).await {
            Ok(r) => parse_cid_info(&r),
            Err(e) => {
                log::warn!("[HID++2.0] getCidInfo({i}) failed: {e}");
                None
            }
        }
    }

    /// Set per-control divert via setCidReporting (REPROG_CONTROLS_V4).
    pub async fn set_cid_reporting(&self, cid: u16, diverted: bool) -> anyhow::Result<()> {
        let idx = self
            .idx(feature::REPROG_CONTROLS_V4)
            .ok_or_else(|| anyhow::anyhow!("REPROG_CONTROLS_V4 not available"))?;
        self.call(
            idx,
            REPROG_SET_CID_REPORTING,
            &encode_set_cid_reporting(cid, diverted),
        )
        .await?;
        Ok(())
    }

    /// Number of G-keys, or 0 when GKEY is unavailable.
    pub async fn gkey_count(&self) -> u8 {
        let Some(idx) = self.idx(feature::GKEY) else {
            return 0;
        };
        match self.call(idx, GKEY_GET_COUNT, &[]).await {
            Ok(r) => r.first().copied().unwrap_or(0),
            Err(e) => {
                log::warn!("[HID++2.0] GKEY getCount failed: {e}");
                0
            }
        }
    }

    /// Toggle GKEY global software control.
    pub async fn set_gkey_software_control(&self, enabled: bool) -> anyhow::Result<()> {
        let idx = self
            .idx(feature::GKEY)
            .ok_or_else(|| anyhow::anyhow!("GKEY not available"))?;
        self.call(idx, GKEY_ENABLE_SW_CONTROL, &[u8::from(enabled)])
            .await?;
        Ok(())
    }

    /// Toggle MOUSE_BUTTON_SPY host-side press reporting.
    pub async fn set_mouse_button_spy(&self, enabled: bool) -> anyhow::Result<()> {
        let idx = self
            .idx(feature::MOUSE_BUTTON_SPY)
            .ok_or_else(|| anyhow::anyhow!("MOUSE_BUTTON_SPY not available"))?;
        self.call(idx, SPY_SET_STATE, &[u8::from(enabled)]).await?;
        Ok(())
    }
}

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
        assert!(
            label.contains("0x1234"),
            "label should contain the CID hex: {label}"
        );
    }

    #[test]
    fn button_bitmap_decodes_to_cids() {
        assert_eq!(button_bitmap_to_cids(0x0000), Vec::<u16>::new());
        assert_eq!(button_bitmap_to_cids(0x0001), vec![1]);
        assert_eq!(button_bitmap_to_cids(0x0080), vec![8]);
        assert_eq!(button_bitmap_to_cids(0x0800), vec![12]);
        assert_eq!(button_bitmap_to_cids(0x0005), vec![1, 3]);
        assert_eq!(button_bitmap_to_cids(0x0803), vec![1, 2, 12]);
    }

    #[test]
    fn parse_button_bitmap_event_reads_le() {
        // 0x0005 little-endian = bits 0,2 → cids 1,3.
        assert_eq!(
            parse_button_bitmap_event(&[0x05, 0x00, 0x00, 0x00]),
            vec![1, 3]
        );
    }

    #[test]
    fn diverted_buttons_event_reads_be_u16_chunks() {
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
        let data: &[u8] = &[0x01, 0xB4, 0x07];
        assert_eq!(parse_diverted_buttons_event(data), vec![0x01B4]);
    }

    #[test]
    fn cid_info_parses_fields() {
        let reply: &[u8] = &[0x00, 0x50, 0x00, 0x38, 0x08, 0x01, 0x02, 0xFF];
        let info = parse_cid_info(reply).unwrap();
        assert_eq!(info.cid, 0x0050);
        assert_eq!(info.task_id, 0x0038);
        assert_eq!(info.flags, 0x08);
        assert_eq!(info.group, 0x02);
        assert!(info.divertable());
    }

    #[test]
    fn cid_info_rejects_short_reply() {
        assert_eq!(parse_cid_info(&[]), None);
        assert_eq!(
            parse_cid_info(&[0x00, 0x50, 0x00, 0x38, 0x08, 0x01, 0x02]),
            None
        );
    }

    #[test]
    fn set_cid_reporting_encodes_divert_flag() {
        let diverted = encode_set_cid_reporting(0x0050, true);
        assert_eq!(diverted.len(), 16);
        assert_eq!(diverted[0], 0x00);
        assert_eq!(diverted[1], 0x50);
        assert_eq!(diverted[2], 0x01);
        assert!(diverted[3..].iter().all(|&b| b == 0));

        let native = encode_set_cid_reporting(0x01B4, false);
        assert_eq!(native[0], 0x01);
        assert_eq!(native[1], 0xB4);
        assert_eq!(native[2], 0x00);
        assert!(native[3..].iter().all(|&b| b == 0));
    }
}
