// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! HID++ 2.0 — the feature-based protocol.
//!
//! Modern Logitech devices expose numbered *features* (battery, RGB, DPI, …)
//! discovered at runtime via ROOT (`0x0000`) / FEATURE_SET (`0x0001`). This
//! module owns the feature address space and the typed [`Hidpp20`] handle; the
//! byte-level codecs live in the [`audio`], [`battery`], [`keys`], [`rgb`] and
//! [`settings`] submodules.
//!
//! Reference: Solaar (GPL-2.0-or-later) — hidpp20.py
use std::collections::HashMap;
use std::sync::Arc;

use anyhow::Result;

use super::HidppChannel;

pub mod audio;
pub mod battery;
pub mod keys;
pub mod rgb;
pub mod settings;

/// HID++ 2.0 feature codes (16-bit). A device advertises a subset; each present
/// feature is assigned a runtime *index* by FEATURE_SET.
pub mod feature {
    pub const ROOT: u16 = 0x0000;
    pub const FEATURE_SET: u16 = 0x0001;
    pub const FIRMWARE_VERSION: u16 = 0x0003;
    pub const DEVICE_NAME: u16 = 0x0005;
    /// Friendlier display name (newer devices, e.g. MX Keys). Falls back to
    /// `DEVICE_NAME` when absent.
    pub const DEVICE_FRIENDLY_NAME: u16 = 0x0007;
    /// Voltage-reporting battery (older gaming devices). Returns a raw cell
    /// voltage that maps to a percentage via a per-device curve.
    pub const BATTERY_VOLTAGE: u16 = 0x1001;
    pub const UNIFIED_BATTERY: u16 = 0x1004;
    /// ADC battery measurement (LIGHTSPEED headsets — G PRO X, G733, G935).
    /// Like `BATTERY_VOLTAGE` it reports a cell voltage + charge status.
    pub const ADC_MEASUREMENT: u16 = 0x1F20;
    pub const ADJUSTABLE_DPI: u16 = 0x2201;
    #[allow(dead_code)]
    pub const EXTENDED_ADJUSTABLE_DPI: u16 = 0x2202;
    pub const REPORT_RATE: u16 = 0x8060;
    pub const EXT_REPORT_RATE: u16 = 0x8061;
    pub const RGB_EFFECTS: u16 = 0x8071;
    pub const COLOR_LED_EFFECTS: u16 = 0x8070;
    pub const PER_KEY_LIGHTING_V2: u16 = 0x8081;
    pub const ONBOARD_PROFILES: u16 = 0x8100;
    pub const KEYBOARD_LAYOUT_2: u16 = 0x4540;
    pub const REPROG_CONTROLS_V4: u16 = 0x1b04;
    /// G-key / macro-key divert (gaming keyboards & mice).
    pub const GKEY: u16 = 0x8010;
    /// Lists the device's controls; used by newer remap-capable devices.
    #[allow(dead_code)]
    pub const CONTROL_LIST: u16 = 0x1b10;
    /// Per-control HID-usage reporting / remap (newer keyboards & mice).
    #[allow(dead_code)]
    pub const REPORT_HID_USAGE: u16 = 0x1bc0;
    /// Full key customization (analog / hot-swap keyboards).
    #[allow(dead_code)]
    pub const FULL_KEY_CUSTOMIZATION: u16 = 0x1b05;
    /// Host-side raw mouse-button reporting (gaming mice without REPROG_CONTROLS).
    pub const MOUSE_BUTTON_SPY: u16 = 0x8110;
    /// Sidetone level (gaming headsets) — mic feedback into the earcups, 0–100.
    pub const SIDETONE: u16 = 0x8300;
    /// Graphic equalizer (gaming headsets) — N signed-dB bands.
    pub const EQUALIZER: u16 = 0x8310;
    /// Wireless device status (read-only). Reports connection info, link quality,
    /// etc. Used by many office mice/keyboards.
    pub const WIRELESS_DEVICE_STATUS: u16 = 0x1D4B;
    /// Hi-resolution scroll wheel. Mode (HID vs HID++), resolution (hi/lo),
    /// invert direction, and ratchet switch on supported devices.
    pub const HIRES_WHEEL: u16 = 0x2121;
    /// Fn-key inversion for K375s-family keyboards (MX Keys, Craft, etc.).
    /// Simple boolean: F-keys send media by default → Fn+F-key = F-key.
    pub const K375S_FN_INVERSION: u16 = 0x40A3;
    /// Keyboard backlight brightness (gaming keyboards). 0–max range, optional
    /// separate on/off toggle.
    pub const BRIGHTNESS_CONTROL: u16 = 0x8040;
}

/// A device's discovered feature codes mapped to their runtime indices.
pub type FeatureTable = HashMap<u16, u8>;

/// Typed HID++ 2.0 handle: a messenger bound to one device number plus that
/// device's feature table. Cheap to construct (an `Arc` clone + table clone) so
/// it composes with the device's snapshot-and-drop transport pattern.
///
/// The per-domain `impl Hidpp20` blocks in the submodules expose typed
/// operations; feature lookup and the raw feature call are kept crate-private so
/// the device never sees function bytes or reply slices.
#[derive(Clone)]
pub struct Hidpp20 {
    pub(crate) msg: Arc<dyn HidppChannel>,
    pub(crate) devnum: u8,
    pub(crate) features: FeatureTable,
}

/// Decoded BRIGHTNESS_CONTROL getInfo response.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct BrightnessInfo {
    pub min: u16,
    pub max: u16,
    pub has_on_off: bool,
    pub steps: u8,
}

// HIRES_WHEEL mode byte bits.
const HWM_INVERT: u8 = 0x04;
const HWM_HI_RES: u8 = 0x02;
const HWM_TARGET: u8 = 0x01;

/// Decoded HIRES_WHEEL (`0x2121`) capabilities + current mode.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct HiresWheel {
    pub multi: u8,
    pub has_invert: bool,
    pub has_ratchet: bool,
    pub invert: bool,
    pub hi_res: bool,
    pub hidpp_target: bool,
    pub ratchet: bool,
}

/// Decode HIRES_WHEEL getCapability (`0x00`), getMode (`0x10`) and getRatchet
/// (`0x30`) replies. `None` if any reply is too short.
fn parse_hires_wheel(caps: &[u8], mode: &[u8], ratchet: &[u8]) -> Option<HiresWheel> {
    if caps.len() < 2 || mode.len() < 2 || ratchet.len() < 2 {
        return None;
    }
    let flags = caps[1];
    let wheel_mode = mode[0];
    Some(HiresWheel {
        multi: caps[0],
        has_invert: (flags & 0x08) != 0,
        has_ratchet: (flags & 0x04) != 0,
        invert: (wheel_mode & HWM_INVERT) != 0,
        hi_res: (wheel_mode & HWM_HI_RES) != 0,
        hidpp_target: (wheel_mode & HWM_TARGET) != 0,
        ratchet: (ratchet[0] & 0x01) != 0,
    })
}

/// Decode BRIGHTNESS_CONTROL getInfo (`0x00`): `[max_be, steps_and_flags, caps,
/// min_be]`. `None` if the reply is too short.
fn parse_brightness_info(reply: &[u8]) -> Option<BrightnessInfo> {
    if reply.len() < 6 {
        return None;
    }
    let steps_and_flags = reply[2];
    let caps = reply[3];
    Some(BrightnessInfo {
        max: u16::from_be_bytes([reply[0], reply[1]]),
        min: u16::from_be_bytes([reply[4], reply[5]]),
        has_on_off: (caps & 0x04) != 0,
        steps: steps_and_flags & 0x0F,
    })
}

/// Decode K375S_FN_INVERSION getState (`0x00`): `(inverted, default_inverted)`
/// from the low bit of bytes 0 and 1. `None` if the reply is too short.
fn parse_fn_inversion(reply: &[u8]) -> Option<(bool, bool)> {
    if reply.len() < 2 {
        return None;
    }
    Some(((reply[0] & 0x01) != 0, (reply[1] & 0x01) != 0))
}

impl Hidpp20 {
    /// Bind a handle to an already-enumerated feature table.
    pub fn new(msg: Arc<dyn HidppChannel>, devnum: u8, features: FeatureTable) -> Self {
        Self {
            msg,
            devnum,
            features,
        }
    }

    /// Enumerate the device's features. Returns the table; the caller builds a
    /// handle with [`Hidpp20::new`] (the device caches it for reuse).
    pub async fn enumerate(msg: &Arc<dyn HidppChannel>, devnum: u8) -> Result<FeatureTable> {
        msg.enumerate_features(devnum).await
    }

    /// Whether the device advertises `feature_code`.
    pub fn has(&self, feature_code: u16) -> bool {
        self.features.contains_key(&feature_code)
    }

    /// Runtime index of a feature, or `None` when the device lacks it.
    pub(crate) fn idx(&self, feature_code: u16) -> Option<u8> {
        self.features.get(&feature_code).copied()
    }

    /// Call a feature function. `function` is the high-nibble byte (0x00, 0x10,
    /// …); the messenger stamps the software id. Crate-private — the device only
    /// sees the typed wrappers.
    pub(crate) async fn call(&self, idx: u8, function: u8, params: &[u8]) -> Result<Vec<u8>> {
        self.msg
            .feature_request(self.devnum, idx, function, params)
            .await
    }

    /// Read the device's advertised name via DEVICE_NAME (`0x0005`). `None` when
    /// the feature is absent (e.g. headsets) or the read yields nothing.
    pub async fn device_name(&self) -> Option<String> {
        let idx = self.idx(feature::DEVICE_NAME)?;
        // func 0x00 returns the name length; func 0x10 + offset returns up to 16
        // chars per chunk.
        let len = self
            .call(idx, 0x00, &[])
            .await
            .ok()
            .and_then(|r| r.first().copied())
            .unwrap_or(0) as usize;

        if len > u8::MAX as usize {
            return None;
        }
        let mut bytes = Vec::with_capacity(len);
        let mut offset: u16 = 0;
        while bytes.len() < len {
            match self.call(idx, 0x10, &[offset as u8]).await {
                Ok(r) => {
                    let take = (len - bytes.len()).min(r.len());
                    if take == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&r[..take]);
                    offset += take as u16;
                }
                Err(_) => break,
            }
        }
        let name = String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        (!name.is_empty()).then_some(name)
    }

    /// Read the device friendly name via DEVICE_FRIENDLY_NAME (`0x0007`).
    /// Same chunked-read pattern as [`device_name`] but used by newer devices
    /// (MX Keys, Craft, etc.). Falls back when `DEVICE_NAME` is absent.
    pub async fn device_friendly_name(&self) -> Option<String> {
        let idx = self.idx(feature::DEVICE_FRIENDLY_NAME)?;
        let len = self
            .call(idx, 0x00, &[])
            .await
            .ok()
            .and_then(|r| r.first().copied())
            .unwrap_or(0) as usize;

        if len > u8::MAX as usize || len == 0 {
            return None;
        }
        let mut bytes = Vec::with_capacity(len);
        while bytes.len() < len {
            match self.call(idx, 0x10, &[(bytes.len()) as u8]).await {
                Ok(r) => {
                    let take = (len - bytes.len()).min(r.len().saturating_sub(1));
                    if take == 0 {
                        break;
                    }
                    bytes.extend_from_slice(&r[1..1 + take]);
                }
                Err(_) => break,
            }
        }
        let name = String::from_utf8_lossy(&bytes)
            .trim_end_matches('\0')
            .trim()
            .to_string();
        (!name.is_empty()).then_some(name)
    }

    // ── HIRES_WHEEL (0x2121) ─────────────────────────────────────────────────

    /// Read the HIRES_WHEEL caps + current mode.
    pub async fn read_hires_wheel(&self) -> Option<HiresWheel> {
        let idx = self.idx(feature::HIRES_WHEEL)?;
        let caps = self.call(idx, 0x00, &[]).await.ok()?;
        let mode = self.call(idx, 0x10, &[]).await.ok()?;
        let ratchet = self.call(idx, 0x30, &[]).await.ok()?;
        parse_hires_wheel(&caps, &mode, &ratchet)
    }

    /// Write HIRES_WHEEL mode byte via read-modify-write.
    pub(crate) async fn set_hires_wheel_mode(&self, mask: u8, value: u8) -> Result<()> {
        let idx = self
            .idx(feature::HIRES_WHEEL)
            .ok_or_else(|| anyhow::anyhow!("HIRES_WHEEL not available"))?;
        let current = self.call(idx, 0x10, &[]).await?;
        let mut new_mode = current.first().copied().unwrap_or(0);
        new_mode &= !mask;
        new_mode |= value & mask;
        self.call(idx, 0x20, &[new_mode, 0x00]).await?;
        Ok(())
    }

    pub async fn set_hires_invert(&self, invert: bool) -> Result<()> {
        self.set_hires_wheel_mode(HWM_INVERT, if invert { HWM_INVERT } else { 0 })
            .await
    }

    pub async fn set_hires_resolution(&self, hi_res: bool) -> Result<()> {
        self.set_hires_wheel_mode(HWM_HI_RES, if hi_res { HWM_HI_RES } else { 0 })
            .await
    }

    pub async fn set_hires_diversion(&self, hidpp_target: bool) -> Result<()> {
        self.set_hires_wheel_mode(HWM_TARGET, if hidpp_target { HWM_TARGET } else { 0 })
            .await
    }

    // ── K375S_FN_INVERSION (0x40A3) ──────────────────────────────────────────

    /// Read K375S_FN_INVERSION: returns `(inverted, default_inverted)`.
    pub async fn read_fn_inversion(&self) -> Option<(bool, bool)> {
        let idx = self.idx(feature::K375S_FN_INVERSION)?;
        let reply = self.call(idx, 0x00, &[]).await.ok()?;
        parse_fn_inversion(&reply)
    }

    /// Write K375S_FN_INVERSION: set the inversion state (true = F-keys send
    /// media by default).
    pub async fn set_fn_inversion(&self, inverted: bool) -> Result<()> {
        let idx = self
            .idx(feature::K375S_FN_INVERSION)
            .ok_or_else(|| anyhow::anyhow!("K375S_FN_INVERSION not available"))?;
        self.call(idx, 0x10, &[if inverted { 0x01 } else { 0x00 }])
            .await?;
        Ok(())
    }

    /// If `sub_id` matches the fn_inversion feature, return the raw inverted
    /// bit from the notification data. `None` when this notification is not
    /// ours or data is too short.
    pub fn handle_fn_inversion_notif(&self, sub_id: u8, data: &[u8]) -> Option<bool> {
        if self.idx(feature::K375S_FN_INVERSION)? != sub_id {
            return None;
        }
        let inverted = (*data.get(1)? & 0x01) != 0;
        Some(inverted)
    }

    /// K375S_FN_INVERSION version. `None` if the feature is absent. Version ≥ 2
    /// supports SET; version 1 is read-only.
    pub async fn fn_inversion_version(&self) -> Option<u8> {
        let idx = self.idx(feature::K375S_FN_INVERSION)?;
        let fs = self.idx(feature::FEATURE_SET)?;
        let reply = self.call(fs, 0x10, &[idx]).await.ok()?;
        reply.get(3).copied()
    }

    // ── BRIGHTNESS_CONTROL (0x8040) ──────────────────────────────────────────

    /// Read BRIGHTNESS_CONTROL getInfo.
    pub async fn read_brightness_info(&self) -> Option<BrightnessInfo> {
        let idx = self.idx(feature::BRIGHTNESS_CONTROL)?;
        let reply = self.call(idx, 0x00, &[]).await.ok()?;
        parse_brightness_info(&reply)
    }

    /// Read current brightness level (0–max).
    pub async fn read_brightness(&self) -> Option<u16> {
        let idx = self.idx(feature::BRIGHTNESS_CONTROL)?;
        let reply = self.call(idx, 0x10, &[]).await.ok()?;
        if reply.len() < 2 {
            return None;
        }
        Some(u16::from_be_bytes([reply[0], reply[1]]))
    }

    /// Set brightness level (0–max).
    pub async fn set_brightness(&self, value: u16) -> Result<()> {
        let idx = self
            .idx(feature::BRIGHTNESS_CONTROL)
            .ok_or_else(|| anyhow::anyhow!("BRIGHTNESS_CONTROL not available"))?;
        self.call(idx, 0x20, &value.to_be_bytes()).await?;
        Ok(())
    }

    /// Read BRIGHTNESS_CONTROL on/off state. Only valid when `has_on_off` is true.
    pub async fn read_brightness_on_off(&self) -> Option<bool> {
        let idx = self.idx(feature::BRIGHTNESS_CONTROL)?;
        let reply = self.call(idx, 0x30, &[]).await.ok()?;
        reply.first().map(|&b| (b & 0x01) != 0)
    }

    /// Set BRIGHTNESS_CONTROL on/off. Only valid when `has_on_off` is true.
    pub async fn set_brightness_on_off(&self, on: bool) -> Result<()> {
        let idx = self
            .idx(feature::BRIGHTNESS_CONTROL)
            .ok_or_else(|| anyhow::anyhow!("BRIGHTNESS_CONTROL not available"))?;
        self.call(idx, 0x40, &[if on { 0x01 } else { 0x00 }])
            .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::feature;
    use super::Hidpp20;
    use crate::drivers::vendors::logitech::protocols::hidpp::test_util::MockHidppChannel;
    use std::collections::HashMap;
    use std::sync::Arc;

    fn hidpp_with_features(features: HashMap<u16, u8>) -> Hidpp20 {
        let ch = Arc::new(MockHidppChannel::new(HashMap::new()));
        Hidpp20::new(ch, 0xff, features)
    }

    #[test]
    fn fn_inversion_notif_parses_data_byte_1() {
        let h = hidpp_with_features(HashMap::from([(feature::K375S_FN_INVERSION, 0x0b)]));
        // data[1]=0x01 → inverted=true (raw device bit: F-keys send media)
        assert_eq!(h.handle_fn_inversion_notif(0x0b, &[0x00, 0x01]), Some(true));
        // data[1]=0x00 → inverted=false (F-keys send F1-F12)
        assert_eq!(
            h.handle_fn_inversion_notif(0x0b, &[0x00, 0x00]),
            Some(false)
        );
    }

    #[test]
    fn fn_inversion_notif_filters_by_sub_id() {
        let h = hidpp_with_features(HashMap::from([(feature::K375S_FN_INVERSION, 0x0b)]));
        assert_eq!(h.handle_fn_inversion_notif(0x0c, &[0x00, 0x01]), None);
    }

    #[test]
    fn fn_inversion_notif_absent_when_feature_missing() {
        let h = hidpp_with_features(HashMap::new());
        assert_eq!(h.handle_fn_inversion_notif(0x0b, &[0x00, 0x01]), None);
    }

    #[test]
    fn fn_inversion_notif_short_data_returns_none() {
        let h = hidpp_with_features(HashMap::from([(feature::K375S_FN_INVERSION, 0x0b)]));
        assert_eq!(h.handle_fn_inversion_notif(0x0b, &[0x00]), None);
    }

    // ── parse_hires_wheel ─────────────────────────────────────────────────

    #[test]
    fn hires_wheel_decodes_caps_and_mode_bits() {
        // caps: multi=8, flags=0x0C → has_invert (0x08) + has_ratchet (0x04).
        // mode: 0x07 → target|hi_res|invert all set. ratchet: 0x01 → engaged.
        let hw = super::parse_hires_wheel(&[0x08, 0x0C], &[0x07, 0x00], &[0x01, 0x00]).unwrap();
        assert_eq!(
            hw,
            super::HiresWheel {
                multi: 8,
                has_invert: true,
                has_ratchet: true,
                invert: true,
                hi_res: true,
                hidpp_target: true,
                ratchet: true,
            }
        );
    }

    #[test]
    fn hires_wheel_clears_bits_when_zero() {
        // flags=0, mode=0, ratchet=0 → every capability/mode bit false.
        let hw = super::parse_hires_wheel(&[0x01, 0x00], &[0x00, 0x00], &[0x00, 0x00]).unwrap();
        assert!(!hw.has_invert && !hw.has_ratchet && !hw.invert && !hw.hi_res);
        assert!(!hw.hidpp_target && !hw.ratchet);
        assert_eq!(hw.multi, 1);
    }

    #[test]
    fn hires_wheel_short_reply_is_none() {
        assert_eq!(
            super::parse_hires_wheel(&[0x08], &[0x07, 0], &[0x01, 0]),
            None
        );
        assert_eq!(
            super::parse_hires_wheel(&[0x08, 0x0C], &[0x07], &[0x01, 0]),
            None
        );
        assert_eq!(
            super::parse_hires_wheel(&[0x08, 0x0C], &[0x07, 0], &[]),
            None
        );
    }

    // ── parse_brightness_info ─────────────────────────────────────────────

    #[test]
    fn brightness_info_decodes_be_fields() {
        // max=0x0258 (600), steps_and_flags=0x0A, caps=0x04 (on_off), min=0x0032 (50).
        let info = super::parse_brightness_info(&[0x02, 0x58, 0x0A, 0x04, 0x00, 0x32]).unwrap();
        assert_eq!(info.max, 600);
        assert_eq!(info.min, 50);
        assert_eq!(info.steps, 0x0A);
        assert!(info.has_on_off);
    }

    #[test]
    fn brightness_info_no_on_off_when_cap_clear() {
        let info = super::parse_brightness_info(&[0x00, 0xFF, 0x00, 0x00, 0x00, 0x00]).unwrap();
        assert!(!info.has_on_off);
        assert_eq!(info.max, 255);
    }

    #[test]
    fn brightness_info_short_reply_is_none() {
        assert_eq!(
            super::parse_brightness_info(&[0x02, 0x58, 0x0A, 0x04, 0x00]),
            None
        );
    }

    // ── parse_fn_inversion ────────────────────────────────────────────────

    #[test]
    fn fn_inversion_decodes_low_bits() {
        assert_eq!(
            super::parse_fn_inversion(&[0x01, 0x00]),
            Some((true, false))
        );
        assert_eq!(
            super::parse_fn_inversion(&[0x00, 0x01]),
            Some((false, true))
        );
        // only the low bit matters.
        assert_eq!(
            super::parse_fn_inversion(&[0xFE, 0xFF]),
            Some((false, true))
        );
    }

    #[test]
    fn fn_inversion_short_reply_is_none() {
        assert_eq!(super::parse_fn_inversion(&[0x01]), None);
    }
}
