// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
//! NZXT Kraken AIO wire protocol.
//!
//! Two wire families exist, selected by `KrakenWire`:
//!   - X3  (0x2007/0x2014): 64-byte HID, `0x22 0x10` per-channel RGB, `0x2A 0x04` logo.
//!   - ZElite (0x3008+):   `0x26 0x14` combined ring+ext RGB. Command packets are
//!     64 bytes on the wire.
//!
//! Status push and pump/fan duty writes are shared across both families. The
//! LCD control channel (brightness, GIF/image buckets, streaming) lives in the
//! [`lcd`] submodule.

mod lcd;

use anyhow::Result;
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Mutex as TokioMutex;

use crate::drivers::{
    transports::{hid::HidTransport, Transport},
    vendors::generic::devices::common::TaskHandle,
    vendors::nzxt::protocols::NzxtBaseProtocol,
};
use halod_shared::types::RgbColor;

// ── Wire family ───────────────────────────────────────────────────────────────

/// Wire-protocol family for a Kraken model; selects HID report size and RGB packet format.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KrakenWire {
    /// X-series (0x2007, 0x2014). 64-byte HID; `0x22 0x10` ring/ext packets;
    /// `0x2A 0x04` logo. Ring and ext are written independently per channel.
    X3,
    /// Z-series and Elite (0x3008+). `0x26 0x14` combined ring+ext packet with
    /// a shared GRB cache for the co-send requirement.
    ZElite,
}

// ── Wire commands ─────────────────────────────────────────────────────────────

/// Named command prefixes.
mod cmd {
    /// Initialization handshake, written in order by `initialize()`.
    pub const INIT_SET: [u8; 5] = [0x70, 0x02, 0x01, 0xB8, 0x01];
    pub const INIT_FW_PUSH: [u8; 2] = [0x70, 0x01];
    pub const INIT_STATUS_PUSH: [u8; 2] = [0x10, 0x01];

    /// Status push report id (subtype `0x01` or `0x02` in byte 1).
    pub const STATUS_REPORT: u8 = 0x75;

    /// Speed-profile headers: `[0x72, channel, …]` then a 40-byte duty curve.
    pub const PUMP_DUTY: [u8; 4] = [0x72, 0x01, 0x00, 0x00];
    pub const FAN_DUTY: [u8; 4] = [0x72, 0x02, 0x01, 0x01];

    /// X3 (64-byte) lighting: data packets `0x22 0x10|n`, commit `0x22 0xA0`.
    pub const X3_LIGHTING: u8 = 0x22;
    pub const X3_DATA: u8 = 0x10;
    pub const X3_COMMIT: u8 = 0xA0;
    /// X3 logo LED header.
    pub const X3_LOGO: [u8; 2] = [0x2A, 0x04];

    /// Z/Elite combined ring+ext lighting header.
    pub const ZE_LIGHTING: [u8; 2] = [0x26, 0x14];
}

// ── Constants ─────────────────────────────────────────────────────────────────

/// Number of LEDs in the Z/Elite ring (used for the GRB wire buffer).
pub(crate) const RING_LED_COUNT: usize = 24;

const RING_BUF_SLOTS: usize = 40;
pub(crate) const RING_BUF_LEN: usize = RING_BUF_SLOTS * 3; // 120 bytes

/// Channel bytes used in `0x26 0x14` Z/Elite lighting packets.
pub(crate) const RING_CHANNEL_BYTE: u8 = 0x01;
pub(crate) const EXT_CHANNEL_BYTE: u8 = 0x02;

/// Temp range for the interpolated speed profile (inclusive, 20°C–59°C).
const PROFILE_TEMP_MIN: u8 = 20;
const PROFILE_TEMP_MAX: u8 = 59;
pub(crate) const PROFILE_LEN: usize = (PROFILE_TEMP_MAX - PROFILE_TEMP_MIN + 1) as usize; // 40

pub(crate) const PUMP_DUTY_MIN: u8 = 20;
const PUMP_DUTY_MAX: u8 = 100;

// ── Status ────────────────────────────────────────────────────────────────────

#[derive(Clone, Default, Debug)]
pub struct KrakenStatus {
    pub liquid_temp: f64,
    pub pump_rpm: u32,
    pub pump_duty: u8,
    pub fan_rpm: u32,
    pub fan_duty: u8,
}

// ── RGB cache ─────────────────────────────────────────────────────────────────

/// Wire-protocol-specific RGB state.
#[derive(Clone, Debug)]
pub enum KrakenRgbCache {
    X3,
    ZElite {
        ring_grb: Arc<TokioMutex<[u8; RING_BUF_LEN]>>,
        ext_grb: Arc<TokioMutex<Vec<u8>>>,
    },
}

impl KrakenRgbCache {
    pub fn for_wire(wire: KrakenWire) -> Self {
        match wire {
            KrakenWire::X3 => Self::X3,
            KrakenWire::ZElite => Self::ZElite {
                ring_grb: Arc::new(TokioMutex::new([0u8; RING_BUF_LEN])),
                ext_grb: Arc::new(TokioMutex::new(vec![])),
            },
        }
    }
}

// ── Protocol struct ───────────────────────────────────────────────────────────

#[derive(Clone)]
pub struct NzxtKrakenProtocol<T: Transport> {
    pub base: NzxtBaseProtocol<T>,
    pub wire: KrakenWire,
    pub rgb_cache: KrakenRgbCache,
    pub poll_task: Arc<TokioMutex<Option<TaskHandle>>>,
    pub status_cache: Arc<TokioMutex<KrakenStatus>>,
    /// Which bucket (0 or 1) is currently active on the device; `None` if no custom image is displayed.
    pub active_bucket: Arc<TokioMutex<Option<u8>>>,
    pub(crate) polling_paused: Arc<AtomicBool>,
    /// Serialises poll-task lifecycle: new task starts only after old one fully stops.
    poll_guard: Arc<TokioMutex<()>>,
    /// Persistent USB bulk transport — opened on first upload, reused after.
    /// Per-frame open/close via rusb is very slow (~100 ms) due to enumeration.
    pub bulk_transport:
        Arc<std::sync::Mutex<Option<crate::drivers::transports::usb_bulk::UsbBulkTransport>>>,
}

impl NzxtKrakenProtocol<HidTransport> {
    const REPORT_SIZE: usize = 64;
    const TIMEOUT_MS: i32 = 1000;

    pub fn open(path: &str, wire: KrakenWire) -> Result<Self> {
        Ok(Self {
            base: NzxtBaseProtocol::open(path, Self::REPORT_SIZE, Self::TIMEOUT_MS)?,
            wire,
            rgb_cache: KrakenRgbCache::for_wire(wire),
            poll_task: Arc::new(TokioMutex::new(None)),
            status_cache: Arc::new(TokioMutex::new(KrakenStatus::default())),
            active_bucket: Arc::new(TokioMutex::new(None)),
            polling_paused: Arc::new(AtomicBool::new(false)),
            poll_guard: Arc::new(TokioMutex::new(())),
            bulk_transport: Arc::new(std::sync::Mutex::new(None)),
        })
    }

    /// Background task that reads HID status pushes into `status_cache`.
    pub async fn resume_polling(&self) {
        // Abort any previous poll task before spawning a new one, then await
        // its full termination via `poll_guard` so two tasks never race on the
        // shared HID transport.
        let old_handle = self.poll_task.lock().await.take();
        drop(old_handle); // abort the old task
        let _guard = self.poll_guard.lock().await; // wait until old task actually stopped

        let transport_clone = self.base.transport.clone();
        let status_cache = Arc::clone(&self.status_cache);
        let paused = Arc::clone(&self.polling_paused);
        let guard = Arc::clone(&self.poll_guard);
        let handle = tokio::task::spawn(async move {
            // Hold the guard for the entire lifetime of the poll task so
            // `resume_polling` can await termination before spawning again.
            let _guard = guard.lock().await;
            loop {
                if !paused.load(Ordering::Relaxed) {
                    if let Ok(pkt) = transport_clone
                        .read_nonblocking(NzxtKrakenProtocol::<HidTransport>::READ_SIZE)
                        .await
                    {
                        if !pkt.is_empty() {
                            if let Some(status) =
                                NzxtKrakenProtocol::<HidTransport>::parse_status(&pkt)
                            {
                                *status_cache.lock().await = status;
                            }
                        }
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });
        *self.poll_task.lock().await = Some(TaskHandle::new(handle));
    }
}

// ── Shared methods (all transport types) ─────────────────────────────────────

impl<T: Transport> NzxtKrakenProtocol<T> {
    const READ_SIZE: usize = 64;

    /// Sends initialization sequence and returns firmware version string.
    pub async fn initialize(&self) -> Result<String> {
        self.base.write(&cmd::INIT_SET).await?;
        self.base.write(&cmd::INIT_FW_PUSH).await?;
        self.base.write(&cmd::INIT_STATUS_PUSH).await?;
        self.base.get_firmware_version().await
    }

    /// Parses a status push packet (`0x75 0x01/02`). Returns `None` if the
    /// packet is not a status report or the temperature sentinel `0xFF 0xFF`
    /// is present.
    pub fn parse_status(pkt: &[u8]) -> Option<KrakenStatus> {
        if pkt.len() < 26 {
            return None;
        }
        if pkt[0] != cmd::STATUS_REPORT || (pkt[1] != 0x01 && pkt[1] != 0x02) {
            return None;
        }
        if pkt[15] == 0xFF && pkt[16] == 0xFF {
            return None;
        }
        // Clamp instead of rejecting: some firmware reports fractional digits above 9.
        let frac = pkt[16].min(9);
        Some(KrakenStatus {
            liquid_temp: pkt[15] as f64 + frac as f64 / 10.0,
            pump_rpm: u16::from_le_bytes([pkt[17], pkt[18]]) as u32,
            pump_duty: pkt[19],
            fan_rpm: u16::from_le_bytes([pkt[23], pkt[24]]) as u32,
            fan_duty: pkt[25],
        })
    }

    fn build_fixed_profile(duty: u8, min_duty: u8) -> [u8; PROFILE_LEN] {
        let clamped = duty.clamp(min_duty, 100);
        [clamped; PROFILE_LEN]
    }

    pub async fn write_pump_duty(&self, duty: u8) -> Result<()> {
        let profile = Self::build_fixed_profile(duty, PUMP_DUTY_MIN);
        let mut pkt = Vec::with_capacity(4 + PROFILE_LEN);
        pkt.extend_from_slice(&cmd::PUMP_DUTY);
        pkt.extend_from_slice(&profile);
        self.base.write(&pkt).await?;
        self.status_cache.lock().await.pump_duty = duty.clamp(PUMP_DUTY_MIN, PUMP_DUTY_MAX);
        Ok(())
    }

    pub async fn write_fan_duty(&self, duty: u8) -> Result<()> {
        let profile = Self::build_fixed_profile(duty, 0);
        let mut pkt = Vec::with_capacity(4 + PROFILE_LEN);
        pkt.extend_from_slice(&cmd::FAN_DUTY);
        pkt.extend_from_slice(&profile);
        self.base.write(&pkt).await?;
        self.status_cache.lock().await.fan_duty = duty.min(100);
        Ok(())
    }

    /// Builds a GRB buffer for the Z/Elite ring (24 LEDs, 40 wire slots).
    /// LEDs occupy slots 0–23 in clockwise order from 12-o'clock; slots 24–39
    /// are zero-padded.
    fn build_ring_grb(colors: &[RgbColor]) -> [u8; RING_BUF_LEN] {
        let mut buf = [0u8; RING_BUF_LEN];
        for (i, color) in colors.iter().enumerate().take(RING_LED_COUNT) {
            buf[i * 3] = color.g;
            buf[i * 3 + 1] = color.r;
            buf[i * 3 + 2] = color.b;
        }
        buf
    }

    pub async fn write_ring_frame(&self, colors: &[RgbColor]) -> Result<()> {
        match &self.rgb_cache {
            KrakenRgbCache::X3 => self.write_x3_channel(0x02, colors).await,
            KrakenRgbCache::ZElite { ring_grb, ext_grb } => {
                *ring_grb.lock().await = Self::build_ring_grb(colors);
                Self::send_ze_channels(&self.base, ring_grb, ext_grb).await
            }
        }
    }

    pub async fn write_ext_frame(&self, colors: &[RgbColor]) -> Result<()> {
        match &self.rgb_cache {
            KrakenRgbCache::X3 => self.write_x3_channel(0x01, colors).await,
            KrakenRgbCache::ZElite { ring_grb, ext_grb } => {
                let mut buf = Vec::with_capacity(colors.len() * 3);
                for c in colors {
                    buf.extend_from_slice(&[c.g, c.r, c.b]);
                }
                *ext_grb.lock().await = buf;
                Self::send_ze_channels(&self.base, ring_grb, ext_grb).await
            }
        }
    }

    /// Writes the X-series logo LED (`0x2A 0x04`). Only call when `has_logo`
    /// is true; callers are responsible for checking the device profile.
    pub async fn write_logo(&self, color: RgbColor) -> Result<()> {
        debug_assert_eq!(
            self.wire,
            KrakenWire::X3,
            "write_logo called on non-X3 device"
        );
        let mut pkt = [0u8; 64];
        pkt[..2].copy_from_slice(&cmd::X3_LOGO);
        pkt[2] = 0x04;
        pkt[3] = 0x04;
        pkt[4] = 0x00;
        pkt[5] = 0x32;
        pkt[6] = 0x00;
        pkt[7] = color.g;
        pkt[8] = color.r;
        pkt[9] = color.b;
        pkt[56] = 0x01;
        pkt[57] = 0x00;
        pkt[58] = 0x01;
        pkt[59] = 0x03;
        self.base.write(&pkt).await
    }

    /// Sends two 64-byte `0x22 0x10` data packets plus a `0x22 0xA0` commit
    /// for the given X-series channel byte (0x02 = ring, 0x01 = ext).
    async fn write_x3_channel(&self, ch: u8, colors: &[RgbColor]) -> Result<()> {
        let mut grb = Vec::with_capacity(colors.len() * 3);
        for c in colors {
            grb.extend_from_slice(&[c.g, c.r, c.b]);
        }
        for pkt_num in 0u8..2 {
            let offset = pkt_num as usize * 60;
            let chunk: &[u8] = if offset < grb.len() {
                &grb[offset..grb.len().min(offset + 60)]
            } else {
                &[]
            };
            let mut pkt = vec![cmd::X3_LIGHTING, cmd::X3_DATA | pkt_num, ch, 0x00];
            pkt.extend_from_slice(chunk);
            pkt.resize(64, 0);
            self.base.write(&pkt).await?;
        }
        self.base
            .write(&[
                cmd::X3_LIGHTING,
                cmd::X3_COMMIT,
                ch,
                0x00,
                0x01,
                0x00,
                0x00,
                0x28,
                0x00,
                0x00,
                0x80,
                0x00,
                0x32,
                0x00,
                0x00,
                0x01,
            ])
            .await
    }

    /// Sends the Z/Elite ring+ext co-send: ring is always written; ext is
    /// written only when non-empty (accessory connected).
    async fn send_ze_channels(
        base: &NzxtBaseProtocol<T>,
        ring_grb: &TokioMutex<[u8; RING_BUF_LEN]>,
        ext_grb: &TokioMutex<Vec<u8>>,
    ) -> Result<()> {
        // Release both locks before I/O to avoid holding them across writes.
        let ring = {
            let guard = ring_grb.lock().await;
            *guard
        };
        let ext = {
            let guard = ext_grb.lock().await;
            guard.clone()
        };
        let mut ring_pkt = Vec::with_capacity(4 + RING_BUF_LEN);
        ring_pkt.extend_from_slice(&cmd::ZE_LIGHTING);
        ring_pkt.extend_from_slice(&[RING_CHANNEL_BYTE, RING_CHANNEL_BYTE]);
        ring_pkt.extend_from_slice(&ring);
        base.write(&ring_pkt).await?;
        if !ext.is_empty() {
            let mut ext_pkt = Vec::with_capacity(4 + ext.len());
            ext_pkt.extend_from_slice(&cmd::ZE_LIGHTING);
            ext_pkt.extend_from_slice(&[EXT_CHANNEL_BYTE, EXT_CHANNEL_BYTE]);
            ext_pkt.extend_from_slice(&ext);
            base.write(&ext_pkt).await?;
        }
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    fn protocol(responses: Vec<Vec<u8>>) -> NzxtKrakenProtocol<MockTransport> {
        protocol_for(KrakenWire::ZElite, responses)
    }

    fn protocol_for(
        wire: KrakenWire,
        responses: Vec<Vec<u8>>,
    ) -> NzxtKrakenProtocol<MockTransport> {
        NzxtKrakenProtocol {
            base: NzxtBaseProtocol::new(MockTransport::new(responses)),
            wire,
            rgb_cache: KrakenRgbCache::for_wire(wire),
            poll_task: Arc::new(TokioMutex::new(None)),
            status_cache: Arc::new(TokioMutex::new(KrakenStatus::default())),
            active_bucket: Arc::new(TokioMutex::new(None)),
            polling_paused: Arc::new(AtomicBool::new(false)),
            poll_guard: Arc::new(TokioMutex::new(())),
            bulk_transport: Arc::new(std::sync::Mutex::new(None)),
        }
    }

    // ── parse_status ──────────────────────────────────────────────────────

    #[test]
    fn parse_status_returns_correct_fields() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x75;
        pkt[1] = 0x01;
        pkt[15] = 28;
        pkt[16] = 5; // → 28.5°C
        pkt[17] = 0xE8;
        pkt[18] = 0x03; // pump_rpm LE: 1000
        pkt[19] = 50;
        pkt[23] = 0x58;
        pkt[24] = 0x02; // fan_rpm LE: 600
        pkt[25] = 40;
        let s = NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).unwrap();
        assert!((s.liquid_temp - 28.5).abs() < 1e-9);
        assert_eq!(s.pump_rpm, 1000);
        assert_eq!(s.pump_duty, 50);
        assert_eq!(s.fan_rpm, 600);
        assert_eq!(s.fan_duty, 40);
    }

    #[test]
    fn parse_status_rejects_short_packet() {
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&[0x75, 0x01]).is_none());
    }

    #[test]
    fn parse_status_accepts_subtype_02() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x75;
        pkt[1] = 0x02;
        pkt[15] = 29;
        pkt[16] = 5;
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).is_some());
    }

    #[test]
    fn parse_status_rejects_wrong_prefix() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x67;
        pkt[1] = 0x01;
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).is_none());
    }

    #[test]
    fn parse_status_rejects_unknown_subtype() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x75;
        pkt[1] = 0x03;
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).is_none());
    }

    #[test]
    fn parse_status_ignores_0xff_temperature() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x75;
        pkt[1] = 0x01;
        pkt[15] = 0xFF;
        pkt[16] = 0xFF;
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).is_none());
    }

    // ── pump profile ──────────────────────────────────────────────────────

    #[test]
    fn pump_profile_clamps_to_min_duty() {
        let p = NzxtKrakenProtocol::<MockTransport>::build_fixed_profile(10, PUMP_DUTY_MIN);
        assert!(p.iter().all(|&d| d == PUMP_DUTY_MIN));
    }

    #[test]
    fn pump_profile_all_set_to_target_duty() {
        let p = NzxtKrakenProtocol::<MockTransport>::build_fixed_profile(60, PUMP_DUTY_MIN);
        assert!(p.iter().all(|&d| d == 60));
        assert_eq!(p.len(), PROFILE_LEN);
    }

    // ── ring GRB mapping ──────────────────────────────────────────────────

    #[test]
    fn build_ring_grb_maps_logical_led_to_sequential_slot() {
        let mut colors = [RgbColor { r: 0, g: 0, b: 0 }; RING_LED_COUNT];
        colors[0] = RgbColor {
            r: 10,
            g: 20,
            b: 30,
        };
        colors[RING_LED_COUNT - 1] = RgbColor {
            r: 40,
            g: 50,
            b: 60,
        };
        let buf = NzxtKrakenProtocol::<MockTransport>::build_ring_grb(&colors);
        assert_eq!(&buf[0..3], &[20, 10, 30]);
        let last = RING_LED_COUNT - 1;
        assert_eq!(&buf[last * 3..last * 3 + 3], &[50, 40, 60]);
    }

    #[test]
    fn build_ring_grb_buf_length() {
        let colors = [RgbColor { r: 0, g: 0, b: 0 }; RING_LED_COUNT];
        assert_eq!(
            NzxtKrakenProtocol::<MockTransport>::build_ring_grb(&colors).len(),
            RING_BUF_LEN
        );
    }

    // ── X3 ring / logo / ext ──────────────────────────────────────────────

    #[tokio::test]
    async fn x3_write_ring_sends_two_packets_plus_commit() {
        let p = protocol_for(KrakenWire::X3, vec![]);
        let colors = [RgbColor {
            r: 10,
            g: 20,
            b: 30,
        }; 8];
        p.write_ring_frame(&colors).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 3);
        assert_eq!(&written[0][0..4], &[0x22, 0x10, 0x02, 0x00]);
        assert_eq!(written[0].len(), 64);
        assert_eq!(&written[0][4..7], &[20, 10, 30]); // GRB
        assert_eq!(&written[1][0..4], &[0x22, 0x11, 0x02, 0x00]);
        assert_eq!(&written[2][0..4], &[0x22, 0xA0, 0x02, 0x00]);
    }

    #[tokio::test]
    async fn x3_write_logo_sends_correct_packet() {
        let p = protocol_for(KrakenWire::X3, vec![]);
        p.write_logo(RgbColor {
            r: 255,
            g: 0,
            b: 128,
        })
        .await
        .unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 1);
        let pkt = &written[0];
        assert_eq!(pkt.len(), 64);
        assert_eq!(&pkt[0..7], &[0x2A, 0x04, 0x04, 0x04, 0x00, 0x32, 0x00]);
        assert_eq!(pkt[7], 0); // G
        assert_eq!(pkt[8], 255); // R
        assert_eq!(pkt[9], 128); // B
        assert_eq!(&pkt[56..60], &[0x01, 0x00, 0x01, 0x03]);
    }

    #[tokio::test]
    async fn x3_write_ext_sends_two_packets_plus_commit() {
        let p = protocol_for(KrakenWire::X3, vec![]);
        let colors = [RgbColor { r: 1, g: 2, b: 3 }; 4];
        p.write_ext_frame(&colors).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 3);
        assert_eq!(&written[0][0..4], &[0x22, 0x10, 0x01, 0x00]);
        assert_eq!(written[0].len(), 64);
        assert_eq!(&written[1][0..4], &[0x22, 0x11, 0x01, 0x00]);
        assert_eq!(&written[2][0..4], &[0x22, 0xA0, 0x01, 0x00]);
    }

    // ── Z/Elite ring / ext ────────────────────────────────────────────────

    #[tokio::test]
    async fn ze_write_ring_sends_ring_packet_with_correct_header() {
        let p = protocol(vec![]);
        let colors = [RgbColor { r: 0, g: 0, b: 0 }; RING_LED_COUNT];
        p.write_ring_frame(&colors).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 1); // ext is empty — only ring sent
        assert_eq!(
            &written[0][0..4],
            &[0x26, 0x14, RING_CHANNEL_BYTE, RING_CHANNEL_BYTE]
        );
    }

    #[tokio::test]
    async fn ze_write_ext_sends_both_when_ring_cached() {
        let p = protocol(vec![]);
        p.write_ring_frame(&[RgbColor { r: 1, g: 2, b: 3 }; RING_LED_COUNT])
            .await
            .unwrap();
        p.write_ext_frame(&[RgbColor { r: 4, g: 5, b: 6 }; 8])
            .await
            .unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 3); // ring + ring+ext
        assert_eq!(
            &written[1][0..4],
            &[0x26, 0x14, RING_CHANNEL_BYTE, RING_CHANNEL_BYTE]
        );
        assert_eq!(
            &written[2][0..4],
            &[0x26, 0x14, EXT_CHANNEL_BYTE, EXT_CHANNEL_BYTE]
        );
    }

    // ── pump / fan duty ───────────────────────────────────────────────────

    #[tokio::test]
    async fn write_pump_duty_sends_correct_header() {
        let p = protocol(vec![]);
        p.write_pump_duty(60).await.unwrap();
        let written = p.base.transport.written.lock().await;
        let pkt = &written[0];
        assert_eq!(&pkt[0..4], &[0x72, 0x01, 0x00, 0x00]);
        assert_eq!(pkt.len(), 4 + PROFILE_LEN);
        assert!(pkt[4..].iter().all(|&d| d == 60));
    }

    #[tokio::test]
    async fn write_fan_duty_sends_correct_header() {
        let p = protocol(vec![]);
        p.write_fan_duty(45).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(&written[0][0..4], &[0x72, 0x02, 0x01, 0x01]);
        assert!(written[0][4..].iter().all(|&d| d == 45));
    }
}
