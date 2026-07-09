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
}
