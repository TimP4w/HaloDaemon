// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! RGB lighting features: RGB_EFFECTS (`0x8071`), COLOR_LED_EFFECTS (`0x8070`),
//! PER_KEY_LIGHTING_V2 (`0x8081`) and the KEYBOARD_LAYOUT_2 (`0x4540`)
//! country-code read.
//!
//! Codecs live in [`effects`] (firmware effects, static block, reply parsers),
//! [`color_led`] (0x8070 zone/effect codecs), and [`per_key`] (the streaming
//! frame encoder); the typed [`Hidpp20`] operations that drive them live here.
//! Device-agnostic.
use std::collections::HashMap;

use anyhow::Result;

use halod_shared::types::{EffectParamValue, KeyboardLayout, RgbColor};

use super::{feature, Hidpp20};

pub mod color_led;
pub mod effects;
pub mod per_key;

pub use effects::find_native_effect;
pub use per_key::PkFrameCache;

// ── RGB_EFFECTS function codes ────────────────────────────────────────────────
const RGB_GET_INFO: u8 = 0x00;
const RGB_SET_EFFECT: u8 = 0x10;
const RGB_SET_SW_CONTROL: u8 = 0x50;

// ── COLOR_LED_EFFECTS function codes ─────────────────────────────────────────
const CLED_GET_INFO: u8 = 0x00;
const CLED_GET_ZONE_INFO: u8 = 0x10;
const CLED_GET_ZONE_EFFECT_INFO: u8 = 0x20;
const CLED_SET_EFFECT: u8 = 0x30;
const CLED_SET_SW_CONTROL: u8 = 0x80;

impl Hidpp20 {
    /// Read the keyboard country code via KEYBOARD_LAYOUT_2 (`0x4540`).
    pub async fn read_keyboard_layout(&self) -> KeyboardLayout {
        let Some(idx) = self.idx(feature::KEYBOARD_LAYOUT_2) else {
            return KeyboardLayout::Unknown;
        };
        match self.call(idx, 0x00, &[]).await {
            Ok(r) if !r.is_empty() => match r[0] {
                1 => KeyboardLayout::US,
                13 => KeyboardLayout::CH,
                14 => KeyboardLayout::IT,
                other => {
                    log::info!("[HID++2.0] unknown keyboard layout country code {other}");
                    KeyboardLayout::Unknown
                }
            },
            Ok(_) => KeyboardLayout::Unknown,
            Err(e) => {
                log::warn!("[HID++2.0] KEYBOARD_LAYOUT_2 failed: {e}");
                KeyboardLayout::Unknown
            }
        }
    }

    /// Total number of RGB_EFFECTS lighting zones (GetInfo global). `None` on
    /// error or malformed reply.
    pub async fn rgb_zone_count(&self) -> Option<u8> {
        let idx = self.idx(feature::RGB_EFFECTS)?;
        match self.call(idx, RGB_GET_INFO, &[0xFF, 0xFF, 0x00]).await {
            Ok(r) => effects::parse_rgb_zone_count(&r),
            Err(e) => {
                log::warn!("[HID++2.0] RGB GetInfo(global) failed: {e}");
                None
            }
        }
    }

    /// Number of effect-table entries for zone `z` (GetInfo zone reply byte 4).
    pub async fn rgb_zone_effect_count(&self, z: u8) -> u8 {
        let Some(idx) = self.idx(feature::RGB_EFFECTS) else {
            return 0;
        };
        match self.call(idx, RGB_GET_INFO, &[z, 0xFF, 0x00]).await {
            Ok(r) => r.get(4).copied().unwrap_or(0),
            Err(e) => {
                log::warn!("[HID++2.0] RGB GetInfo(zone={z}) failed: {e}");
                0
            }
        }
    }

    /// The effect id stored in zone `z` slot `slot`, or `None`.
    pub async fn rgb_effect_id(&self, z: u8, slot: u8) -> Option<u16> {
        let idx = self.idx(feature::RGB_EFFECTS)?;
        match self.call(idx, RGB_GET_INFO, &[z, slot, 0x00]).await {
            Ok(r) => effects::parse_rgb_effect_table_entry(&r),
            Err(e) => {
                log::warn!("[HID++2.0] RGB GetInfo(zone={z}, slot={slot}) failed: {e}");
                None
            }
        }
    }

    /// Apply a static colour to one zone/slot via RGB_EFFECTS SetEffect.
    pub async fn rgb_set_static_effect(
        &self,
        zone_idx: u8,
        slot: u8,
        color: RgbColor,
    ) -> Result<()> {
        let idx = self
            .idx(feature::RGB_EFFECTS)
            .ok_or_else(|| anyhow::anyhow!("No RGB_EFFECTS feature"))?;
        self.call(
            idx,
            RGB_SET_EFFECT,
            &effects::encode_set_effect_static(zone_idx, slot, color),
        )
        .await?;
        Ok(())
    }

    /// Apply a native firmware effect (id resolved from the `NATIVE_EFFECTS`
    /// table; `values` overlaid onto its base block). `base[0] = 0xFF` addresses
    /// all zones.
    pub async fn rgb_set_native_effect(
        &self,
        id: &str,
        values: &HashMap<String, EffectParamValue>,
    ) -> Result<()> {
        let idx = self
            .idx(feature::RGB_EFFECTS)
            .ok_or_else(|| anyhow::anyhow!("No RGB_EFFECTS feature"))?;
        let block = find_native_effect(id)
            .map(|e| e.encode(values))
            .ok_or_else(|| anyhow::anyhow!("Unknown native effect: {id}"))?;
        self.call(idx, RGB_SET_EFFECT, &block)
            .await
            .map_err(|e| e.context(format!("native effect '{id}' rejected by device")))?;
        Ok(())
    }

    /// Re-enable software LED control via RGB_EFFECTS func 0x50 `[0x01, 0x01]`.
    /// Best-effort; no-op (Ok) when the device lacks RGB_EFFECTS.
    pub async fn rgb_enable_sw_control(&self) {
        if let Some(idx) = self.idx(feature::RGB_EFFECTS) {
            let _ = self.call(idx, RGB_SET_SW_CONTROL, &[0x01, 0x01]).await;
        }
    }

    // ── COLOR_LED_EFFECTS (0x8070) operations ───────────────────────────────

    /// Total number of COLOR_LED_EFFECTS lighting zones (GetInfo fn 0x00).
    pub async fn color_led_zone_count(&self) -> Option<u8> {
        let idx = self.idx(feature::COLOR_LED_EFFECTS)?;
        match self.call(idx, CLED_GET_INFO, &[]).await {
            Ok(r) => color_led::parse_color_led_zone_count(&r),
            Err(e) => {
                log::warn!("[HID++2.0] COLOR_LED GetInfo failed: {e}");
                None
            }
        }
    }

    /// Zone index, location, and effect count for zone `z` (GetZoneInfo fn 0x10).
    /// Returns `(zone_index, location, effect_count)`.
    pub async fn color_led_zone_info(&self, z: u8) -> Option<(u8, u16, u8)> {
        let idx = self.idx(feature::COLOR_LED_EFFECTS)?;
        match self.call(idx, CLED_GET_ZONE_INFO, &[z, 0xFF, 0x00]).await {
            Ok(r) => color_led::parse_color_led_zone_info(&r),
            Err(e) => {
                log::warn!("[HID++2.0] COLOR_LED GetZoneInfo(z={z}) failed: {e}");
                None
            }
        }
    }

    /// Effect id stored in zone `z` slot `slot` (GetZoneEffectInfo fn 0x20).
    pub async fn color_led_effect_id(&self, z: u8, slot: u8) -> Option<u16> {
        let idx = self.idx(feature::COLOR_LED_EFFECTS)?;
        match self
            .call(idx, CLED_GET_ZONE_EFFECT_INFO, &[z, slot, 0x00])
            .await
        {
            Ok(r) => color_led::parse_color_led_effect_entry(&r),
            Err(e) => {
                log::warn!(
                    "[HID++2.0] COLOR_LED GetZoneEffectInfo(z={z}, slot={slot}) failed: {e}"
                );
                None
            }
        }
    }

    /// Apply a static colour to one zone via COLOR_LED_EFFECTS SetEffect (fn 0x30).
    pub async fn color_led_set_static_effect(
        &self,
        zone_idx: u8,
        slot: u8,
        color: RgbColor,
    ) -> Result<()> {
        let idx = self
            .idx(feature::COLOR_LED_EFFECTS)
            .ok_or_else(|| anyhow::anyhow!("No COLOR_LED_EFFECTS feature"))?;
        let payload = color_led::encode_color_led_set_effect_static(zone_idx, slot, color);
        log::debug!(
            "COLOR_LED SetEffect idx={idx} fn=0x30 zone_idx={zone_idx:#04x} slot={slot} color=({:02x},{:02x},{:02x}) payload={:02x?}",
            color.r, color.g, color.b,
            &payload[..]
        );
        self.call(idx, CLED_SET_EFFECT, &payload).await?;
        Ok(())
    }

    /// Claim software LED control via COLOR_LED_EFFECTS SetSWControl (fn 0x80).
    /// Best-effort; no-op when the device lacks COLOR_LED_EFFECTS.
    pub async fn color_led_enable_sw_control(&self) {
        if let Some(idx) = self.idx(feature::COLOR_LED_EFFECTS) {
            let _ = self.call(idx, CLED_SET_SW_CONTROL, &[0x01]).await;
        }
    }

    /// Paint the whole key range a single colour via PER_KEY_LIGHTING
    /// (SET_RANGE then COMMIT). Both calls are awaited so a rejection surfaces.
    pub async fn per_key_set_all(&self, color: RgbColor) -> Result<()> {
        let pk = self
            .idx(feature::PER_KEY_LIGHTING_V2)
            .ok_or_else(|| anyhow::anyhow!("No PER_KEY_LIGHTING feature"))?;
        self.call(pk, 0x50, &[0x00, 0xFF, color.r, color.g, color.b])
            .await?;
        self.call(pk, 0x70, &[0x00]).await?;
        Ok(())
    }

    /// Discover a mouse's per-key LED ids by reading the 3-page (×13 byte)
    /// PER_KEY_LIGHTING bitmap. Returns the low-range firmware ids (1..31).
    pub async fn read_pk_led_ids(&self) -> Vec<u8> {
        let Some(pk) = self.idx(feature::PER_KEY_LIGHTING_V2) else {
            return Vec::new();
        };
        let mut bitmap = vec![0u8; 39]; // 3 pages × 13 bytes
        for page in 0u8..3 {
            if let Ok(r) = self.call(pk, RGB_GET_INFO, &[0x00, 0x00, page]).await {
                if r.len() >= 15 {
                    let start = page as usize * 13;
                    bitmap[start..start + 13].copy_from_slice(&r[2..15]);
                }
            }
        }
        effects::parse_pk_led_bitmap(&bitmap)
    }

    /// Encode a streaming per-key animation frame (diffed against `cache`).
    /// Returns no packets when nothing changed.
    pub fn encode_per_key_frame(
        &self,
        keys: &[(u8, RgbColor)],
        cache: &mut PkFrameCache,
    ) -> Vec<Vec<u8>> {
        let Some(pk) = self.idx(feature::PER_KEY_LIGHTING_V2) else {
            return Vec::new();
        };
        per_key::encode_frame(keys, cache, self.devnum, pk)
    }

    /// Write explicit per-key colour pairs via PER_KEY_LIGHTING setIndividual
    /// batches + commit.
    pub async fn write_per_key_pairs(&self, pairs: &[(u8, u8, u8, u8)]) -> Result<()> {
        let Some(pk) = self.idx(feature::PER_KEY_LIGHTING_V2) else {
            return Ok(());
        };
        if pairs.is_empty() {
            return Ok(());
        }
        self.send_packets(per_key::encode_individual_pairs(pairs, self.devnum, pk))
            .await
    }

    /// Fire a batch of pre-built PER_KEY packets (no response awaited).
    pub async fn send_packets(&self, packets: Vec<Vec<u8>>) -> Result<()> {
        self.msg.feature_send_many_fire(packets).await
    }

    /// This device's HID++ device number — used by the per-receiver write
    /// coordinator to key concurrent posts.
    pub fn devnum(&self) -> u8 {
        self.devnum
    }
}
