// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: Martin Hartl (inlart) — OpenRGB project
/// Protocol reference: OpenRGB AsusAuraUSBController / AsusAuraMainboardController
///   by Martin Hartl and contributors (GPL-2.0-or-later)
///   https://github.com/OpenRGB/OpenRGB
use std::collections::HashMap;

use anyhow::Result;

use crate::drivers::transports::{hid::HidTransport, Transport};
use halod_protocol::types::{
    EffectParamDescriptor, EffectParamValue, LedPosition, NativeEffect, ParamKind, RgbColor,
    RgbDescriptor, RgbZone, ZoneTopology,
};

// ── Protocol constants ────────────────────────────────────────────────────────

pub(crate) const AURA_HDR: u8 = 0xEC;
const CMD_FIRMWARE: u8 = 0x82;
const CMD_CONFIG: u8 = 0xB0;
pub(crate) const CMD_DIRECT: u8 = 0x40;
pub(crate) const CMD_SETMODE: u8 = 0x35;
pub(crate) const CMD_ADDR_EFFECT: u8 = 0x3B;

/// Max LEDs streamed per CMD_DIRECT sub-packet (20 × 3 = 60 bytes, fits in a 65-byte packet).
pub(crate) const LEDS_PER_PACKET: usize = 20;
// 65-byte raw HID write matching OpenRGB / legacy Python behaviour: no report-ID prefix,
// byte 0 is 0xEC on the wire.  report_size=None (raw passthrough) in HidTransport.
pub(crate) const REPORT_SIZE: usize = 65;
const TIMEOUT_MS: i32 = 1000;

const MODE_OFF: u8 = 0x00;
const MODE_BREATHING: u8 = 0x02;
const MODE_SPECTRUM_CYCLE: u8 = 0x04;
const MODE_RAINBOW_WAVE: u8 = 0x05;
pub(crate) const MODE_DIRECT: u8 = 0xFF;

/// CMD_DIRECT channel for the mainboard fixed-LED zone.  The protocol assigns
/// channels 0–3 to the ARGB headers and reserves channel 4 for the on-board LEDs
/// (chipset, I/O cover, etc.) including any 12V RGB header positions.
const MB_DIRECT_CHANNEL: u8 = 0x04;

// Config table offsets (ct = resp[4..], 60 bytes)
const CT_ARGB_CH: usize = 0x02; // number of 5V ARGB channels
const CT_MB_LEDS: usize = 0x1B; // total mainboard LED count (on-board + 12V headers)
const CT_CH_BLOCK_OFF: usize = 4; // start of per-channel 6-byte blocks
const CT_CH_BLOCK_SZ: usize = 6;
const CT_CH_LEDS_OFF: usize = 2; // LED count offset within each block

pub(crate) const DEFAULT_ARGB_LEDS: u8 = 30; // fallback when config block reports 0
pub(crate) const MAX_ARGB_LEDS: u8 = 120; // hardware cap per channel

// ── Channel descriptor ────────────────────────────────────────────────────────

/// One 5V ARGB channel discovered from the config table.
#[derive(Debug, Clone)]
pub struct AuraChannel {
    /// Effect channel number (i+1 for ARGB channel i).
    pub(crate) effect_channel: u8,
    /// Direct channel number (0-indexed, equals i for ARGB channel i).
    pub(crate) direct_channel: u8,
    /// LED count for this channel (from config block, capped at 120).
    pub(crate) num_leds: u8,
}

// ── Config parsing ────────────────────────────────────────────────────────────

/// Parse ARGB channel count, per-channel LED counts, and fixed mainboard LED count
/// from the 60-byte config table.
pub(crate) fn parse_config(config: &[u8; 60]) -> (u8, Vec<u8>, u8) {
    let argb_count = config[CT_ARGB_CH];
    // CT_MB_LEDS is the total on-board LED count including 12V RGB header positions.
    // All of these are written as a contiguous block to MB_DIRECT_CHANNEL; subtracting
    // header LEDs would leave their positions dark rather than under separate control.
    let mb_leds = config[CT_MB_LEDS];

    let mut led_counts = Vec::new();
    for i in 0..argb_count as usize {
        let block_off = CT_CH_BLOCK_OFF + i * CT_CH_BLOCK_SZ + CT_CH_LEDS_OFF;
        let leds = if block_off < 60 { config[block_off] } else { 0 };
        led_counts.push(if leds == 0 {
            DEFAULT_ARGB_LEDS
        } else {
            leds.min(MAX_ARGB_LEDS)
        });
    }

    (argb_count, led_counts, mb_leds)
}

/// Split a flat channel list into chainable and non-chainable groups, building the
/// `RgbDescriptor` for the non-chainable zones.
///
/// `non_chainable` is a caller-supplied slice of `(direct_channel, zone_id, zone_name)`
/// tuples describing which channels are fixed zones rather than daisy-chainable headers.
/// The caller (device layer) owns the board-quirk table and filters it to the current
/// device before passing it in, keeping PID/VID knowledge out of the protocol layer.
pub(crate) fn split_channels(
    channels: &[AuraChannel],
    non_chainable: &[(u8, &str, &str)],
) -> (RgbDescriptor, Vec<u8>, HashMap<String, u8>) {
    let mut zones: Vec<RgbZone> = Vec::new();
    let mut non_chainable_channels: Vec<u8> = Vec::new();
    let mut chainable_map: HashMap<String, u8> = HashMap::new();

    for ch in channels {
        let known = non_chainable
            .iter()
            .find(|(dc, _, _)| *dc == ch.direct_channel);

        if let Some((_, zid, zname)) = known {
            let leds: Vec<LedPosition> = (0..ch.num_leds as u32)
                .map(|i| LedPosition {
                    id: i,
                    x: if ch.num_leds > 1 {
                        i as f32 / (ch.num_leds - 1) as f32
                    } else {
                        0.5
                    },
                    y: 0.5,
                })
                .collect();
            zones.push(RgbZone {
                id: zid.to_string(),
                name: zname.to_string(),
                topology: ZoneTopology::Linear,
                leds,
            });
            non_chainable_channels.push(ch.direct_channel);
        } else {
            chainable_map.insert(argb_channel_id(ch.direct_channel), ch.direct_channel);
        }
    }

    let descriptor = RgbDescriptor {
        zones,
        native_effects: aura_native_effects(),
    };
    (descriptor, non_chainable_channels, chainable_map)
}

fn argb_channel_id(direct_channel: u8) -> String {
    format!("argb_{direct_channel}")
}

// ── Protocol layer ────────────────────────────────────────────────────────────

pub struct AuraUsbProtocol<T: Transport> {
    pub(crate) transport: T,
}

impl AuraUsbProtocol<HidTransport> {
    pub fn open(path: &str) -> Result<Self> {
        Ok(Self {
            // report_size = None: raw passthrough.
            // We send 65-byte frames starting with 0xEC
            transport: HidTransport::open(path, None, TIMEOUT_MS, false)?,
        })
    }
}

impl<T: Transport> AuraUsbProtocol<T> {
    /// Builds a zero-padded 65-byte command frame with 0xEC at byte 0.
    pub fn make_packet(cmd: u8) -> [u8; REPORT_SIZE] {
        let mut buf = [0u8; REPORT_SIZE];
        buf[0] = AURA_HDR;
        buf[1] = cmd;
        buf
    }

    /// Disable legacy gen-2 continuous-cycle mode so we can take over direct
    /// control.  Must be sent before any other command.
    pub async fn stop_gen2(&self) {
        let mut pkt = [0u8; REPORT_SIZE];
        pkt[0] = AURA_HDR;
        pkt[1] = 0x52;
        pkt[2] = 0x53;
        pkt[3] = 0x00;
        pkt[4] = 0x01;
        let _ = self.transport.write(&pkt).await;
    }

    pub async fn get_firmware_version(&self) -> Option<String> {
        let pkt = Self::make_packet(CMD_FIRMWARE);
        self.transport.write(&pkt).await.ok()?;
        let resp = self.transport.read_matching(
            REPORT_SIZE,
            |r| r.len() >= 4 && r[0] == AURA_HDR && r[1] == 0x02,
            8,
        ).await?;
        let raw = &resp[2..18.min(resp.len())];
        let end = raw.iter().position(|&b| b == 0).unwrap_or(raw.len());
        Some(String::from_utf8_lossy(&raw[..end]).into_owned())
    }

    pub async fn get_config_table(&self) -> Option<[u8; 60]> {
        let pkt = Self::make_packet(CMD_CONFIG);
        self.transport.write(&pkt).await.ok()?;
        let resp = self.transport.read_matching(
            REPORT_SIZE,
            |r| r.len() >= 8 && r[0] == AURA_HDR && r[1] == 0x30,
            8,
        ).await?;
        let mut out = [0u8; 60];
        let src = &resp[4..64.min(resp.len())];
        out[..src.len()].copy_from_slice(src);
        Some(out)
    }

    /// Set one effect channel to direct (software) control mode.
    /// Channel 0 = mainboard, channels 1..N = ARGB headers.
    pub async fn set_channel_direct(&self, channel: u8) {
        let mut pkt = Self::make_packet(CMD_SETMODE);
        pkt[2] = channel;
        pkt[5] = MODE_DIRECT;
        let _ = self.transport.write(&pkt).await;
        log::debug!("[ASUS Aura USB] channel {} → direct", channel);
    }

    /// Stream per-LED RGB data to one ARGB direct channel.
    /// `direct_channel` is 0-indexed (matches ARGB channel i).
    pub async fn send_direct(&self, direct_channel: u8, colors: &[RgbColor]) -> Result<()> {
        if colors.is_empty() {
            return Ok(());
        }
        let led_count = colors.len();
        let mut offset = 0;
        while offset < led_count {
            let end = (offset + LEDS_PER_PACKET).min(led_count);
            let is_last = end == led_count;
            let count = end - offset;

            let mut pkt = [0u8; REPORT_SIZE];
            pkt[0] = AURA_HDR;
            pkt[1] = CMD_DIRECT;
            pkt[2] = direct_channel | if is_last { 0x80 } else { 0x00 };
            pkt[3] = u8::try_from(offset).map_err(|_| {
                anyhow::anyhow!("LED offset {offset} exceeds 255; too many LEDs per channel")
            })?;
            pkt[4] = count as u8;
            for i in 0..count {
                let c = colors[offset + i];
                pkt[5 + i * 3] = c.r;
                pkt[5 + i * 3 + 1] = c.g;
                pkt[5 + i * 3 + 2] = c.b;
            }
            self.transport.write(&pkt).await?;
            offset = end;
        }
        Ok(())
    }

    /// Stream per-LED RGB data to the fixed mainboard LED zone.
    pub async fn send_direct_mb(&self, colors: &[RgbColor]) -> Result<()> {
        self.send_direct(MB_DIRECT_CHANNEL, colors).await
    }

    /// Send a native effect by effect-ID string to one ARGB effect channel.
    /// The color param is extracted from `params["color"]`; defaults to white if absent.
    pub async fn send_effect(
        &self,
        id: &str,
        effect_channel: u8,
        params: &HashMap<String, EffectParamValue>,
    ) -> Result<()> {
        let mode = match id {
            "off" => MODE_OFF,
            "breathing" => MODE_BREATHING,
            "spectrum_cycle" => MODE_SPECTRUM_CYCLE,
            "rainbow_wave" => MODE_RAINBOW_WAVE,
            other => anyhow::bail!("unknown effect: {other}"),
        };
        let color = params
            .get("color")
            .and_then(|v| if let EffectParamValue::Color(c) = v { Some(*c) } else { None })
            .unwrap_or(RgbColor { r: 255, g: 255, b: 255 });
        self.send_effect_argb(effect_channel, mode, color.r, color.g, color.b)
            .await
    }

    /// Send a native-effect command to one ARGB effect channel (0x3B packet).
    pub async fn send_effect_argb(
        &self,
        effect_channel: u8,
        mode: u8,
        r: u8,
        g: u8,
        b: u8,
    ) -> Result<()> {
        let mut pkt = Self::make_packet(CMD_ADDR_EFFECT);
        pkt[2] = effect_channel;
        pkt[4] = mode;
        pkt[5] = r;
        pkt[6] = g;
        pkt[7] = b;
        self.transport.write(&pkt).await
    }
}

// ── Native effects ────────────────────────────────────────────────────────────

pub fn aura_native_effects() -> Vec<NativeEffect> {
    vec![
        NativeEffect {
            id: "off".into(),
            name: "Off".into(),
            params: vec![],
        },
        NativeEffect {
            id: "breathing".into(),
            name: "Breathing".into(),
            params: vec![EffectParamDescriptor {
                id: "color".into(),
                label: "Color".into(),
                kind: ParamKind::Color,
                default: EffectParamValue::Color(RgbColor {
                    r: 255,
                    g: 255,
                    b: 255,
                }),
            }],
        },
        NativeEffect {
            id: "spectrum_cycle".into(),
            name: "Spectrum Cycle".into(),
            params: vec![],
        },
        NativeEffect {
            id: "rainbow_wave".into(),
            name: "Rainbow Wave".into(),
            params: vec![],
        },
    ]
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    fn proto(responses: Vec<Vec<u8>>) -> AuraUsbProtocol<MockTransport> {
        AuraUsbProtocol {
            transport: MockTransport::new(responses),
        }
    }

    fn ch(direct_channel: u8, num_leds: u8) -> AuraChannel {
        AuraChannel {
            effect_channel: direct_channel + 1,
            direct_channel,
            num_leds,
        }
    }

    // ── make_packet ───────────────────────────────────────────────────────────

    #[test]
    fn make_packet_header_bytes() {
        let pkt = AuraUsbProtocol::<MockTransport>::make_packet(CMD_DIRECT);
        assert_eq!(pkt[0], AURA_HDR);
        assert_eq!(pkt[1], CMD_DIRECT);
        assert_eq!(pkt.len(), REPORT_SIZE);
        assert!(pkt[2..].iter().all(|&b| b == 0));
    }

    // ── send_direct ───────────────────────────────────────────────────────────

    #[tokio::test]
    async fn send_direct_single_led() {
        let p = proto(vec![]);
        let color = RgbColor {
            r: 0x11,
            g: 0x22,
            b: 0x33,
        };
        p.send_direct(0x00, &[color]).await.unwrap();

        let w = p.transport.written.lock().await;
        assert_eq!(w.len(), 1);
        let pkt = &w[0];
        assert_eq!(pkt[0], AURA_HDR);
        assert_eq!(pkt[1], CMD_DIRECT);
        assert_eq!(pkt[2], 0x00 | 0x80); // direct_channel=0, apply bit set
        assert_eq!(pkt[3], 0x00);
        assert_eq!(pkt[4], 0x01);
        assert_eq!(&pkt[5..8], &[0x11, 0x22, 0x33]);
    }

    #[tokio::test]
    async fn send_direct_apply_bit_only_on_last_packet() {
        let p = proto(vec![]);
        let colors = vec![RgbColor { r: 1, g: 2, b: 3 }; 21];
        p.send_direct(0x00, &colors).await.unwrap();

        let w = p.transport.written.lock().await;
        assert_eq!(w.len(), 2);
        // First packet: 20 LEDs, no apply bit
        assert_eq!(w[0][2] & 0x80, 0x00);
        assert_eq!(w[0][4], 20);
        // Second packet: 1 LED, apply bit set
        assert_eq!(w[1][2] & 0x80, 0x80);
        assert_eq!(w[1][3], 20); // offset
        assert_eq!(w[1][4], 1);
    }

    #[tokio::test]
    async fn send_direct_exactly_20_leds_in_one_packet() {
        let p = proto(vec![]);
        let colors = vec![RgbColor { r: 0, g: 0, b: 0 }; 20];
        p.send_direct(0x02, &colors).await.unwrap();

        let w = p.transport.written.lock().await;
        assert_eq!(w.len(), 1);
        assert_eq!(w[0][2] & 0x80, 0x80); // apply bit set
        assert_eq!(w[0][4], 20);
    }

    #[tokio::test]
    async fn send_direct_empty_sends_nothing() {
        let p = proto(vec![]);
        p.send_direct(0x00, &[]).await.unwrap();
        let w = p.transport.written.lock().await;
        assert_eq!(w.len(), 0);
    }

    // ── send_effect_argb ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn send_effect_argb_correct_bytes() {
        let p = proto(vec![]);
        p.send_effect_argb(0x01, MODE_RAINBOW_WAVE, 0x11, 0x22, 0x33)
            .await
            .unwrap();

        let w = p.transport.written.lock().await;
        assert_eq!(w.len(), 1);
        assert_eq!(w[0][1], CMD_ADDR_EFFECT);
        assert_eq!(w[0][2], 0x01);
        assert_eq!(w[0][4], MODE_RAINBOW_WAVE);
        assert_eq!(w[0][5], 0x11);
        assert_eq!(w[0][6], 0x22);
        assert_eq!(w[0][7], 0x33);
    }

    // ── set_channel_direct ────────────────────────────────────────────────────

    #[tokio::test]
    async fn set_channel_direct_sends_setmode_direct() {
        let p = proto(vec![]);
        p.set_channel_direct(2).await;

        let w = p.transport.written.lock().await;
        assert_eq!(w.len(), 1);
        assert_eq!(w[0][1], CMD_SETMODE);
        assert_eq!(w[0][2], 2);
        assert_eq!(w[0][5], MODE_DIRECT);
    }

    // ── get_firmware_version ──────────────────────────────────────────────────

    #[tokio::test]
    async fn get_firmware_version_parses_response() {
        let mut resp = vec![0u8; 65];
        resp[0] = AURA_HDR;
        resp[1] = 0x02;
        resp[2..10].copy_from_slice(b"AURA_1.0");
        let p = proto(vec![resp]);
        let fw = p.get_firmware_version().await;
        assert_eq!(fw.as_deref(), Some("AURA_1.0"));
    }

    #[tokio::test]
    async fn get_firmware_version_returns_none_on_wrong_id() {
        let mut resp = vec![0u8; 65];
        resp[0] = AURA_HDR;
        resp[1] = 0x99;
        // Provide 8 identical non-matching responses to exhaust the retry loop
        let p = proto(vec![resp.clone(); 8]);
        assert!(p.get_firmware_version().await.is_none());
    }

    // ── get_config_table ──────────────────────────────────────────────────────

    #[tokio::test]
    async fn get_config_table_parses_response() {
        let mut resp = vec![0u8; 65];
        resp[0] = AURA_HDR;
        resp[1] = 0x30;
        resp[4] = 0xAB; // first byte of config
        let p = proto(vec![resp]);
        let config = p.get_config_table().await.unwrap();
        assert_eq!(config[0], 0xAB);
        assert_eq!(config.len(), 60);
    }

    #[tokio::test]
    async fn get_config_table_returns_none_on_wrong_id() {
        let mut resp = vec![0u8; 65];
        resp[0] = AURA_HDR;
        resp[1] = 0x99;
        let p = proto(vec![resp.clone(); 8]);
        assert!(p.get_config_table().await.is_none());
    }

    // ── parse_config ─────────────────────────────────────────────────────────

    #[test]
    fn parse_config_extracts_argb_count_and_led_counts() {
        let mut config = [0u8; 60];
        config[CT_ARGB_CH] = 3;
        config[CT_MB_LEDS] = 5;
        // Per-channel LED counts at ct[4 + i*6 + 2]
        config[CT_CH_BLOCK_OFF + 0 * CT_CH_BLOCK_SZ + CT_CH_LEDS_OFF] = 120;
        config[CT_CH_BLOCK_OFF + 1 * CT_CH_BLOCK_SZ + CT_CH_LEDS_OFF] = 60;
        config[CT_CH_BLOCK_OFF + 2 * CT_CH_BLOCK_SZ + CT_CH_LEDS_OFF] = 0; // → default 30

        let (count, leds, mb) = parse_config(&config);
        assert_eq!(count, 3);
        assert_eq!(leds, vec![120, 60, DEFAULT_ARGB_LEDS]);
        // CT_MB_LEDS covers on-board + 12V header LEDs; returned unchanged
        assert_eq!(mb, 5);
    }

    #[test]
    fn parse_config_caps_leds_at_120() {
        let mut config = [0u8; 60];
        config[CT_ARGB_CH] = 1;
        config[CT_CH_BLOCK_OFF + CT_CH_LEDS_OFF] = 200; // over cap
        let (_, leds, _) = parse_config(&config);
        assert_eq!(leds[0], MAX_ARGB_LEDS);
    }

    // ── split_channels ────────────────────────────────────────────────────────

    #[test]
    fn split_channels_all_chainable_when_no_non_chainable_given() {
        let chs = vec![ch(0, 120), ch(1, 60)];
        let (desc, non_chainable, chainable) = split_channels(&chs, &[]);
        assert!(desc.zones.is_empty());
        assert!(non_chainable.is_empty());
        assert_eq!(chainable.len(), 2);
        assert!(chainable.contains_key("argb_0"));
        assert!(chainable.contains_key("argb_1"));
    }

    #[test]
    fn split_channels_has_four_native_effects() {
        let (desc, _, _) = split_channels(&[], &[]);
        assert_eq!(desc.native_effects.len(), 4);
        let ids: Vec<&str> = desc.native_effects.iter().map(|e| e.id.as_str()).collect();
        assert!(ids.contains(&"off"));
        assert!(ids.contains(&"breathing"));
        assert!(ids.contains(&"spectrum_cycle"));
        assert!(ids.contains(&"rainbow_wave"));
    }

    #[test]
    fn split_channels_non_chainable_channel_becomes_named_zone() {
        // Channels 0-2 chainable, channel 3 is a fixed logo zone.
        let chs = vec![ch(0, 30), ch(1, 30), ch(2, 30), ch(3, 1)];
        let (desc, non_chainable, chainable) = split_channels(&chs, &[(3, "logo", "ROG Logo")]);
        assert_eq!(desc.zones.len(), 1, "only logo is a zone");
        assert_eq!(desc.zones[0].id, "logo");
        assert_eq!(non_chainable, vec![3]);
        assert_eq!(chainable.len(), 3);
        assert!(chainable.contains_key("argb_0"));
        assert!(chainable.contains_key("argb_1"));
        assert!(chainable.contains_key("argb_2"));
        assert_eq!(chainable["argb_0"], 0);
    }

    #[test]
    fn split_channels_all_chainable_when_no_entry_matches() {
        let chs = vec![ch(0, 30), ch(1, 30), ch(2, 30), ch(3, 1)];
        let (desc, non_chainable, chainable) = split_channels(&chs, &[]);
        assert!(desc.zones.is_empty());
        assert!(non_chainable.is_empty());
        assert_eq!(chainable.len(), 4);
    }

    // ── send_direct offset overflow guard ─────────────────────────────────────

    #[test]
    fn send_direct_offsets_up_to_255_leds_fit_in_u8() {
        // With LEDS_PER_PACKET = 20, the first 13 batches (offsets 0..240) are within u8.
        for offset in (0usize..=240).step_by(LEDS_PER_PACKET) {
            assert!(
                u8::try_from(offset).is_ok(),
                "offset {offset} should fit in u8"
            );
        }
    }

    #[test]
    fn send_direct_offset_260_would_truncate_without_try_from() {
        // Batch 14 starts at offset 260, which silently wraps to 4 via `as u8`.
        let offset: usize = 260;
        assert_eq!(offset as u8, 4, "silent truncation confirmed");
        assert!(
            u8::try_from(offset).is_err(),
            "try_from correctly rejects offset {offset}"
        );
    }
}
