// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
/// NZXT Kraken AIO wire protocol.
///
/// Two wire families exist, selected by `KrakenWire`:
///   - X3  (0x2007/0x2014): 64-byte HID, `0x22 0x10` per-channel RGB, `0x2A 0x04` logo.
///   - ZElite (0x3008+):   512-byte HID, `0x26 0x14` combined ring+ext RGB.
///
/// Status push, pump/fan duty writes, and the LCD bucket pipeline are shared
/// across both families.
use anyhow::Result;
use q565::{
    encode::Q565EncodeContext,
    utils::{encode_rgb565_unchecked, rgb888_to_rgb565},
};
use std::sync::{
    atomic::{AtomicBool, Ordering},
    Arc,
};
use tokio::sync::Mutex as TokioMutex;

use crate::drivers::{
    vendors::nzxt::protocols::NzxtBaseProtocol,
    transports::{hid::HidTransport, Transport},
};
use halod_protocol::types::RgbColor;

// ── Wire family ───────────────────────────────────────────────────────────────

/// Wire-protocol family for a Kraken model. Controls HID report size and
/// RGB packet format. Adding a new model requires only a row in
/// `KRAKEN_PROFILES` in `kraken.rs` — the protocol is selected from this field.
#[derive(Copy, Clone, PartialEq, Eq, Debug)]
pub enum KrakenWire {
    /// X-series (0x2007, 0x2014). 64-byte HID; `0x22 0x10` ring/ext packets;
    /// `0x2A 0x04` logo. Ring and ext are written independently per channel.
    X3,
    /// Z-series and Elite (0x3008+). 512-byte HID; `0x26 0x14` combined
    /// ring+ext packet with a shared GRB cache for the co-send requirement.
    ZElite,
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

const LCD_TOTAL_MEMORY_KB: u32 = 24320;
const LCD_BULK_CHUNK: usize = 1024 * 1024 * 2; // 2 MB

/// LCD memory is split into two equal ping-pong buckets. Any single upload
/// must fit within one bucket. Fixed partition avoids overrun when frame sizes
/// change across uploads (GIFs are much larger than one RGBA32 frame).
pub(crate) const BUCKET_SIZE_KB: u32 = LCD_TOTAL_MEMORY_KB / 2;

// ── Status ────────────────────────────────────────────────────────────────────

#[derive(Clone, Default)]
pub struct KrakenStatus {
    pub liquid_temp: f64,
    pub pump_rpm: u32,
    pub pump_duty: u8,
    pub fan_rpm: u32,
    pub fan_duty: u8,
}

// ── RGB cache ─────────────────────────────────────────────────────────────────

/// Wire-protocol-specific RGB state. X3 needs no shared cache; Z/Elite must
/// re-send the ring buffer with every ext write and vice versa.
#[derive(Clone)]
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
    pub poll_task: Arc<TokioMutex<Option<tokio::task::JoinHandle<()>>>>,
    pub status_cache: Arc<TokioMutex<KrakenStatus>>,
    /// Which bucket (0 or 1) is currently active on the device. `None` means
    /// no custom image is displayed; next upload must do a full reset first.
    pub active_bucket: Arc<TokioMutex<Option<u8>>>,
    polling_paused: Arc<AtomicBool>,
    /// Persistent USB bulk transport — opened on first upload, reused after.
    /// Per-frame open/close via rusb is very slow (~100 ms) due to enumeration.
    pub bulk_transport: Arc<std::sync::Mutex<Option<crate::drivers::transports::usb_bulk::UsbBulkTransport>>>,
}

impl NzxtKrakenProtocol<HidTransport> {
    const REPORT_SIZE_LARGE: usize = 512; // Z/Elite
    const REPORT_SIZE_SMALL: usize = 64;  // X3
    const TIMEOUT_MS: i32 = 1000;

    pub fn open(path: &str, wire: KrakenWire) -> Result<Self> {
        let report_size = match wire {
            KrakenWire::X3 => Self::REPORT_SIZE_SMALL,
            KrakenWire::ZElite => Self::REPORT_SIZE_LARGE,
        };
        Ok(Self {
            base: NzxtBaseProtocol::open(path, report_size, Self::TIMEOUT_MS)?,
            wire,
            rgb_cache: KrakenRgbCache::for_wire(wire),
            poll_task: Arc::new(TokioMutex::new(None)),
            status_cache: Arc::new(TokioMutex::new(KrakenStatus::default())),
            active_bucket: Arc::new(TokioMutex::new(None)),
            polling_paused: Arc::new(AtomicBool::new(false)),
            bulk_transport: Arc::new(std::sync::Mutex::new(None)),
        })
    }
}

// ── Shared methods (all transport types) ─────────────────────────────────────

impl<T: Transport> NzxtKrakenProtocol<T> {
    const READ_SIZE: usize = 64;

    /// Sends initialization sequence and returns firmware version string.
    pub async fn initialize(&self) -> Result<String> {
        self.base.write(&[0x70, 0x02, 0x01, 0xB8, 0x01]).await?;
        self.base.write(&[0x70, 0x01]).await?;
        self.base.write(&[0x10, 0x01]).await?;
        self.base.get_firmware_version().await
    }

    /// Parses a status push packet (`0x75 0x01/02`). Returns `None` if the
    /// packet is not a status report or the temperature sentinel `0xFF 0xFF`
    /// is present.
    pub fn parse_status(pkt: &[u8]) -> Option<KrakenStatus> {
        if pkt.len() < 26 {
            return None;
        }
        if pkt[0] != 0x75 || (pkt[1] != 0x01 && pkt[1] != 0x02) {
            return None;
        }
        if pkt[15] == 0xFF && pkt[16] == 0xFF {
            return None;
        }
        Some(KrakenStatus {
            liquid_temp: pkt[15] as f64 + pkt[16] as f64 / 10.0,
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
        pkt.extend_from_slice(&[0x72, 0x01, 0x00, 0x00]);
        pkt.extend_from_slice(&profile);
        self.base.write(&pkt).await?;
        self.status_cache.lock().await.pump_duty = duty.clamp(PUMP_DUTY_MIN, PUMP_DUTY_MAX);
        Ok(())
    }

    pub async fn write_fan_duty(&self, duty: u8) -> Result<()> {
        let profile = Self::build_fixed_profile(duty, 0);
        let mut pkt = Vec::with_capacity(4 + PROFILE_LEN);
        pkt.extend_from_slice(&[0x72, 0x02, 0x01, 0x01]);
        pkt.extend_from_slice(&profile);
        self.base.write(&pkt).await?;
        self.status_cache.lock().await.fan_duty = duty.clamp(0, 100);
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
        debug_assert_eq!(self.wire, KrakenWire::X3, "write_logo called on non-X3 device");
        let mut pkt = [0u8; 64];
        pkt[0] = 0x2A; pkt[1] = 0x04; pkt[2] = 0x04; pkt[3] = 0x04;
        pkt[4] = 0x00; pkt[5] = 0x32; pkt[6] = 0x00;
        pkt[7] = color.g; pkt[8] = color.r; pkt[9] = color.b;
        pkt[56] = 0x01; pkt[57] = 0x00; pkt[58] = 0x01; pkt[59] = 0x03;
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
            let mut pkt = vec![0x22, 0x10 | pkt_num, ch, 0x00];
            pkt.extend_from_slice(chunk);
            pkt.resize(64, 0);
            self.base.write(&pkt).await?;
        }
        self.base
            .write(&[0x22, 0xA0, ch, 0x00, 0x01, 0x00, 0x00, 0x28, 0x00, 0x00, 0x80, 0x00, 0x32, 0x00, 0x00, 0x01])
            .await
    }

    /// Sends the Z/Elite ring+ext co-send: ring is always written; ext is
    /// written only when non-empty (accessory connected).
    async fn send_ze_channels(
        base: &NzxtBaseProtocol<T>,
        ring_grb: &Arc<TokioMutex<[u8; RING_BUF_LEN]>>,
        ext_grb: &Arc<TokioMutex<Vec<u8>>>,
    ) -> Result<()> {
        let ring = *ring_grb.lock().await;
        let ext = ext_grb.lock().await.clone();
        let mut ring_pkt = Vec::with_capacity(4 + RING_BUF_LEN);
        ring_pkt.extend_from_slice(&[0x26, 0x14, RING_CHANNEL_BYTE, RING_CHANNEL_BYTE]);
        ring_pkt.extend_from_slice(&ring);
        base.write(&ring_pkt).await?;
        if !ext.is_empty() {
            let mut ext_pkt = Vec::with_capacity(4 + ext.len());
            ext_pkt.extend_from_slice(&[0x26, 0x14, EXT_CHANNEL_BYTE, EXT_CHANNEL_BYTE]);
            ext_pkt.extend_from_slice(&ext);
            base.write(&ext_pkt).await?;
        }
        Ok(())
    }

    /// Send brightness + rotation config packet to the device.
    pub async fn write_screen_config(&self, brightness: u8, rotation_degrees: u32) -> Result<()> {
        let rot_idx = ((rotation_degrees / 90) % 4) as u8;
        self.base
            .write(&[0x30, 0x02, 0x01, brightness, 0x00, 0x00, 0x01, rot_idx])
            .await
    }

    /// Encodes a raw RGBA8 frame as a Q565 stream (the codec NZXT CAM uses
    /// for the type-0x08 LCD path). The returned bytes are the complete
    /// type-0x08 payload including magic, dimensions, and `OP_END`.
    pub fn rgba_to_q565_payload(rgba: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
        let pixels = (width as usize) * (height as usize);
        let expected = pixels * 4;
        if rgba.len() != expected {
            anyhow::bail!(
                "RGBA frame size mismatch: got {} bytes, expected {expected} for {width}x{height}",
                rgba.len()
            );
        }
        if width > u16::MAX as u32 || height > u16::MAX as u32 {
            anyhow::bail!("frame {width}x{height} exceeds Q565 u16 dimension limit");
        }
        let rgb565: Vec<u16> = rgba
            .chunks_exact(4)
            .map(|p| encode_rgb565_unchecked(rgb888_to_rgb565([p[0], p[1], p[2]])))
            .collect();
        let mut out = Vec::with_capacity(8 + pixels);
        if !Q565EncodeContext::encode_to_vec(width as u16, height as u16, &rgb565, &mut out) {
            anyhow::bail!("Q565 encode failed for {width}x{height}");
        }
        Ok(out)
    }

    /// Builds the 20-byte bulk header URB for a streaming frame.
    pub fn stream_bulk_header(payload_len: u32) -> [u8; 20] {
        [
            0x12, 0xFA, 0x01, 0xE8, 0xAB, 0xCD, 0xEF, 0x98, 0x76, 0x54, 0x32, 0x10,
            0x08, 0x00, 0x00, 0x00,
            (payload_len & 0xFF) as u8,
            ((payload_len >> 8) & 0xFF) as u8,
            ((payload_len >> 16) & 0xFF) as u8,
            ((payload_len >> 24) & 0xFF) as u8,
        ]
    }
}

// ── HidTransport-only methods (LCD, polling) ──────────────────────────────────

impl NzxtKrakenProtocol<HidTransport> {
    /// Reads brightness + rotation from the device.
    /// Returns `(brightness, rotation_degrees)`.
    pub async fn read_lcd_state(&self) -> Option<(u8, u32)> {
        self.base.write(&[0x30, 0x01]).await.ok()?;
        let pkt = self
            .base
            .transport
            .read_matching(64, |p| p.len() >= 27 && p[0] == 0x31 && p[1] == 0x01, 8)
            .await?;
        let brightness = pkt[0x18];
        let rot_idx = pkt[0x1A];
        Some((brightness, (rot_idx as u32).min(3) * 90))
    }

    /// Switches the LCD back to the device's built-in default display.
    pub async fn switch_to_default_display(&self) -> Result<()> {
        self.base.write(&[0x38, 0x01, 0x02, 0x00]).await?;
        self.base
            .transport
            .read_matching(64, |p| p.len() >= 2 && p[0] == 0x39 && p[1] == 0x01, 16)
            .await;
        Ok(())
    }

    pub async fn resume_polling(&self) {
        let transport_clone = self.base.transport.clone();
        let status_cache = Arc::clone(&self.status_cache);
        let paused = Arc::clone(&self.polling_paused);
        let handle = tokio::task::spawn(async move {
            loop {
                if !paused.load(Ordering::Relaxed) {
                    match transport_clone
                        .read_nonblocking(NzxtKrakenProtocol::<HidTransport>::READ_SIZE)
                        .await
                    {
                        Ok(pkt) if !pkt.is_empty() => {
                            if let Some(status) =
                                NzxtKrakenProtocol::<HidTransport>::parse_status(&pkt)
                            {
                                *status_cache.lock().await = status;
                            }
                        }
                        _ => {}
                    }
                }
                tokio::time::sleep(tokio::time::Duration::from_millis(10)).await;
            }
        });
        *self.poll_task.lock().await = Some(handle);
    }

    /// Uploads an animated GIF into a device memory bucket. Pauses the HID
    /// poll task while running.
    pub async fn upload_gif(
        &self,
        vid: u16,
        pid: u16,
        raw_bytes: &[u8],
        width: u32,
        height: u32,
    ) -> Result<()> {
        let t0 = std::time::Instant::now();
        let raw = raw_bytes.to_vec();
        let prepared = tokio::task::spawn_blocking(move || {
            crate::usecases::lcd::resize_gif(&raw, width, height)
        })
        .await??;
        log::debug!(
            "[Kraken LCD] GIF prepared in {}ms ({} bytes)",
            t0.elapsed().as_millis(),
            prepared.len()
        );
        let data_len = prepared.len() as u32;
        let bulk_info: [u8; 8] = [
            0x01, 0x00, 0x00, 0x00,
            (data_len & 0xFF) as u8,
            ((data_len >> 8) & 0xFF) as u8,
            ((data_len >> 16) & 0xFF) as u8,
            ((data_len >> 24) & 0xFF) as u8,
        ];
        self.polling_paused.store(true, Ordering::Relaxed);
        let result = self.run_bucket_pipeline(vid, pid, &prepared, &bulk_info).await;
        self.polling_paused.store(false, Ordering::Relaxed);
        log::debug!("[Kraken LCD] GIF upload total: {}ms", t0.elapsed().as_millis());
        result
    }

    /// Pushes one frame via the type-0x08 streaming path. `payload` must be a
    /// complete Q565 file. Used for both live LCD-engine frames and one-shot
    /// static-image uploads.
    pub async fn stream_frame(&self, vid: u16, pid: u16, payload: &[u8]) -> Result<()> {
        self.polling_paused.store(true, Ordering::Relaxed);
        let result = self.run_stream_frame(vid, pid, payload).await;
        self.polling_paused.store(false, Ordering::Relaxed);
        result
    }

    async fn drain_hid_nonblocking(&self) {
        for _ in 0..64 {
            match self.base.transport.read_nonblocking(64).await {
                Ok(p) if !p.is_empty() => continue,
                _ => break,
            }
        }
    }

    async fn run_stream_frame(&self, vid: u16, pid: u16, payload: &[u8]) -> Result<()> {
        use crate::drivers::transports::usb_bulk::UsbBulkTransport;

        let t = std::time::Instant::now();
        let header = Self::stream_bulk_header(payload.len() as u32);

        // Drain HID ACKs queued from prior frames so the firmware does not
        // desync (>~19 unread `0x37 02` ACKs → artifacts then firmware crash).
        self.drain_hid_nonblocking().await;

        self.base.write(&[0x36, 0x01, 0x00, 0x01, 0x08]).await?;
        self.base
            .transport
            .read_matching(64, |p| p.len() >= 2 && p[0] == 0x37 && p[1] == 0x01, 8)
            .await;

        let bulk = Arc::clone(&self.bulk_transport);
        let header_c = header.to_vec();
        let data_c = payload.to_vec();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut guard = bulk.lock().unwrap();
            if guard.is_none() {
                log::info!("[Kraken LCD] opening persistent USB bulk transport");
                *guard = Some(UsbBulkTransport::open(vid, pid)?);
            }
            let transport = guard.as_ref().unwrap();
            transport.write(&header_c)?;
            for chunk in data_c.chunks(LCD_BULK_CHUNK) {
                transport.write(chunk)?;
            }
            Ok(())
        })
        .await??;

        self.base.write(&[0x36, 0x02]).await?;
        log::trace!("[11] frame sent in {}µs", t.elapsed().as_micros());
        Ok(())
    }

    async fn full_reset(&self) -> Result<()> {
        log::debug!("[Kraken LCD] full_reset: switching to default display");
        self.base.write(&[0x38, 0x01, 0x02, 0x00]).await?;
        self.base
            .transport
            .read_matching(64, |p| p.len() >= 2 && p[0] == 0x39 && p[1] == 0x01, 8)
            .await;
        for bi in 0u8..16 {
            self.base.write(&[0x32, 0x02, bi]).await?;
            self.base
                .transport
                .read_matching(64, |p| p.len() >= 2 && p[0] == 0x33 && p[1] == 0x02, 8)
                .await;
        }
        log::debug!("[Kraken LCD] full_reset: all 16 buckets deleted");
        Ok(())
    }

    async fn run_bucket_pipeline(
        &self,
        vid: u16,
        pid: u16,
        data: &[u8],
        bulk_info: &[u8; 8],
    ) -> Result<()> {
        use crate::drivers::transports::usb_bulk::UsbBulkTransport;

        let tp = std::time::Instant::now();
        let header: Vec<u8> = {
            let mut h = vec![0x12, 0xFA, 0x01, 0xE8, 0xAB, 0xCD, 0xEF, 0x98, 0x76, 0x54, 0x32, 0x10];
            h.extend_from_slice(bulk_info);
            h
        };
        let total_len = (header.len() + data.len()) as u32;
        let data_kb = (total_len + 1023) / 1024;

        if data_kb >= BUCKET_SIZE_KB {
            anyhow::bail!("LCD image too large: {data_kb} KB exceeds bucket size {BUCKET_SIZE_KB} KB");
        }

        let mut active = self.active_bucket.lock().await;
        let (bucket_idx, mem_start_kb) = if active.is_none() {
            self.full_reset().await?;
            log::debug!("[Kraken LCD timing] full_reset: {}ms", tp.elapsed().as_millis());
            (0u8, 0u32)
        } else {
            let next: u8 = match *active { Some(0) => 1, _ => 0 };
            // Fixed offset per bucket — never derived from frame size, so that
            // a shrunken frame cannot let bucket 1 overwrite the live bucket 0.
            let start_kb: u32 = if next == 0 { 0 } else { BUCKET_SIZE_KB };
            self.base.write(&[0x32, 0x02, next]).await?;
            self.base
                .transport
                .read_matching(64, |p| p.len() >= 2 && p[0] == 0x33 && p[1] == 0x02, 8)
                .await;
            log::debug!("[Kraken LCD timing] delete write: {}ms", tp.elapsed().as_millis());
            (next, start_kb)
        };

        let size_lo = (data_kb & 0xFF) as u8;
        let size_hi = ((data_kb >> 8) & 0xFF) as u8;
        let start_lo = (mem_start_kb & 0xFF) as u8;
        let start_hi = ((mem_start_kb >> 8) & 0xFF) as u8;
        self.base
            .write(&[0x32, 0x01, bucket_idx, bucket_idx + 1, start_lo, start_hi, size_lo, size_hi, 0x01])
            .await?;
        self.base
            .transport
            .read_matching(64, |p| p.len() >= 2 && p[0] == 0x33 && p[1] == 0x01, 8)
            .await;
        log::debug!("[Kraken LCD timing] setup ack: {}ms", tp.elapsed().as_millis());

        self.base.write(&[0x36, 0x01, bucket_idx]).await?;
        self.base
            .transport
            .read_matching(64, |p| p.len() >= 2 && p[0] == 0x37 && p[1] == 0x01, 8)
            .await;
        log::debug!("[Kraken LCD timing] write-start ack: {}ms", tp.elapsed().as_millis());

        let bulk = Arc::clone(&self.bulk_transport);
        let header_c = header.clone();
        let data_c = data.to_vec();
        let bulk_bytes = header_c.len() + data_c.len();
        tokio::task::spawn_blocking(move || -> Result<()> {
            let mut guard = bulk.lock().unwrap();
            if guard.is_none() {
                log::info!("[Kraken LCD] opening persistent USB bulk transport");
                *guard = Some(UsbBulkTransport::open(vid, pid)?);
            }
            let transport = guard.as_ref().unwrap();
            transport.write(&header_c)?;
            for chunk in data_c.chunks(LCD_BULK_CHUNK) {
                transport.write(chunk)?;
            }
            Ok(())
        })
        .await??;
        log::debug!(
            "[Kraken LCD timing] bulk transfer done ({bulk_bytes} bytes): {}ms",
            tp.elapsed().as_millis()
        );

        self.base.write(&[0x36, 0x02]).await?;
        self.base
            .transport
            .read_matching(64, |p| p.len() >= 2 && p[0] == 0x37 && p[1] == 0x02, 8)
            .await;
        log::debug!("[Kraken LCD timing] write-finish ack: {}ms", tp.elapsed().as_millis());

        self.base.write(&[0x38, 0x01, 0x04, bucket_idx]).await?;
        self.base
            .transport
            .read_matching(64, |p| p.len() >= 15 && p[0] == 0x39 && p[1] == 0x01, 16)
            .await;
        log::debug!("[Kraken LCD timing] switch ack: {}ms", tp.elapsed().as_millis());
        *active = Some(bucket_idx);
        Ok(())
    }
}

// ── Free functions ────────────────────────────────────────────────────────────

/// Decodes a static image (PNG/JPEG/…) and resizes it to the panel's native
/// resolution, returning a raw `width*height*4` RGBA8 buffer. CPU-heavy
/// (Lanczos3 resize) — call inside `spawn_blocking`.
pub fn decode_static_image_rgba(data: &[u8], width: u32, height: u32) -> Result<Vec<u8>> {
    let img = image::load_from_memory(data)?;
    let img = if img.width() == width && img.height() == height {
        img
    } else {
        img.resize_exact(width, height, image::imageops::FilterType::Lanczos3)
    };
    Ok(img.to_rgba8().into_raw())
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;

    fn protocol(responses: Vec<Vec<u8>>) -> NzxtKrakenProtocol<MockTransport> {
        protocol_for(KrakenWire::ZElite, responses)
    }

    fn protocol_for(wire: KrakenWire, responses: Vec<Vec<u8>>) -> NzxtKrakenProtocol<MockTransport> {
        NzxtKrakenProtocol {
            base: NzxtBaseProtocol::new(MockTransport::new(responses)),
            wire,
            rgb_cache: KrakenRgbCache::for_wire(wire),
            poll_task: Arc::new(TokioMutex::new(None)),
            status_cache: Arc::new(TokioMutex::new(KrakenStatus::default())),
            active_bucket: Arc::new(TokioMutex::new(None)),
            polling_paused: Arc::new(AtomicBool::new(false)),
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
        pkt[17] = 0xE8; pkt[18] = 0x03; // pump_rpm LE: 1000
        pkt[19] = 50;
        pkt[23] = 0x58; pkt[24] = 0x02; // fan_rpm LE: 600
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
        pkt[0] = 0x75; pkt[1] = 0x02;
        pkt[15] = 29; pkt[16] = 5;
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).is_some());
    }

    #[test]
    fn parse_status_rejects_wrong_prefix() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x67; pkt[1] = 0x01;
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).is_none());
    }

    #[test]
    fn parse_status_rejects_unknown_subtype() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x75; pkt[1] = 0x03;
        assert!(NzxtKrakenProtocol::<MockTransport>::parse_status(&pkt).is_none());
    }

    #[test]
    fn parse_status_ignores_0xff_temperature() {
        let mut pkt = vec![0u8; 26];
        pkt[0] = 0x75; pkt[1] = 0x01;
        pkt[15] = 0xFF; pkt[16] = 0xFF;
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
        colors[0] = RgbColor { r: 10, g: 20, b: 30 };
        colors[RING_LED_COUNT - 1] = RgbColor { r: 40, g: 50, b: 60 };
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
        let colors = [RgbColor { r: 10, g: 20, b: 30 }; 8];
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
        p.write_logo(RgbColor { r: 255, g: 0, b: 128 }).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(written.len(), 1);
        let pkt = &written[0];
        assert_eq!(pkt.len(), 64);
        assert_eq!(&pkt[0..7], &[0x2A, 0x04, 0x04, 0x04, 0x00, 0x32, 0x00]);
        assert_eq!(pkt[7], 0);   // G
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
        assert_eq!(&written[0][0..4], &[0x26, 0x14, RING_CHANNEL_BYTE, RING_CHANNEL_BYTE]);
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
        assert_eq!(&written[1][0..4], &[0x26, 0x14, RING_CHANNEL_BYTE, RING_CHANNEL_BYTE]);
        assert_eq!(&written[2][0..4], &[0x26, 0x14, EXT_CHANNEL_BYTE, EXT_CHANNEL_BYTE]);
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

    // ── screen config ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_screen_config_sends_correct_packet() {
        let p = protocol(vec![]);
        p.write_screen_config(75, 90).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(&written[0][..], &[0x30, 0x02, 0x01, 75, 0x00, 0x00, 0x01, 1]);
    }

    #[tokio::test]
    async fn write_screen_config_wraps_rotation_index() {
        let p = protocol(vec![]);
        p.write_screen_config(50, 270).await.unwrap();
        assert_eq!(p.base.transport.written.lock().await[0][7], 3);
    }

    // ── decode_static_image_rgba ──────────────────────────────────────────

    #[test]
    fn decode_static_image_rgba_resizes_to_native() {
        let dynimg = image::DynamicImage::ImageRgba8(image::RgbaImage::from_pixel(
            2, 2, image::Rgba([255u8, 0, 0, 255]),
        ));
        let mut png = std::io::Cursor::new(Vec::new());
        dynimg.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let rgba = decode_static_image_rgba(png.get_ref(), 4, 4).unwrap();
        assert_eq!(rgba.len(), 4 * 4 * 4);
        assert!(rgba.chunks_exact(4).all(|px| px == [255, 0, 0, 255]));
    }

    // ── LCD bucket layout ─────────────────────────────────────────────────

    #[test]
    fn buckets_never_overlap_for_largest_allowed_upload() {
        let data_kb = BUCKET_SIZE_KB - 1;
        assert!(data_kb <= BUCKET_SIZE_KB);
        assert!(BUCKET_SIZE_KB + data_kb <= LCD_TOTAL_MEMORY_KB);
    }

    #[test]
    fn bucket_holds_far_more_than_one_rgba32_frame() {
        let one_frame_kb = (640 * 640 * 4 + 1023) / 1024;
        assert!(BUCKET_SIZE_KB > one_frame_kb * 4);
    }

    // ── streaming header ──────────────────────────────────────────────────

    #[test]
    fn stream_bulk_header_encodes_type_and_length() {
        let h = NzxtKrakenProtocol::<MockTransport>::stream_bulk_header(0x19D8);
        assert_eq!(&h[..12], &[0x12, 0xFA, 0x01, 0xE8, 0xAB, 0xCD, 0xEF, 0x98, 0x76, 0x54, 0x32, 0x10]);
        assert_eq!(h[12], 0x08);
        assert_eq!(&h[16..20], &[0xD8, 0x19, 0x00, 0x00]);
    }

    // ── Q565 payload ──────────────────────────────────────────────────────

    #[test]
    fn q565_payload_has_magic_dimensions_and_end() {
        let rgba = [255u8; 4 * 4 * 4];
        let out = NzxtKrakenProtocol::<MockTransport>::rgba_to_q565_payload(&rgba, 4, 4).unwrap();
        assert_eq!(&out[0..4], b"q565");
        assert_eq!(&out[4..8], &[4, 0, 4, 0]);
        assert_eq!(*out.last().unwrap(), 0xFF);
        assert!(out.len() < 4 * 4 * 2);
    }

    #[test]
    fn q565_payload_roundtrips_via_decoder() {
        let rgba: Vec<u8> = std::iter::repeat([255u8, 0, 0, 255])
            .take(8 * 8)
            .flatten()
            .collect();
        let out = NzxtKrakenProtocol::<MockTransport>::rgba_to_q565_payload(&rgba, 8, 8).unwrap();
        let mut decoded = Vec::new();
        let (hdr, _) = q565::decode::Q565DecodeContext::decode::<q565::byteorder::LittleEndian>(
            &out,
            q565::decode::VecDecodeOutput::<q565::Rgb565>::new(&mut decoded),
        )
        .expect("decode failed");
        assert_eq!((hdr.width, hdr.height), (8, 8));
        assert!(decoded.iter().all(|&px| px == 0xF800));
    }

    #[test]
    fn q565_payload_rejects_size_mismatch() {
        assert!(
            NzxtKrakenProtocol::<MockTransport>::rgba_to_q565_payload(&[0u8; 4], 2, 2).is_err()
        );
    }
}
