// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: 2012-2013 Daniel Pavel
// SPDX-FileCopyrightText: 2014-2024 Solaar Contributors <https://pwr-solaar.github.io/Solaar/>
//! Audio capabilities for `LogitechDevice` — EQUALIZER (`0x8310`) and SIDETONE
//! (`0x8300`), present on LIGHTSPEED gaming headsets. `init_audio` reads them at
//! startup; the `EqualizerCapability` / `RangeCapability` impls drive them. All
//! wire work lives in the protocol ([`Hidpp20`] audio ops).

use std::collections::HashMap;

use anyhow::Result;
use async_trait::async_trait;

use crate::drivers::vendors::logitech::devices::generic::device::LogitechDevice;
use crate::drivers::vendors::logitech::devices::generic::state::LogitechDeviceState;
use crate::drivers::vendors::logitech::protocols::hidpp::feature;
use crate::drivers::vendors::logitech::protocols::hidpp::v2::audio::EqReading;
use crate::drivers::{EqualizerCapability, RangeCapability, RangeStateCache};
use halod_shared::types::{DeviceCapability, EqBand, EqPreset, Equalizer, Range};

/// "31 Hz" / "1 kHz" label for an EQ band centre frequency.
fn freq_label(hz: u16) -> String {
    if hz >= 1000 && hz.is_multiple_of(1000) {
        format!("{} kHz", hz / 1000)
    } else if hz >= 1000 {
        format!("{:.1} kHz", hz as f32 / 1000.0)
    } else {
        format!("{hz} Hz")
    }
}

fn build_equalizer(eq: &EqReading) -> Option<Equalizer> {
    let info = eq.info?;
    let bands = (0..info.count as usize)
        .map(|i| {
            let hz = eq.freqs.get(i).copied();
            EqBand {
                index: i,
                label: hz
                    .map(freq_label)
                    .unwrap_or_else(|| format!("Band {}", i + 1)),
                min: info.db_min as f32,
                max: info.db_max as f32,
                step: 1.0,
                value: eq.bands.get(i).copied().unwrap_or(0) as f32,
            }
        })
        .collect();
    Some(Equalizer {
        // 0x8310 has no named presets — a single editable custom curve.
        presets: vec![EqPreset {
            id: "custom".into(),
            label: "Custom".into(),
            is_custom: true,
            is_firmware: false,
            bands: None,
        }],
        selected_preset: 0,
        bands,
        editable: true,
    })
}

impl LogitechDevice {
    /// Read the equalizer + sidetone state at init, gated on the respective
    /// features. Mirrors the EQ into `eq_cache` for the sync `current_state`.
    pub(super) async fn init_audio(
        &self,
        features: &HashMap<u16, u8>,
        state: &mut LogitechDeviceState,
    ) {
        let hidpp = self.hidpp2_with(features).await;
        if features.contains_key(&feature::SIDETONE) {
            state.audio.sidetone = hidpp.read_sidetone().await;
        }
        if features.contains_key(&feature::EQUALIZER) {
            *state.audio.eq.lock().unwrap_or_else(|p| p.into_inner()) =
                hidpp.read_equalizer().await;
        }
    }
}

#[async_trait]
impl RangeCapability for LogitechDevice {
    fn range_cache(&self) -> &RangeStateCache {
        &self.range_cache
    }

    async fn to_wire(&self) -> Option<DeviceCapability> {
        let state = self.state.lock().await;
        if !state.features.contains_key(&feature::SIDETONE) {
            return None;
        }
        Some(DeviceCapability::Range(vec![Range {
            key: "sidetone".into(),
            label: "Sidetone".into(),
            min: 0,
            max: 100,
            step: 1,
            value: state.audio.sidetone.unwrap_or(0) as i32,
            read_only: false,
            category: "Microphone".into(),
            start_label: None,
            end_label: None,
            display: Default::default(),
            visible_when: None,
        }]))
    }

    async fn set_range(&self, key: &str, value: i32) -> Result<()> {
        if key != "sidetone" {
            anyhow::bail!("unknown range key: {key}");
        }
        self.range_cache.record(key, value);
        let level = value.clamp(0, 100) as u8;
        self.hidpp2().await.set_sidetone(level).await?;
        self.state.lock().await.audio.sidetone = Some(level);
        Ok(())
    }
}

#[async_trait]
impl EqualizerCapability for LogitechDevice {
    async fn get_equalizer(&self) -> Result<Equalizer> {
        let s = self.state.lock().await;
        let eq = s.audio.eq.lock().unwrap_or_else(|p| p.into_inner());
        build_equalizer(&eq).ok_or_else(|| anyhow::anyhow!("equalizer not available"))
    }

    /// The only preset is the editable custom curve, so selecting it is a no-op.
    async fn set_eq_preset(&self, _preset_index: usize) -> Result<()> {
        Ok(())
    }

    async fn set_eq_bands(&self, values: &[f32]) -> Result<()> {
        let info = {
            let s = self.state.lock().await;
            let eq = s.audio.eq.lock().unwrap_or_else(|p| p.into_inner());
            eq.info
                .ok_or_else(|| anyhow::anyhow!("equalizer info not read"))?
        };
        if values.len() != info.count as usize {
            anyhow::bail!(
                "expected {} EQ band values, got {}",
                info.count,
                values.len()
            );
        }
        let bands: Vec<i8> = values
            .iter()
            .map(|&v| v.round().clamp(i8::MIN as f32, i8::MAX as f32) as i8)
            .collect();
        let clamped = self.hidpp2().await.set_eq_bands(&bands, info).await?;
        self.state
            .lock()
            .await
            .audio
            .eq
            .lock()
            .unwrap_or_else(|p| p.into_inner())
            .bands = clamped;
        Ok(())
    }

    fn current_state(&self) -> Option<Equalizer> {
        let Ok(state) = self.state.try_lock() else {
            return None;
        };
        let eq = state.audio.eq.lock().unwrap_or_else(|p| p.into_inner());
        // `eq` borrows from `state`; clone the reading so we can drop both guards.
        let snapshot = EqReading {
            info: eq.info,
            freqs: eq.freqs.clone(),
            bands: eq.bands.clone(),
        };
        drop(eq);
        drop(state);
        build_equalizer(&snapshot)
    }

    /// Bands are read back from the device at init, so only re-apply persisted
    /// custom levels matching the device's actual band count.
    async fn restore_state(&self, v: &serde_json::Value) {
        if let Some(arr) = v["bands"].as_array() {
            let count = self
                .state
                .lock()
                .await
                .audio
                .eq
                .lock()
                .unwrap_or_else(|p| p.into_inner())
                .info
                .map(|i| i.count as usize)
                .unwrap_or(0);
            if count > 0 && arr.len() == count {
                let values: Vec<f32> = arr
                    .iter()
                    .map(|b| b.as_f64().unwrap_or(0.0) as f32)
                    .collect();
                if let Err(e) = self.set_eq_bands(&values).await {
                    log::warn!("[{}] EQ restore failed: {e}", self.id);
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::vendors::logitech::protocols::hidpp::v2::audio::EqInfo;

    #[test]
    fn freq_label_formats() {
        assert_eq!(freq_label(31), "31 Hz");
        assert_eq!(freq_label(250), "250 Hz");
        assert_eq!(freq_label(1000), "1 kHz");
        assert_eq!(freq_label(16000), "16 kHz");
        assert_eq!(freq_label(1500), "1.5 kHz");
    }

    #[test]
    fn build_equalizer_uses_info_and_freqs() {
        let eq = EqReading {
            info: Some(EqInfo {
                count: 2,
                db_min: -12,
                db_max: 12,
            }),
            freqs: vec![100, 1000],
            bands: vec![-3, 6],
        };
        let built = build_equalizer(&eq).unwrap();
        assert_eq!(built.bands.len(), 2);
        assert_eq!(built.bands[0].label, "100 Hz");
        assert_eq!(built.bands[1].label, "1 kHz");
        assert_eq!(built.bands[0].value, -3.0);
        assert_eq!(built.bands[1].value, 6.0);
        assert_eq!(built.bands[0].min, -12.0);
        assert!(built.editable);
        assert_eq!(built.presets.len(), 1);
        assert!(built.presets[0].is_custom);
        assert!(!built.presets[0].is_firmware);
    }

    #[test]
    fn build_equalizer_none_without_info() {
        assert!(build_equalizer(&EqReading::default()).is_none());
    }
}
