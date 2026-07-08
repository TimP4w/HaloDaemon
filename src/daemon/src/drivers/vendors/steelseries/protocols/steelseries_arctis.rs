// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: linux-arctis-manager contributors <https://github.com/elegos/Linux-Arctis-Manager>
// Protocol reference: linux-arctis-manager by elegos (GPL-3.0)
//   https://github.com/elegos/Linux-Arctis-Manager
//   and sennheiser-gsx-control (MIT)

//! Settings reply layout: 0x04 gain, 0x06 preset, 0x07..=0x10 Custom-EQ bands
//! (0x14 = 0 dB), 0x12 sidetone.

use anyhow::Result;

use crate::drivers::transports::{hid::HidTransport, Transport};

const PACKET_SIZE: usize = 64;
const EQ_BASELINE: f32 = 20.0; // 0x14 = 20

pub const POWER_OFFLINE: u8 = 0x01;
pub const POWER_CHARGING: u8 = 0x02;
pub const POWER_ONLINE: u8 = 0x08;

const REPORT_CMD: u8 = 0x06; // host→device command, and the device's reply to one
const REPORT_NOTIFY: u8 = 0x07; // unsolicited device→host notification

/// Message identifier (packet byte 2); host commands and their matching notifications share the same IDs.
#[derive(Clone, Copy, PartialEq, Eq)]
#[repr(u8)]
enum Msg {
    Persist = 0x09,
    Settings = 0x20,
    StationVolume = 0x25,
    MicGain = 0x27,
    EqPreset = 0x2e,
    EqBands = 0x33,
    MicVolume = 0x37,
    Sidetone = 0x39,
    ChatMix = 0x45,
    ChatmixDisplay = 0x49,
    ScreenMode = 0x89,
    SonarEq = 0x8d,
    Status = 0xb0,
    NcLevel = 0xb9,
    NcMode = 0xbd,
    MicLedBrightness = 0xbf,
    AutoOff = 0xc1,
    WirelessMode = 0xc3,
}

impl Msg {
    /// The raw wire byte for this message.
    const fn id(self) -> u8 {
        self as u8
    }
}

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
    pub bt_powerup_state: u8,
    pub bt_auto_mute: u8,
    pub bt_power_status: u8,
    pub bt_connection: u8,
    pub bt_wireless_pairing: u8,
}

pub struct ArctisSettingsFields {
    pub gain: u8,
    pub eq_preset: u8,
    pub sidetone: u8,
    /// The 10 Custom-EQ band values in dB; the frame always carries the Custom curve.
    pub eq_bands: [f32; 10],
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
            transport: HidTransport::open(path, Some(PACKET_SIZE), 1000, false, None)?,
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
    if d.len() >= 4 && d[0] == REPORT_NOTIFY && d[1] == Msg::ChatMix.id() {
        Some(ArctisChatMixFields {
            game: d[2],
            chat: d[3],
        })
    } else {
        None
    }
}

/// Mic-volume notification `07 37 <raw>` (raw 1–10), emitted on base-station change.
fn parse_mic_volume(data: &[u8]) -> Option<u8> {
    let d = strip_report_id(data);
    if d.len() >= 3 && d[0] == REPORT_NOTIFY && d[1] == Msg::MicVolume.id() {
        Some(d[2])
    } else {
        None
    }
}

/// Station-volume notification `07 25 <raw>` (raw 0–100), emitted when the
/// base-station main volume dial is turned.
fn parse_station_volume(data: &[u8]) -> Option<u8> {
    let d = strip_report_id(data);
    if d.len() >= 3 && d[0] == REPORT_NOTIFY && d[1] == Msg::StationVolume.id() {
        Some(d[2])
    } else {
        None
    }
}

fn parse_status(data: &[u8]) -> Option<ArctisStatusFields> {
    let d = strip_report_id(data);
    if d.len() < 0x10 || d[0] != REPORT_CMD || d[1] != Msg::Status.id() {
        return None;
    }
    Some(ArctisStatusFields {
        power_status: *d.get(0x0F).unwrap_or(&POWER_OFFLINE),
        headset_battery: (d.get(0x06).copied().unwrap_or(0).min(8) as u32 * 100 / 8) as u8,
        slot_battery: (d.get(0x07).copied().unwrap_or(0).min(8) as u32 * 100 / 8) as u8,
        nc_level: d.get(0x08).copied().unwrap_or(0).min(10),
        mic_muted: d.get(0x09).copied().unwrap_or(0) != 0,
        nc_mode: d.get(0x0A).copied().unwrap_or(0).min(2),
        mic_led_brightness: d.get(0x0B).copied().unwrap_or(0).min(10) * 10,
        auto_off_raw: d.get(0x0C).copied().unwrap_or(0).min(6),
        wireless_mode: d.get(0x0D).copied().unwrap_or(0).min(1),
        bt_powerup_state: d.get(0x02).copied().unwrap_or(0),
        bt_auto_mute: d.get(0x03).copied().unwrap_or(0),
        bt_power_status: d.get(0x04).copied().unwrap_or(0),
        bt_connection: d.get(0x05).copied().unwrap_or(0),
        bt_wireless_pairing: d.get(0x0E).copied().unwrap_or(0),
    })
}

fn parse_settings(data: &[u8]) -> Option<ArctisSettingsFields> {
    let d = strip_report_id(data);
    if d.len() < 0x13 || d[0] != REPORT_CMD || d[1] != Msg::Settings.id() {
        return None;
    }
    let gain_raw = d.get(0x04).copied().unwrap_or(0x01);
    let mut eq_bands = [0.0f32; 10];
    for (i, band) in eq_bands.iter_mut().enumerate() {
        let raw = d.get(0x07 + i).copied().unwrap_or(EQ_BASELINE as u8);
        *band = eq_raw_to_db(raw);
    }
    Some(ArctisSettingsFields {
        gain: if gain_raw == 0x02 { 1 } else { 0 },
        eq_preset: d.get(0x06).copied().unwrap_or(0),
        sidetone: d.get(0x12).copied().unwrap_or(0).min(3),
        eq_bands,
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

    pub async fn activate_chatmix_display(&self) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::ChatmixDisplay.id(), 0x01])
            .await
    }

    pub async fn persist(&self) -> Result<()> {
        self.transport.write(&[REPORT_CMD, Msg::Persist.id()]).await
    }

    /// NC mode (0=off, 1=transparent, 2=on) via `06 bd` (inferred; needs hw confirmation).
    pub async fn send_nc_mode(&self, mode: u8) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::NcMode.id(), mode.min(2)])
            .await
    }

    pub async fn send_sidetone(&self, level: u8) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::Sidetone.id(), level.min(3)])
            .await
    }

    /// `level` is the capture-volume level 1–10 (1 = muted, 10 = 100%).
    pub async fn send_mic_volume(&self, level: u8) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::MicVolume.id(), level.clamp(1, 10)])
            .await
    }

    pub async fn send_wireless_mode(&self, mode: u8) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::WirelessMode.id(), mode.min(1)])
            .await
    }

    pub async fn send_mic_gain(&self, high: bool) -> Result<()> {
        let raw: u8 = if high { 0x02 } else { 0x01 };
        self.transport
            .write(&[REPORT_CMD, Msg::MicGain.id(), raw])
            .await
    }

    pub async fn send_auto_off(&self, index: u8) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::AutoOff.id(), index.min(6)])
            .await
    }

    pub async fn send_sonar_eq(&self, enabled: bool) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::SonarEq.id(), enabled as u8])
            .await
    }

    pub async fn send_screen_mode(&self, simple: bool) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::ScreenMode.id(), simple as u8])
            .await
    }

    /// `percent` is 0–100 in steps of 10; encoded to 0–10 internally.
    pub async fn send_mic_led_brightness(&self, percent: u8) -> Result<()> {
        self.transport
            .write(&[
                REPORT_CMD,
                Msg::MicLedBrightness.id(),
                (percent / 10).min(10),
            ])
            .await
    }

    /// Set the transparency level 0–10 via `06 b9`, mirroring the `07 b9` notification.
    pub async fn send_nc_level(&self, level: u8) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::NcLevel.id(), level.min(10)])
            .await
    }

    /// `preset` is the raw device preset byte (0–4 standard, higher = game presets).
    pub async fn send_eq_preset(&self, preset: u8) -> Result<()> {
        self.transport
            .write(&[REPORT_CMD, Msg::EqPreset.id(), preset])
            .await
    }

    /// `db_values` is 10 EQ band values in dB; encoded to raw bytes internally.
    pub async fn send_eq_bands(&self, db_values: &[f32]) -> Result<()> {
        let mut pkt = vec![REPORT_CMD, Msg::EqBands.id()];
        pkt.extend(
            db_values
                .iter()
                .map(|&v| eq_db_to_raw(v.clamp(-10.0, 10.0))),
        );
        self.transport.write(&pkt).await
    }

    /// Single polling pass: prompt the base station for status + settings, then
    /// drain every queued packet and parse each as status, settings, or ChatMix.
    pub async fn poll(&self) -> ArctisPoll {
        let _ = self.transport.write(&[REPORT_CMD, Msg::Status.id()]).await;
        let _ = self
            .transport
            .write(&[REPORT_CMD, Msg::Settings.id()])
            .await;

        let mut out = ArctisPoll::default();
        for i in 0..MAX_POLL_READS {
            // First read waits for a reply; the rest drain whatever is already queued.
            let pkt = if i == 0 {
                self.transport.read(PACKET_SIZE).await
            } else {
                self.transport.read_nonblocking(PACKET_SIZE).await
            };
            match pkt {
                Ok(p) if !p.is_empty() => {
                    if let Some(s) = parse_status(&p) {
                        out.status = Some(s);
                    } else if let Some(s) = parse_settings(&p) {
                        out.settings = Some(s);
                    } else if let Some(cm) = parse_chatmix(&p) {
                        out.chatmix.push(cm);
                    } else if let Some(raw) = parse_mic_volume(&p) {
                        out.mic_volume_raw = Some(raw);
                    } else if let Some(raw) = parse_station_volume(&p) {
                        out.station_volume_raw = Some(raw);
                    }
                }
                _ => break,
            }
        }
        out
    }
}

/// Maximum packets drained per `poll()` pass. Bounds the loop while leaving ample
/// headroom for the device's streamed packets to surface the status reply.
const MAX_POLL_READS: usize = 32;

#[derive(Default)]
pub struct ArctisPoll {
    pub status: Option<ArctisStatusFields>,
    pub settings: Option<ArctisSettingsFields>,
    pub chatmix: Vec<ArctisChatMixFields>,
    /// Mic-volume hardware level (1–10) from a `07 37` notification, if any.
    pub mic_volume_raw: Option<u8>,
    /// Station main-volume level (0–100) from a `07 25` notification, if any.
    pub station_volume_raw: Option<u8>,
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

    proptest::proptest! {
        /// Property: `eq_db_to_raw` / `eq_raw_to_db` round-trip for all valid
        /// raw values (0x00–0x28 covers -10 to +10 dB range, 0.5 dB steps).
        #[test]
        fn eq_roundtrip_raw(raw in 0u8..=0x28) {
            let db = eq_raw_to_db(raw);
            let back = eq_db_to_raw(db);
            assert_eq!(back, raw, "roundtrip failed: raw {raw} → {db} dB → raw {back}");
        }
    }

    #[test]
    fn parse_mic_volume_reads_level() {
        assert_eq!(
            parse_mic_volume(&[REPORT_NOTIFY, Msg::MicVolume.id(), 0x06]),
            Some(6)
        );
        assert_eq!(
            parse_mic_volume(&[REPORT_NOTIFY, Msg::ChatMix.id(), 0x06]),
            None
        );
    }

    #[test]
    fn parse_station_volume_reads_level() {
        assert_eq!(
            parse_station_volume(&[REPORT_NOTIFY, Msg::StationVolume.id(), 0x64]),
            Some(100)
        );
        // Wrong message ID must not match.
        assert_eq!(
            parse_station_volume(&[REPORT_NOTIFY, Msg::MicVolume.id(), 0x64]),
            None
        );
        // A command report (0x06) is not a notification.
        assert_eq!(
            parse_station_volume(&[REPORT_CMD, Msg::StationVolume.id(), 0x64]),
            None
        );
    }

    #[test]
    fn parse_status_extracts_fields() {
        let mut data = vec![0u8; 64];
        data[0] = REPORT_CMD;
        data[1] = Msg::Status.id();
        data[0x06] = 6; // headset battery raw → 75%
        data[0x07] = 4; // slot battery raw → 50%
        data[0x08] = 7; // nc_level → level 7
        data[0x09] = 1; // mic_muted
        data[0x0A] = 2; // nc_mode
        data[0x0B] = 5; // mic_led_brightness raw → 50%
        data[0x0C] = 3; // auto_off_raw
        data[0x0D] = 1; // wireless_mode
        data[0x0F] = POWER_ONLINE;

        let f = parse_status(&data).expect("should parse");
        assert_eq!(f.headset_battery, 75);
        assert_eq!(f.slot_battery, 50);
        assert_eq!(f.nc_level, 7);
        assert!(f.mic_muted);
        assert_eq!(f.nc_mode, 2);
        assert_eq!(f.mic_led_brightness, 50);
        assert_eq!(f.auto_off_raw, 3);
        assert_eq!(f.wireless_mode, 1);
        assert_eq!(f.power_status, POWER_ONLINE);
    }

    #[test]
    fn parse_status_extracts_bluetooth_block() {
        let mut data = vec![0u8; 64];
        data[0] = REPORT_CMD;
        data[1] = Msg::Status.id();
        data[0x02] = 1; // bt powerup state
        data[0x03] = 2; // bt auto-mute
        data[0x04] = 8; // bt power status
        data[0x05] = 1; // bt connection
        data[0x0E] = 3; // bt wireless pairing

        let f = parse_status(&data).expect("should parse");
        assert_eq!(f.bt_powerup_state, 1);
        assert_eq!(f.bt_auto_mute, 2);
        assert_eq!(f.bt_power_status, 8);
        assert_eq!(f.bt_connection, 1);
        assert_eq!(f.bt_wireless_pairing, 3);
    }

    #[test]
    fn parse_status_returns_none_on_bad_prefix() {
        let data = vec![0x00u8; 65];
        assert!(parse_status(&data).is_none());
    }

    #[test]
    fn parse_settings_extracts_fields() {
        let mut data = vec![0u8; 60];
        data[0] = REPORT_CMD;
        data[1] = Msg::Settings.id();
        data[0x04] = 0x02; // gain high → index 1
        data[0x06] = 2; // eq_preset = Focus
        data[0x12] = 3; // sidetone = high
        data[0x07] = 0x28; // band 1 = +10 dB (max)
        data[0x10] = 0x00; // band 10 = -10 dB (min)

        let f = parse_settings(&data).expect("should parse");
        assert_eq!(f.gain, 1);
        assert_eq!(f.eq_preset, 2);
        assert_eq!(f.sidetone, 3);
        assert_eq!(f.eq_bands[0], 10.0);
        assert_eq!(f.eq_bands[9], -10.0);
    }

    #[test]
    fn parse_settings_keeps_raw_game_preset_byte() {
        let mut data = vec![0u8; 60];
        data[0] = REPORT_CMD;
        data[1] = Msg::Settings.id();
        data[0x06] = 0x10; // PUBG game preset — kept raw, mapped to a slot later
        let f = parse_settings(&data).expect("should parse");
        assert_eq!(f.eq_preset, 0x10);
    }

    #[test]
    fn parse_settings_decodes_real_capture() {
        // Real frame: gain high, Custom preset (4), sidetone high, custom curve.
        let bytes: [u8; 25] = [
            0x06, 0x20, 0x00, 0x04, 0x02, 0x00, 0x04, 0x14, 0x14, 0x14, 0x14, 0x14, 0x22, 0x05,
            0x05, 0x16, 0x28, 0x0a, 0x03, 0x01, 0x4b, 0x64, 0x64, 0x00, 0x64,
        ];
        let mut data = vec![0u8; 64];
        data[..bytes.len()].copy_from_slice(&bytes);
        let f = parse_settings(&data).expect("should parse");
        assert_eq!(f.gain, 1); // 0x02 high
        assert_eq!(f.eq_preset, 4); // Custom
        assert_eq!(f.sidetone, 3); // byte 0x12 == 0x03 → high
                                   // Band 6 (0x22) → +7 dB, band 10 (0x28) → +10 dB.
        assert_eq!(f.eq_bands[5], 7.0);
        assert_eq!(f.eq_bands[9], 10.0);
    }

    #[test]
    fn parse_chatmix_extracts_game_chat() {
        let data = [REPORT_NOTIFY, Msg::ChatMix.id(), 80, 60];
        let f = parse_chatmix(&data).expect("should parse");
        assert_eq!(f.game, 80);
        assert_eq!(f.chat, 60);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_chatmix_with_report_id() {
        let data = [0x00u8, REPORT_NOTIFY, Msg::ChatMix.id(), 75, 50];
        let f = parse_chatmix(&data).expect("should parse with stripped report ID");
        assert_eq!(f.game, 75);
        assert_eq!(f.chat, 50);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_status_with_report_id() {
        let mut data = vec![0u8; 65];
        data[0] = 0x00; // Linux report ID — stripped off by strip_report_id
        data[1] = REPORT_CMD;
        data[2] = Msg::Status.id();
        data[0x07] = 6; // d[0x06] headset battery raw → 75%
        data[0x08] = 4; // d[0x07] slot battery raw → 50%
        data[0x0A] = 1; // d[0x09] mic_muted
        let f = parse_status(&data).expect("should parse with stripped report ID");
        assert_eq!(f.headset_battery, 75);
        assert_eq!(f.slot_battery, 50);
        assert!(f.mic_muted);
    }

    #[cfg(target_os = "linux")]
    #[test]
    fn parse_settings_with_report_id() {
        let mut data = vec![0u8; 65];
        data[0] = 0x00; // Linux report ID — stripped off by strip_report_id
        data[1] = REPORT_CMD;
        data[2] = Msg::Settings.id();
        data[0x05] = 0x02; // d[0x04] gain high → index 1
        data[0x07] = 2; // d[0x06] eq_preset
        data[0x13] = 3; // d[0x12] sidetone
        let f = parse_settings(&data).expect("should parse with stripped report ID");
        assert_eq!(f.gain, 1);
        assert_eq!(f.eq_preset, 2);
        assert_eq!(f.sidetone, 3);
    }

    use async_trait::async_trait;
    use std::collections::VecDeque;
    use std::sync::Arc;
    use tokio::sync::Mutex as TokioMutex;

    /// Clone-able transport that hands out queued packets in order (front first)
    /// and records writes. Cloning shares the same queue, matching `ArctisProtocol`'s
    /// `Transport + Clone` bound.
    #[derive(Clone)]
    struct QueueTransport {
        responses: Arc<TokioMutex<VecDeque<Vec<u8>>>>,
        written: Arc<TokioMutex<Vec<Vec<u8>>>>,
        rate: crate::drivers::Metered<()>,
    }

    impl QueueTransport {
        fn new(packets: Vec<Vec<u8>>) -> Self {
            Self {
                responses: Arc::new(TokioMutex::new(packets.into())),
                written: Arc::new(TokioMutex::new(Vec::new())),
                rate: crate::drivers::Metered::new((), None),
            }
        }
    }

    #[async_trait]
    impl Transport for QueueTransport {
        async fn write(&self, data: &[u8]) -> Result<()> {
            self.rate.write_access(data.len()).await?;
            self.written.lock().await.push(data.to_vec());
            Ok(())
        }
        async fn read(&self, _size: usize) -> Result<Vec<u8>> {
            Ok(self.responses.lock().await.pop_front().unwrap_or_default())
        }
        fn rate_status(&self) -> halod_shared::types::WriteRateStatus {
            self.rate.status()
        }
        fn set_write_rate_limit(&self, limit: Option<halod_shared::types::WriteRateLimit>) {
            self.rate.set_limit(limit);
        }
    }

    fn status_packet(mic_muted: bool) -> Vec<u8> {
        let mut p = vec![0u8; 64];
        p[0] = REPORT_CMD;
        p[1] = Msg::Status.id();
        p[0x09] = mic_muted as u8;
        p[0x0F] = POWER_ONLINE;
        p
    }

    fn settings_packet(sidetone: u8) -> Vec<u8> {
        let mut p = vec![0u8; 64];
        p[0] = REPORT_CMD;
        p[1] = Msg::Settings.id();
        p[0x12] = sidetone;
        p
    }

    fn chatmix_packet(game: u8, chat: u8) -> Vec<u8> {
        vec![REPORT_NOTIFY, Msg::ChatMix.id(), game, chat]
    }

    #[tokio::test]
    async fn poll_parses_station_volume_notification() {
        let proto = ArctisProtocol::new(QueueTransport::new(vec![
            status_packet(false),
            vec![REPORT_NOTIFY, Msg::StationVolume.id(), 0x32],
        ]));
        let out = proto.poll().await;
        assert_eq!(out.station_volume_raw, Some(0x32));
    }

    #[tokio::test]
    async fn poll_parses_status_settings_and_chatmix() {
        let proto = ArctisProtocol::new(QueueTransport::new(vec![
            status_packet(true),
            settings_packet(2),
            chatmix_packet(80, 60),
        ]));
        let out = proto.poll().await;
        assert!(out.status.is_some_and(|s| s.mic_muted));
        assert!(out.settings.is_some_and(|s| s.sidetone == 2));
        assert_eq!(out.chatmix.len(), 1);
        assert_eq!(out.chatmix[0].game, 80);
        assert_eq!(out.chatmix[0].chat, 60);
    }

    #[tokio::test]
    async fn poll_prompts_device_for_status_and_settings() {
        let transport = QueueTransport::new(vec![]);
        let proto = ArctisProtocol::new(transport.clone());
        proto.poll().await;
        let written = transport.written.lock().await;
        assert!(written
            .iter()
            .any(|w| w.starts_with(&[REPORT_CMD, Msg::Status.id()])));
        assert!(written
            .iter()
            .any(|w| w.starts_with(&[REPORT_CMD, Msg::Settings.id()])));
    }

    #[tokio::test]
    async fn poll_finds_status_buried_behind_many_chatmix_packets() {
        // The old request/response matcher only inspected ~5 reads, so a status
        // reply this deep in the stream was missed. Draining must still find it.
        let mut packets: Vec<Vec<u8>> = (0..20).map(|_| chatmix_packet(50, 50)).collect();
        packets.push(status_packet(false));
        let proto = ArctisProtocol::new(QueueTransport::new(packets));
        let out = proto.poll().await;
        assert!(
            out.status.is_some_and(|s| !s.mic_muted),
            "status buried behind 20 chatmix packets must still be parsed"
        );
        assert_eq!(out.chatmix.len(), 20);
    }

    #[tokio::test]
    async fn poll_keeps_latest_status_when_multiple_arrive() {
        let proto = ArctisProtocol::new(QueueTransport::new(vec![
            status_packet(true),
            status_packet(false),
        ]));
        let out = proto.poll().await;
        assert!(
            out.status.is_some_and(|s| !s.mic_muted),
            "the last status packet in the stream wins"
        );
    }

    #[tokio::test]
    async fn poll_returns_none_when_no_status_or_settings() {
        let proto = ArctisProtocol::new(QueueTransport::new(vec![
            chatmix_packet(10, 90),
            chatmix_packet(20, 80),
        ]));
        let out = proto.poll().await;
        assert!(out.status.is_none());
        assert!(out.settings.is_none());
        assert_eq!(out.chatmix.len(), 2);
    }

    #[tokio::test]
    async fn poll_stops_draining_at_max_reads() {
        // Far more packets than the cap; poll must terminate (and bound the work).
        let packets: Vec<Vec<u8>> = (0..MAX_POLL_READS * 4)
            .map(|_| chatmix_packet(1, 1))
            .collect();
        let proto = ArctisProtocol::new(QueueTransport::new(packets));
        let out = proto.poll().await;
        assert_eq!(out.chatmix.len(), MAX_POLL_READS);
    }
}
