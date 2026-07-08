// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! REPORT_RATE (`0x8060`) and EXT_REPORT_RATE (`0x8061`) — polling-rate reads
//! and writes. Owns the extended-rate table so the device never sees rate
//! indices or wire bytes.
use anyhow::Result;

use super::super::{feature, Hidpp20};

// ── Function codes ────────────────────────────────────────────────────────────
const EXT_GET_LIST: u8 = 0x10;
const EXT_GET_CURRENT: u8 = 0x20;
const EXT_SET: u8 = 0x30;
const PLAIN_GET_LIST: u8 = 0x00;
const PLAIN_GET_CURRENT: u8 = 0x10;
const PLAIN_SET: u8 = 0x20;

/// Extended report-rate (0x8061) table: rate index → (label, ms).
/// `ms` is 0 for sub-millisecond rates (identified by their rate index).
const EXT_REPORT_RATES: [(&str, u8); 7] = [
    ("8ms", 8),
    ("4ms", 4),
    ("2ms", 2),
    ("1ms", 1),
    ("500µs", 0),
    ("250µs", 0),
    ("125µs", 0),
];

/// One selectable report rate.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRateOption {
    /// Value written to the device to select this rate (rate index for ext,
    /// milliseconds for plain).
    pub wire_index: u8,
    /// Milliseconds (0 for sub-ms ext rates).
    pub ms: u8,
    /// Display label (e.g. "1ms", "500µs").
    pub label: String,
}

/// The device's report-rate capability: the selectable options, the current
/// selection (matched by `wire_index`), and whether it's the extended feature.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ReportRateInfo {
    pub options: Vec<ReportRateOption>,
    /// Currently-selected `wire_index`, if known.
    pub current: Option<u8>,
    pub ext: bool,
}

impl Hidpp20 {
    /// Read the device's report-rate options + current selection. Prefers
    /// EXT_REPORT_RATE, falling back to REPORT_RATE.
    pub async fn read_report_rates(&self) -> Option<ReportRateInfo> {
        if self.has(feature::EXT_REPORT_RATE) {
            if let Some(info) = self.read_ext_report_rates().await {
                return Some(info);
            }
        }
        if self.has(feature::REPORT_RATE) {
            return self.read_plain_report_rates().await;
        }
        None
    }

    async fn read_ext_report_rates(&self) -> Option<ReportRateInfo> {
        let idx = self.idx(feature::EXT_REPORT_RATE)?;
        let rates = self.call(idx, EXT_GET_LIST, &[]).await.ok()?;
        let cur = self.call(idx, EXT_GET_CURRENT, &[]).await.ok()?;
        let flags = if rates.len() >= 2 {
            ((rates[0] as u16) << 8) | rates[1] as u16
        } else {
            rates.first().copied().unwrap_or(0) as u16
        };
        Some(ReportRateInfo {
            options: ext_report_rate_options(flags),
            current: cur.first().copied(),
            ext: true,
        })
    }

    async fn read_plain_report_rates(&self) -> Option<ReportRateInfo> {
        let idx = self.idx(feature::REPORT_RATE)?;
        let rates = self.call(idx, PLAIN_GET_LIST, &[]).await.ok()?;
        let cur = self.call(idx, PLAIN_GET_CURRENT, &[]).await.ok()?;
        let flags = rates.first().copied().unwrap_or(0);
        Some(ReportRateInfo {
            options: plain_report_rate_options(flags),
            current: cur.first().copied(),
            ext: false,
        })
    }

    /// Write a report rate by `wire_index` (from a [`ReportRateOption`]).
    pub async fn set_report_rate(&self, wire_index: u8, ext: bool) -> Result<()> {
        if ext {
            let idx = self
                .idx(feature::EXT_REPORT_RATE)
                .ok_or_else(|| anyhow::anyhow!("EXT_REPORT_RATE not available"))?;
            self.call(idx, EXT_SET, &[wire_index]).await?;
        } else {
            let idx = self
                .idx(feature::REPORT_RATE)
                .ok_or_else(|| anyhow::anyhow!("REPORT_RATE not available"))?;
            self.call(idx, PLAIN_SET, &[wire_index]).await?;
        }
        Ok(())
    }
}

// ── Pure flag-to-option mappings (testable without hardware) ──────────────────

/// Decode an EXT_REPORT_RATE 16-bit flags word into available options.
pub(super) fn ext_report_rate_options(flags: u16) -> Vec<ReportRateOption> {
    EXT_REPORT_RATES
        .iter()
        .enumerate()
        .filter(|(i, _)| flags & (1 << i) != 0)
        .map(|(i, (label, ms))| ReportRateOption {
            wire_index: i as u8,
            ms: *ms,
            label: label.to_string(),
        })
        .collect()
}

/// Decode a plain REPORT_RATE 8-bit flags byte into available options.
pub(super) fn plain_report_rate_options(flags: u8) -> Vec<ReportRateOption> {
    (0..8u8)
        .filter(|i| (flags >> i) & 1 != 0)
        .map(|i| {
            let ms = i + 1;
            ReportRateOption {
                wire_index: ms,
                ms,
                label: format!("{ms}ms"),
            }
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    // ── EXT_REPORT_RATE flag decoding ──────────────────────────────────────

    #[test]
    fn ext_flags_0x0009_yields_two_options() {
        let opts = ext_report_rate_options(0x0009);
        assert_eq!(opts.len(), 2);
        assert_eq!(opts[0].wire_index, 0); // 8ms
        assert_eq!(opts[1].wire_index, 3); // 1ms
    }

    #[test]
    fn ext_flags_0x0001_yields_only_8ms() {
        let opts = ext_report_rate_options(0x0001);
        assert_eq!(opts.len(), 1);
        assert_eq!(opts[0].wire_index, 0);
        assert_eq!(opts[0].label, "8ms");
    }

    #[test]
    fn ext_flags_all_bits_yields_7_options() {
        let opts = ext_report_rate_options(0x007F);
        assert_eq!(opts.len(), 7);
        assert_eq!(opts[0].label, "8ms");
        assert_eq!(opts[6].label, "125µs");
    }

    #[test]
    fn ext_flags_0x0000_yields_empty() {
        let opts = ext_report_rate_options(0x0000);
        assert!(opts.is_empty());
    }

    // ── Plain REPORT_RATE flag decoding ────────────────────────────────────

    #[test]
    fn plain_flags_0x0f_yields_four_options() {
        let opts = plain_report_rate_options(0x0F);
        assert_eq!(opts.len(), 4);
        assert_eq!(opts[0].ms, 1);
        assert_eq!(opts[3].ms, 4);
    }

    #[test]
    fn plain_flags_0x00_yields_empty() {
        let opts = plain_report_rate_options(0x00);
        assert!(opts.is_empty());
    }

    #[test]
    fn plain_flags_0xff_yields_8_options_1ms_through_8ms() {
        let opts = plain_report_rate_options(0xFF);
        assert_eq!(opts.len(), 8);
        assert_eq!(opts[0].label, "1ms");
        assert_eq!(opts[7].label, "8ms");
    }
}
