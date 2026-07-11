// SPDX-License-Identifier: GPL-3.0-or-later
//! Equivalence tests for the built-in NZXT Kraken plugin's LCD path. They drive
//! the real worker + LcdCapability against a recording HID transport and a
//! recording bulk endpoint, asserting the type-0x08 (Q565) frame handshake
//! matches the native protocol — critically, that every `0x36` transfer start/
//! end consumes its `0x37` ACK (a mis-sequenced ACK bricks the panel firmware).

use std::collections::VecDeque;
use std::path::Path;
use std::sync::{Arc, Mutex};

use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

use super::device::LuaDevice;
use super::parse_manifest;
use super::transport::{BulkEndpoint, PluginIo};
use super::worker::DevMatch;
use crate::drivers::transports::Transport;
use crate::drivers::{Device, LcdCapability};

const KRAKEN_SRC: &str = include_str!("builtins/nzxt_kraken.lua");
const REPORT: usize = 64;

/// A HID transport that records writes and serves queued read replies.
/// `read_nonblocking` always returns empty so the plugin's ACK-drain terminates.
struct RecordingHid {
    written: Mutex<Vec<Vec<u8>>>,
    responses: Mutex<VecDeque<Vec<u8>>>,
}

impl RecordingHid {
    fn new(responses: Vec<Vec<u8>>) -> Self {
        Self {
            written: Mutex::new(Vec::new()),
            responses: Mutex::new(responses.into()),
        }
    }
}

#[async_trait]
impl Transport for RecordingHid {
    async fn write(&self, data: &[u8]) -> Result<()> {
        self.written.lock().unwrap().push(data.to_vec());
        Ok(())
    }
    async fn read(&self, _size: usize) -> Result<Vec<u8>> {
        self.responses
            .lock()
            .unwrap()
            .pop_front()
            .ok_or_else(|| anyhow::anyhow!("RecordingHid: no more responses queued"))
    }
    async fn read_nonblocking(&self, _size: usize) -> Result<Vec<u8>> {
        Ok(Vec::new()) // nothing pending — ends drain_hid immediately
    }
    async fn write_then_read(&self, data: &[u8], _size: usize) -> Result<Vec<u8>> {
        self.written.lock().unwrap().push(data.to_vec());
        self.read(0).await
    }
    async fn feature_exchange(&self, data: &[u8], _size: usize) -> Result<Vec<u8>> {
        self.written.lock().unwrap().push(data.to_vec());
        self.read(0).await
    }
    fn rate_status(&self) -> WriteRateStatus {
        WriteRateStatus::default()
    }
    fn set_write_rate_limit(&self, _limit: Option<WriteRateLimit>) {}
}

/// A 64-byte `0x31 0x01` LCD-state reply: brightness at 0x18, rotation idx at 0x1A.
fn lcd_state_reply(brightness: u8, rot_idx: u8) -> Vec<u8> {
    let mut r = vec![0u8; REPORT];
    r[0] = 0x31;
    r[1] = 0x01;
    r[0x18] = brightness;
    r[0x1A] = rot_idx;
    r
}

fn ack(sub: u8) -> Vec<u8> {
    vec![0x37, sub, 0x00]
}

fn kraken_lcd(responses: Vec<Vec<u8>>) -> (LuaDevice, Arc<RecordingHid>, Arc<BulkEndpoint>) {
    let manifest = parse_manifest(KRAKEN_SRC, Path::new("nzxt_kraken.lua")).unwrap();
    let spec = manifest.match_specs[0].clone();
    let hid = Arc::new(RecordingHid::new(responses));
    let bulk = BulkEndpoint::recording();
    let io = PluginIo::Stream {
        transport: hid.clone() as Arc<dyn Transport>,
        bulk: Some(bulk.clone()),
    };
    let dev_match = DevMatch {
        transport: "hid".into(),
        bus: None,
        addr: None,
        pid: Some(0x300E),
    };
    let dev = LuaDevice::with_transport(
        "kraken-lcd".into(),
        &manifest,
        &spec,
        dev_match,
        io,
        tokio::runtime::Handle::current(),
    );
    (dev, hid, bulk)
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_reports_lcd_descriptor_from_pid() {
    // read_lcd_state reply (brightness 50, rotation 0) consumed during init.
    let (dev, _hid, _bulk) = kraken_lcd(vec![lcd_state_reply(50, 0)]);
    assert!(dev.initialize().await.unwrap());
    let d = LcdCapability::lcd_descriptor(&dev);
    assert_eq!((d.width, d.height), (240, 240)); // pid 0x300E
    assert_eq!(d.supported_rotations.len(), 4);
    assert!(d.latches_last_frame);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn q565_stream_frame_emits_native_handshake_and_consumes_acks() {
    // init reads the state reply; the frame consumes the 0x37 01 / 0x37 02 ACKs.
    let (dev, hid, bulk) = kraken_lcd(vec![lcd_state_reply(50, 0), ack(0x01), ack(0x02)]);
    dev.initialize().await.unwrap();
    hid.written.lock().unwrap().clear(); // drop init writes

    let rgba = vec![0u8; 240 * 240 * 4];
    LcdCapability::stream_frame(&dev, &rgba, 240, 240)
        .await
        .unwrap();

    // HID control: exactly transfer-start (asset mode 0x08) then transfer-end.
    let written = hid.written.lock().unwrap().clone();
    assert_eq!(
        written,
        vec![vec![0x36, 0x01, 0x00, 0x01, 0x08], vec![0x36, 0x02]]
    );

    // All queued responses (both ACKs) were consumed — none left unread.
    assert!(
        hid.responses.lock().unwrap().is_empty(),
        "ACKs must be consumed"
    );

    // Bulk: a 20-byte header (asset mode 0x08 at [12]) then a q565 payload.
    let b = bulk.recorded();
    assert_eq!(b.len(), 2, "header + payload");
    assert_eq!(b[0].len(), 20);
    assert_eq!(b[0][12], 0x08, "asset mode Q565");
    assert_eq!(&b[1][0..4], b"q565", "payload is a Q565 file");
    // header length field (LE u32 at [16]) equals the payload length.
    let len = u32::from_le_bytes([b[0][16], b[0][17], b[0][18], b[0][19]]) as usize;
    assert_eq!(len, b[1].len());
}
