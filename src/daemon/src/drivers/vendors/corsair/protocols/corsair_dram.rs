// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
// SPDX-FileCopyrightText: Erik Gilling (konkers) — OpenRGB project
// Reference: OpenRGB Corsair DRAM controller
// refs/OpenRGB/Controllers/CorsairDRAMController/

use anyhow::{anyhow, Result};
use std::sync::Arc;
use std::time::Duration;

use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};

// ── Register map ──────────────────────────────────────────────────────────────

const REG_RESET_BUFFER: u8 = 0x0B;
const REG_SET_BINARY_DATA: u8 = 0x20;
const REG_BINARY_START: u8 = 0x21;
const REG_STATUS: u8 = 0x30;
const REG_COLOR_BUFFER_BLOCK_1: u8 = 0x31;
const REG_COLOR_BUFFER_BLOCK_2: u8 = 0x32;
const REG_GET_BINARY_DATA: u8 = 0x40;
const REG_GET_CHECKSUM: u8 = 0x42;
const REG_SENTINEL_A: u8 = 0x43;
const REG_SENTINEL_B: u8 = 0x44;
const REG_GET_DEVICE_INFO: u8 = 0x61;
const REG_WRITE_CONFIGURATION: u8 = 0x82;

const CONFIG_ID_EFFECT: u8 = 0x01;
const CONFIG_ID_COLOR_DATA: u8 = 0x02;

const STATUS_BUSY_BIT: u8 = 0x08;

pub const CORSAIR_DRAM_VID: u16 = 0x1B1C;

// ── Native effect modes ───────────────────────────────────────────────────────

#[allow(dead_code)]
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum CorsairDramMode {
    ColorShift = 0x00,
    ColorPulse = 0x01,
    RainbowWave = 0x03,
    ColorWave = 0x04,
    Visor = 0x05,
    Rain = 0x06,
    Marquee = 0x07,
    Rainbow = 0x08,
    Sequential = 0x09,
    Static = 0x10,
}

/// Maps a native-effect string ID to its protocol mode.
/// Returns `None` for IDs that are not hardware effects (e.g. "off").
pub fn corsair_mode_from_id(id: &str) -> Option<CorsairDramMode> {
    match id {
        "breathing" => Some(CorsairDramMode::ColorPulse),
        "rainbow_wave" => Some(CorsairDramMode::RainbowWave),
        "color_shift" => Some(CorsairDramMode::ColorShift),
        _ => None,
    }
}

#[repr(u8)]
#[derive(Clone, Copy)]
pub enum CorsairDramSpeed {
    Slow = 0x00,
    Medium = 0x01,
    Fast = 0x02,
}

pub fn corsair_speed_from_str(s: &str) -> CorsairDramSpeed {
    match s {
        "slow" => CorsairDramSpeed::Slow,
        "fast" => CorsairDramSpeed::Fast,
        _ => CorsairDramSpeed::Medium,
    }
}

#[repr(u8)]
#[derive(Clone, Copy)]
pub enum CorsairDramDirection {
    Up = 0x00,
    Down = 0x01,
    Left = 0x02,
    Right = 0x03,
}

pub fn corsair_direction_from_str(s: &str) -> CorsairDramDirection {
    match s {
        "up" => CorsairDramDirection::Up,
        "down" => CorsairDramDirection::Down,
        "left" => CorsairDramDirection::Left,
        _ => CorsairDramDirection::Right,
    }
}

// ── Device info ───────────────────────────────────────────────────────────────

pub struct CorsairDramInfo {
    pub pid: u16,
    pub led_count: usize,
    pub reverse: bool,
    pub model_name: &'static str,
    pub firmware: String,
    pub protocol_version: u8,
}

// ── Device table ──────────────────────────────────────────────────────────────

struct DeviceEntry {
    pids: &'static [u16],
    name: &'static str,
    led_count: usize,
    reverse: bool,
}

static DEVICE_TABLE: &[DeviceEntry] = &[
    DeviceEntry {
        pids: &[0x0700, 0x0701, 0x0900, 0x0901, 0x0910, 0x0911],
        name: "Corsair Vengeance RGB DDR5",
        led_count: 10,
        reverse: false,
    },
    DeviceEntry {
        pids: &[0x0600, 0x0601],
        name: "Corsair Dominator Platinum RGB DDR5",
        led_count: 12,
        reverse: true,
    },
    DeviceEntry {
        pids: &[0x0800, 0x0801, 0x0810, 0x0811],
        name: "Corsair Dominator Titanium RGB DDR5",
        led_count: 12,
        reverse: true,
    },
    DeviceEntry {
        pids: &[0x0A00, 0x0A01, 0x0A10, 0x0A11],
        name: "Corsair Vengeance Shugo Series DDR5",
        led_count: 10,
        reverse: false,
    },
    DeviceEntry {
        pids: &[0x0B00, 0x0B01],
        name: "Corsair Vengeance RGB RS DDR5",
        led_count: 6,
        reverse: false,
    },
    DeviceEntry {
        pids: &[0x0100, 0x0101],
        name: "Corsair Vengeance RGB Pro DDR4",
        led_count: 10,
        reverse: false,
    },
    DeviceEntry {
        pids: &[0x0200, 0x0201],
        name: "Corsair Dominator Platinum RGB DDR4",
        led_count: 12,
        reverse: true,
    },
    DeviceEntry {
        pids: &[0x0300, 0x0301],
        name: "Corsair Vengeance RGB Pro SL DDR4",
        led_count: 10,
        reverse: false,
    },
    DeviceEntry {
        pids: &[0x0400, 0x0401],
        name: "Corsair Vengeance RGB RS DDR4",
        led_count: 6,
        reverse: false,
    },
];

pub fn device_info_from_pid(pid: u16) -> (&'static str, usize, bool) {
    for entry in DEVICE_TABLE {
        if entry.pids.contains(&pid) {
            return (entry.name, entry.led_count, entry.reverse);
        }
    }
    log::warn!(
        "[Corsair DRAM] Unknown PID 0x{:04X}, assuming 10 LEDs (Vengeance DDR5 layout)",
        pid
    );
    ("Corsair DRAM RGB", 10, false)
}

// ── CRC-8 (poly 0x07, init 0x00, no reflection) ──────────────────────────────

pub fn crc8(data: &[u8]) -> u8 {
    let mut crc: u8 = 0;
    for &byte in data {
        crc ^= byte;
        for _ in 0..8 {
            if crc & 0x80 != 0 {
                crc = (crc << 1) ^ 0x07;
            } else {
                crc <<= 1;
            }
        }
    }
    crc
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// Returns true if a Corsair DRAM controller is present at `addr`.
/// Three-step probe: ACK on write_quick, sentinel checks at 0x43 and 0x44.
pub async fn test_for_corsair_dram(bus: &SmBusDevice, addr: u8) -> bool {
    bus.run_batch(move |ops| Ok(probe_corsair_dram(ops, addr)))
        .await
        .unwrap_or(false)
}

fn probe_corsair_dram(ops: &mut dyn SmBusSyncOps, addr: u8) -> bool {
    if !ops.write_quick(addr).unwrap_or(false) {
        return false;
    }
    match ops.read_byte_data(addr, REG_SENTINEL_A) {
        Ok(v) if v == 0x1A || v == 0x1B || v == 0x1C => {}
        _ => return false,
    }
    match ops.read_byte_data(addr, REG_SENTINEL_B) {
        Ok(v) if v == 0x01 || v == 0x03 || v == 0x04 => {}
        _ => return false,
    }
    true
}

// ── Device info read ──────────────────────────────────────────────────────────

/// Read 32-byte device info block, verify CRC8, extract VID/PID/firmware/protocol.
pub async fn read_corsair_dram_info(bus: &SmBusDevice, addr: u8) -> Result<CorsairDramInfo> {
    bus.run_batch(move |ops| sync_read_device_info(ops, addr))
        .await
}

fn sync_read_device_info(ops: &mut dyn SmBusSyncOps, addr: u8) -> Result<CorsairDramInfo> {
    // Request device info binary buffer
    ops.write_byte_data(addr, REG_GET_DEVICE_INFO, 0x00)?;
    ops.write_byte_data(addr, REG_BINARY_START, 0x00)?;

    let mut data = [0u8; 32];
    for slot in &mut data {
        *slot = ops.read_byte_data(addr, REG_GET_BINARY_DATA)?;
    }

    let calc_crc = crc8(&data);
    let device_crc = ops.read_byte_data(addr, REG_GET_CHECKSUM)?;
    if calc_crc != device_crc {
        log::warn!(
            "[Corsair DRAM] addr 0x{:02X}: device info CRC mismatch (calc=0x{:02X} device=0x{:02X})",
            addr, calc_crc, device_crc
        );
    }

    let vid = u16::from_le_bytes([data[0], data[1]]);
    let pid = u16::from_le_bytes([data[2], data[3]]);
    let firmware = format!(
        "{}.{}.{}",
        data[9],
        data[8],
        u16::from_le_bytes([data[10], data[11]])
    );
    let protocol_version = data[28];

    if vid != CORSAIR_DRAM_VID {
        return Err(anyhow!(
            "addr 0x{:02X}: unexpected VID 0x{:04X} (expected 0x{:04X})",
            addr,
            vid,
            CORSAIR_DRAM_VID
        ));
    }

    let (model_name, led_count, reverse) = device_info_from_pid(pid);

    Ok(CorsairDramInfo {
        pid,
        led_count,
        reverse,
        model_name,
        firmware,
        protocol_version,
    })
}

// ── Color write — version-dispatched ─────────────────────────────────────────

/// Write per-LED colors, choosing the correct path based on `info.protocol_version`.
pub async fn corsair_set_colors(
    bus: &SmBusDevice,
    addr: u8,
    info: &CorsairDramInfo,
    colors: &[[u8; 3]],
) -> Result<()> {
    if info.protocol_version >= 4 {
        corsair_set_colors_direct(bus, addr, info, colors).await
    } else {
        corsair_set_colors_effect(bus, addr, info, colors).await
    }
}

// ── Color write — direct mode (protocol ≥ 4) ─────────────────────────────────

/// Write per-LED colors using direct block-write mode.
/// Packet: [led_count, R₀, G₀, B₀, …, CRC8(packet[0..n-1])]
/// Fits first 32 bytes to BLOCK_1, remainder to BLOCK_2.
pub async fn corsair_set_colors_direct(
    bus: &SmBusDevice,
    addr: u8,
    info: &CorsairDramInfo,
    colors: &[[u8; 3]],
) -> Result<()> {
    let packet = build_direct_packet(info, colors);
    bus.run_batch(move |ops| write_direct_packet(ops, addr, &packet))
        .await
}

pub(crate) fn build_direct_packet(info: &CorsairDramInfo, colors: &[[u8; 3]]) -> Vec<u8> {
    let led_count = info.led_count;
    let packet_size = led_count * 3 + 2;
    let mut packet = vec![0u8; packet_size];
    packet[0] = led_count as u8;

    for led_idx in 0..led_count {
        let color_idx = if info.reverse {
            led_count - 1 - led_idx
        } else {
            led_idx
        };
        let color = colors.get(color_idx).copied().unwrap_or([0, 0, 0]);
        let offset = led_idx * 3 + 1;
        packet[offset] = color[0];
        packet[offset + 1] = color[1];
        packet[offset + 2] = color[2];
    }

    let crc = crc8(&packet[..packet_size - 1]);
    packet[packet_size - 1] = crc;
    packet
}

fn write_direct_packet(ops: &mut dyn SmBusSyncOps, addr: u8, packet: &[u8]) -> Result<()> {
    let first_chunk = &packet[..packet.len().min(32)];
    ops.write_block_data(addr, REG_COLOR_BUFFER_BLOCK_1, first_chunk)?;
    if packet.len() > 32 {
        ops.write_block_data(addr, REG_COLOR_BUFFER_BLOCK_2, &packet[32..])?;
    }
    Ok(())
}

// ── Color write — effect mode (protocol < 4) ──────────────────────────────────

/// Write per-LED colors via binary streaming (legacy DDR4 protocol).
/// Format: R, G, B, 0xFF per LED streamed byte-by-byte.
pub async fn corsair_set_colors_effect(
    bus: &SmBusDevice,
    addr: u8,
    info: &CorsairDramInfo,
    colors: &[[u8; 3]],
) -> Result<()> {
    let led_count = info.led_count;
    let reverse = info.reverse;
    let mut color_data = Vec::with_capacity(led_count * 4);
    for led_idx in 0..led_count {
        let color_idx = if reverse {
            led_count - 1 - led_idx
        } else {
            led_idx
        };
        let [r, g, b] = colors.get(color_idx).copied().unwrap_or([0, 0, 0]);
        color_data.extend_from_slice(&[r, g, b, 0xFF]);
    }

    bus.run_batch(move |ops| {
        stream_binary(ops, addr, &color_data)?;
        let calc_crc = crc8(&color_data);
        let device_crc = ops.read_byte_data(addr, REG_GET_CHECKSUM)?;
        if calc_crc == device_crc {
            ops.write_byte_data(addr, REG_WRITE_CONFIGURATION, CONFIG_ID_COLOR_DATA)?;
            wait_ready(ops, addr);
        } else {
            log::warn!("[Corsair DRAM] 0x{:02X}: color CRC mismatch, skipping apply", addr);
        }
        Ok(())
    })
    .await
}

// ── Native effect write ───────────────────────────────────────────────────────

pub struct NativeEffectParams {
    pub mode: CorsairDramMode,
    pub speed: CorsairDramSpeed,
    pub direction: CorsairDramDirection,
    pub color1: [u8; 3],
    pub color2: [u8; 3],
    pub brightness: u8,
    pub random: bool,
}

/// Write a native hardware effect via binary streaming (all protocol versions).
pub async fn corsair_set_native_effect(bus: &SmBusDevice, addr: u8, p: NativeEffectParams) -> Result<()> {
    let mut effect = [0u8; 20];
    effect[0] = p.mode as u8;
    effect[1] = p.speed as u8;
    effect[2] = if p.random { 0x00 } else { 0x01 };
    effect[3] = p.direction as u8;
    effect[4] = p.color1[0];
    effect[5] = p.color1[1];
    effect[6] = p.color1[2];
    effect[7] = p.brightness;
    effect[8] = p.color2[0];
    effect[9] = p.color2[1];
    effect[10] = p.color2[2];
    effect[11] = p.brightness;
    // [12..19] remain 0

    bus.run_batch(move |ops| {
        stream_binary(ops, addr, &effect)?;
        let calc_crc = crc8(&effect);
        let device_crc = ops.read_byte_data(addr, REG_GET_CHECKSUM)?;
        if calc_crc == device_crc {
            ops.write_byte_data(addr, REG_WRITE_CONFIGURATION, CONFIG_ID_EFFECT)?;
            wait_ready(ops, addr);
        } else {
            log::warn!("[Corsair DRAM] 0x{:02X}: effect CRC mismatch, skipping apply", addr);
        }
        Ok(())
    })
    .await
}

// ── Shared helpers ────────────────────────────────────────────────────────────

fn stream_binary(ops: &mut dyn SmBusSyncOps, addr: u8, data: &[u8]) -> Result<()> {
    ops.write_byte_data(addr, REG_RESET_BUFFER, 0x00)?;
    ops.write_byte_data(addr, REG_BINARY_START, 0x00)?;
    for &byte in data {
        ops.write_byte_data(addr, REG_SET_BINARY_DATA, byte)?;
    }
    Ok(())
}

fn wait_ready(ops: &mut dyn SmBusSyncOps, addr: u8) {
    for _ in 0..5 {
        match ops.read_byte_data(addr, REG_STATUS) {
            Ok(status) if (status & STATUS_BUSY_BIT) == 0 => return,
            _ => {}
        }
        std::thread::sleep(Duration::from_millis(10));
    }
}

// ── Protocol struct wrapper ───────────────────────────────────────────────────

pub struct CorsairDramProtocol {
    bus: Arc<SmBusDevice>,
    addr: u8,
}

impl CorsairDramProtocol {
    pub fn new(bus: Arc<SmBusDevice>, addr: u8) -> Self {
        Self { bus, addr }
    }

    pub fn bus_number(&self) -> u8 {
        self.bus.bus_number
    }

    pub fn addr(&self) -> u8 {
        self.addr
    }

    pub async fn test(&self) -> bool {
        test_for_corsair_dram(&self.bus, self.addr).await
    }

    pub async fn read_info(&self) -> Result<CorsairDramInfo> {
        read_corsair_dram_info(&self.bus, self.addr).await
    }

    pub async fn set_colors(&self, info: &CorsairDramInfo, colors: &[[u8; 3]]) -> Result<()> {
        corsair_set_colors(&self.bus, self.addr, info, colors).await
    }

    pub async fn set_colors_direct(
        &self,
        info: &CorsairDramInfo,
        colors: &[[u8; 3]],
    ) -> Result<()> {
        corsair_set_colors_direct(&self.bus, self.addr, info, colors).await
    }

    pub async fn set_colors_effect(
        &self,
        info: &CorsairDramInfo,
        colors: &[[u8; 3]],
    ) -> Result<()> {
        corsair_set_colors_effect(&self.bus, self.addr, info, colors).await
    }

    pub async fn set_native_effect(&self, p: NativeEffectParams) -> Result<()> {
        corsair_set_native_effect(&self.bus, self.addr, p).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── MockSmBusOps ──────────────────────────────────────────────────────────

    struct MockOps {
        reads: std::collections::HashMap<u8, u8>,
        quick_ack: bool,
        written_bytes: Vec<(u8, u8)>,
        written_blocks: Vec<(u8, Vec<u8>)>,
    }

    impl MockOps {
        fn new(quick_ack: bool) -> Self {
            Self {
                reads: Default::default(),
                quick_ack,
                written_bytes: Vec::new(),
                written_blocks: Vec::new(),
            }
        }

        fn set_read(&mut self, cmd: u8, val: u8) {
            self.reads.insert(cmd, val);
        }
    }

    impl SmBusSyncOps for MockOps {
        fn read_byte(&mut self, _addr: u8) -> Result<u8> {
            Ok(0)
        }
        fn read_byte_data(&mut self, _addr: u8, cmd: u8) -> Result<u8> {
            self.reads.get(&cmd).copied().ok_or_else(|| anyhow!("no mock for cmd 0x{:02X}", cmd))
        }
        fn write_quick(&mut self, _addr: u8) -> Result<bool> {
            Ok(self.quick_ack)
        }
        fn write_byte_data(&mut self, _addr: u8, cmd: u8, val: u8) -> Result<()> {
            self.written_bytes.push((cmd, val));
            Ok(())
        }
        fn write_word_data(&mut self, _addr: u8, _cmd: u8, _val: u16) -> Result<()> {
            Ok(())
        }
        fn write_block_data(&mut self, _addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
            self.written_blocks.push((cmd, data.to_vec()));
            Ok(())
        }
    }

    // ── CRC-8 ─────────────────────────────────────────────────────────────────

    #[test]
    fn crc8_empty_is_zero() {
        assert_eq!(crc8(&[]), 0x00);
    }

    #[test]
    fn crc8_single_zero_is_zero() {
        assert_eq!(crc8(&[0x00]), 0x00);
    }

    #[test]
    fn crc8_known_values() {
        // CRC-8/SMBUS of [0x01, 0x02, 0x03]
        // Verified against: https://crccalc.com (CRC-8/SMBUS = 0x48)
        assert_eq!(crc8(&[0x01, 0x02, 0x03]), 0x48);
    }

    #[test]
    fn crc8_all_ones_byte() {
        // CRC-8/SMBUS of [0xFF] = 0xF3 (243)
        assert_eq!(crc8(&[0xFF]), 0xF3);
    }

    // ── Probe ─────────────────────────────────────────────────────────────────

    #[test]
    fn probe_fails_on_nack() {
        let mut ops = MockOps::new(false);
        assert!(!probe_corsair_dram(&mut ops, 0x58));
    }

    #[test]
    fn probe_fails_on_wrong_sentinel_a() {
        let mut ops = MockOps::new(true);
        ops.set_read(REG_SENTINEL_A, 0x00); // wrong
        ops.set_read(REG_SENTINEL_B, 0x01);
        assert!(!probe_corsair_dram(&mut ops, 0x58));
    }

    #[test]
    fn probe_fails_on_wrong_sentinel_b() {
        let mut ops = MockOps::new(true);
        ops.set_read(REG_SENTINEL_A, 0x1A);
        ops.set_read(REG_SENTINEL_B, 0x00); // wrong
        assert!(!probe_corsair_dram(&mut ops, 0x58));
    }

    #[test]
    fn probe_succeeds_on_all_sentinel_combinations() {
        for a in [0x1A, 0x1B, 0x1C] {
            for b in [0x01, 0x03, 0x04] {
                let mut ops = MockOps::new(true);
                ops.set_read(REG_SENTINEL_A, a);
                ops.set_read(REG_SENTINEL_B, b);
                assert!(probe_corsair_dram(&mut ops, 0x58), "a=0x{a:02X} b=0x{b:02X}");
            }
        }
    }

    // ── Device table ──────────────────────────────────────────────────────────

    #[test]
    fn device_info_vengeance_ddr5_pid_0700() {
        let (name, leds, reverse) = device_info_from_pid(0x0700);
        assert_eq!(name, "Corsair Vengeance RGB DDR5");
        assert_eq!(leds, 10);
        assert!(!reverse);
    }

    #[test]
    fn device_info_dominator_platinum_ddr5_reversed() {
        let (_, leds, reverse) = device_info_from_pid(0x0600);
        assert_eq!(leds, 12);
        assert!(reverse);
    }

    #[test]
    fn device_info_unknown_pid_defaults_to_10_leds() {
        let (_, leds, reverse) = device_info_from_pid(0xFFFF);
        assert_eq!(leds, 10);
        assert!(!reverse);
    }

    // ── Direct packet ─────────────────────────────────────────────────────────

    #[test]
    fn direct_packet_size_is_led_count_times_3_plus_2() {
        let info = CorsairDramInfo {
            pid: 0x0700,
            led_count: 10,
            reverse: false,
            model_name: "test",
            firmware: "1.0.0".into(),
            protocol_version: 4,
        };
        let colors = vec![[0u8; 3]; 10];
        let packet = build_direct_packet(&info, &colors);
        assert_eq!(packet.len(), 10 * 3 + 2);
    }

    #[test]
    fn direct_packet_first_byte_is_led_count() {
        let info = CorsairDramInfo {
            pid: 0x0700,
            led_count: 10,
            reverse: false,
            model_name: "test",
            firmware: "1.0.0".into(),
            protocol_version: 4,
        };
        let colors = vec![[0u8; 3]; 10];
        let packet = build_direct_packet(&info, &colors);
        assert_eq!(packet[0], 10);
    }

    #[test]
    fn direct_packet_last_byte_is_valid_crc8() {
        let info = CorsairDramInfo {
            pid: 0x0700,
            led_count: 3,
            reverse: false,
            model_name: "test",
            firmware: "1.0.0".into(),
            protocol_version: 4,
        };
        let colors = vec![[0xAAu8, 0xBB, 0xCC]; 3];
        let packet = build_direct_packet(&info, &colors);
        let expected_crc = crc8(&packet[..packet.len() - 1]);
        assert_eq!(*packet.last().unwrap(), expected_crc);
    }

    #[test]
    fn direct_packet_reverse_flag_reverses_color_order() {
        let info_normal = CorsairDramInfo {
            pid: 0x0700,
            led_count: 2,
            reverse: false,
            model_name: "test",
            firmware: "1.0.0".into(),
            protocol_version: 4,
        };
        let info_reversed = CorsairDramInfo {
            pid: 0x0700,
            led_count: 2,
            reverse: true,
            model_name: "test",
            firmware: "1.0.0".into(),
            protocol_version: 4,
        };
        let colors = vec![[0xFFu8, 0x00, 0x00], [0x00, 0x00, 0xFF]];

        let normal = build_direct_packet(&info_normal, &colors);
        let reversed = build_direct_packet(&info_reversed, &colors);

        // Normal: LED0=red, LED1=blue; Reversed: LED0=blue, LED1=red
        assert_eq!(&normal[1..4], &[0xFF, 0x00, 0x00]); // LED0 = red
        assert_eq!(&reversed[1..4], &[0x00, 0x00, 0xFF]); // LED0 = blue (was LED1)
    }

    #[test]
    fn write_direct_packet_splits_at_32_bytes() {
        let info = CorsairDramInfo {
            pid: 0x0700,
            led_count: 10,
            reverse: false,
            model_name: "test",
            firmware: "1.0.0".into(),
            protocol_version: 4,
        };
        // 10 LEDs * 3 + 2 = 32 bytes exactly — fits in one block
        let colors = vec![[0u8; 3]; 10];
        let packet = build_direct_packet(&info, &colors);
        assert_eq!(packet.len(), 32);

        let mut ops = MockOps::new(true);
        write_direct_packet(&mut ops, 0x58, &packet).unwrap();
        assert_eq!(ops.written_blocks.len(), 1);
        assert_eq!(ops.written_blocks[0].0, REG_COLOR_BUFFER_BLOCK_1);
    }

    #[test]
    fn write_direct_packet_uses_block_2_for_large_payload() {
        // 12 LEDs * 3 + 2 = 38 bytes → needs BLOCK_2
        let info = CorsairDramInfo {
            pid: 0x0600,
            led_count: 12,
            reverse: true,
            model_name: "test",
            firmware: "1.0.0".into(),
            protocol_version: 4,
        };
        let colors = vec![[0u8; 3]; 12];
        let packet = build_direct_packet(&info, &colors);
        assert_eq!(packet.len(), 38);

        let mut ops = MockOps::new(true);
        write_direct_packet(&mut ops, 0x58, &packet).unwrap();
        assert_eq!(ops.written_blocks.len(), 2);
        assert_eq!(ops.written_blocks[0].0, REG_COLOR_BUFFER_BLOCK_1);
        assert_eq!(ops.written_blocks[0].1.len(), 32);
        assert_eq!(ops.written_blocks[1].0, REG_COLOR_BUFFER_BLOCK_2);
        assert_eq!(ops.written_blocks[1].1.len(), 6);
    }
}
