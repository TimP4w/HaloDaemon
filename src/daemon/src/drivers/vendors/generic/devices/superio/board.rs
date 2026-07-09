// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]

//! Motherboard identity lookup (Windows DMI/SMBIOS via WMI) and per-board
//! fan-label tables.
//!
//! The known-board table maps each `(manufacturer, model)` pair to a per-fan
//! channel silkscreen label (CPU_FAN, CHA_FAN1, AIO_PUMP, …). On misses we
//! fall back to a chip-shape-derived heuristic.

use crate::drivers::vendors::generic::devices::superio::DetectedChip;
use std::collections::HashMap;

/// SMBIOS baseboard identity.
#[derive(Debug, Clone, Default)]
pub struct BoardInfo {
    pub manufacturer: String,
    pub product: String,
}

impl BoardInfo {
    /// Lowercase, hyphen-stripped key for table lookup.
    pub fn key(&self) -> (String, String) {
        let n = |s: &str| s.to_lowercase().replace(['-', '_', ' ', '.'], "");
        (n(&self.manufacturer), n(&self.product))
    }
}

/// Read `Win32_BaseBoard.Manufacturer` + `Win32_BaseBoard.Product` via WMI.
/// Returns an empty record on any error — labels then fall back to heuristic.
pub fn read_board_info() -> BoardInfo {
    use wmi::{COMLibrary, Variant, WMIConnection};

    let com = match COMLibrary::new() {
        Ok(c) => c,
        Err(e) => {
            log::debug!("[SuperIO] board: COM init failed: {e}");
            return BoardInfo::default();
        }
    };
    let conn = match WMIConnection::new(com) {
        Ok(c) => c,
        Err(e) => {
            log::debug!("[SuperIO] board: WMI connect failed: {e}");
            return BoardInfo::default();
        }
    };

    let rows: Vec<HashMap<String, Variant>> =
        match conn.raw_query("SELECT Manufacturer, Product FROM Win32_BaseBoard") {
            Ok(r) => r,
            Err(e) => {
                log::debug!("[SuperIO] board: WMI query failed: {e}");
                return BoardInfo::default();
            }
        };

    let str_of = |v: Option<&Variant>| -> String {
        match v {
            Some(Variant::String(s)) => s.trim().to_string(),
            _ => String::new(),
        }
    };

    let info = rows
        .first()
        .map(|row| BoardInfo {
            manufacturer: str_of(row.get("Manufacturer")),
            product: str_of(row.get("Product")),
        })
        .unwrap_or_default();

    log::info!(
        "[SuperIO] board: manufacturer={:?} product={:?}",
        info.manufacturer,
        info.product
    );
    info
}

/// Per-channel fan label for a known board. Returns `None` if the board
/// isn't in the table.
fn known_board_labels(board: &BoardInfo) -> Option<&'static [&'static str]> {
    // Lookup is case- and separator-insensitive (see BoardInfo::key).
    let (mfr, product) = board.key();
    match (mfr.as_str(), product.as_str()) {
        // ASUS X870E ProArt Creator WiFi (NCT6796DR, 7 channels)
        ("asus" | "asustek" | "asustekcomputerinc", p) if p.contains("proartx870e") => Some(&[
            "CPU Fan",
            "CPU OPT",
            "Chassis Fan 1",
            "Chassis Fan 2",
            "Chassis Fan 3",
            "AIO Pump",
            "Water Pump",
        ]),
        // ASUS ROG STRIX X870E-E / X870E-F GAMING WIFI
        ("asus" | "asustek" | "asustekcomputerinc", p)
            if p.contains("x870e") && p.contains("strix") =>
        {
            Some(&[
                "CPU Fan",
                "CPU OPT",
                "Chassis Fan 1",
                "Chassis Fan 2",
                "Chassis Fan 3",
                "AIO Pump",
                "Water Pump",
            ])
        }
        // ASUS ROG CROSSHAIR X870E HERO / APEX / EXTREME
        ("asus" | "asustek" | "asustekcomputerinc", p)
            if p.contains("x870e")
                && (p.contains("hero") || p.contains("apex") || p.contains("extreme")) =>
        {
            Some(&[
                "CPU Fan",
                "CPU OPT",
                "Chassis Fan 1",
                "Chassis Fan 2",
                "Chassis Fan 3",
                "AIO Pump",
                "Water Pump",
            ])
        }
        // ASUS TUF GAMING X870-PLUS WIFI
        ("asus" | "asustek" | "asustekcomputerinc", p)
            if p.contains("x870") && p.contains("tuf") =>
        {
            Some(&[
                "CPU Fan",
                "CPU OPT",
                "Chassis Fan 1",
                "Chassis Fan 2",
                "Chassis Fan 3",
                "AIO Pump",
                "Water Pump",
            ])
        }
        _ => None,
    }
}

/// Heuristic label for boards that aren't in the table. Matches the typical
/// silkscreen on modern ATX/μATX boards: CPU first, then CPU OPT (if available),
/// then chassis fans, then pumps on 7-channel chips.
fn heuristic_label(fan_count: u8, channel: u8) -> String {
    let n = fan_count;
    let ch = channel as usize;
    let chassis = |idx: usize| format!("Chassis Fan {idx}");
    match (n, ch) {
        // 7-channel NCT677xD
        (7, 0) => "CPU Fan".into(),
        (7, 1) => "CPU OPT".into(),
        (7, 5) => "AIO Pump".into(),
        (7, 6) => "Water Pump".into(),
        (7, i) => chassis(i - 1),
        // 8-channel NCT668x EC family
        (8, 0) => "CPU Fan".into(),
        (8, 1) => "Pump".into(),
        (8, i) => chassis(i - 1),
        // 5- and 6-channel chips (IT87xx, NCT6776F)
        (5 | 6, 0) => "CPU Fan".into(),
        (5 | 6, 1) => "CPU OPT".into(),
        (5 | 6, i) => chassis(i - 1),
        // 3-channel low-end (NCT610XD)
        (3, 0) => "CPU Fan".into(),
        (3, i) => chassis(i),
        // Anything else
        (_, 0) => "CPU Fan".into(),
        (_, i) => format!("Fan {}", i + 1),
    }
}

/// Compute a friendly label for `channel` on `chip`, using a known-board
/// table where possible and a heuristic fallback otherwise.
pub fn fan_label(board: &BoardInfo, chip: DetectedChip, channel: u8) -> String {
    if let Some(labels) = known_board_labels(board) {
        if let Some(label) = labels.get(channel as usize) {
            return (*label).to_string();
        }
    }
    heuristic_label(chip.fan_count(), channel)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::generic::devices::superio::{nct677x, DetectedChip};

    fn fake_chip(fan_count_target: u8) -> DetectedChip {
        // Pick a variant whose fan_count matches what we want.
        let v = match fan_count_target {
            3 => nct677x::Nct677xVariant::Nct610Xd,
            5 => nct677x::Nct677xVariant::Nct6776F,
            7 => nct677x::Nct677xVariant::Nct6796DR,
            8 => nct677x::Nct677xVariant::Nct6687D,
            _ => nct677x::Nct677xVariant::Nct6796D,
        };
        DetectedChip::Nct677x(nct677x::Detected {
            probe_port: 0x2E,
            variant: v,
            hwm_base: 0x0290,
        })
    }

    #[test]
    fn heuristic_7_channel_layout() {
        let c = fake_chip(7);
        assert_eq!(heuristic_label(c.fan_count(), 0), "CPU Fan");
        assert_eq!(heuristic_label(c.fan_count(), 1), "CPU OPT");
        assert_eq!(heuristic_label(c.fan_count(), 2), "Chassis Fan 1");
        assert_eq!(heuristic_label(c.fan_count(), 4), "Chassis Fan 3");
        assert_eq!(heuristic_label(c.fan_count(), 5), "AIO Pump");
        assert_eq!(heuristic_label(c.fan_count(), 6), "Water Pump");
    }

    #[test]
    fn heuristic_8_channel_ec() {
        let c = fake_chip(8);
        assert_eq!(heuristic_label(c.fan_count(), 0), "CPU Fan");
        assert_eq!(heuristic_label(c.fan_count(), 1), "Pump");
        assert_eq!(heuristic_label(c.fan_count(), 2), "Chassis Fan 1");
        assert_eq!(heuristic_label(c.fan_count(), 7), "Chassis Fan 6");
    }

    #[test]
    fn board_key_is_separator_and_case_insensitive() {
        let a = BoardInfo {
            manufacturer: "ASUSTeK COMPUTER INC.".into(),
            product: "ProArt X870E-CREATOR WIFI".into(),
        };
        let b = BoardInfo {
            manufacturer: "ASUSTeK computer inc".into(),
            product: "proart-x870e-creator-wifi".into(),
        };
        assert_eq!(a.key(), b.key());
    }

    #[test]
    fn known_board_resolves_asus_proart_x870e() {
        let b = BoardInfo {
            manufacturer: "ASUSTeK COMPUTER INC.".into(),
            product: "ProArt X870E-CREATOR WIFI".into(),
        };
        let labels = known_board_labels(&b).expect("ProArt X870E should resolve");
        assert_eq!(labels[0], "CPU Fan");
        assert_eq!(labels[5], "AIO Pump");
    }

    #[test]
    fn unknown_board_falls_back_to_heuristic() {
        let b = BoardInfo {
            manufacturer: "Some OEM".into(),
            product: "Unknown board".into(),
        };
        assert_eq!(fan_label(&b, fake_chip(7), 0), "CPU Fan");
        assert_eq!(fan_label(&b, fake_chip(7), 5), "AIO Pump");
    }
}
