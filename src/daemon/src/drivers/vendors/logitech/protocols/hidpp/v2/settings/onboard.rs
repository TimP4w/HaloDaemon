// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! ONBOARD_PROFILES (feature `0x8100`) codecs + typed operations: profile-flash
//! sector I/O, host/onboard mode, capabilities, and active-profile reads.
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use anyhow::Result;

use super::super::super::crc16;
use super::super::{feature, Hidpp20};
use halod_shared::types::{OnboardProfileSlot, OnboardProfiles};

// ── Function codes ────────────────────────────────────────────────────────────
const GET_INFO: u8 = 0x00;
const SET_MODE: u8 = 0x10;
const GET_MODE: u8 = 0x20;
const SET_CURRENT_PROFILE: u8 = 0x30;
const GET_CURRENT_PROFILE: u8 = 0x40;
const MEMORY_READ: u8 = 0x50;
const MEMORY_ADDR_WRITE: u8 = 0x60;
const MEMORY_WRITE: u8 = 0x70;
const MEMORY_WRITE_END: u8 = 0x80;

/// Mode byte: host (software controls).
pub const MODE_HOST: u8 = 0x02;
/// Mode byte: onboard (firmware profiles).
pub const MODE_ONBOARD: u8 = 0x01;

/// ONBOARD_PROFILES capabilities decoded from getInfo (func 0x00).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct OnboardCaps {
    /// Number of factory (ROM) profiles.
    pub rom_profile_count: u8,
    /// Flash sector size in bytes.
    pub sector_size: usize,
}

/// Parse ONBOARD_PROFILES getInfo: `info[4]` = ROM profile count, `info[7:9]` =
/// sector size (BE u16). `None` when too short or sector size is 0.
pub fn parse_onboard_caps(info: &[u8]) -> Option<OnboardCaps> {
    if info.len() < 9 {
        return None;
    }
    let sector_size = u16::from_be_bytes([info[7], info[8]]) as usize;
    if sector_size == 0 {
        return None;
    }
    Some(OnboardCaps {
        rom_profile_count: info[4],
        sector_size,
    })
}

/// Parse the profile directory (sector 0x0000) into `(sector, enabled)` entries.
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
/// active profile sector. Returns None when the directory is empty.
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

/// ROM source sector to seed RAM `slot` (1-based) from. Slots beyond the
/// device's factory profile count are seeded from ROM profile 1 (`0x0101`).
pub fn rom_source_sector(slot: u8, rom_profile_count: u8) -> u16 {
    if slot <= rom_profile_count {
        0x0100 | slot as u16
    } else {
        0x0101
    }
}

/// Set the trailing big-endian CRC16 of a flash sector over `bytes[..size-2]`.
pub fn set_sector_crc(bytes: &mut [u8], sector_size: usize) -> Result<()> {
    anyhow::ensure!(
        sector_size >= 2 && bytes.len() >= sector_size,
        "set_sector_crc: buffer (len {}) too short for sector_size {sector_size}",
        bytes.len()
    );
    let crc = crc16(&bytes[..sector_size - 2]).to_be_bytes();
    bytes[sector_size - 2] = crc[0];
    bytes[sector_size - 1] = crc[1];
    Ok(())
}

/// Patch a profile sector in-place: write new DPI steps, clamp resolution
/// indices, and recompute the trailing CRC16.
pub fn patch_profile_sector(
    sector_bytes: &mut [u8],
    new_steps: &[u16],
    sector_size: usize,
) -> Result<()> {
    anyhow::ensure!(
        sector_bytes.len() >= sector_size && sector_size >= 13,
        "patch_profile_sector: buffer (len {}) too short for sector_size {sector_size}",
        sector_bytes.len()
    );
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
    set_sector_crc(sector_bytes, sector_size)?;
    Ok(())
}

/// Parse DPI steps from a profile sector: bytes[3..13], 5 × u16 little-endian,
/// excluding the 0x0000 and 0xFFFF unused-slot markers.
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

/// Byte range `[skip, end)` to copy from the final (tail) sector chunk.
fn tail_slice_bounds(chunk_len: usize, o: usize, size: usize) -> (usize, usize) {
    let end = chunk_len.min(16);
    let skip = (16 + o).saturating_sub(size).min(end);
    (skip, end)
}

/// memoryRead attempts per 16-byte chunk. The onboard flash read is relayed
/// over the wireless link and transiently returns INVALID_ARGUMENT while the
/// receiver bus is busy (e.g. concurrent RGB writes); a couple of quick retries
/// let the read complete instead of dropping the whole sector — which would
/// otherwise blank the profile directory and flicker the onboard UI.
const MEMORY_READ_ATTEMPTS: u8 = 3;
const MEMORY_READ_RETRY_DELAY: std::time::Duration = std::time::Duration::from_millis(20);

impl Hidpp20 {
    /// Read the onboard mode byte (func 0x20). `None` on error.
    /// NOTE: func 0x10 is *setMode* — never call it with empty params to "read".
    pub async fn read_onboard_mode(&self) -> Option<u8> {
        let idx = self.idx(feature::ONBOARD_PROFILES)?;
        match self.call(idx, GET_MODE, &[]).await {
            Ok(r) => r.first().copied(),
            Err(e) => {
                log::warn!("[HID++2.0] ONBOARD_PROFILES getOnboardMode failed: {e}");
                None
            }
        }
    }

    /// Set host (`true`) or onboard (`false`) mode (func 0x10).
    pub async fn set_onboard_mode(&self, host: bool) -> Result<()> {
        let idx = self
            .idx(feature::ONBOARD_PROFILES)
            .ok_or_else(|| anyhow::anyhow!("ONBOARD_PROFILES not available"))?;
        let mode = if host { MODE_HOST } else { MODE_ONBOARD };
        self.call(idx, SET_MODE, &[mode]).await?;
        Ok(())
    }

    /// Read ONBOARD_PROFILES capabilities (func 0x00).
    pub async fn read_onboard_capabilities(&self) -> Option<OnboardCaps> {
        let idx = self.idx(feature::ONBOARD_PROFILES)?;
        match self.call(idx, GET_INFO, &[]).await {
            Ok(r) => parse_onboard_caps(&r),
            Err(e) => {
                log::warn!("[HID++2.0] ONBOARD_PROFILES getInfo failed: {e}");
                None
            }
        }
    }

    /// Read the active profile sector (func 0x40). `None` when no onboard
    /// profile is active (host mode) — the firmware returns `0xFFFF`/`0x0000`.
    pub async fn read_active_profile_sector(&self) -> Option<u16> {
        let idx = self.idx(feature::ONBOARD_PROFILES)?;
        match self.call(idx, GET_CURRENT_PROFILE, &[]).await {
            Ok(r) if r.len() >= 2 && r[0..2] != [0xFF, 0xFF] && r[0..2] != [0x00, 0x00] => {
                Some(u16::from_be_bytes([r[0], r[1]]))
            }
            _ => None,
        }
    }

    /// Select the active onboard profile by 1-based slot (func 0x30).
    pub async fn set_current_profile(&self, slot: u8) -> Result<()> {
        let idx = self
            .idx(feature::ONBOARD_PROFILES)
            .ok_or_else(|| anyhow::anyhow!("ONBOARD_PROFILES not available"))?;
        self.call(idx, SET_CURRENT_PROFILE, &[0x00, slot, 0x00])
            .await?;
        Ok(())
    }

    /// Read one 16-byte flash chunk at `offset`, retrying transient errors.
    /// `None` once every attempt fails.
    async fn memory_read_chunk(&self, idx: u8, sector: u16, offset: usize) -> Option<Vec<u8>> {
        let params = sector_offset_params(sector, offset);
        for attempt in 1..=MEMORY_READ_ATTEMPTS {
            match self.call(idx, MEMORY_READ, &params).await {
                Ok(chunk) => return Some(chunk),
                Err(e) if attempt == MEMORY_READ_ATTEMPTS => {
                    log::debug!(
                        "read_profile_sector sector={sector:#06x} offset={offset}: {e} \
                         (after {MEMORY_READ_ATTEMPTS} attempts)"
                    );
                }
                Err(_) => tokio::time::sleep(MEMORY_READ_RETRY_DELAY).await,
            }
        }
        None
    }

    /// Read a full flash sector (`size` bytes) via repeated memoryRead (0x50).
    /// `None` when a chunk can't be read after its retries.
    pub async fn read_profile_sector(&self, sector: u16, size: usize) -> Option<Vec<u8>> {
        let idx = self.idx(feature::ONBOARD_PROFILES)?;
        if size < 16 {
            log::warn!("read_profile_sector: size {size} < 16");
            return None;
        }
        let mut buf = vec![0xFFu8; size];
        let mut o: usize = 0;
        while o + 15 < size {
            let chunk = self.memory_read_chunk(idx, sector, o).await?;
            let n = 16.min(chunk.len());
            buf[o..o + n].copy_from_slice(&chunk[..n]);
            o += 16;
        }
        // If size is an exact multiple of 16, the main loop already covered it.
        if !size.is_multiple_of(16) {
            let tail = size - 16;
            let chunk = self.memory_read_chunk(idx, sector, tail).await?;
            let (skip, end) = tail_slice_bounds(chunk.len(), o, size);
            let src = &chunk[skip..end];
            buf[o..o + src.len()].copy_from_slice(src);
        }
        Some(buf)
    }

    /// Erase, write (16-byte chunks) and commit a writable RAM sector
    /// (funcs 0x60/0x70/0x80). `write_sector` must not be a read-only ROM address.
    pub async fn write_profile_sector(&self, write_sector: u16, bytes: &[u8]) -> Result<()> {
        let idx = self
            .idx(feature::ONBOARD_PROFILES)
            .ok_or_else(|| anyhow::anyhow!("ONBOARD_PROFILES not available"))?;
        let sector_size = bytes.len();
        let ws = write_sector.to_be_bytes();
        let sz = (sector_size as u16).to_be_bytes();
        let erase_params = [ws[0], ws[1], 0, 0, sz[0], sz[1]];
        self.call(idx, MEMORY_ADDR_WRITE, &erase_params).await?;

        let mut woffset: usize = 0;
        while woffset < sector_size {
            let end = (woffset + 16).min(sector_size);
            let chunk = &bytes[woffset..end];
            if chunk.len() < 16 {
                let mut padded = [0u8; 16];
                padded[..chunk.len()].copy_from_slice(chunk);
                self.call(idx, MEMORY_WRITE, &padded).await?;
            } else {
                self.call(idx, MEMORY_WRITE, chunk).await?;
            }
            woffset += 16;
        }
        self.call(idx, MEMORY_WRITE_END, &[]).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::crc16;

    fn profile2_before() -> Vec<u8> {
        let mut v = vec![0xFFu8; 255];
        let bytes: &[u8] = &[
            0x02, 0x02, 0x00, 0x20, 0x03, 0xb0, 0x04, 0x40, 0x06, 0x60, 0x09, 0x80, 0x0c, 0xff,
            0xff, 0xff, 0xff, 0x00, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0x3c, 0x00, 0x2c, 0x01, 0x80, 0x01, 0x00, 0x01, 0x80, 0x01, 0x00, 0x02, 0x80, 0x01,
            0x00, 0x04, 0x80, 0x01, 0x00, 0x08, 0x90, 0x0b, 0x00, 0x00, 0x80, 0x01, 0x00, 0x10,
            0x90, 0x01, 0x00, 0x00, 0x90, 0x02, 0x00, 0x00, 0x90, 0x0a, 0x00, 0x00, 0x90, 0x03,
            0x00, 0x00, 0x90, 0x04, 0x00, 0x00, 0x90, 0x10, 0x00, 0x00, 0x90, 0x11, 0x00, 0x00,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff, 0x80, 0x01,
            0x00, 0x01, 0x80, 0x01, 0x00, 0x02, 0x80, 0x02, 0x01, 0x17, 0x80, 0x01, 0x00, 0x08,
            0xff, 0xff, 0xff, 0xff, 0x80, 0x01, 0x00, 0x10, 0x80, 0x02, 0x03, 0x2b, 0x80, 0x02,
            0x01, 0x2b, 0x80, 0x02, 0x01, 0x27, 0x80, 0x02, 0x01, 0x1d, 0x80, 0x02, 0x01, 0x1b,
            0x80, 0x03, 0x00, 0xea, 0x80, 0x03, 0x00, 0xe9, 0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
            0xff, 0xff, 0xff, 0xff, 0xff, 0xff,
        ];
        v[..bytes.len()].copy_from_slice(bytes);
        let tail: &[u8] = &[
            0x0f, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x0f, 0x00, 0x00,
            0x00, 0x00, 0x00, 0x00, 0x00, 0x64, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00,
            0x00, 0x00, 0x64, 0x00, 0x00, 0x10, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x00, 0x64,
            0x00, 0x00, 0x03, 0x0b, 0x6e,
        ];
        v[0xd0..0xd0 + tail.len()].copy_from_slice(tail);
        v
    }

    #[test]
    fn parse_onboard_caps_reads_rom_count_and_sector_size() {
        // info[4]=2 (rom), info[7:9]=0x00FF (255).
        let info = [0, 0, 0, 4, 2, 0, 0, 0x00, 0xFF];
        assert_eq!(
            parse_onboard_caps(&info),
            Some(OnboardCaps {
                rom_profile_count: 2,
                sector_size: 255
            })
        );
        assert!(parse_onboard_caps(&[0, 0, 0, 0, 0, 0, 0, 0]).is_none());
        // sector_size 0 → None.
        assert!(parse_onboard_caps(&[0, 0, 0, 4, 2, 0, 0, 0, 0]).is_none());
    }

    #[test]
    fn patch_dpi_steps_updates_bytes_3_to_12() {
        let mut sector = profile2_before();
        patch_profile_sector(&mut sector, &[1200, 1600], 255).unwrap();
        assert_eq!(u16::from_le_bytes([sector[3], sector[4]]), 1200);
        assert_eq!(u16::from_le_bytes([sector[5], sector[6]]), 1600);
        for i in 2..5usize {
            assert_eq!(
                u16::from_le_bytes([sector[3 + i * 2], sector[4 + i * 2]]),
                0
            );
        }
    }

    #[test]
    fn patch_clamps_resolution_default_index() {
        let mut sector = profile2_before();
        assert_eq!(sector[1], 0x02);
        patch_profile_sector(&mut sector, &[1200, 1600], 255).unwrap();
        assert_eq!(sector[1], 1);
    }

    #[test]
    fn patch_preserves_button_data() {
        let before = profile2_before();
        let mut sector = before.clone();
        patch_profile_sector(&mut sector, &[1200, 1600], 255).unwrap();
        assert_eq!(&sector[32..160], &before[32..160]);
    }

    #[test]
    fn patch_updates_crc() {
        let mut sector = profile2_before();
        patch_profile_sector(&mut sector, &[1200, 1600], 255).unwrap();
        assert_eq!([sector[253], sector[254]], [0x89, 0x1a]);
        let computed = crc16(&sector[..253]);
        assert_eq!(computed.to_be_bytes(), [sector[253], sector[254]]);
    }

    #[test]
    fn patch_crc_matches_unmodified_original() {
        let sector = profile2_before();
        let stored = u16::from_be_bytes([sector[253], sector[254]]);
        assert_eq!(crc16(&sector[..253]), stored);
    }

    #[test]
    fn tail_slice_bounds_clamps_and_handles_short_chunk() {
        assert_eq!(tail_slice_bounds(16, 240, 255), (1, 16));
        assert_eq!(tail_slice_bounds(8, 240, 255), (1, 8));
        assert_eq!(tail_slice_bounds(0, 240, 255), (0, 0));
    }

    #[test]
    fn patch_5_steps_no_zeroing_no_clamp() {
        let mut sector = profile2_before();
        let steps = [800u16, 1200, 1600, 2400, 3200];
        patch_profile_sector(&mut sector, &steps, 255).unwrap();
        for (i, &expected) in steps.iter().enumerate() {
            assert_eq!(
                u16::from_le_bytes([sector[3 + i * 2], sector[4 + i * 2]]),
                expected
            );
        }
        assert_eq!(sector[1], 0x02);
    }

    #[test]
    fn patch_empty_steps_does_not_panic() {
        let mut sector = profile2_before();
        let before_byte1 = sector[1];
        let before_byte2 = sector[2];
        patch_profile_sector(&mut sector, &[], 255).unwrap();
        for i in 0..5usize {
            assert_eq!(
                u16::from_le_bytes([sector[3 + i * 2], sector[4 + i * 2]]),
                0
            );
        }
        assert_eq!(sector[1], before_byte1);
        assert_eq!(sector[2], before_byte2);
    }

    #[test]
    fn parse_profile_directory_reads_all_entries() {
        let dir = vec![
            0x00, 0x01, 0x01, 0x20, 0x00, 0x02, 0x01, 0x40, 0x00, 0x03, 0x00, 0x20, 0xFF, 0xFF,
            0x00, 0x00,
        ];
        assert_eq!(
            parse_profile_directory(&dir),
            vec![(0x0001, true), (0x0002, true), (0x0003, false)],
        );
    }

    #[test]
    fn parse_profile_directory_stops_at_terminator_and_sector_zero() {
        assert!(parse_profile_directory(&[0xFF, 0xFF, 0x00, 0x00]).is_empty());
        assert!(parse_profile_directory(&[0xFF; 64]).is_empty());
        let dir = vec![
            0x00, 0x01, 0x01, 0x00, 0x00, 0x00, 0x01, 0x00, 0x00, 0x02, 0x01, 0x00,
        ];
        assert_eq!(parse_profile_directory(&dir), vec![(0x0001, true)]);
    }

    #[test]
    fn directory_enable_flip_roundtrips_with_crc() {
        let sector_size = 64usize;
        let mut dir = vec![0xFFu8; sector_size];
        dir[0..12].copy_from_slice(&[
            0x00, 0x01, 0x01, 0x00, 0x00, 0x02, 0x01, 0x00, 0x00, 0x03, 0x01, 0x00,
        ]);
        dir[12..14].copy_from_slice(&[0xFF, 0xFF]);
        set_sector_crc(&mut dir, sector_size).unwrap();
        dir[2 * 4 + 2] = 0;
        set_sector_crc(&mut dir, sector_size).unwrap();
        let entries = parse_profile_directory(&dir);
        assert_eq!(
            entries,
            vec![(0x0001, true), (0x0002, true), (0x0003, false)]
        );
        let crc = crc16(&dir[..sector_size - 2]).to_be_bytes();
        assert_eq!(&dir[sector_size - 2..], &crc);
    }

    #[test]
    fn rom_ram_sector_mapping() {
        for slot in 1u8..=5 {
            let rom_sector = 0x0100u16 | slot as u16;
            assert_eq!(rom_sector & 0xFF, slot as u16);
            assert!(rom_sector >= 0x0100);
        }
    }

    #[test]
    fn build_onboard_profiles_marks_active_slot() {
        let dir = vec![(0x0001u16, true), (0x0002, true), (0x0003, false)];
        let profiles = build_onboard_profiles(&dir, 0x0002, 2).expect("non-empty directory");
        assert_eq!(profiles.active_slot, 2);
        assert_eq!(profiles.slots.len(), 3);
        assert!(profiles.slots[1].active);
        assert!(!profiles.slots[0].active);
        assert!(!profiles.slots[2].enabled);
        let rom_active = build_onboard_profiles(&dir, 0x0102, 2).expect("non-empty");
        assert_eq!(rom_active.active_slot, 2);
        assert!(build_onboard_profiles(&[], 0x0001, 2).is_none());
    }

    #[test]
    fn build_onboard_profiles_marks_rom_backed_slots() {
        let dir = vec![
            (0x0001u16, true),
            (0x0002, true),
            (0x0003, false),
            (0x0004, false),
        ];
        let profiles = build_onboard_profiles(&dir, 0x0001, 2).expect("non-empty");
        assert!(profiles.slots[0].has_rom_default);
        assert!(profiles.slots[1].has_rom_default);
        assert!(!profiles.slots[2].has_rom_default);
        assert!(!profiles.slots[3].has_rom_default);
    }

    #[test]
    fn rom_source_sector_falls_back_to_profile_1() {
        assert_eq!(rom_source_sector(1, 2), 0x0101);
        assert_eq!(rom_source_sector(2, 2), 0x0102);
        assert_eq!(rom_source_sector(3, 2), 0x0101);
        assert_eq!(rom_source_sector(5, 2), 0x0101);
    }

    fn onboard_channel(
        memory_read: Vec<std::result::Result<Vec<u8>, String>>,
    ) -> std::sync::Arc<dyn crate::drivers::vendors::logitech::protocols::hidpp::HidppChannel> {
        use crate::drivers::vendors::logitech::protocols::hidpp::test_util::MockHidppChannel;
        std::sync::Arc::new(MockHidppChannel::new(std::collections::HashMap::from([(
            super::MEMORY_READ,
            memory_read.into(),
        )])))
    }

    fn onboard_hidpp(
        ch: std::sync::Arc<dyn crate::drivers::vendors::logitech::protocols::hidpp::HidppChannel>,
    ) -> super::Hidpp20 {
        super::Hidpp20::new(
            ch,
            0x01,
            std::collections::HashMap::from([(feature::ONBOARD_PROFILES, 0x0b)]),
        )
    }

    // A transient memoryRead error must be retried, not fatal: two failures
    // followed by a good chunk still yields the sector.
    #[tokio::test]
    async fn read_profile_sector_retries_transient_errors() {
        let chunk = vec![0xABu8; 16];
        let ch = onboard_channel(vec![
            Err("code=0x02".into()),
            Err("code=0x02".into()),
            Ok(chunk.clone()),
        ]);
        let sector = onboard_hidpp(ch).read_profile_sector(0x0001, 16).await;
        assert_eq!(sector, Some(chunk));
    }

    // Once the retries are exhausted the read gives up (returns None) rather than
    // looping forever or panicking.
    #[tokio::test]
    async fn read_profile_sector_gives_up_after_exhausting_retries() {
        let ch = onboard_channel(vec![
            Err("code=0x02".into()),
            Err("code=0x02".into()),
            Err("code=0x02".into()),
        ]);
        let sector = onboard_hidpp(ch).read_profile_sector(0x0001, 16).await;
        assert_eq!(sector, None);
    }

    #[test]
    fn parse_dpi_steps_from_sector_extracts_steps() {
        let mut sector = vec![0u8; 32];
        sector[3..5].copy_from_slice(&800u16.to_le_bytes());
        sector[5..7].copy_from_slice(&1600u16.to_le_bytes());
        sector[7..9].copy_from_slice(&3200u16.to_le_bytes());
        sector[9..11].copy_from_slice(&0u16.to_le_bytes());
        sector[11..13].copy_from_slice(&0xFFFFu16.to_le_bytes());
        assert_eq!(parse_dpi_steps_from_sector(&sector), vec![800, 1600, 3200]);
        assert!(parse_dpi_steps_from_sector(&[0u8; 8]).is_empty());
    }
}
