// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
/// HID++ 2.0 ONBOARD_PROFILES (feature 0x8100) codecs.
///
/// Pure byte encoders/decoders for the Logitech onboard-profile flash format,
/// plus the sector read/write I/O helpers that drive them over a
/// [`HidppMessenger`]. Kept device-agnostic so any HID++ device with the
/// 0x8100 feature can reuse them.
///
/// Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use anyhow::Result;

use super::{crc16, HidppMessenger};
use halod_protocol::types::{OnboardProfileSlot, OnboardProfiles};

/// Parse the profile directory (sector 0x0000) into `(sector, enabled)` entries.
/// Directory entries are 4 bytes: sector (BE u16) + enabled byte + reserved;
/// the list ends at a 0xFFFF sector. Pure function — no I/O.
pub fn parse_profile_directory(dir: &[u8]) -> Vec<(u16, bool)> {
    let mut entries = Vec::new();
    let mut entry = 0;
    while entry * 4 + 2 < dir.len() {
        let sector = u16::from_be_bytes([dir[entry * 4], dir[entry * 4 + 1]]);
        if sector == 0xFFFF || sector == 0x0000 {
            break;
        }
        entries.push((sector, dir[entry * 4 + 2] != 0));
        entry += 1;
    }
    entries
}

/// Build the wire `OnboardProfiles` snapshot from the parsed directory and the
/// active profile sector. Slot index is the low byte of the sector address
/// (RAM sector 0x000N → slot N). Returns None when the directory is empty.
pub fn build_onboard_profiles(
    profile_dir: &[(u16, bool)],
    profile_sector: u16,
    rom_profile_count: u8,
) -> Option<OnboardProfiles> {
    if profile_dir.is_empty() {
        return None;
    }
    let active_slot = (profile_sector & 0xFF) as u8;
    let slots = profile_dir
        .iter()
        .map(|&(sector, enabled)| {
            let index = (sector & 0xFF) as u8;
            OnboardProfileSlot {
                index,
                enabled,
                active: active_slot != 0 && index == active_slot,
                has_rom_default: index != 0 && index <= rom_profile_count,
            }
        })
        .collect();
    Some(OnboardProfiles { active_slot, slots })
}

/// ROM source sector to seed RAM `slot` (1-based) from. Slots within the
/// device's factory profile count use their own ROM default (`0x010N`); slots
/// beyond it have none, so they are seeded from ROM profile 1 (`0x0101`).
pub fn rom_source_sector(slot: u8, rom_profile_count: u8) -> u16 {
    if slot <= rom_profile_count {
        0x0100 | slot as u16
    } else {
        0x0101
    }
}

/// Set the trailing big-endian CRC16 of a flash sector over `bytes[..size-2]`.
pub fn set_sector_crc(bytes: &mut [u8], sector_size: usize) {
    let crc = crc16(&bytes[..sector_size - 2]).to_be_bytes();
    bytes[sector_size - 2] = crc[0];
    bytes[sector_size - 1] = crc[1];
}

/// Patch a profile sector in-place: write new DPI steps, clamp resolution indices,
/// and recompute the trailing CRC16. Pure function — no I/O.
pub fn patch_profile_sector(sector_bytes: &mut [u8], new_steps: &[u16], sector_size: usize) {
    for (i, &dpi) in new_steps.iter().take(5).enumerate() {
        let le = dpi.to_le_bytes();
        sector_bytes[3 + i * 2] = le[0];
        sector_bytes[3 + i * 2 + 1] = le[1];
    }
    for i in new_steps.len()..5 {
        sector_bytes[3 + i * 2] = 0;
        sector_bytes[3 + i * 2 + 1] = 0;
    }
    let n = new_steps.len();
    if n > 0 && (sector_bytes[1] as usize) >= n {
        sector_bytes[1] = (n - 1) as u8;
    }
    if n > 0 && (sector_bytes[2] as usize) >= n {
        sector_bytes[2] = 0;
    }
    set_sector_crc(sector_bytes, sector_size);
}

/// Parse DPI steps from a profile sector: bytes[3..13], 5 × u16 little-endian,
/// excluding the 0x0000 and 0xFFFF unused-slot markers. Pure function.
pub fn parse_dpi_steps_from_sector(sector_data: &[u8]) -> Vec<u16> {
    if sector_data.len() < 13 {
        return Vec::new();
    }
    (0..5)
        .map(|i| u16::from_le_bytes([sector_data[3 + i * 2], sector_data[4 + i * 2]]))
        .filter(|&v| v != 0 && v != 0xFFFF)
        .collect()
}

/// Pack an ONBOARD_PROFILES read param block: a sector address and a byte
/// offset, both big-endian u16, for the func-0x50 request.
fn sector_offset_params(sector: u16, offset: usize) -> [u8; 4] {
    let s = sector.to_be_bytes();
    let o = (offset as u16).to_be_bytes();
    [s[0], s[1], o[0], o[1]]
}

/// Read a full ONBOARD_PROFILES sector via `msg`, using standard byte offsets.
/// Returns None and logs on error.
pub async fn read_full_sector_via(
    msg: &HidppMessenger,
    devnum: u8,
    op_idx: u8,
    sector: u16,
    size: usize,
) -> Option<Vec<u8>> {
    if size < 16 {
        log::warn!("read_full_sector: size {size} < 16");
        return None;
    }
    let mut buf = vec![0xFFu8; size];
    let mut o: usize = 0;
    while o + 15 < size {
        let params = sector_offset_params(sector, o);
        match msg.feature_request(devnum, op_idx, 0x50, &params).await {
            Ok(chunk) => { buf[o..o + 16.min(chunk.len())].copy_from_slice(&chunk[..16.min(chunk.len())]); }
            Err(e) => { log::warn!("read_full_sector offset={o}: {e}"); return None; }
        }
        o += 16;
    }
    let tail = size - 16;
    let params = sector_offset_params(sector, tail);
    match msg.feature_request(devnum, op_idx, 0x50, &params).await {
        Ok(chunk) => {
            let skip = 16 + o - size;
            let src = &chunk[skip..chunk.len().min(16)];
            buf[o..o + src.len()].copy_from_slice(src);
        }
        Err(e) => { log::warn!("read_full_sector tail: {e}"); return None; }
    }
    Some(buf)
}

/// Erase, write (in 16-byte chunks) and commit a full flash sector via
/// ONBOARD_PROFILES funcs 0x60/0x70/0x80. `write_sector` must be a writable
/// RAM sector (not a read-only 0x01xx ROM address).
pub async fn write_full_sector_via(
    msg: &HidppMessenger,
    devnum: u8,
    op_idx: u8,
    write_sector: u16,
    bytes: &[u8],
) -> Result<()> {
    let sector_size = bytes.len();

    // Erase (MEMORY_ADDR_WRITE = func=0x60) — addresses the flash write sector
    let ws = write_sector.to_be_bytes();
    let sz = (sector_size as u16).to_be_bytes();
    let erase_params = [ws[0], ws[1], 0, 0, sz[0], sz[1]];
    msg.feature_request(devnum, op_idx, 0x60, &erase_params).await?;

    // Write in 16-byte chunks
    let mut woffset: usize = 0;
    while woffset < sector_size {
        let end = (woffset + 16).min(sector_size);
        msg.feature_request(devnum, op_idx, 0x70, &bytes[woffset..end]).await?;
        woffset += 16;
    }

    // Commit
    msg.feature_request(devnum, op_idx, 0x80, &[]).await?;
    Ok(())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::crc16;

    // Profile 2 sector captured from a G502X Plus (sector 0x0002, 255 bytes).
    // Used as the ground-truth for patch tests.
    fn profile2_before() -> Vec<u8> {
        let mut v = vec![0xFFu8; 255];
        let bytes: &[u8] = &[
            0x02, 0x02, 0x00, 0x20, 0x03, 0xb0, 0x04, 0x40, 0x06, 0x60, 0x09, 0x80, 0x0c, 0xff, 0xff, 0xff,
            0xff, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x3c, 0x00, 0x2c, 0x01,
            0x80, 0x01, 0x00, 0x01, 0x80, 0x01, 0x00, 0x02, 0x80, 0x01, 0x00, 0x04, 0x80, 0x01, 0x00, 0x08,
            0x90, 0x0b, 0x00, 0x00, 0x80, 0x01, 0x00, 0x10, 0x90, 0x01, 0x00, 0x00, 0x90, 0x02, 0x00, 0x00,
            0x90, 0x0a, 0x00, 0x00, 0x90, 0x03, 0x00, 0x00, 0x90, 0x04, 0x00, 0x00, 0x90, 0x10, 0x00, 0x00,
            0x90, 0x11, 0x00, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0x80, 0x01, 0x00, 0x01, 0x80, 0x01, 0x00, 0x02, 0x80, 0x02, 0x01, 0x17, 0x80, 0x01, 0x00, 0x08,
            0xff, 0xff, 0xff, 0xff, 0x80, 0x01, 0x00, 0x10, 0x80, 0x02, 0x03, 0x2b, 0x80, 0x02, 0x01, 0x2b,
            0x80, 0x02, 0x01, 0x27, 0x80, 0x02, 0x01, 0x1d, 0x80, 0x02, 0x01, 0x1b, 0x80, 0x03, 0x00, 0xea,
            0x80, 0x03, 0x00, 0xe9, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            // 0xa0-0xcf: all 0xFF (already set by vec init)
        ];
        v[..bytes.len()].copy_from_slice(bytes);
        // 0xd0-0xfe from the dump
        let tail: &[u8] = &[
            0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x0f, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00,
            0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x03, 0x0b, 0x6e,
        ];
        v[0xd0..0xd0 + tail.len()].copy_from_slice(tail);
        v
    }

    #[test]
    fn patch_dpi_steps_updates_bytes_3_to_12() {
        let mut sector = profile2_before();
        patch_profile_sector(&mut sector, &[1200, 1600], 255);

        // Steps at bytes 3-6
        assert_eq!(u16::from_le_bytes([sector[3], sector[4]]), 1200);
        assert_eq!(u16::from_le_bytes([sector[5], sector[6]]), 1600);
        // Unused slots zeroed
        for i in 2..5usize {
            assert_eq!(u16::from_le_bytes([sector[3 + i*2], sector[4 + i*2]]), 0);
        }
    }

    #[test]
    fn patch_clamps_resolution_default_index() {
        // BEFORE: byte[1]=0x02 (default on step 2), applying only 2 steps → clamp to 1
        let mut sector = profile2_before();
        assert_eq!(sector[1], 0x02);
        patch_profile_sector(&mut sector, &[1200, 1600], 255);
        assert_eq!(sector[1], 1, "resolution_default_index must be clamped to n-1");
    }

    #[test]
    fn patch_preserves_button_data() {
        let before = profile2_before();
        let mut sector = before.clone();
        patch_profile_sector(&mut sector, &[1200, 1600], 255);
        // Buttons start at byte 32; g-buttons at 96 — must be untouched
        assert_eq!(&sector[32..160], &before[32..160]);
    }

    #[test]
    fn patch_updates_crc() {
        let mut sector = profile2_before();
        patch_profile_sector(&mut sector, &[1200, 1600], 255);
        // CRC captured from the real write log
        assert_eq!([sector[253], sector[254]], [0x89, 0x1a]);
        // Verify CRC is self-consistent
        let computed = crc16(&sector[..253]);
        assert_eq!(computed.to_be_bytes(), [sector[253], sector[254]]);
    }

    #[test]
    fn patch_crc_matches_unmodified_original() {
        // The original sector's trailing CRC should verify against its own content
        let sector = profile2_before();
        let stored = u16::from_be_bytes([sector[253], sector[254]]);
        assert_eq!(crc16(&sector[..253]), stored);
    }

    #[test]
    fn patch_5_steps_no_zeroing_no_clamp() {
        let mut sector = profile2_before();
        let steps = [800u16, 1200, 1600, 2400, 3200];
        patch_profile_sector(&mut sector, &steps, 255);
        for (i, &expected) in steps.iter().enumerate() {
            assert_eq!(u16::from_le_bytes([sector[3 + i*2], sector[4 + i*2]]), expected);
        }
        // default_index=2 is still valid with 5 steps — should not be clamped
        assert_eq!(sector[1], 0x02);
    }

    #[test]
    fn patch_empty_steps_does_not_panic() {
        let mut sector = profile2_before();
        let before_byte1 = sector[1];
        let before_byte2 = sector[2];
        patch_profile_sector(&mut sector, &[], 255);
        for i in 0..5usize {
            assert_eq!(u16::from_le_bytes([sector[3 + i*2], sector[4 + i*2]]), 0);
        }
        assert_eq!(sector[1], before_byte1);
        assert_eq!(sector[2], before_byte2);
    }

    #[test]
    fn parse_profile_directory_reads_all_entries() {
        // Directory: profile 1 @ 0x0001 enabled, 2 @ 0x0002 enabled, 3 @ 0x0003
        // disabled, then a 0xFFFF terminator. 4-byte entries: sector(BE) +
        // enabled + reserved.
        let dir = vec![
            0x00, 0x01, 0x01, 0x20,
            0x00, 0x02, 0x01, 0x40,
            0x00, 0x03, 0x00, 0x20,
            0xFF, 0xFF, 0x00, 0x00,
        ];
        assert_eq!(
            parse_profile_directory(&dir),
            vec![(0x0001, true), (0x0002, true), (0x0003, false)],
        );
    }

    #[test]
    fn parse_profile_directory_stops_at_terminator_and_sector_zero() {
        // Immediate 0xFFFF terminator → no entries.
        assert!(parse_profile_directory(&[0xFF, 0xFF, 0x00, 0x00]).is_empty());
        // All-0xFF sector (unread flash) → no entries.
        assert!(parse_profile_directory(&[0xFF; 64]).is_empty());
        // A 0x0000 sector also terminates the list.
        let dir = vec![
            0x00, 0x01, 0x01, 0x00,
            0x00, 0x00, 0x01, 0x00,
            0x00, 0x02, 0x01, 0x00,
        ];
        assert_eq!(parse_profile_directory(&dir), vec![(0x0001, true)]);
    }

    #[test]
    fn directory_enable_flip_roundtrips_with_crc() {
        // 64-byte directory sector with three enabled profiles + trailing CRC16.
        let sector_size = 64usize;
        let mut dir = vec![0xFFu8; sector_size];
        dir[0..12].copy_from_slice(&[
            0x00, 0x01, 0x01, 0x00,
            0x00, 0x02, 0x01, 0x00,
            0x00, 0x03, 0x01, 0x00,
        ]);
        dir[12..14].copy_from_slice(&[0xFF, 0xFF]); // terminator
        set_sector_crc(&mut dir, sector_size);

        // Disable slot 3 (entry index 2) and recompute the CRC.
        dir[2 * 4 + 2] = 0;
        set_sector_crc(&mut dir, sector_size);

        let entries = parse_profile_directory(&dir);
        assert_eq!(entries, vec![(0x0001, true), (0x0002, true), (0x0003, false)]);
        // CRC must validate over bytes[..size-2].
        let crc = crc16(&dir[..sector_size - 2]).to_be_bytes();
        assert_eq!(&dir[sector_size - 2..], &crc);
    }

    #[test]
    fn rom_ram_sector_mapping() {
        // ROM defaults live at 0x01xx; the writable RAM sector is the low byte.
        for slot in 1u8..=5 {
            let rom_sector = 0x0100u16 | slot as u16;
            assert_eq!(rom_sector & 0xFF, slot as u16);
            assert!(rom_sector >= 0x0100);
        }
    }

    #[test]
    fn build_onboard_profiles_marks_active_slot() {
        let dir = vec![(0x0001u16, true), (0x0002, true), (0x0003, false)];
        // Device with 2 factory ROM profiles.
        let profiles = build_onboard_profiles(&dir, 0x0002, 2).expect("non-empty directory");
        assert_eq!(profiles.active_slot, 2);
        assert_eq!(profiles.slots.len(), 3);
        assert!(profiles.slots[1].active, "slot 2 is active");
        assert!(!profiles.slots[0].active);
        assert!(!profiles.slots[2].enabled);
        // ROM active sector resolves to the same RAM slot.
        let rom_active = build_onboard_profiles(&dir, 0x0102, 2).expect("non-empty");
        assert_eq!(rom_active.active_slot, 2);
        // Empty directory → None.
        assert!(build_onboard_profiles(&[], 0x0001, 2).is_none());
    }

    #[test]
    fn build_onboard_profiles_marks_rom_backed_slots() {
        let dir = vec![(0x0001u16, true), (0x0002, true), (0x0003, false), (0x0004, false)];
        // Only the first 2 slots are backed by a factory ROM profile.
        let profiles = build_onboard_profiles(&dir, 0x0001, 2).expect("non-empty");
        assert!(profiles.slots[0].has_rom_default, "slot 1 has a ROM default");
        assert!(profiles.slots[1].has_rom_default, "slot 2 has a ROM default");
        assert!(!profiles.slots[2].has_rom_default, "slot 3 has no ROM default");
        assert!(!profiles.slots[3].has_rom_default, "slot 4 has no ROM default");
    }

    #[test]
    fn rom_source_sector_falls_back_to_profile_1() {
        // Slots within the factory profile count use their own ROM sector.
        assert_eq!(rom_source_sector(1, 2), 0x0101);
        assert_eq!(rom_source_sector(2, 2), 0x0102);
        // Slots beyond it are seeded from ROM profile 1.
        assert_eq!(rom_source_sector(3, 2), 0x0101);
        assert_eq!(rom_source_sector(5, 2), 0x0101);
    }

    #[test]
    fn parse_dpi_steps_from_sector_extracts_steps() {
        let mut sector = vec![0u8; 32];
        // bytes[3..13] = 5 × u16 LE: 800, 1600, 3200, then 2 unused slots.
        sector[3..5].copy_from_slice(&800u16.to_le_bytes());
        sector[5..7].copy_from_slice(&1600u16.to_le_bytes());
        sector[7..9].copy_from_slice(&3200u16.to_le_bytes());
        sector[9..11].copy_from_slice(&0u16.to_le_bytes());
        sector[11..13].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(parse_dpi_steps_from_sector(&sector), vec![800, 1600, 3200]);
        // Too-short input yields no steps.
        assert!(parse_dpi_steps_from_sector(&[0u8; 8]).is_empty());
    }
}
