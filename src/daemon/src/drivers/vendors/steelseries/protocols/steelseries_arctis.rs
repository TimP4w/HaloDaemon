// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: linux-arctis-manager contributors <https://github.com/elegos/Linux-Arctis-Manager>
// Protocol reference: linux-arctis-manager by elegos (GPL-3.0)
//   https://github.com/elegos/Linux-Arctis-Manager
//   and sennheiser-gsx-control (MIT)
use anyhow::Result;

use crate::drivers::transports::{hid::HidTransport, Transport};

const PACKET_SIZE: usize = 64;
const EQ_BASELINE: f32 = 20.0; // 0x14 = 20

pub const POWER_OFFLINE: u8 = 0x01;
pub const POWER_CHARGING: u8 = 0x02;
pub const POWER_ONLINE: u8 = 0x08;

pub struct ArctisStatusFields {
    pub headset_battery: u8,
    pub slot_battery: u8,
    pub power_status: u8,
    pub mic_muted: bool,
    pub nc_mode: u8,
    pub nc_level: u8,
    pub wireless_mode: u8,
    pub auto_off_raw: u8,
    pub mic_led_brightness: u8,
}

pub struct ArctisSettingsFields {
    pub gain: u8,
    pub eq_preset: usize,
    pub sidetone: u8,
}

pub struct ArctisChatMixFields {
    pub game: u8,
    pub chat: u8,
}

/// Wire-level commands for the SteelSeries Arctis Nova Pro Wireless headset.
///
/// All byte sequences come from empirical HID capture; `persist()` must be
/// called after any setting change to commit it to NVRAM.
#[derive(Clone)]
pub struct ArctisProtocol<T: Transport + Clone> {
    pub(crate) transport: T,
}

impl ArctisProtocol<HidTransport> {
    pub fn open(path: &str) -> Result<Self> {
        Ok(Self {
            transport: HidTransport::open(path, Some(PACKET_SIZE), 1000, false)?,
        })
    }
}

#[cfg(target_os = "linux")]
fn strip_report_id(data: &[u8]) -> &[u8] {
    if data.first() == Some(&0x00) {
        &data[1..]
    } else {
        data
    }
}

#[cfg(not(target_os = "linux"))]
fn strip_report_id(data: &[u8]) -> &[u8] {
    data
}

fn parse_chatmix(data: &[u8]) -> Option<ArctisChatMixFields> {
    let d = strip_report_id(data);
    if d.len() >= 4 && d[0] == 0x07 && d[1] == 0x45 {
        Some(ArctisChatMixFields {
            game: d[2],
            chat: d[3],
        })
    } else {
        None
    }
}

fn parse_status(data: &[u8]) -> Option<ArctisStatusFields> {
    let d = strip_report_id(data);
    if d.len() < 0x10 || d[0] != 0x06 || d[1] != 0xb0 {
        return None;
    }
    Some(ArctisStatusFields {
        power_status: *d.get(0x0F).unwrap_or(&POWER_OFFLINE),
        headset_battery: (d.get(0x06).copied().unwrap_or(0).min(8) as u32 * 100 / 8) as u8,
        slot_battery: (d.get(0x07).copied().unwrap_or(0).min(8) as u32 * 100 / 8) as u8,
        nc_level: d.get(0x08).copied().unwrap_or(0).min(10) * 10,
        mic_muted: d.get(0x09).copied().unwrap_or(0) != 0,
        nc_mode: d.get(0x0A).copied().unwrap_or(0).min(2),
        mic_led_brightness: d.get(0x0B).copied().unwrap_or(0).min(10) * 10,
        auto_off_raw: d.get(0x0C).copied().unwrap_or(0).min(6),
        wireless_mode: d.get(0x0D).copied().unwrap_or(0).min(1),
    })
}

fn parse_settings(data: &[u8]) -> Option<ArctisSettingsFields> {
    let d = strip_report_id(data);
    if d.len() < 0x3A || d[0] != 0x06 || d[1] != 0x20 {
        return None;
    }
    let gain_raw = d.get(0x05).copied().unwrap_or(0x01);
    Some(ArctisSettingsFields {
        gain: if gain_raw == 0x02 { 1 } else { 0 },
        eq_preset: d.get(0x09).copied().unwrap_or(0).min(4) as usize,
        sidetone: d.get(0x35).copied().unwrap_or(0).min(3),
    })
}

pub fn eq_raw_to_db(raw: u8) -> f32 {
    (raw as f32 - EQ_BASELINE) * 0.5
}

pub fn eq_db_to_raw(db: f32) -> u8 {
    (EQ_BASELINE + (db / 0.5).round()).clamp(0.0, 255.0) as u8
}

impl<T: Transport + Clone> ArctisProtocol<T> {
    pub fn new(transport: T) -> Self {
        Self { transport }
    }

    /// Write `cmd` and read the response, collecting any interleaved ChatMix packets.
    async fn poll_with_prefix(
        &self,
        cmd: &[u8],
        prefix: [u8; 2],
    ) -> (Option<Vec<u8>>, Vec<ArctisChatMixFields>) {
        let Ok(data) = self.transport.write_then_read(cmd, PACKET_SIZE).await else {
            return (None, vec![]);
        };
        let d = strip_report_id(&data);
        if d.len() >= 2 && d[0] == prefix[0] && d[1] == prefix[1] {
            return (Some(data), vec![]);
        }
        let mut chatmix = Vec::new();
        if let Some(cm) = parse_chatmix(&data) {
            chatmix.push(cm);
        }
        for _ in 0..4 {
            match self.transport.read_nonblocking(PACKET_SIZE).await {
                Ok(retry) if !retry.is_empty() => {
                    let d = strip_report_id(&retry);
                    if d.len() >= 2 && d[0] == prefix[0] && d[1] == prefix[1] {
                        return (Some(retry), chatmix);
                    }
                    if let Some(cm) = parse_chatmix(&retry) {
                        chatmix.push(cm);
                    }
                }
                _ => break,
            }
        }
        (None, chatmix)
    }

    pub async fn activate_chatmix_display(&self) -> Result<()> {
        self.transport.write(&[0x06, 0x49, 0x01]).await
    }

    pub async fn persist(&self) -> Result<()> {
        self.transport.write(&[0x06, 0x09]).await
    }

    pub async fn send_nc_mode(&self, mode: u8) -> Result<()> {
        let raw = mode.min(2);
        self.transport.write(&[0x06, 0x47, raw, 0x00, raw]).await
    }

    pub async fn send_sidetone(&self, level: u8) -> Result<()> {
        self.transport.write(&[0x06, 0x39, level.min(3)]).await
    }

    pub async fn send_wireless_mode(&self, mode: u8) -> Result<()> {
        self.transport.write(&[0x06, 0xc3, mode.min(1)]).await
    }

    pub async fn send_mic_gain(&self, high: bool) -> Result<()> {
        let raw: u8 = if high { 0x02 } else { 0x01 };
        self.transport.write(&[0x06, 0x27, raw]).await
    }

    pub async fn send_auto_off(&self, index: u8) -> Result<()> {
        self.transport.write(&[0x06, 0xc1, index.min(6)]).await
    }

    pub async fn send_sonar_eq(&self, enabled: bool) -> Result<()> {
        self.transport.write(&[0x06, 0x8d, enabled as u8]).await
    }

    pub async fn send_screen_mode(&self, simple: bool) -> Result<()> {
        self.transport.write(&[0x06, 0x89, simple as u8]).await
    }

    /// `percent` is 0–100 in steps of 10; encoded to 0–10 internally.
    pub async fn send_mic_led_brightness(&self, percent: u8) -> Result<()> {
        self.transport
            .write(&[0x06, 0xbf, (percent / 10).min(10)])
            .await
    }

    /// `percent` is 0–100 in steps of 10; encoded to 0–10 internally.
    pub async fn send_nc_level(&self, percent: u8) -> Result<()> {
        let r = (percent / 10).min(10);
        self.transport.write(&[0x06, 0x33, r, r, r]).await
    }

    pub async fn send_eq_preset(&self, preset: u8) -> Result<()> {
        self.transport.write(&[0x06, 0x2e, preset.min(4)]).await
    }

    /// `db_values` is 10 EQ band values in dB; encoded to raw bytes internally.
    pub async fn send_eq_bands(&self, db_values: &[f32]) -> Result<()> {
        let mut pkt = vec![0x06u8, 0x33];
        pkt.extend(
            db_values
                .iter()
                .map(|&v| eq_db_to_raw(v.clamp(-10.0, 10.0))),
        );
        self.transport.write(&pkt).await
    }

    pub async fn poll_status(&self) -> (Option<ArctisStatusFields>, Vec<ArctisChatMixFields>) {
        let (raw, chatmix) = self.poll_with_prefix(&[0x06, 0xb0], [0x06, 0xb0]).await;
        (raw.as_deref().and_then(parse_status), chatmix)
    }

    pub async fn poll_settings(&self) -> (Option<ArctisSettingsFields>, Vec<ArctisChatMixFields>) {
        let (raw, chatmix) = self.poll_with_prefix(&[0x06, 0x20], [0x06, 0x20]).await;
        (raw.as_deref().and_then(parse_settings), chatmix)
    }

    pub async fn drain_chatmix(&self, count: usize) -> Vec<ArctisChatMixFields> {
        let mut result = Vec::new();
        for _ in 0..count {
            match self.transport.read_nonblocking(PACKET_SIZE).await {
                Ok(pkt) if !pkt.is_empty() => {
                    if let Some(cm) = parse_chatmix(&pkt) {
                        result.push(cm);
                    }
                }
                _ => break,
            }
        }
        result
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn eq_band_encoding_roundtrip() {
        for db in [-10.0f32, -5.0, 0.0, 3.5, 10.0] {
            let raw = eq_db_to_raw(db);
            let back = eq_raw_to_db(raw);
            assert!(
                (back - db).abs() < 0.01,
                "roundtrip failed for {db} dB → raw {raw} → {back}"
            );
        }
    }

    #[test]
    fn eq_baseline_is_zero_db() {
        assert_eq!(eq_raw_to_db(0x14), 0.0);
    }

    #[test]
    fn parse_status_extracts_fields() {
        let mut data = vec![0u8; 64];
        data[0] = 0x06;
        data[1] = 0xb0;
        data[0x06] = 6; // headset battery raw → 75%
        data[0x07] = 4; // slot battery raw → 50%
        data[0x08] = 7; // nc_level raw → 70%
        data[0x09] = 1; // mic_muted
        data[0x0A] = 2; // nc_mode
        data[0x0B] = 5; // mic_led_brightness raw → 50%
        data[0x0C] = 3; // auto_off_raw
        data[0x0D] = 1; // wireless_mode
        data[0x0F] = POWER_ONLINE;

        let f = parse_status(&data).expect("should parse");
        assert_eq!(f.headset_battery, 75);
        assert_eq!(f.slot_battery, 50);
        assert_eq!(f.nc_level, 70);
        assert!(f.mic_muted);
        assert_eq!(f.nc_mode, 2);
        assert_eq!(f.mic_led_brightness, 50);
        assert_eq!(f.auto_off_raw, 3);
        assert_eq!(f.wireless_mode, 1);
        assert_eq!(f.power_status, POWER_ONLINE);
    }

    #[test]
    fn parse_status_returns_none_on_bad_prefix() {
        let data = vec![0x00u8; 65];
        assert!(parse_status(&data).is_none());
    }

    #[test]
    fn parse_settings_extracts_fields() {
        let mut data = vec![0u8; 60];
        data[0] = 0x06;
        data[1] = 0x20;
        data[0x05] = 0x02; // gain high → index 1
        data[0x09] = 2; // eq_preset = Reference
        data[0x35] = 3; // sidetone = high

        let f = parse_settings(&data).expect("should parse");
        assert_eq!(f.gain, 1);
        assert_eq!(f.eq_preset, 2);
        assert_eq!(f.sidetone, 3);
    }

    #[test]
    fn parse_chatmix_extracts_game_chat() {
        let data = [0x07u8, 0x45, 80, 60];
        let f = parse_chatmix(&data).expect("should parse");
        assert_eq!(f.game, 80);
        assert_eq!(f.chat, 60);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_chatmix_with_report_id() {
        let data = [0x00u8, 0x07, 0x45, 75, 50];
        let f = parse_chatmix(&data).expect("should parse with stripped report ID");
        assert_eq!(f.game, 75);
        assert_eq!(f.chat, 50);
    }
}
