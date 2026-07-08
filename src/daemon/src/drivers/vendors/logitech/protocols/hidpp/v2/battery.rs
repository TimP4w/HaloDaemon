// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! Battery features: UNIFIED_BATTERY (`0x1004`, percentage) and the voltage
//! features ADC_MEASUREMENT (`0x1F20`) / BATTERY_VOLTAGE (`0x1001`) that report
//! a raw cell voltage mapped to a percentage via a discharge curve.
//!
//! The protocol owns source detection and decoding; the device only decides how
//! often to re-read (the voltage features emit no change notifications).
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use super::{feature, Hidpp20};

const UNIFIED_GET_STATUS: u8 = 0x10;
const VOLTAGE_GET: u8 = 0x00;

/// Where a device reports its battery, resolved from the feature table.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum BatterySource {
    #[default]
    None,
    /// UNIFIED_BATTERY (percentage) at this feature index.
    Unified(u8),
    /// A voltage feature (ADC_MEASUREMENT / BATTERY_VOLTAGE) at this index.
    Voltage(u8),
}

/// A decoded battery reading.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BatteryReading {
    pub percent: u8,
    pub charging: bool,
}

/// Voltage→percentage calibration as `(percent, mV)` points in descending order.
const VOLTAGE_CURVE: &[(u8, u16)] = &[
    (100, 4186),
    (90, 4067),
    (80, 3989),
    (70, 3922),
    (60, 3859),
    (50, 3811),
    (40, 3778),
    (30, 3751),
    (20, 3717),
    (10, 3671),
    (5, 3646),
    (2, 3579),
    (0, 3500),
];

/// Decode a battery voltage reply into `(percent, charging)`. Voltage is a
/// big-endian u16 (mV) in bytes 0–1; byte 2 == `0x03` means charging (`0x01` =
/// discharging). Shared by ADC_MEASUREMENT (`0x1F20`) and BATTERY_VOLTAGE
/// (`0x1001`), which report the same layout.
pub fn parse_battery_voltage(reply: &[u8]) -> Option<(u8, bool)> {
    if reply.len() < 2 {
        return None;
    }
    let mv = u16::from_be_bytes([reply[0], reply[1]]);
    if mv == 0 {
        return None; // headset asleep / no reading
    }
    let charging = reply.get(2).copied() == Some(0x03);
    Some((voltage_to_percent(mv), charging))
}

/// Piecewise-linear interpolation of a cell voltage (mV) to a 0–100% charge.
pub fn voltage_to_percent(mv: u16) -> u8 {
    let first = VOLTAGE_CURVE[0];
    let last = VOLTAGE_CURVE[VOLTAGE_CURVE.len() - 1];
    if mv >= first.1 {
        return first.0;
    }
    if mv <= last.1 {
        return last.0;
    }
    for w in VOLTAGE_CURVE.windows(2) {
        let (hi_pct, hi_mv) = w[0];
        let (lo_pct, lo_mv) = w[1];
        if mv <= hi_mv && mv >= lo_mv {
            let span = (hi_mv - lo_mv) as u32;
            let frac = (mv - lo_mv) as u32;
            let pct = lo_pct as u32 + (hi_pct - lo_pct) as u32 * frac / span;
            return pct as u8;
        }
    }
    last.0
}

impl Hidpp20 {
    /// Resolve the device's battery source. UNIFIED (percentage) is preferred;
    /// the voltage features are the fallback for cell-voltage-only devices.
    pub fn battery_source(&self) -> BatterySource {
        if let Some(idx) = self.idx(feature::UNIFIED_BATTERY) {
            BatterySource::Unified(idx)
        } else if let Some(idx) = self.idx(feature::ADC_MEASUREMENT) {
            BatterySource::Voltage(idx)
        } else if let Some(idx) = self.idx(feature::BATTERY_VOLTAGE) {
            BatterySource::Voltage(idx)
        } else {
            BatterySource::None
        }
    }

    /// Read the battery for a resolved source. UNIFIED returns a percentage
    /// directly; the voltage features decode a cell voltage. `None` when asleep,
    /// unsupported, or the read fails.
    pub async fn read_battery(&self, source: BatterySource) -> Option<BatteryReading> {
        match source {
            BatterySource::None => None,
            BatterySource::Unified(idx) => match self.call(idx, UNIFIED_GET_STATUS, &[]).await {
                Ok(reply) if !reply.is_empty() => Some(BatteryReading {
                    percent: reply[0].min(100),
                    charging: reply.get(2).copied().unwrap_or(0) != 0,
                }),
                Ok(_) => None,
                Err(e) => {
                    log::debug!("[HID++2.0] UNIFIED_BATTERY failed: {e}");
                    None
                }
            },
            BatterySource::Voltage(idx) => match self.call(idx, VOLTAGE_GET, &[0x00]).await {
                Ok(reply) => parse_battery_voltage(&reply)
                    .map(|(percent, charging)| BatteryReading { percent, charging }),
                Err(e) => {
                    log::debug!("[HID++2.0] battery voltage read failed: {e}");
                    None
                }
            },
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn voltage_to_percent_endpoints_and_interpolation() {
        assert_eq!(voltage_to_percent(4200), 100); // above max
        assert_eq!(voltage_to_percent(4186), 100); // at max
        assert_eq!(voltage_to_percent(3500), 0); // at min
        assert_eq!(voltage_to_percent(3000), 0); // below min
        assert_eq!(voltage_to_percent(4067), 90);
        assert_eq!(voltage_to_percent(4028), 85);
        let mut prev = 0;
        for mv in (3500..=4186).step_by(10) {
            let p = voltage_to_percent(mv);
            assert!(p >= prev, "non-monotonic at {mv} mV");
            prev = p;
        }
    }

    #[test]
    fn parse_battery_voltage_decodes_mv_and_charging() {
        let (pct, charging) = parse_battery_voltage(&[0x0F, 0x66, 0x03]).unwrap();
        assert_eq!(pct, voltage_to_percent(3942));
        assert!(charging);
        let (_, charging) = parse_battery_voltage(&[0x0F, 0x66, 0x01]).unwrap();
        assert!(!charging);
        assert!(parse_battery_voltage(&[0x0F]).is_none());
        // 0 mV means the headset is asleep — no reading.
        assert!(parse_battery_voltage(&[0x00, 0x00, 0x01]).is_none());
    }
}
