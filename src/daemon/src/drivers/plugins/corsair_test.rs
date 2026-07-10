// SPDX-License-Identifier: GPL-3.0-or-later
//! Equivalence tests for the built-in Corsair DRAM plugin. These replace the
//! unit tests that lived in the deleted native `corsair_dram` driver: they drive
//! the *actual* Lua plugin through the real worker + register transport against a
//! recording SMBus backend, and assert the emitted register sequences match the
//! native protocol (sentinel probe, 32-byte info block + CRC, reverse-aware
//! direct packet with trailing CRC8, native-effect commit).

use std::sync::{Arc, Mutex};

use super::device::LuaDevice;
use super::parse_manifest;
use super::transport::{AddrScope, PluginIo, RegisterBus};
use super::worker::DevMatch;
use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};
use crate::drivers::{Device, RgbCapability};
use anyhow::{bail, Result};
use halod_shared::types::{RgbColor, RgbState};
use std::collections::HashSet;
use std::path::Path;

const CORSAIR_SRC: &str = include_str!("builtins/corsair_dram.lua");

const REG_RESET_BUFFER: u8 = 0x0B;
const REG_SET_BINARY_DATA: u8 = 0x20;
const REG_STATUS: u8 = 0x30;
const REG_COLOR_BUFFER_BLOCK_1: u8 = 0x31;
const REG_GET_BINARY_DATA: u8 = 0x40;
const REG_GET_CHECKSUM: u8 = 0x42;
const REG_SENTINEL_A: u8 = 0x43;
const REG_SENTINEL_B: u8 = 0x44;
const REG_GET_DEVICE_INFO: u8 = 0x61;
const REG_WRITE_CONFIGURATION: u8 = 0x82;

/// CRC-8/SMBus — mirrors the plugin (and native) implementation.
fn crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &b in data {
        crc ^= b;
        for _ in 0..8 {
            crc = if crc & 0x80 != 0 {
                (crc << 1) ^ 0x07
            } else {
                crc << 1
            };
        }
    }
    crc
}

#[derive(Debug, Clone, PartialEq)]
enum Rec {
    Byte { cmd: u8, val: u8 },
    Block { cmd: u8, data: Vec<u8> },
}

#[derive(Default)]
struct RecState {
    ops: Vec<Rec>,
    present: HashSet<u8>,
    /// 32-byte device-info block returned sequentially on `REG_GET_BINARY_DATA`.
    info: [u8; 32],
    info_idx: usize,
    /// Bytes streamed via `REG_SET_BINARY_DATA` since the last reset.
    stream: Vec<u8>,
    sentinel_a: u8,
    sentinel_b: u8,
}

#[derive(Clone)]
struct RecordingOps(Arc<Mutex<RecState>>);

impl RecordingOps {
    fn new(state: RecState) -> Self {
        Self(Arc::new(Mutex::new(state)))
    }
    fn drain(&self) -> Vec<Rec> {
        std::mem::take(&mut self.0.lock().unwrap().ops)
    }
}

impl SmBusSyncOps for RecordingOps {
    fn read_byte(&mut self, addr: u8) -> Result<u8> {
        if self.0.lock().unwrap().present.contains(&addr) {
            Ok(0)
        } else {
            bail!("no device at 0x{addr:02x}")
        }
    }
    fn read_byte_data(&mut self, _addr: u8, cmd: u8) -> Result<u8> {
        let mut s = self.0.lock().unwrap();
        match cmd {
            REG_SENTINEL_A => Ok(s.sentinel_a),
            REG_SENTINEL_B => Ok(s.sentinel_b),
            REG_GET_BINARY_DATA => {
                let v = s.info.get(s.info_idx).copied().unwrap_or(0);
                s.info_idx += 1;
                Ok(v)
            }
            REG_GET_CHECKSUM => {
                // During info read the stream buffer is empty → checksum the info
                // block; during effect/colour streaming → checksum the stream.
                if s.stream.is_empty() {
                    Ok(crc8(&s.info))
                } else {
                    Ok(crc8(&s.stream))
                }
            }
            REG_STATUS => Ok(0x00), // never busy
            _ => Ok(0),
        }
    }
    fn write_quick(&mut self, addr: u8) -> Result<bool> {
        Ok(self.0.lock().unwrap().present.contains(&addr))
    }
    fn write_byte_data(&mut self, _addr: u8, cmd: u8, val: u8) -> Result<()> {
        let mut s = self.0.lock().unwrap();
        match cmd {
            REG_GET_DEVICE_INFO => s.info_idx = 0,
            REG_RESET_BUFFER => s.stream.clear(),
            REG_SET_BINARY_DATA => s.stream.push(val),
            _ => {}
        }
        s.ops.push(Rec::Byte { cmd, val });
        Ok(())
    }
    fn write_word_data(&mut self, _addr: u8, _cmd: u8, _val: u16) -> Result<()> {
        Ok(())
    }
    fn write_block_data(&mut self, _addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        self.0.lock().unwrap().ops.push(Rec::Block {
            cmd,
            data: data.to_vec(),
        });
        Ok(())
    }
    fn supports_block_write(&self) -> bool {
        true
    }
}

/// A Corsair device seeded with a PID + protocol version, present at 0x58.
fn seed(pid: u16, protocol_version: u8) -> RecState {
    let mut info = [0u8; 32];
    info[0] = 0x1C; // VID lo (0x1B1C)
    info[1] = 0x1B; // VID hi
    info[2] = (pid & 0xFF) as u8;
    info[3] = (pid >> 8) as u8;
    info[28] = protocol_version;
    RecState {
        present: HashSet::from([0x58]),
        info,
        sentinel_a: 0x1A,
        sentinel_b: 0x01,
        ..Default::default()
    }
}

fn device(ops: RecordingOps) -> LuaDevice {
    let manifest = parse_manifest(CORSAIR_SRC, Path::new("corsair_dram.lua")).unwrap();
    let spec = manifest.match_specs[0].clone();
    let bus = SmBusDevice::from_ops(1, Box::new(ops));
    let io = PluginIo::Register(RegisterBus::new(bus, AddrScope::single(0x58)));
    let dev_match = DevMatch {
        transport: "smbus".into(),
        bus: Some("chipset".into()),
        addr: Some(0x58),
        pid: None,
    };
    LuaDevice::with_transport(
        "corsair-dram".into(),
        &manifest,
        &spec,
        dev_match,
        io,
        tokio::runtime::Handle::current(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_reports_model_and_led_count() {
    // PID 0x0700 = Vengeance RGB DDR5, 10 LEDs, protocol 4.
    let dev = device(RecordingOps::new(seed(0x0700, 4)));
    assert!(dev.initialize().await.unwrap(), "valid Corsair initializes");
    assert_eq!(dev.model(), "Corsair Vengeance RGB DDR5");
    assert_eq!(dev.descriptor().zones.len(), 1);
    assert_eq!(dev.descriptor().zones[0].leds.len(), 10);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_rejects_wrong_sentinel() {
    let mut state = seed(0x0700, 4);
    state.sentinel_a = 0x00; // not a valid sentinel
    let dev = device(RecordingOps::new(state));
    assert!(!dev.initialize().await.unwrap(), "bad sentinel is rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_rejects_wrong_vid() {
    let mut state = seed(0x0700, 4);
    state.info[1] = 0x00; // corrupt VID hi
    let dev = device(RecordingOps::new(state));
    assert!(!dev.initialize().await.unwrap(), "wrong VID is rejected");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_apply_writes_direct_packet_with_crc() {
    let ops = RecordingOps::new(seed(0x0700, 4));
    let dev = device(ops.clone());
    dev.initialize().await.unwrap();
    ops.drain();

    dev.apply(RgbState::Static {
        color: RgbColor { r: 1, g: 2, b: 3 },
    })
    .await
    .unwrap();

    // Expected direct packet: [led_count, R,G,B ×10, CRC8].
    let mut expected = vec![10u8];
    for _ in 0..10 {
        expected.extend_from_slice(&[1, 2, 3]);
    }
    expected.push(crc8(&expected));

    let blocks: Vec<_> = ops
        .drain()
        .into_iter()
        .filter_map(|r| match r {
            Rec::Block {
                cmd: REG_COLOR_BUFFER_BLOCK_1,
                data,
            } => Some(data),
            _ => None,
        })
        .collect();
    assert_eq!(blocks.len(), 1, "10 LEDs fit one 32-byte block");
    assert_eq!(blocks[0], expected);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn dominator_ddr5_reverses_led_order() {
    // PID 0x0600 = Dominator Platinum DDR5, 12 LEDs, reverse = true, protocol 4.
    let ops = RecordingOps::new(seed(0x0600, 4));
    let dev = device(ops.clone());
    dev.initialize().await.unwrap();
    ops.drain();

    // Per-LED: LED0 red, LED11 blue, rest off.
    let mut zones = std::collections::HashMap::new();
    let mut leds = std::collections::HashMap::new();
    leds.insert("0".to_string(), RgbColor { r: 255, g: 0, b: 0 });
    leds.insert("11".to_string(), RgbColor { r: 0, g: 0, b: 255 });
    zones.insert("leds".to_string(), leds);
    dev.apply(RgbState::PerLed { zones }).await.unwrap();

    // Reverse ⇒ wire LED0 = logical LED11 (blue). 12 LEDs = 38 bytes → two blocks;
    // first block starts [12, 0,0,255, …].
    let first = ops
        .drain()
        .into_iter()
        .find_map(|r| match r {
            Rec::Block {
                cmd: REG_COLOR_BUFFER_BLOCK_1,
                data,
            } => Some(data),
            _ => None,
        })
        .expect("block 1 written");
    assert_eq!(&first[0..4], &[12, 0, 0, 255]);
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_effect_streams_and_commits() {
    let ops = RecordingOps::new(seed(0x0700, 4));
    let dev = device(ops.clone());
    dev.initialize().await.unwrap();
    ops.drain();

    let mut params = std::collections::HashMap::new();
    params.insert(
        "color".to_string(),
        halod_shared::types::EffectParamValue::Color(RgbColor { r: 9, g: 8, b: 7 }),
    );
    dev.apply(RgbState::NativeEffect {
        id: "breathing".to_string(),
        params,
    })
    .await
    .unwrap();

    let recorded = ops.drain();
    // The effect descriptor is streamed, then committed via REG_WRITE_CONFIGURATION.
    let committed = recorded.iter().any(|r| {
        matches!(
            r,
            Rec::Byte {
                cmd: REG_WRITE_CONFIGURATION,
                val: 0x01
            }
        )
    });
    assert!(
        committed,
        "effect must commit config id 0x01 after a CRC match"
    );
    // First streamed byte is the mode (breathing = 0x01).
    let first_stream = recorded.iter().find_map(|r| match r {
        Rec::Byte {
            cmd: REG_SET_BINARY_DATA,
            val,
        } => Some(*val),
        _ => None,
    });
    assert_eq!(first_stream, Some(0x01), "breathing mode byte");
}
