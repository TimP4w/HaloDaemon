// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! ADJUSTABLE_DPI (feature `0x2201`) codecs + typed operations.
use anyhow::Result;

use super::super::{feature, Hidpp20};

// ── Function codes ────────────────────────────────────────────────────────────
const GET_DPI_LIST: u8 = 0x10;
const GET_DPI: u8 = 0x20;
const SET_DPI: u8 = 0x30;

/// Parse raw DPI list bytes from ADJUSTABLE_DPI func=0x10 chunks.
pub fn parse_dpi_list(raw: &[u8]) -> Vec<u16> {
    let mut list: Vec<u16> = Vec::new();
    let mut i = 0;
    while i + 1 < raw.len() {
        let val = u16::from_be_bytes([raw[i], raw[i + 1]]);
        if val == 0 {
            break;
        }
        if val >> 13 == 0b111 {
            let step = val & 0x1FFF;
            if i + 3 < raw.len() && step > 0 {
                let end = u16::from_be_bytes([raw[i + 2], raw[i + 3]]);
                if let Some(&last) = list.last() {
                    let mut cur = last.checked_add(step);
                    while let Some(c) = cur.filter(|c| *c <= end) {
                        list.push(c);
                        cur = c.checked_add(step);
                    }
                }
                i += 4;
                continue;
            } else {
                break;
            }
        } else {
            list.push(val);
            i += 2;
        }
    }
    list
}

/// Parse the current DPI from an ADJUSTABLE_DPI func=0x20 (getDpi) reply.
pub fn parse_current_dpi(reply: &[u8]) -> Option<u16> {
    let dpi = if reply.len() >= 3 {
        u16::from_be_bytes([reply[1], reply[2]])
    } else if reply.len() >= 2 {
        u16::from_be_bytes([reply[0], reply[1]])
    } else {
        return None;
    };
    (dpi != 0).then_some(dpi)
}

/// Encode the ADJUSTABLE_DPI (0x2201) setSensorDPI (func 0x30) parameter block:
/// `[sensor=0, dpi_hi, dpi_lo]`.
pub fn encode_set_dpi(dpi: u16) -> [u8; 3] {
    [0x00, (dpi >> 8) as u8, dpi as u8]
}

impl Hidpp20 {
    /// Read the device's supported DPI list (chunked, range-decoded). Empty when
    /// the feature is absent.
    pub async fn read_dpi_list(&self) -> Vec<u16> {
        let Some(idx) = self.idx(feature::ADJUSTABLE_DPI) else {
            return Vec::new();
        };
        let mut raw = Vec::new();
        for chunk_idx in 0u8..16 {
            match self.call(idx, GET_DPI_LIST, &[0x00, 0x00, chunk_idx]).await {
                Ok(chunk) => {
                    let payload = if chunk.len() > 1 {
                        &chunk[1..]
                    } else {
                        &chunk[..]
                    };
                    raw.extend_from_slice(payload);
                    if payload.windows(2).any(|w| w == [0, 0]) {
                        break;
                    }
                }
                Err(e) => {
                    log::warn!("[HID++2.0] DPI list chunk {chunk_idx} failed: {e}");
                    break;
                }
            }
        }
        parse_dpi_list(&raw)
    }

    /// Read the current sensor DPI. `None` on error / unsupported.
    pub async fn read_current_dpi(&self) -> Option<u16> {
        let idx = self.idx(feature::ADJUSTABLE_DPI)?;
        match self.call(idx, GET_DPI, &[]).await {
            Ok(reply) => parse_current_dpi(&reply),
            Err(e) => {
                log::warn!("[HID++2.0] ADJUSTABLE_DPI getCurrentDpi failed: {e}");
                None
            }
        }
    }

    /// Set the sensor DPI (setSensorDPI, func 0x30).
    pub async fn set_dpi(&self, dpi: u16) -> Result<()> {
        let idx = self
            .idx(feature::ADJUSTABLE_DPI)
            .ok_or_else(|| anyhow::anyhow!("ADJUSTABLE_DPI not available"))?;
        self.call(idx, SET_DPI, &encode_set_dpi(dpi)).await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dpi_list_explicit_values() {
        let raw: &[u8] = &[0x01, 0x90, 0x03, 0x20, 0x06, 0x40, 0x00, 0x00];
        assert_eq!(parse_dpi_list(raw), vec![400, 800, 1600]);
    }

    #[test]
    fn test_dpi_list_range_encoding() {
        let raw: &[u8] = &[0x01, 0x90, 0xE1, 0x90, 0x06, 0x40, 0x00, 0x00];
        assert_eq!(parse_dpi_list(raw), vec![400, 800, 1200, 1600]);
    }

    #[test]
    fn dpi_list_range_step_past_u16_does_not_panic() {
        let raw: &[u8] = &[0xDF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00];
        assert_eq!(parse_dpi_list(raw), vec![57343, 65534]);
    }

    #[test]
    fn dpi_list_range_to_max_end_terminates() {
        let raw: &[u8] = &[
            0x90, 0x00, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0xFF, 0x00, 0x00,
        ];
        assert_eq!(parse_dpi_list(raw), vec![36864, 45055, 53246, 61437]);
    }

    #[test]
    fn test_parse_current_dpi_long_reply_with_sensor_echo() {
        assert_eq!(
            parse_current_dpi(&[0x00, 0x06, 0x40, 0x00, 0x00]),
            Some(1600)
        );
    }

    #[test]
    fn test_parse_current_dpi_short_reply() {
        assert_eq!(parse_current_dpi(&[0x03, 0x20]), Some(800));
    }

    #[test]
    fn test_parse_current_dpi_rejects_zero() {
        assert_eq!(parse_current_dpi(&[0x00, 0x00, 0x00]), None);
        assert_eq!(parse_current_dpi(&[0x00, 0x00]), None);
    }

    #[test]
    fn test_parse_current_dpi_too_short() {
        assert_eq!(parse_current_dpi(&[]), None);
        assert_eq!(parse_current_dpi(&[0x42]), None);
    }

    #[test]
    fn test_encode_set_dpi() {
        assert_eq!(encode_set_dpi(1600), [0x00, 0x06, 0x40]);
        assert_eq!(encode_set_dpi(800), [0x00, 0x03, 0x20]);
        assert_eq!(encode_set_dpi(3200), [0x00, 0x0c, 0x80]);
        assert_eq!(encode_set_dpi(0), [0x00, 0x00, 0x00]);
    }

    proptest::proptest! {
        /// Property: `parse_dpi_list` never panics on arbitrary byte input,
        /// and every returned value is a valid `u16`.
        #[test]
        fn parse_dpi_list_never_panics(raw: Vec<u8>) {
            let list = parse_dpi_list(&raw);
            // If parsing succeeds (it always does for parse_dpi_list — it just
            // returns a best-effort Vec), every value must be a valid u16.
            for dpi in &list {
                let _: u16 = *dpi;
            }
        }
    }
}
