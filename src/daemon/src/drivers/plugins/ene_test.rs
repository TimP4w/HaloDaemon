// SPDX-License-Identifier: GPL-3.0-or-later
//! Equivalence tests for the built-in ENE SMBus plugin. These replace the unit
//! tests that lived in the deleted native `ene_smbus` driver: they drive the
//! *actual* Lua plugin through the real worker + register transport against a
//! recording SMBus backend, and assert the emitted register sequences match the
//! native protocol (direct-mode preamble, RBG wire order, block-only frames,
//! firmware layout, DRAM remap, probe rejection).

use std::collections::{HashMap, HashSet};
use std::path::Path;
use std::sync::{Arc, Mutex};

use super::device::LuaDevice;
use super::parse_manifest;
use super::transport::{AddrScope, PluginIo, RegisterBus};
use super::worker::DevMatch;
use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};
use crate::drivers::{Device, RgbCapability, SensorCapability};
use anyhow::{bail, Result};
use halod_shared::types::{RgbColor, RgbState};

const ENE_SRC: &str = include_str!("builtins/ene_smbus.lua");

// Registers mirrored from the plugin/native protocol.
const REG_DEVICE_NAME: u16 = 0x1000;
const REG_CONFIG_TABLE: u16 = 0x1C00;
const REG_MODE: u16 = 0x8021;
const REG_APPLY: u16 = 0x80A0;
const REG_DIRECT: u16 = 0x8020;
const REG_SLOT_INDEX: u16 = 0x80F8;
const REG_I2C_ADDRESS: u16 = 0x80F9;
const DRAM_BROADCAST: u8 = 0x77;

/// Byte-swap the 16-bit register (its own inverse) — the ENE two-stage address.
fn swap(reg: u16) -> u16 {
    ((reg << 8) & 0xFF00) | ((reg >> 8) & 0x00FF)
}

#[derive(Debug, Clone, PartialEq)]
enum Rec {
    Word { addr: u8, cmd: u8, val: u16 },
    Byte { addr: u8, cmd: u8, val: u8 },
    Block { addr: u8, cmd: u8, data: Vec<u8> },
    Quick { addr: u8, ack: bool },
}

#[derive(Default)]
struct RecState {
    ops: Vec<Rec>,
    /// Register values returned via the two-stage read (cmd 0x81).
    regmap: HashMap<u16, u8>,
    /// Register set by the last `write_word_data(_, 0x00, _)`.
    pending: Option<u16>,
    /// Addresses that ACK `write_quick` / `read_byte`.
    present: HashSet<u8>,
    block_supported: bool,
}

/// A recording SMBus backend: models ENE two-stage register reads and records
/// every op so a test can assert the exact wire sequence.
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
        let s = self.0.lock().unwrap();
        if (0xA0..=0xAF).contains(&cmd) {
            return Ok(cmd - 0xA0); // incrementing pattern
        }
        if cmd == 0x81 {
            let reg = s.pending.unwrap_or(0);
            return Ok(s.regmap.get(&reg).copied().unwrap_or(0));
        }
        Ok(0)
    }
    fn write_quick(&mut self, addr: u8) -> Result<bool> {
        let mut s = self.0.lock().unwrap();
        let ack = s.present.contains(&addr);
        s.ops.push(Rec::Quick { addr, ack });
        Ok(ack)
    }
    fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
        self.0
            .lock()
            .unwrap()
            .ops
            .push(Rec::Byte { addr, cmd, val });
        Ok(())
    }
    fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
        let mut s = self.0.lock().unwrap();
        if cmd == 0x00 {
            s.pending = Some(swap(val));
        }
        s.ops.push(Rec::Word { addr, cmd, val });
        Ok(())
    }
    fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
        let mut s = self.0.lock().unwrap();
        if !s.block_supported {
            bail!("block writes unsupported");
        }
        s.ops.push(Rec::Block {
            addr,
            cmd,
            data: data.to_vec(),
        });
        Ok(())
    }
    fn supports_block_write(&self) -> bool {
        self.0.lock().unwrap().block_supported
    }
}

/// Collapse recorded ops into high-level steps: a word address-set (cmd 0x00)
/// followed by a byte value is a register write; followed by a block is a block.
#[derive(Debug, PartialEq)]
enum Step {
    Reg(u16, u8),
    Block(Vec<u8>),
}

fn steps(ops: &[Rec]) -> Vec<Step> {
    let mut out = Vec::new();
    let mut pending = None;
    for op in ops {
        match op {
            Rec::Word { cmd: 0x00, val, .. } => pending = Some(swap(*val)),
            Rec::Byte { cmd: 0x01, val, .. } => out.push(Step::Reg(
                pending.take().expect("byte needs address set"),
                *val,
            )),
            Rec::Block { data, .. } => {
                pending.take().expect("block needs address set");
                out.push(Step::Block(data.clone()));
            }
            other => panic!("unexpected op {other:?}"),
        }
    }
    out
}

/// A valid ENE DRAM device seeded with `version` + `led_count`, present at 0x70.
fn seed_dram(version: &str, led_count: u8) -> RecState {
    let mut regmap = HashMap::new();
    for (i, b) in version.bytes().enumerate() {
        regmap.insert(REG_DEVICE_NAME + i as u16, b);
    }
    regmap.insert(REG_DEVICE_NAME + version.len() as u16, 0);
    regmap.insert(REG_CONFIG_TABLE + 0x02, led_count);
    RecState {
        regmap,
        present: HashSet::from([0x70]),
        block_supported: true,
        ..Default::default()
    }
}

fn dram_device(ops: RecordingOps) -> LuaDevice {
    let manifest = parse_manifest(ENE_SRC, Path::new("ene_smbus.lua")).unwrap();
    let spec = manifest.match_specs[0].clone(); // chipset DRAM spec
    let bus = SmBusDevice::from_ops(1, Box::new(ops));
    let io = PluginIo::Register(RegisterBus::new(bus, AddrScope::single(0x70)));
    let dev_match = DevMatch {
        transport: "smbus".into(),
        bus: Some("chipset".into()),
        addr: Some(0x70),
        pid: None,
    };
    LuaDevice::with_transport(
        "ene-dram".into(),
        &manifest,
        &spec,
        dev_match,
        io,
        tokio::runtime::Handle::current(),
    )
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_reports_firmware_and_led_count() {
    let ops = RecordingOps::new(seed_dram("LED-0116", 8));
    let dev = dram_device(ops);
    assert!(dev.initialize().await.unwrap(), "valid ENE initializes");
    assert_eq!(dev.model(), "LED-0116");
    assert_eq!(dev.descriptor().zones.len(), 1);
    assert_eq!(dev.descriptor().zones[0].leds.len(), 8);
}

#[test]
fn gpu_spec_has_read_byte_probe_and_curated_pci_gate() {
    let manifest = parse_manifest(ENE_SRC, Path::new("ene_smbus.lua")).unwrap();
    let gpu = manifest
        .match_specs
        .iter()
        .find(|s| s.bus.as_deref() == Some("gpu"))
        .expect("ENE declares a gpu spec");

    // Gentle confirm method, never write_quick, on the display-shared bus.
    use crate::drivers::plugins::manifest::ProbeMode;
    assert_eq!(gpu.probe, ProbeMode::ReadByte);

    let gate = &gpu.pci_match;
    // Two broad detectors (unconfirmed) + a curated whitelist (confirmed).
    let broad = gate.iter().filter(|m| !m.confirmed).count();
    let confirmed = gate.iter().filter(|m| m.confirmed).count();
    assert_eq!(broad, 2, "NVIDIA + AMD broad ASUS detectors");
    assert!(confirmed > 100, "full curated board table present");

    // A broad detector wildcards the sub_device; a confirmed entry pins it.
    assert!(gate
        .iter()
        .any(|m| !m.confirmed && m.vendor == Some(0x10DE) && m.sub_device.is_none()));
    // ROG STRIX RTX 4090 O24G Gaming — a known confirmed board.
    assert!(gate.iter().any(|m| m.confirmed
        && m.vendor == Some(0x10DE)
        && m.sub_vendor == Some(0x1043)
        && m.sub_device == Some(0x889C)));
    // TUF RX 7900 XTX — a known confirmed AMD board.
    assert!(gate
        .iter()
        .any(|m| m.confirmed && m.vendor == Some(0x1002) && m.sub_device == Some(0x0506)));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_rejects_micron_module() {
    let mut state = seed_dram("LED-0116", 8);
    // Micron check region (0x1030..) reads back "Micron".
    for (i, b) in b"Micron".iter().enumerate() {
        state.regmap.insert(0x1030 + i as u16, *b);
    }
    let dev = dram_device(RecordingOps::new(state));
    assert!(
        !dev.initialize().await.unwrap(),
        "Micron module must be rejected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_apply_emits_direct_preamble_in_one_batch() {
    let ops = RecordingOps::new(seed_dram("LED-0116", 4));
    let dev = dram_device(ops.clone());
    dev.initialize().await.unwrap();
    ops.drain(); // discard initialize's set_direct_mode writes

    dev.apply(RgbState::Static {
        color: RgbColor { r: 1, g: 2, b: 3 },
    })
    .await
    .unwrap();

    let recorded = ops.drain();
    let s = steps(&recorded);
    // R,B,G × 4 LEDs.
    let block: Vec<u8> = [1u8, 3, 2].iter().cycle().take(12).copied().collect();
    assert_eq!(
        s,
        vec![
            Step::Reg(REG_MODE, 1), // Static
            Step::Reg(REG_APPLY, 1),
            Step::Reg(REG_DIRECT, 1),
            Step::Reg(REG_APPLY, 1),
            Step::Block(block),
            Step::Reg(REG_DIRECT, 1),
            Step::Reg(REG_APPLY, 1),
        ]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn close_leaves_direct_mode_to_hand_lighting_back() {
    let ops = RecordingOps::new(seed_dram("LED-0116", 4));
    let dev = dram_device(ops.clone());
    dev.initialize().await.unwrap();
    ops.drain(); // discard initialize's writes

    dev.close().await;

    // set_direct_mode(false): DIRECT=0 then APPLY, nothing else.
    assert_eq!(
        steps(&ops.drain()),
        vec![Step::Reg(REG_DIRECT, 0), Step::Reg(REG_APPLY, 1)]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_frame_emits_only_the_color_block_in_rbg_order() {
    let ops = RecordingOps::new(seed_dram("LED-0116", 2));
    let dev = dram_device(ops.clone());
    dev.initialize().await.unwrap();
    ops.drain();

    dev.write_frame(
        "leds",
        &[
            RgbColor {
                r: 0xAA,
                g: 0xBB,
                b: 0xCC,
            },
            RgbColor {
                r: 0x11,
                g: 0x22,
                b: 0x33,
            },
        ],
    )
    .await
    .unwrap();

    let recorded = ops.drain();
    // No mode/apply register writes — frame is color data only.
    assert!(
        !recorded
            .iter()
            .any(|op| matches!(op, Rec::Byte { cmd: 0x01, .. })),
        "frame must not touch MODE/DIRECT/APPLY"
    );
    // One block, RBG wire order.
    let s = steps(&recorded);
    assert_eq!(
        s,
        vec![Step::Block(vec![0xAA, 0xCC, 0xBB, 0x11, 0x33, 0x22])]
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn write_frame_falls_back_to_byte_writes_without_block_support() {
    let mut state = seed_dram("LED-0116", 1);
    state.block_supported = false;
    let ops = RecordingOps::new(state);
    let dev = dram_device(ops.clone());
    dev.initialize().await.unwrap();
    ops.drain();

    dev.write_frame(
        "leds",
        &[RgbColor {
            r: 0xAA,
            g: 0xBB,
            b: 0xCC,
        }],
    )
    .await
    .unwrap();

    let recorded = ops.drain();
    // No block ops; three color bytes written one at a time (RBG order).
    assert!(!recorded.iter().any(|op| matches!(op, Rec::Block { .. })));
    let byte_vals: Vec<u8> = recorded
        .iter()
        .filter_map(|op| match op {
            Rec::Byte { cmd: 0x01, val, .. } => Some(*val),
            _ => None,
        })
        .collect();
    assert_eq!(byte_vals, vec![0xAA, 0xCC, 0xBB]);
}

#[test]
fn firmware_layout_matches_native_for_known_versions() {
    // Drive `initialize` synchronously is awkward; instead assert the plugin's
    // version→led_count table via the descriptor, one version per layout family.
    let rt = tokio::runtime::Builder::new_multi_thread()
        .worker_threads(2)
        .enable_all()
        .build()
        .unwrap();
    rt.block_on(async {
        // GPU firmware reads led count from config[0x03], not [0x02].
        let mut state = seed_dram("AUMA0-E6K5-0107", 0);
        state.regmap.insert(REG_CONFIG_TABLE + 0x03, 5);
        let dev = dram_device(RecordingOps::new(state));
        assert!(dev.initialize().await.unwrap());
        assert_eq!(dev.descriptor().zones[0].leds.len(), 5);
    });
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn led_count_clamps_to_thirty() {
    let ops = RecordingOps::new(seed_dram("LED-0116", 100));
    let dev = dram_device(ops);
    dev.initialize().await.unwrap();
    assert_eq!(dev.descriptor().zones[0].leds.len(), 30);
}

#[test]
fn pre_scan_remaps_dram_sticks_off_the_broadcast_address() {
    // Broadcast present, first candidate free (NAK): slot 0 is bound to it.
    let state = RecState {
        present: HashSet::from([DRAM_BROADCAST]),
        block_supported: true,
        ..Default::default()
    };
    let ops = RecordingOps::new(state);
    let bus = SmBusDevice::from_ops(1, Box::new(ops.clone()));
    let scope = vec![
        0x70,
        0x71,
        0x72,
        0x73,
        0x74,
        0x75,
        0x76,
        0x4F,
        0x66,
        0x67,
        0x39,
        0x3A,
        0x3B,
        0x3C,
        0x3D,
        DRAM_BROADCAST,
    ];

    let rt = tokio::runtime::Builder::new_current_thread()
        .build()
        .unwrap();
    let handle = rt.handle().clone();
    std::thread::spawn(move || {
        crate::drivers::plugins::run_pre_scan(ENE_SRC, bus, scope, &[], handle).unwrap();
    })
    .join()
    .unwrap();

    let recorded = ops.drain();
    // First slot: SLOT_INDEX=0 then I2C_ADDRESS=(0x70<<1), both on the broadcast.
    let reg_writes: Vec<(u16, u8, u8)> = {
        let mut pending = None;
        let mut out = Vec::new();
        for op in &recorded {
            match op {
                Rec::Word {
                    cmd: 0x00,
                    val,
                    addr,
                } => pending = Some((swap(*val), *addr)),
                Rec::Byte { cmd: 0x01, val, .. } => {
                    let (reg, addr) = pending.take().unwrap();
                    out.push((reg, addr, *val));
                }
                _ => {}
            }
        }
        out
    };
    assert_eq!(reg_writes[0], (REG_SLOT_INDEX, DRAM_BROADCAST, 0));
    assert_eq!(reg_writes[1], (REG_I2C_ADDRESS, DRAM_BROADCAST, 0x70 << 1));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn register_ops_outside_scope_are_rejected() {
    // The plugin is scoped to 0x70; a batch touching another address must error.
    let src = r#"
        return {
          match = { { transport = "smbus", bus = "chipset", addresses = { 0x70 } } },
          identity = { vendor = "T", model = "M" },
          sensor = {},
          get_sensors = function(dev)
            -- 0x50 is outside the declared scope → raises.
            dev.transport:batch(function(ops) return ops:read_byte_data(0x50, 0x00) end)
            return {}
          end,
        }
    "#;
    let manifest = parse_manifest(src, Path::new("scoped.lua")).unwrap();
    let bus = SmBusDevice::from_ops(1, Box::new(RecordingOps::new(RecState::default())));
    let io = PluginIo::Register(RegisterBus::new(bus, AddrScope::single(0x70)));
    let dev = LuaDevice::with_transport(
        "scoped".into(),
        &manifest,
        &manifest.match_specs[0].clone(),
        DevMatch {
            transport: "smbus".into(),
            bus: Some("chipset".into()),
            addr: Some(0x70),
            pid: None,
        },
        io,
        tokio::runtime::Handle::current(),
    );
    let err = dev.get_sensors().await.unwrap_err();
    assert!(
        format!("{err:#}").contains("outside its declared scope"),
        "expected scope violation, got: {err:#}"
    );
}
