// SPDX-License-Identifier: GPL-3.0-or-later
// SPDX-FileCopyrightText: liquidctl contributors <https://github.com/liquidctl/liquidctl>
//! Kraken LCD: brightness/rotation config, the GIF/image bucket pipeline, and
//! the two live streaming paths (Q565 `0x08` and raw BGR888 `0x09`). Sits on the
//! HID control channel plus a separate USB bulk endpoint for frame payloads.

use anyhow::{Context, Result};
use q565::{
    encode::Q565EncodeContext,
    utils::{encode_rgb565_unchecked, rgb888_to_rgb565},
};
use std::collections::HashMap;
use std::sync::{atomic::Ordering, Arc};

use super::NzxtKrakenProtocol;
use crate::drivers::transports::{hid::HidTransport, usb_bulk::UsbBulkTransport, Transport};

const LCD_TOTAL_MEMORY_KB: u32 = 24320;
const LCD_BULK_CHUNK: usize = 1024 * 1024 * 2; // 2 MB

/// Acquire the lazy-opened USB bulk transport guard, opening the device on
/// first use.
fn acquire_bulk(
    bulk: &Arc<std::sync::Mutex<Option<UsbBulkTransport>>>,
    vid: u16,
    pid: u16,
) -> Result<std::sync::MutexGuard<'_, Option<UsbBulkTransport>>> {
    let mut guard = bulk
        .lock()
        .map_err(|_| anyhow::anyhow!("Kraken LCD bulk transport mutex poisoned"))?;
    if guard.is_none() {
        log::info!("[Kraken LCD] opening persistent USB bulk transport");
        *guard = Some(UsbBulkTransport::open(vid, pid, None)?);
    }
    Ok(guard)
}

/// Named opcodes for the LCD control channel, so each command reads as
/// `write([cmd::GROUP, sub, …])`. Tests pin the literal bytes.
mod cmd {
    pub const CONFIG: u8 = 0x30;
    pub const CONFIG_REPLY: u8 = 0x31;
    /// Bucket memory `0x32`: sub `0x01` setup, `0x02` delete.
    pub const BUCKET_MEM: u8 = 0x32;
    /// Transfer control `0x36`: sub `0x01` start, `0x02` end, `0x03`/`0x04` mode select.
    pub const XFER: u8 = 0x36;
    /// Transfer ACK report `0x37`; its sub mirrors the `0x36` sub it acknowledges.
    pub const XFER_ACK: u8 = 0x37;
    /// Bucket switch / display select `0x38`: sub `0x01`.
    pub const BUCKET_SWITCH: u8 = 0x38;
    pub const BUCKET_SWITCH_REPLY: u8 = 0x39;

    /// 12-byte signature prefixing every USB-bulk frame header.
    pub const BULK_MAGIC: [u8; 12] = [
        0x12, 0xFA, 0x01, 0xE8, 0xAB, 0xCD, 0xEF, 0x98, 0x76, 0x54, 0x32, 0x10,
    ];
}

impl<T: Transport> NzxtKrakenProtocol<T> {
    /// Send brightness + rotation config packet to the device.
    pub async fn write_screen_config(&self, brightness: u8, rotation_degrees: u32) -> Result<()> {
        let rot_idx = ((rotation_degrees / 90) % 4) as u8;
        self.base
            .write(&[
                cmd::CONFIG,
                0x02,
                0x01,
                brightness,
                0x00,
                0x00,
                0x01,
                rot_idx,
            ])
            .await
    }

    /// Encodes a raw RGBA8 frame as a Q565 stream (the codec the panel's
    /// type-0x08 LCD path expects), including magic, dimensions, and `OP_END`.
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

    /// 20-byte bulk header. `asset_mode`: `0x08` = Q565, `0x09` = raw BGR888.
    pub fn stream_bulk_header(payload_len: u32, asset_mode: u8) -> [u8; 20] {
        let mut h = [0u8; 20];
        h[..12].copy_from_slice(&cmd::BULK_MAGIC);
        h[12] = asset_mode;
        h[16..20].copy_from_slice(&payload_len.to_le_bytes());
        h
    }

    /// Non-multiples of 90 and non-square buffers are returned unrotated.
    pub fn rotate_rgba_square(rgba: &[u8], size: u32, degrees: u32) -> Vec<u8> {
        let n = size as usize;
        let step = degrees % 360;
        if step == 0 || rgba.len() != n * n * 4 {
            return rgba.to_vec();
        }
        Self::rotate_rgba_square_impl(rgba, n, step)
    }

    /// Owned variant for the streaming path: at 0° the buffer is returned as-is.
    pub fn rotate_rgba_square_owned(rgba: Vec<u8>, size: u32, degrees: u32) -> Vec<u8> {
        let n = size as usize;
        let step = degrees % 360;
        if step == 0 || rgba.len() != n * n * 4 {
            return rgba;
        }
        Self::rotate_rgba_square_impl(&rgba, n, step)
    }

    fn rotate_rgba_square_impl(rgba: &[u8], n: usize, step: u32) -> Vec<u8> {
        let mut out = vec![0u8; rgba.len()];
        for y in 0..n {
            for x in 0..n {
                let (cx, cy) = match step {
                    90 => (n - 1 - y, x),
                    180 => (n - 1 - x, n - 1 - y),
                    270 => (y, n - 1 - x),
                    _ => (x, y),
                };
                let s = (y * n + x) * 4;
                let d = (cy * n + cx) * 4;
                out[d..d + 4].copy_from_slice(&rgba[s..s + 4]);
            }
        }
        out
    }

    /// RGBA8 → BGR888 (drops alpha), as the raw `0x09` path expects.
    pub fn rgba_to_bgr888(rgba: &[u8]) -> Vec<u8> {
        let mut out = Vec::with_capacity(rgba.len() / 4 * 3);
        for px in rgba.chunks_exact(4) {
            out.extend_from_slice(&[px[2], px[1], px[0]]); // B, G, R
        }
        out
    }

    /// Lowest bucket whose info (bytes 15+) is all-zero; `None` if all occupied.
    fn find_next_unoccupied(buckets: &HashMap<u8, Vec<u8>>) -> Option<u8> {
        let mut keys: Vec<u8> = buckets.keys().copied().collect();
        keys.sort_unstable();
        keys.into_iter()
            .find(|idx| buckets[idx].iter().skip(15).all(|&b| b == 0))
    }

    /// LE start offset (1024-byte units) for `data_units` in `bucket_index`:
    /// reuse the slot if it fits, else append/wrap, else `None` (no room → wipe).
    fn bucket_memory_offset(
        buckets: &HashMap<u8, Vec<u8>>,
        bucket_index: u8,
        data_units: u32,
    ) -> Option<[u8; 2]> {
        let Some(cur) = buckets.get(&bucket_index) else {
            return Some([0x00, 0x00]);
        };
        let cur_offset = (cur[17] as u32) | ((cur[18] as u32) << 8);
        let cur_size = (cur[19] as u32) | ((cur[20] as u32) << 8);

        if data_units <= cur_size {
            return Some([cur[17], cur[18]]);
        }

        let mut min_occupied = cur_offset;
        let mut max_occupied = 0u32;
        let mut overlap = false;
        for (idx, b) in buckets {
            let start = (b[17] as u32) | ((b[18] as u32) << 8);
            let end = start + ((b[19] as u32) | ((b[20] as u32) << 8));
            if end > max_occupied {
                max_occupied = end;
            }
            if start < min_occupied {
                min_occupied = start;
            }
            if (start > cur_offset && start < cur_offset + data_units)
                || (start < cur_offset && end > cur_offset)
                || (start == cur_offset && *idx != bucket_index)
            {
                overlap = true;
            }
        }

        if !overlap {
            return Some([cur[17], cur[18]]);
        }
        if max_occupied + data_units < LCD_TOTAL_MEMORY_KB {
            return Some([
                (max_occupied & 0xFF) as u8,
                ((max_occupied >> 8) & 0xFF) as u8,
            ]);
        }
        if data_units < min_occupied {
            return Some([0x00, 0x00]);
        }
        None
    }

    /// Consume a transfer ACK (`0x37 <sub>`): `0x01` for a `0x36 01` start,
    /// `0x02` for a `0x36 02` end. Unread ACKs desync the firmware and can
    /// crash it into the bootloader.
    async fn await_xfer_ack(&self, sub: u8) -> Result<()> {
        self.base
            .transport
            .read_matching(
                64,
                move |p| p.len() >= 2 && p[0] == cmd::XFER_ACK && p[1] == sub,
                8,
            )
            .await
            .ok_or_else(|| anyhow::anyhow!("LCD ACK timeout for sub={sub:#04x}"))?;
        Ok(())
    }
}

// ── Asset mode 0x09: raw BGR888 streaming ─────────────────────────────────────

/// Per-frame look-up tables replayed before every raw frame to keep the
/// firmware's colour mapping in sync.
fn stream_lut1() -> [u8; 45] {
    let mut lut = [0x3Fu8; 45];
    lut[..4].copy_from_slice(&[0x72, 0x01, 0x01, 0x00]);
    lut
}

fn stream_lut2() -> [u8; 45] {
    let mut lut = [0x1Fu8; 45];
    lut[..4].copy_from_slice(&[0x72, 0x02, 0x01, 0x01]);
    lut
}

// ── HID control channel: state, GIF upload, streaming ─────────────────────────

impl NzxtKrakenProtocol<HidTransport> {
    /// Reads `(brightness, rotation_degrees)` from the device.
    pub async fn read_lcd_state(&self) -> Option<(u8, u32)> {
        self.base.write(&[cmd::CONFIG, 0x01]).await.ok()?;
        let pkt = self
            .base
            .transport
            .read_matching(
                64,
                |p| p.len() >= 27 && p[0] == cmd::CONFIG_REPLY && p[1] == 0x01,
                8,
            )
            .await?;
        let brightness = pkt[0x18];
        let rot_idx = pkt[0x1A];
        Some((brightness, (rot_idx as u32).min(3) * 90))
    }

    /// Switches the LCD back to the device's built-in default display.
    pub async fn switch_to_default_display(&self) -> Result<()> {
        self.base
            .write(&[cmd::BUCKET_SWITCH, 0x01, 0x02, 0x00])
            .await?;
        if self
            .base
            .transport
            .read_matching(
                64,
                |p| p.len() >= 2 && p[0] == cmd::BUCKET_SWITCH_REPLY && p[1] == 0x01,
                16,
            )
            .await
            .is_none()
        {
            log::warn!("[Kraken LCD] switch_to_default_display: no reply from device");
        }
        Ok(())
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
            crate::util::image::resize_gif(&raw, width, height, |_| {})
        })
        .await??;
        log::debug!(
            "[Kraken LCD] GIF prepared in {}ms ({} bytes)",
            t0.elapsed().as_millis(),
            prepared.len()
        );
        let data_len = u32::try_from(prepared.len()).context("GIF payload exceeds 4 GiB")?;
        let bulk_info: [u8; 8] = [
            0x01,
            0x00,
            0x00,
            0x00,
            (data_len & 0xFF) as u8,
            ((data_len >> 8) & 0xFF) as u8,
            ((data_len >> 16) & 0xFF) as u8,
            ((data_len >> 24) & 0xFF) as u8,
        ];
        self.polling_paused.store(true, Ordering::Relaxed);
        let result = self
            .run_bucket_pipeline(vid, pid, &prepared, &bulk_info)
            .await;
        self.polling_paused.store(false, Ordering::Relaxed);
        log::debug!(
            "[Kraken LCD] GIF upload total: {}ms",
            t0.elapsed().as_millis()
        );
        result
    }

    /// Pushes one frame via the type-0x08 streaming path. `payload` must be a
    /// complete Q565 file, taken by value so the bulk write moves the caller's
    /// buffer instead of copying ~1 MB per frame. Used for live LCD-engine
    /// frames and static uploads.
    pub async fn stream_frame(&self, vid: u16, pid: u16, payload: Vec<u8>) -> Result<()> {
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

    async fn run_stream_frame(&self, vid: u16, pid: u16, payload: Vec<u8>) -> Result<()> {
        let t = std::time::Instant::now();
        let payload_len = u32::try_from(payload.len()).context("stream payload exceeds 4 GiB")?;
        let header = Self::stream_bulk_header(payload_len, 0x08);

        // Drain HID ACKs queued from prior frames so the firmware does not
        // desync (>~19 unread `0x37 02` ACKs → artifacts then firmware crash).
        self.drain_hid_nonblocking().await;

        self.base
            .write(&[cmd::XFER, 0x01, 0x00, 0x01, 0x08])
            .await?;
        self.await_xfer_ack(0x01).await?;

        let bulk = Arc::clone(&self.bulk_transport);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = acquire_bulk(&bulk, vid, pid)?;
            let transport = guard.as_ref().unwrap();
            transport.write(&header)?;
            for chunk in payload.chunks(LCD_BULK_CHUNK) {
                transport.write(chunk)?;
            }
            Ok(())
        })
        .await??;

        self.base.write(&[cmd::XFER, 0x02]).await?;
        self.await_xfer_ack(0x02).await?;
        log::trace!(
            "[LCD] q565 frame: {} bytes transferred in {}µs",
            payload_len,
            t.elapsed().as_micros()
        );
        Ok(())
    }

    /// Writes a command and reads the first report the device returns (any
    /// prefix). Used for the streaming-mode handshake where each step is simply
    /// ACKed, and for the bucket pipeline's request/response commands.
    async fn write_then_read(&self, pkt: &[u8]) -> Result<()> {
        self.base.write(pkt).await?;
        if self
            .base
            .transport
            .read_matching(64, |_| true, 8)
            .await
            .is_none()
        {
            log::warn!("[Kraken LCD] no ACK for command {pkt:02x?}");
        }
        Ok(())
    }

    /// One-time HID handshake into live-streaming mode; call once before
    /// [`Self::stream_frame_raw`]. Clears all buckets to a known state.
    pub async fn enter_streaming_mode(&self, brightness: u8) -> Result<()> {
        let pct = brightness.min(100);
        self.drain_hid_nonblocking().await;
        self.write_then_read(&[0x10, 0x02]).await?;
        self.write_then_read(&[0x70, 0x02, 0x01, 0xB8, 0x0B])
            .await?;
        self.write_then_read(&[0x74, 0x01]).await?;
        self.write_then_read(&[cmd::XFER, 0x04]).await?;
        self.write_then_read(&[cmd::CONFIG, 0x01]).await?;
        self.write_then_read(&[cmd::XFER, 0x03]).await?;
        self.write_then_read(&[cmd::CONFIG, 0x02, 0x00, 0x00, 0x00, 0x00, 0x1E])
            .await?;
        self.write_then_read(&[cmd::BUCKET_SWITCH, 0x01, 0x02])
            .await?; // switch to liquid
        for bi in 0u8..16 {
            self.write_then_read(&[cmd::BUCKET_MEM, 0x02, bi]).await?; // delete all buckets
        }
        self.write_then_read(&[cmd::CONFIG, 0x02, 0x01, pct, 0x00, 0x00, 0x00, 0x1E])
            .await?;
        self.drain_hid_nonblocking().await;
        *self.active_bucket.lock().await = None;
        Ok(())
    }

    /// Streams one raw BGR888 frame (asset mode `0x09`), taken by value so the
    /// bulk write moves the caller's buffer instead of copying it per frame.
    /// Requires a prior [`Self::enter_streaming_mode`]; no per-frame bucket
    /// setup/switch.
    pub async fn stream_frame_raw(&self, vid: u16, pid: u16, bgr888: Vec<u8>) -> Result<()> {
        let t = std::time::Instant::now();
        let len = bgr888.len();
        // Pause status polling so it doesn't race the streaming reads.
        self.polling_paused.store(true, Ordering::Relaxed);
        let result = self.run_stream_frame_raw(vid, pid, bgr888).await;
        self.polling_paused.store(false, Ordering::Relaxed);
        result?;
        log::trace!(
            "[LCD] raw frame: {} bytes transferred in {}µs",
            len,
            t.elapsed().as_micros()
        );
        Ok(())
    }

    async fn run_stream_frame_raw(&self, vid: u16, pid: u16, bgr888: Vec<u8>) -> Result<()> {
        // USB bulk transfer chunk size for raw frames.
        const STREAM_URB: usize = 245_760;

        // Drain queued ACKs so the bulk data doesn't race ahead of "start".
        self.drain_hid_nonblocking().await;
        self.write_then_read(&stream_lut1()).await?;
        self.write_then_read(&stream_lut2()).await?;
        self.base
            .write(&[cmd::XFER, 0x01, 0x00, 0x01, 0x09])
            .await?; // start, asset mode 0x09
        self.await_xfer_ack(0x01).await?;

        let payload_len =
            u32::try_from(bgr888.len()).context("raw stream payload exceeds 4 GiB")?;
        let header = Self::stream_bulk_header(payload_len, 0x09);
        let bulk = Arc::clone(&self.bulk_transport);
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = acquire_bulk(&bulk, vid, pid)?;
            let transport = guard.as_ref().unwrap();
            transport.write(&header)?;
            for chunk in bgr888.chunks(STREAM_URB) {
                transport.write(chunk)?;
            }
            Ok(())
        })
        .await??;

        self.base.write(&[0x36, 0x02]).await?; // end
        self.await_xfer_ack(0x02).await?;
        Ok(())
    }

    // ── 16-bucket dynamic allocator ──────────────────────────────────────────

    /// Writes `pkt` and returns the first reply matching `(pkt[0]+1, pkt[1])`,
    /// skipping stray reports. `None` if none arrived.
    async fn command(&self, pkt: &[u8]) -> Option<Vec<u8>> {
        self.base.write(pkt).await.ok()?;
        let want0 = pkt[0].wrapping_add(1);
        let want1 = pkt[1];
        self.base
            .transport
            .read_matching(
                64,
                move |p| p.len() >= 2 && p[0] == want0 && p[1] == want1,
                24,
            )
            .await
    }

    async fn query_buckets(&self) -> HashMap<u8, Vec<u8>> {
        let mut buckets = HashMap::new();
        for i in 0u8..16 {
            if let Some(msg) = self.command(&[cmd::CONFIG, 0x04, i]).await {
                buckets.insert(i, msg);
            }
        }
        buckets
    }

    async fn delete_bucket(&self, index: u8) -> bool {
        match self.command(&[cmd::BUCKET_MEM, 0x02, index]).await {
            Some(msg) => msg.len() > 14 && msg[14] == 0x01,
            None => false,
        }
    }

    async fn delete_all_buckets(&self) {
        self.switch_bucket(0, 0x02).await; // back to liquid mode first
        for i in 0u8..16 {
            self.delete_bucket(i).await;
        }
    }

    async fn setup_bucket(&self, start: u8, end: u8, mem: [u8; 2], size: [u8; 2]) -> bool {
        // No reliable success byte on fw 2.x; any reply means accepted.
        self.command(&[
            cmd::BUCKET_MEM,
            0x01,
            start,
            end,
            mem[0],
            mem[1],
            size[0],
            size[1],
            0x01,
        ])
        .await
        .is_some()
    }

    async fn switch_bucket(&self, index: u8, mode: u8) -> bool {
        self.command(&[cmd::BUCKET_SWITCH, 0x01, mode, index])
            .await
            .is_some()
    }

    /// Deletes buckets walking forward from `bucket_index` until landing on a free one.
    async fn prepare_bucket(&self, mut bucket_index: u8, mut bucket_filled: bool) -> Result<u8> {
        loop {
            if bucket_index >= 16 {
                anyhow::bail!("Kraken LCD: reached max bucket (16)");
            }
            if !self.delete_bucket(bucket_index).await {
                bucket_index += 1;
                bucket_filled = true;
                continue;
            }
            if bucket_filled {
                bucket_filled = false;
                continue;
            }
            return Ok(bucket_index);
        }
    }

    /// Uploads `data` into a memory bucket: query → prepare → place → transfer →
    /// switch. `bulk_info` is the 8-byte `[asset_mode, 0,0,0, len_le32]` header tail.
    async fn run_bucket_pipeline(
        &self,
        vid: u16,
        pid: u16,
        data: &[u8],
        bulk_info: &[u8; 8],
    ) -> Result<()> {
        let tp = std::time::Instant::now();
        let header: Vec<u8> = {
            let mut h = cmd::BULK_MAGIC.to_vec();
            h.extend_from_slice(bulk_info);
            h
        };
        let total_len = (header.len() + data.len()) as u32;
        let data_units = total_len.div_ceil(1024);

        if data_units >= LCD_TOTAL_MEMORY_KB {
            anyhow::bail!(
                "LCD image too large: {data_units} KB exceeds total LCD memory {LCD_TOTAL_MEMORY_KB} KB"
            );
        }

        self.write_then_read(&[cmd::XFER, 0x03]).await?;
        let buckets = self.query_buckets().await;
        let found = Self::find_next_unoccupied(&buckets);
        let bucket_index = tokio::time::timeout(
            std::time::Duration::from_secs(5),
            self.prepare_bucket(found.unwrap_or(0), found.is_none()),
        )
        .await
        .context("Kraken LCD: prepare_bucket timed out after 5 s")??;
        log::debug!(
            "[Kraken LCD timing] bucket prepared ({bucket_index}): {}ms",
            tp.elapsed().as_millis()
        );

        let size_bytes = [(data_units & 0xFF) as u8, ((data_units >> 8) & 0xFF) as u8];
        let (bucket_index, mem_start) =
            match Self::bucket_memory_offset(&buckets, bucket_index, data_units) {
                Some(mem) => (bucket_index, mem),
                None => {
                    log::debug!("[Kraken LCD] no contiguous room, wiping all buckets");
                    self.delete_all_buckets().await;
                    (0u8, [0x00, 0x00])
                }
            };

        if !self
            .setup_bucket(bucket_index, bucket_index + 1, mem_start, size_bytes)
            .await
        {
            anyhow::bail!("Kraken LCD: bucket setup was rejected");
        }

        self.base.write(&[cmd::XFER, 0x01, bucket_index]).await?;
        self.await_xfer_ack(0x01).await?;
        log::debug!(
            "[Kraken LCD timing] write-start ack: {}ms",
            tp.elapsed().as_millis()
        );

        let bulk = Arc::clone(&self.bulk_transport);
        let data_c = data.to_vec();
        let bulk_bytes = header.len() + data_c.len();
        let header_c = header;
        tokio::task::spawn_blocking(move || -> Result<()> {
            let guard = acquire_bulk(&bulk, vid, pid)?;
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

        self.base.write(&[cmd::XFER, 0x02]).await?;
        self.await_xfer_ack(0x02).await?;

        if !self.switch_bucket(bucket_index, 0x04).await {
            log::warn!("[Kraken LCD] bucket {bucket_index} switch not acked");
        }
        log::debug!(
            "[Kraken LCD timing] switch ack: {}ms",
            tp.elapsed().as_millis()
        );
        *self.active_bucket.lock().await = Some(bucket_index);
        Ok(())
    }
}

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
    Ok(img.into_rgba8().into_raw())
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::drivers::transports::mock::test_transport::MockTransport;
    use crate::drivers::vendors::nzxt::protocols::nzxt_kraken::{
        KrakenRgbCache, KrakenStatus, KrakenWire,
    };
    use crate::drivers::vendors::nzxt::protocols::NzxtBaseProtocol;
    use std::sync::atomic::AtomicBool;
    use tokio::sync::Mutex as TokioMutex;

    fn protocol(responses: Vec<Vec<u8>>) -> NzxtKrakenProtocol<MockTransport> {
        let wire = KrakenWire::ZElite;
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

    // ── transfer ACK ──────────────────────────────────────────────────────
    // Invariant: every `0x36 02` end must consume its `0x37 02` ACK. Leaving
    // ACKs unread desyncs the panel firmware and crashes it into the bootloader.

    #[tokio::test]
    async fn await_xfer_ack_consumes_matching_end_ack() {
        // Queue an end ACK; await_xfer_ack(0x02) must read (drain) it.
        let p = protocol(vec![vec![0x37, 0x02, 0x00]]);
        p.await_xfer_ack(0x02).await.unwrap();
        assert!(
            p.base.transport.responses.lock().await.is_empty(),
            "end ACK must be consumed"
        );
    }

    #[tokio::test]
    async fn await_xfer_ack_skips_stray_then_consumes_ack() {
        // A stray status report precedes the ACK; await must skip it and still
        // land on (and consume) the ACK.
        let p = protocol(vec![vec![0x75, 0x01, 0x00], vec![0x37, 0x02, 0x00]]);
        p.await_xfer_ack(0x02).await.unwrap();
        assert!(
            p.base.transport.responses.lock().await.is_empty(),
            "both the stray report and the ACK must be consumed"
        );
    }

    #[tokio::test]
    async fn await_xfer_ack_start_does_not_match_end() {
        // Sub-byte must match: awaiting a start ACK (0x01) must not swallow an
        // end ACK (0x02) — it should keep reading past it (here: time out).
        let p = protocol(vec![vec![0x37, 0x02, 0x00]]);
        p.await_xfer_ack(0x01).await.ok();
        assert!(
            p.base.transport.responses.lock().await.is_empty(),
            "read_matching drains non-matching reports while searching"
        );
    }

    // ── screen config ─────────────────────────────────────────────────────

    #[tokio::test]
    async fn write_screen_config_sends_correct_packet() {
        let p = protocol(vec![]);
        p.write_screen_config(75, 90).await.unwrap();
        let written = p.base.transport.written.lock().await;
        assert_eq!(
            &written[0][..],
            &[0x30, 0x02, 0x01, 75, 0x00, 0x00, 0x01, 1]
        );
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
            2,
            2,
            image::Rgba([255u8, 0, 0, 255]),
        ));
        let mut png = std::io::Cursor::new(Vec::new());
        dynimg.write_to(&mut png, image::ImageFormat::Png).unwrap();
        let rgba = decode_static_image_rgba(png.get_ref(), 4, 4).unwrap();
        assert_eq!(rgba.len(), 4 * 4 * 4);
        assert!(rgba.chunks_exact(4).all(|px| px == [255, 0, 0, 255]));
    }

    // ── LCD bucket layout ─────────────────────────────────────────────────

    /// Builds a fake 64-byte bucket info report with the given memory offset and
    /// size (in 1024-byte units) at the wire byte positions the driver reads.
    fn bucket_info(offset: u32, size: u32, occupied: bool) -> Vec<u8> {
        let mut b = vec![0u8; 64];
        b[17] = (offset & 0xFF) as u8;
        b[18] = ((offset >> 8) & 0xFF) as u8;
        b[19] = (size & 0xFF) as u8;
        b[20] = ((size >> 8) & 0xFF) as u8;
        if occupied {
            b[21] = 1; // any non-zero byte from index 15+ marks it occupied
        }
        b
    }

    #[test]
    fn find_next_unoccupied_returns_lowest_free_index() {
        let mut buckets = HashMap::new();
        buckets.insert(0u8, bucket_info(0, 10, true));
        // A never-used bucket reads as all-zero (incl. its offset/size bytes).
        buckets.insert(1u8, bucket_info(0, 0, false));
        buckets.insert(2u8, bucket_info(0, 0, false));
        assert_eq!(
            NzxtKrakenProtocol::<MockTransport>::find_next_unoccupied(&buckets),
            Some(1)
        );
    }

    #[test]
    fn find_next_unoccupied_none_when_all_full() {
        let mut buckets = HashMap::new();
        buckets.insert(0u8, bucket_info(0, 10, true));
        buckets.insert(1u8, bucket_info(10, 10, true));
        assert_eq!(
            NzxtKrakenProtocol::<MockTransport>::find_next_unoccupied(&buckets),
            None
        );
    }

    #[test]
    fn bucket_offset_reuses_slot_when_payload_fits() {
        let mut buckets = HashMap::new();
        buckets.insert(0u8, bucket_info(0x0040, 100, true));
        let off = NzxtKrakenProtocol::<MockTransport>::bucket_memory_offset(&buckets, 0, 50);
        assert_eq!(off, Some([0x40, 0x00]));
    }

    #[test]
    fn bucket_offset_appends_past_highest_when_outgrown() {
        let mut buckets = HashMap::new();
        buckets.insert(0u8, bucket_info(0, 100, true));
        buckets.insert(1u8, bucket_info(100, 50, true)); // max_occupied = 150
        let off = NzxtKrakenProtocol::<MockTransport>::bucket_memory_offset(&buckets, 0, 200);
        assert_eq!(off, Some([150u8, 0u8]));
    }

    #[test]
    fn bucket_offset_none_when_no_room_anywhere() {
        let mut buckets = HashMap::new();
        buckets.insert(0u8, bucket_info(0, 10, true));
        buckets.insert(1u8, bucket_info(10, LCD_TOTAL_MEMORY_KB - 10, true));
        let off = NzxtKrakenProtocol::<MockTransport>::bucket_memory_offset(
            &buckets,
            0,
            LCD_TOTAL_MEMORY_KB - 5,
        );
        assert_eq!(off, None);
    }

    #[test]
    fn bucket_offset_fresh_index_starts_at_zero() {
        let buckets = HashMap::new();
        let off = NzxtKrakenProtocol::<MockTransport>::bucket_memory_offset(&buckets, 3, 50);
        assert_eq!(off, Some([0x00, 0x00]));
    }

    // Pins the overlap clause `start < cur_offset && end > cur_offset`: a bucket
    // sitting *below* the current slot only conflicts when its extent actually
    // reaches into the current offset, otherwise the slot is reused in place.
    #[test]
    fn bucket_offset_lower_bucket_overlaps_only_when_extent_reaches_current() {
        // Current slot at offset 100 needs to grow (50 > cur_size 10).
        // Lower bucket [0, 50) ends before 100 → no overlap → reuse the slot.
        let mut clear = HashMap::new();
        clear.insert(0u8, bucket_info(0, 50, true));
        clear.insert(1u8, bucket_info(100, 10, true));
        let off = NzxtKrakenProtocol::<MockTransport>::bucket_memory_offset(&clear, 1, 50);
        assert_eq!(
            off,
            Some([100u8, 0u8]),
            "non-reaching lower bucket reuses slot"
        );

        // Lower bucket [0, 120) reaches past 100 → overlap → append past highest.
        let mut reaching = HashMap::new();
        reaching.insert(0u8, bucket_info(0, 120, true));
        reaching.insert(1u8, bucket_info(100, 10, true));
        let off = NzxtKrakenProtocol::<MockTransport>::bucket_memory_offset(&reaching, 1, 50);
        assert_eq!(
            off,
            Some([120u8, 0u8]),
            "reaching lower bucket forces append"
        );
    }

    // ── streaming header / luts / bgr ─────────────────────────────────────

    #[test]
    fn stream_bulk_header_encodes_type_and_length() {
        let h = NzxtKrakenProtocol::<MockTransport>::stream_bulk_header(0x19D8, 0x08);
        assert_eq!(
            &h[..12],
            &[0x12, 0xFA, 0x01, 0xE8, 0xAB, 0xCD, 0xEF, 0x98, 0x76, 0x54, 0x32, 0x10]
        );
        assert_eq!(h[12], 0x08);
        assert_eq!(&h[16..20], &[0xD8, 0x19, 0x00, 0x00]);
    }

    #[test]
    fn stream_bulk_header_encodes_raw_asset_mode() {
        let h = NzxtKrakenProtocol::<MockTransport>::stream_bulk_header(1_228_800, 0x09);
        assert_eq!(h[12], 0x09);
        assert_eq!(&h[16..20], &[0x00, 0xC0, 0x12, 0x00]); // 1,228,800 LE
    }

    #[test]
    fn rotate_rgba_square_90_moves_top_left_to_top_right() {
        let mut src = vec![0u8; 2 * 2 * 4];
        src[0..4].copy_from_slice(&[1, 1, 1, 255]);
        let out = NzxtKrakenProtocol::<MockTransport>::rotate_rgba_square(&src, 2, 90);
        assert_eq!(&out[4..8], &[1, 1, 1, 255]);
        assert_eq!(&out[0..4], &[0, 0, 0, 0]);
    }

    #[test]
    fn rotate_rgba_square_zero_and_bad_size_passthrough() {
        let src = vec![9u8; 2 * 2 * 4];
        assert_eq!(
            NzxtKrakenProtocol::<MockTransport>::rotate_rgba_square(&src, 2, 0),
            src
        );
        assert_eq!(
            NzxtKrakenProtocol::<MockTransport>::rotate_rgba_square(&src, 4, 90),
            src
        );
    }

    proptest::proptest! {
        #[test]
        fn rotate_rgba_square_four_quarters_is_identity(
            pixels in proptest::collection::vec(proptest::num::u8::ANY, 5 * 5 * 4..=5 * 5 * 4)
        ) {
            let mut img = pixels.clone();
            for _ in 0..4 {
                img = NzxtKrakenProtocol::<MockTransport>::rotate_rgba_square(&img, 5, 90);
            }
            proptest::prop_assert_eq!(img, pixels);
        }

        #[test]
        fn rotate_rgba_square_180_equals_two_90s(
            pixels in proptest::collection::vec(proptest::num::u8::ANY, 5 * 5 * 4..=5 * 5 * 4)
        ) {
            let once = NzxtKrakenProtocol::<MockTransport>::rotate_rgba_square(&pixels, 5, 180);
            let step1 = NzxtKrakenProtocol::<MockTransport>::rotate_rgba_square(&pixels, 5, 90);
            let twice = NzxtKrakenProtocol::<MockTransport>::rotate_rgba_square(&step1, 5, 90);
            proptest::prop_assert_eq!(once, twice);
        }
    }

    #[test]
    fn rgba_to_bgr888_reorders_and_drops_alpha() {
        let rgba = [10u8, 20, 30, 255, 40, 50, 60, 128];
        let bgr = NzxtKrakenProtocol::<MockTransport>::rgba_to_bgr888(&rgba);
        assert_eq!(bgr, vec![30, 20, 10, 60, 50, 40]); // B,G,R per pixel
    }

    #[test]
    fn stream_luts_have_correct_prefixes_and_fill() {
        let l1 = stream_lut1();
        assert_eq!(&l1[..4], &[0x72, 0x01, 0x01, 0x00]);
        assert!(l1[4..].iter().all(|&b| b == 0x3F));
        assert_eq!(l1.len(), 45);
        let l2 = stream_lut2();
        assert_eq!(&l2[..4], &[0x72, 0x02, 0x01, 0x01]);
        assert!(l2[4..].iter().all(|&b| b == 0x1F));
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
        let rgba: Vec<u8> = std::iter::repeat_n([255u8, 0, 0, 255], 8 * 8)
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
