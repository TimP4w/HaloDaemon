// SPDX-License-Identifier: GPL-3.0-or-later
//! Equivalence tests for the built-in Zotac SPECTRA GPU plugin. These replace the
//! unit tests that lived in the deleted native `spectra_gpu` / `spectra_blackwell`
//! driver: they drive the *actual* Lua plugin through the real worker + register
//! transport against a recording SMBus backend, and assert the staged register
//! layout (0x20..0x2F + commit 0x17) matches the native protocol.

use std::path::Path;
use std::sync::{Arc, Mutex};

use super::device::LuaDevice;
use super::parse_manifest;
use super::transport::{AddrScope, PluginIo, RegisterBus};
use super::worker::DevMatch;
use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};
use crate::drivers::{Device, RgbCapability};
use anyhow::{bail, Result};
use halod_shared::types::{EffectParamValue, RgbColor, RgbState};

const ZOTAC_SRC: &str = include_str!("builtins/zotac_spectra_gpu.lua");

const ZOTAC_ADDR: u8 = 0x4B;
const REG_BASE: u8 = 0x20;
const REG_DETECT: u8 = 0x10;
const REG_COMMIT: u8 = 0x17;

#[derive(Default)]
struct RecState {
    /// (cmd, val) for every byte write, in order.
    writes: Vec<(u8, u8)>,
    present: bool,
}

#[derive(Clone)]
struct RecordingOps(Arc<Mutex<RecState>>);

impl RecordingOps {
    fn new(present: bool) -> Self {
        Self(Arc::new(Mutex::new(RecState {
            present,
            ..Default::default()
        })))
    }
    fn writes(&self) -> Vec<(u8, u8)> {
        self.0.lock().unwrap().writes.clone()
    }
}

impl SmBusSyncOps for RecordingOps {
    fn read_byte(&mut self, _addr: u8) -> Result<u8> {
        Ok(0)
    }
    fn read_byte_data(&mut self, _addr: u8, cmd: u8) -> Result<u8> {
        if cmd == REG_DETECT && !self.0.lock().unwrap().present {
            bail!("no device");
        }
        Ok(0)
    }
    fn write_quick(&mut self, _addr: u8) -> Result<bool> {
        Ok(self.0.lock().unwrap().present)
    }
    fn write_byte_data(&mut self, _addr: u8, cmd: u8, val: u8) -> Result<()> {
        self.0.lock().unwrap().writes.push((cmd, val));
        Ok(())
    }
    fn write_word_data(&mut self, _addr: u8, _cmd: u8, _val: u16) -> Result<()> {
        Ok(())
    }
    fn write_block_data(&mut self, _addr: u8, _cmd: u8, _data: &[u8]) -> Result<()> {
        Ok(())
    }
}

fn device(ops: RecordingOps) -> LuaDevice {
    let manifest = parse_manifest(ZOTAC_SRC, Path::new("zotac_spectra_gpu.lua")).unwrap();
    let spec = manifest.match_specs[0].clone();
    let bus = SmBusDevice::from_ops(1, Box::new(ops));
    let io = PluginIo::Register(RegisterBus::new(bus, AddrScope::single(ZOTAC_ADDR)));
    let dev_match = DevMatch {
        transport: "smbus".into(),
        bus: Some("gpu".into()),
        addr: Some(ZOTAC_ADDR),
    };
    LuaDevice::with_transport(
        "zotac-gpu".into(),
        &manifest,
        &spec,
        dev_match,
        io,
        tokio::runtime::Handle::current(),
    )
}

/// The manifest declares the GPU PCI gate (mandatory for `bus = "gpu"`).
#[test]
fn manifest_declares_gpu_pci_gate() {
    let manifest = parse_manifest(ZOTAC_SRC, Path::new("zotac_spectra_gpu.lua")).unwrap();
    let spec = &manifest.match_specs[0];
    assert_eq!(spec.bus.as_deref(), Some("gpu"));
    assert!(
        !spec.pci_match.is_empty(),
        "GPU spec must carry a pci_match"
    );
    assert_eq!(spec.pci_match[0].vendor, Some(0x10DE));
    assert_eq!(spec.pci_match[0].sub_vendor, Some(0x19DA));
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_reports_three_zones() {
    let dev = device(RecordingOps::new(true));
    assert!(dev.initialize().await.unwrap(), "present controller inits");
    let ids: Vec<&str> = dev
        .descriptor()
        .zones
        .iter()
        .map(|z| z.id.as_str())
        .collect();
    assert_eq!(ids, vec!["logo", "side_bar", "infinity_mirror"]);
    for z in &dev.descriptor().zones {
        assert_eq!(z.leds.len(), 1, "each zone is a single LED");
    }
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn initialize_rejects_absent_controller() {
    let dev = device(RecordingOps::new(false));
    assert!(
        !dev.initialize().await.unwrap(),
        "absent controller rejected"
    );
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn static_apply_stages_each_zone_and_commits() {
    let ops = RecordingOps::new(true);
    let dev = device(ops.clone());
    dev.initialize().await.unwrap();

    dev.apply(RgbState::Static {
        color: RgbColor {
            r: 0x11,
            g: 0x22,
            b: 0x33,
        },
    })
    .await
    .unwrap();

    let w = ops.writes();
    // Three zones × (16 staging writes + 1 commit).
    assert_eq!(w.len(), 3 * 17);
    // Zone 0 staging: header, zone, mode=Static(0x01), color1, black color2, …
    assert_eq!(w[0], (REG_BASE, 0x00), "0x20 fixed header");
    assert_eq!(w[1], (REG_BASE + 1, 0x00), "0x21 zone index 0");
    assert_eq!(w[2], (REG_BASE + 2, 0x01), "0x22 mode = Static");
    assert_eq!(
        &w[3..6],
        &[(0x23, 0x11), (0x24, 0x22), (0x25, 0x33)],
        "color1"
    );
    assert_eq!(w[16], (REG_COMMIT, 0x01), "commit after 16 staging writes");
    // Second zone starts at index 17 with zone id 1.
    assert_eq!(w[18], (REG_BASE + 1, 0x01), "second block zone index 1");
}

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn native_effect_writes_mode_and_speed() {
    let ops = RecordingOps::new(true);
    let dev = device(ops.clone());
    dev.initialize().await.unwrap();

    let mut params = std::collections::HashMap::new();
    params.insert("speed".to_string(), EffectParamValue::Float(80.0));
    dev.apply(RgbState::NativeEffect {
        id: "volta".to_string(),
        params,
    })
    .await
    .unwrap();

    let w = ops.writes();
    // Volta mode = 0x22 at register 0x22; speed 80 at register 0x2A.
    assert_eq!(w[2], (REG_BASE + 2, 0x22), "0x22 mode = Volta");
    assert_eq!(w[10], (REG_BASE + 10, 80), "0x2A speed = 80");
}
