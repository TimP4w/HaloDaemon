// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project
// Reference: OpenRGB ENE SMBus implementation
// https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusInterface/ENESMBusInterface_i2c_smbus.cpp
// SPD reference: https://gitlab.com/CalcProgrammer1/OpenRGB/-/blob/master/Controllers/ENESMBusController/ENESMBusController.cpp

use anyhow::{anyhow, Result};
use std::sync::Arc;

use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};

// ── Register map ──────────────────────────────────────────────────────────────

pub const ENE_REG_DEVICE_NAME: u16 = 0x1000;
pub const ENE_REG_MICRON_CHECK: u16 = 0x1030;
pub const ENE_REG_CONFIG_TABLE: u16 = 0x1C00;
pub const ENE_REG_COLORS_DIRECT: u16 = 0x8000;
pub const ENE_REG_COLORS_EFFECT: u16 = 0x8010;
pub const ENE_REG_DIRECT: u16 = 0x8020;
pub const ENE_REG_MODE: u16 = 0x8021;
pub const ENE_REG_SPEED: u16 = 0x8022;
pub const ENE_REG_DIRECTION: u16 = 0x8023;
pub const ENE_REG_APPLY: u16 = 0x80A0;
pub const ENE_REG_SLOT_INDEX: u16 = 0x80F8;
pub const ENE_REG_I2C_ADDRESS: u16 = 0x80F9;
pub const ENE_REG_COLORS_DIRECT_V2: u16 = 0x8100;
pub const ENE_REG_COLORS_EFFECT_V2: u16 = 0x8160;

pub const ENE_APPLY_VAL: u8 = 0x01;
pub const ENE_DRAM_BROADCAST: u8 = 0x77;

// ── Modes and speeds ──────────────────────────────────────────────────────────

#[allow(dead_code)]
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum EneMode {
    Off = 0,
    Static = 1,
    Breathing = 2,
    Flashing = 3,
    SpectrumCycle = 4,
    Rainbow = 5,
    SpectrumCycleBreathing = 6,
    ChaseFade = 7,
    SpectrumCycleChaseFade = 8,
    Chase = 9,
    SpectrumCycleChase = 10,
    SpectrumCycleWave = 11,
    ChaseRainbowPulse = 12,
    RandomFlicker = 13,
    DoubleFade = 14,
}

#[allow(dead_code)]
#[repr(u8)]
#[derive(Clone, Copy)]
pub enum EneSpeed {
    Fastest = 0,
    Fast = 1,
    Normal = 2,
    Slow = 3,
    Slowest = 4,
}

// ── Device info ───────────────────────────────────────────────────────────────

pub struct EneDeviceInfo {
    pub version: String,
    pub led_count: usize,
    pub direct_reg: u16,
    pub effect_reg: u16,
}

// ── Low-level register I/O ────────────────────────────────────────────────────

/// ENE two-stage register addressing: byte-swap the 16-bit register address
/// and write it to command 0x00, then read/write from command 0x81/0x01.
pub(crate) fn reg_addr_bytes(reg: u16) -> u16 {
    ((reg << 8) & 0xFF00) | ((reg >> 8) & 0x00FF)
}

/// ENE controllers use BGR wire order (R and B swapped relative to RGB).
fn to_rbg(r: u8, g: u8, b: u8) -> [u8; 3] {
    [r, b, g]
}

/// Sync helpers — called inside `run_batch` closures; no async overhead.
fn read_reg(ops: &mut dyn SmBusSyncOps, addr: u8, reg: u16) -> Result<u8> {
    ops.write_word_data(addr, 0x00, reg_addr_bytes(reg))?;
    ops.read_byte_data(addr, 0x81)
}

fn write_reg(ops: &mut dyn SmBusSyncOps, addr: u8, reg: u16, val: u8) -> Result<()> {
    ops.write_word_data(addr, 0x00, reg_addr_bytes(reg))?;
    ops.write_byte_data(addr, 0x01, val)
}

fn write_reg_block(ops: &mut dyn SmBusSyncOps, addr: u8, reg: u16, data: &[u8]) -> Result<()> {
    // Fast path: I2C_SMBUS_BLOCK_DATA with cmd 0x03.
    // ENE auto-increments its internal register pointer across the block.
    // Falls back to byte-at-a-time (cmd 0x01) for platforms without block write support.
    const MAX_BLOCK: usize = 32;
    let mut offset = 0;
    while offset < data.len() {
        let chunk_end = (offset + MAX_BLOCK).min(data.len());
        let chunk = &data[offset..chunk_end];
        ops.write_word_data(addr, 0x00, reg_addr_bytes(reg + offset as u16))?;
        if ops.write_block_data(addr, 0x03, chunk).is_ok() {
            offset = chunk_end;
        } else {
            for (j, &byte) in chunk.iter().enumerate() {
                ops.write_word_data(addr, 0x00, reg_addr_bytes(reg + (offset + j) as u16))
                    .map_err(|e| {
                        log::warn!(
                            "[ENE] 0x{addr:02X}: write_word_data for reg 0x{:04X} failed: {e}",
                            reg + (offset + j) as u16
                        );
                        e
                    })?;
                ops.write_byte_data(addr, 0x01, byte).map_err(|e| {
                    log::warn!(
                        "[ENE] 0x{addr:02X}: write_byte_data byte {} at reg 0x{:04X} failed: {e}",
                        j,
                        reg + (offset + j) as u16
                    );
                    e
                })?;
            }
            offset = chunk_end;
        }
    }
    Ok(())
}

/// Build the BGR wire buffer for `led_count` LEDs from a slice of RGB triples.
/// Excess colours are dropped; a short slice is padded with black so the result
/// is always exactly `led_count * 3` bytes — the layout every ENE colour write
/// expects.
fn build_ene_color_buffer(colors: &[[u8; 3]], led_count: usize) -> Vec<u8> {
    let mut buf: Vec<u8> = colors
        .iter()
        .take(led_count)
        .flat_map(|&[r, g, b]| to_rbg(r, g, b))
        .collect();
    buf.resize(led_count * 3, 0);
    buf
}

// ── Detection ─────────────────────────────────────────────────────────────────

/// Returns true if an ENE controller is present at `addr`.
/// Runs entirely in one blocking task to avoid spawn_blocking overhead per ioctl.
pub async fn test_for_ene_controller(bus: &SmBusDevice, addr: u8) -> bool {
    bus.run_batch(move |ops| Ok(probe_ene_controller(ops, addr)))
        .await
        .unwrap_or(false)
}

fn probe_ene_controller(ops: &mut dyn SmBusSyncOps, addr: u8) -> bool {
    // Quick probe
    let ok = ops.read_byte(addr).is_ok() || ops.read_byte_data(addr, 0x00).is_ok();
    if !ok {
        return false;
    }

    // Verify incrementing pattern at 0xA0-0xAF
    for i in 0u8..0x10 {
        match ops.read_byte_data(addr, 0xA0 + i) {
            Ok(v) if v == i => {}
            _ => return false,
        }
    }

    // Reject Micron devices
    let mut buf = [0u8; 6];
    for (i, slot) in buf.iter_mut().enumerate() {
        match read_reg(ops, addr, ENE_REG_MICRON_CHECK + i as u16) {
            Ok(v) => *slot = v,
            Err(_) => return false,
        }
    }
    if &buf == b"Micron" {
        return false;
    }

    true
}

/// Remap DRAM sticks from broadcast 0x77 to individual candidate addresses.
/// Runs the entire loop in one blocking task.
pub async fn remap_dram_addresses(bus: &SmBusDevice, addresses: &'static [u8]) -> Result<()> {
    bus.run_batch(move |ops| sync_remap_dram_addresses(ops, addresses))
        .await
}

fn sync_remap_dram_addresses(ops: &mut dyn SmBusSyncOps, addresses: &[u8]) -> Result<()> {
    let mut addr_idx: i32 = -1;

    for slot in 0u8..8 {
        if !ops.write_quick(ENE_DRAM_BROADCAST).unwrap_or(false) {
            break;
        }

        // Find next free candidate address (one that NAKs)
        loop {
            addr_idx += 1;
            if addr_idx as usize >= addresses.len() {
                return Ok(());
            }
            let candidate = addresses[addr_idx as usize];
            if !ops.write_quick(candidate).unwrap_or(true) {
                break; // NACK = address is free
            }
        }

        if addr_idx as usize >= addresses.len() {
            break;
        }
        let target = addresses[addr_idx as usize];
        log::debug!("[ENE] Remapping slot {} → 0x{:02X}", slot, target);
        write_reg(ops, ENE_DRAM_BROADCAST, ENE_REG_SLOT_INDEX, slot)?;
        write_reg(ops, ENE_DRAM_BROADCAST, ENE_REG_I2C_ADDRESS, target << 1)?;
    }
    Ok(())
}

/// Read firmware version + config table from a confirmed ENE device.
/// All 80 register reads happen in a single blocking task.
pub async fn build_ene_device(bus: &SmBusDevice, addr: u8) -> Result<EneDeviceInfo> {
    bus.run_batch(move |ops| sync_build_ene_device(ops, addr))
        .await
}

fn sync_build_ene_device(ops: &mut dyn SmBusSyncOps, addr: u8) -> Result<EneDeviceInfo> {
    let mut version_buf = [0u8; 16];
    for i in 0u16..16 {
        version_buf[i as usize] = read_reg(ops, addr, ENE_REG_DEVICE_NAME + i)?;
    }
    let null_pos = version_buf.iter().position(|&b| b == 0).unwrap_or(16);
    let version = String::from_utf8_lossy(&version_buf[..null_pos]).to_string();

    let mut config = [0u8; 64];
    for i in 0u16..64 {
        config[i as usize] = read_reg(ops, addr, ENE_REG_CONFIG_TABLE + i)?;
    }

    let (led_count, direct_reg, effect_reg) = apply_version_layout(&version, &config);
    if led_count == 0 {
        return Err(anyhow!(
            "ENE device at 0x{:02X} reported 0 LEDs (version: {:?})",
            addr,
            version
        ));
    }
    Ok(EneDeviceInfo {
        version,
        led_count,
        direct_reg,
        effect_reg,
    })
}

/// Determine direct_reg, effect_reg, and led_count from the firmware version string.
/// Pure function for testability. Matches Python's `_apply_version_layout` exactly.
pub fn apply_version_layout(version: &str, config: &[u8]) -> (usize, u16, u16) {
    let (led_count_offset, direct_reg, effect_reg) = match version {
        "LED-0116" | "DIMM_LED-0102" | "DIMM_LED-0103" | "AUMA0-E8K4-0101" => {
            (0x02usize, ENE_REG_COLORS_DIRECT, ENE_REG_COLORS_EFFECT)
        }
        "AUDA0-E6K5-0101" => (0x02, ENE_REG_COLORS_DIRECT_V2, ENE_REG_COLORS_EFFECT_V2),
        "AUMA0-E6K5-0106" | "AUMA0-E6K5-0105" | "AUMA0-E6K5-0104" => {
            (0x02, ENE_REG_COLORS_DIRECT_V2, ENE_REG_COLORS_EFFECT_V2)
        }
        "AUMA0-E6K5-0107" | "AUMA0-E6K5-1110" | "AUMA0-E6K5-1111" | "AUMA0-E6K5-1107"
        | "AUMA0-E6K5-0008" | "AUMA0-E6K5-1113" | "AUMA0-E6K5-1114" => {
            // GPU controllers — LED count at 0x03
            (0x03, ENE_REG_COLORS_DIRECT_V2, ENE_REG_COLORS_EFFECT_V2)
        }
        _ => {
            log::debug!(
                "[ENE] Unknown firmware {:?}, assuming V1 registers",
                version
            );
            (0x02, ENE_REG_COLORS_DIRECT, ENE_REG_COLORS_EFFECT)
        }
    };

    let count_at_offset = config.get(led_count_offset).copied().unwrap_or(0) as usize;
    let count_at_03 = config.get(0x03).copied().unwrap_or(0) as usize;
    // Take the larger of both fields.
    let led_count = count_at_offset.max(count_at_03).clamp(0, 30);

    (led_count, direct_reg, effect_reg)
}

// ── Color control ─────────────────────────────────────────────────────────────
// Each function batches all its register writes into one spawn_blocking call.

pub async fn ene_set_direct_mode(bus: &SmBusDevice, addr: u8, enable: bool) -> Result<()> {
    let result = bus
        .run_batch(move |ops| {
            if enable {
                // Write Static mode before enabling direct control so the controller exits
                // EneMode::Off (mode=0) if it got stuck there after sleep/resume. GPU ENE
                // controllers power down with the GPU and may boot in Off mode on resume,
                // causing ENE_REG_DIRECT writes to be silently ignored.
                write_reg(ops, addr, ENE_REG_MODE, EneMode::Static as u8).map_err(|e| {
                    log::warn!("[ENE] 0x{addr:02X}: write ENE_REG_MODE=Static failed: {e}");
                    e
                })?;
                write_reg(ops, addr, ENE_REG_APPLY, ENE_APPLY_VAL).map_err(|e| {
                    log::warn!("[ENE] 0x{addr:02X}: write ENE_REG_APPLY (mode) failed: {e}");
                    e
                })?;
            }
            write_reg(ops, addr, ENE_REG_DIRECT, if enable { 0x01 } else { 0x00 }).map_err(
                |e| {
                    log::warn!("[ENE] 0x{addr:02X}: write ENE_REG_DIRECT failed: {e}");
                    e
                },
            )?;
            write_reg(ops, addr, ENE_REG_APPLY, ENE_APPLY_VAL).map_err(|e| {
                log::warn!("[ENE] 0x{addr:02X}: write ENE_REG_APPLY failed: {e}");
                e
            })
        })
        .await;
    if let Err(ref e) = result {
        log::warn!("[ENE] 0x{addr:02X}: set_direct_mode({enable}) failed: {e}");
    }
    result
}

pub async fn ene_set_static_color(
    bus: &SmBusDevice,
    addr: u8,
    info: &EneDeviceInfo,
    r: u8,
    g: u8,
    b: u8,
) -> Result<()> {
    let buf = build_ene_color_buffer(&vec![[r, g, b]; info.led_count], info.led_count);
    let direct_reg = info.direct_reg;
    let result = bus
        .run_batch(move |ops| {
            write_reg_block(ops, addr, direct_reg, &buf).map_err(|e| {
                log::warn!(
                    "[ENE] 0x{addr:02X}: color block write to reg 0x{direct_reg:04X} failed: {e}"
                );
                e
            })?;
            write_reg(ops, addr, ENE_REG_DIRECT, 0x01).map_err(|e| {
                log::warn!("[ENE] 0x{addr:02X}: set ENE_REG_DIRECT=1 failed: {e}");
                e
            })?;
            write_reg(ops, addr, ENE_REG_APPLY, ENE_APPLY_VAL).map_err(|e| {
                log::warn!("[ENE] 0x{addr:02X}: write ENE_REG_APPLY failed: {e}");
                e
            })
        })
        .await;
    if let Err(ref e) = result {
        log::warn!("[ENE] 0x{addr:02X}: set_static_color failed: {e}");
    }
    result
}

pub async fn ene_set_colors(
    bus: &SmBusDevice,
    addr: u8,
    info: &EneDeviceInfo,
    colors: &[[u8; 3]],
) -> Result<()> {
    let buf = build_ene_color_buffer(colors, info.led_count);
    let direct_reg = info.direct_reg;
    bus.run_batch(move |ops| {
        write_reg_block(ops, addr, direct_reg, &buf)?;
        write_reg(ops, addr, ENE_REG_DIRECT, 0x01)?;
        write_reg(ops, addr, ENE_REG_APPLY, ENE_APPLY_VAL)
    })
    .await
}

/// Animation frame write — color data only, no mode or apply registers.
/// Device must already be in direct mode (set once via ene_set_direct_mode).
/// 4 fewer SMBus transactions per frame vs ene_set_colors: keeps the NVIDIA
/// i2c lock held for the minimum time possible, preventing compositor stutter.
pub async fn ene_write_frame_colors(
    bus: &SmBusDevice,
    addr: u8,
    info: &EneDeviceInfo,
    colors: &[[u8; 3]],
) -> Result<()> {
    let buf = build_ene_color_buffer(colors, info.led_count);
    let direct_reg = info.direct_reg;
    bus.run_batch(move |ops| write_reg_block(ops, addr, direct_reg, &buf))
        .await
}

pub async fn ene_set_effect_colors(
    bus: &SmBusDevice,
    addr: u8,
    info: &EneDeviceInfo,
    colors: &[[u8; 3]],
) -> Result<()> {
    let buf = build_ene_color_buffer(colors, info.led_count);
    let effect_reg = info.effect_reg;
    bus.run_batch(move |ops| {
        write_reg_block(ops, addr, effect_reg, &buf)?;
        write_reg(ops, addr, ENE_REG_APPLY, ENE_APPLY_VAL)
    })
    .await
}

pub async fn ene_set_mode(
    bus: &SmBusDevice,
    addr: u8,
    mode: EneMode,
    speed: EneSpeed,
    direction: u8,
) -> Result<()> {
    bus.run_batch(move |ops| {
        write_reg(ops, addr, ENE_REG_MODE, mode as u8)?;
        write_reg(ops, addr, ENE_REG_SPEED, speed as u8)?;
        write_reg(ops, addr, ENE_REG_DIRECTION, direction)?;
        write_reg(ops, addr, ENE_REG_APPLY, ENE_APPLY_VAL)
    })
    .await
}

// ── Protocol struct wrapper ───────────────────────────────────────────────────

pub struct EneSmBusProtocol {
    bus: Arc<SmBusDevice>,
    addr: u8,
}

impl EneSmBusProtocol {
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
        test_for_ene_controller(&self.bus, self.addr).await
    }

    pub async fn build_device(&self) -> Result<EneDeviceInfo> {
        build_ene_device(&self.bus, self.addr).await
    }

    pub async fn set_direct_mode(&self, enable: bool) -> Result<()> {
        ene_set_direct_mode(&self.bus, self.addr, enable).await
    }

    pub async fn set_static_color(&self, info: &EneDeviceInfo, r: u8, g: u8, b: u8) -> Result<()> {
        ene_set_static_color(&self.bus, self.addr, info, r, g, b).await
    }

    pub async fn set_colors(&self, info: &EneDeviceInfo, colors: &[[u8; 3]]) -> Result<()> {
        ene_set_colors(&self.bus, self.addr, info, colors).await
    }

    pub async fn write_frame_colors(&self, info: &EneDeviceInfo, colors: &[[u8; 3]]) -> Result<()> {
        ene_write_frame_colors(&self.bus, self.addr, info, colors).await
    }

    pub async fn set_effect_colors(&self, info: &EneDeviceInfo, colors: &[[u8; 3]]) -> Result<()> {
        ene_set_effect_colors(&self.bus, self.addr, info, colors).await
    }

    pub async fn set_mode(&self, mode: EneMode, speed: EneSpeed, direction: u8) -> Result<()> {
        ene_set_mode(&self.bus, self.addr, mode, speed, direction).await
    }
}

// ── Tests ─────────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    // ── MockSmBusOps ──────────────────────────────────────────────────────────

    #[derive(Debug, PartialEq)]
    enum Op {
        WriteWordData { addr: u8, cmd: u8, val: u16 },
        WriteByteData { addr: u8, cmd: u8, val: u8 },
        WriteBlockData { addr: u8, cmd: u8, data: Vec<u8> },
    }

    struct MockSmBusOps {
        ops: Vec<Op>,
        block_write_supported: bool,
    }

    impl MockSmBusOps {
        fn new(block_write_supported: bool) -> Self {
            Self {
                ops: Vec::new(),
                block_write_supported,
            }
        }
    }

    impl SmBusSyncOps for MockSmBusOps {
        fn read_byte(&mut self, _addr: u8) -> Result<u8> {
            Ok(0)
        }
        fn read_byte_data(&mut self, _addr: u8, _cmd: u8) -> Result<u8> {
            Ok(0)
        }
        fn write_quick(&mut self, _addr: u8) -> Result<bool> {
            Ok(true)
        }
        fn write_byte_data(&mut self, addr: u8, cmd: u8, val: u8) -> Result<()> {
            self.ops.push(Op::WriteByteData { addr, cmd, val });
            Ok(())
        }
        fn write_word_data(&mut self, addr: u8, cmd: u8, val: u16) -> Result<()> {
            self.ops.push(Op::WriteWordData { addr, cmd, val });
            Ok(())
        }
        fn write_block_data(&mut self, addr: u8, cmd: u8, data: &[u8]) -> Result<()> {
            if self.block_write_supported {
                self.ops.push(Op::WriteBlockData {
                    addr,
                    cmd,
                    data: data.to_vec(),
                });
                Ok(())
            } else {
                Err(anyhow::anyhow!("not supported"))
            }
        }
    }

    #[test]
    fn test_write_reg_block_uses_block_write() {
        let mut ops = MockSmBusOps::new(true);
        let data: Vec<u8> = (0..30).collect(); // 10 LEDs × 3 bytes
        write_reg_block(&mut ops, 0x70, ENE_REG_COLORS_DIRECT, &data).unwrap();

        // Should be exactly: 1 write_word_data + 1 write_block_data (fits in 32-byte chunk)
        assert_eq!(ops.ops.len(), 2);
        assert!(matches!(ops.ops[0], Op::WriteWordData { cmd: 0x00, .. }));
        assert!(matches!(
            &ops.ops[1],
            Op::WriteBlockData { cmd: 0x03, data: d, .. } if d == &data
        ));
    }

    #[test]
    fn test_write_reg_block_chunks_large_payload() {
        let mut ops = MockSmBusOps::new(true);
        let data: Vec<u8> = (0..90u8).collect(); // 30 LEDs × 3 bytes, needs 3 chunks
        write_reg_block(&mut ops, 0x70, ENE_REG_COLORS_DIRECT, &data).unwrap();

        // 3 chunks × (1 write_word_data + 1 write_block_data) = 6 ops
        assert_eq!(ops.ops.len(), 6);
        let chunks: Vec<_> = ops.ops.chunks(2).collect();
        assert!(matches!(chunks[0][0], Op::WriteWordData { cmd: 0x00, .. }));
        assert!(
            matches!(&chunks[0][1], Op::WriteBlockData { cmd: 0x03, data: d, .. } if d.len() == 32)
        );
        assert!(matches!(chunks[1][0], Op::WriteWordData { cmd: 0x00, .. }));
        assert!(
            matches!(&chunks[1][1], Op::WriteBlockData { cmd: 0x03, data: d, .. } if d.len() == 32)
        );
        assert!(matches!(chunks[2][0], Op::WriteWordData { cmd: 0x00, .. }));
        assert!(
            matches!(&chunks[2][1], Op::WriteBlockData { cmd: 0x03, data: d, .. } if d.len() == 26)
        );
    }

    #[test]
    fn test_write_reg_block_fallback_to_byte_at_a_time() {
        let mut ops = MockSmBusOps::new(false);
        let data = vec![0xAAu8, 0xBB, 0xCC]; // 1 LED
        write_reg_block(&mut ops, 0x70, ENE_REG_COLORS_DIRECT, &data).unwrap();

        // Falls back: 1 write_word_data (fast path sets reg), then 3 × (write_word_data + write_byte_data)
        // First write_word_data is the fast-path attempt, then 3 more for each byte
        let word_data_count = ops
            .ops
            .iter()
            .filter(|o| matches!(o, Op::WriteWordData { .. }))
            .count();
        let byte_data_count = ops
            .ops
            .iter()
            .filter(|o| matches!(o, Op::WriteByteData { cmd: 0x01, .. }))
            .count();
        assert_eq!(word_data_count, 4); // 1 (fast path) + 3 (fallback per byte)
        assert_eq!(byte_data_count, 3);
    }

    fn make_config(byte02: u8, byte03: u8) -> Vec<u8> {
        let mut cfg = vec![0u8; 64];
        cfg[0x02] = byte02;
        cfg[0x03] = byte03;
        cfg
    }

    #[test]
    fn test_apply_version_layout_v1() {
        let cfg = make_config(10, 0);
        let (count, direct, effect) = apply_version_layout("LED-0116", &cfg);
        assert_eq!(count, 10);
        assert_eq!(direct, ENE_REG_COLORS_DIRECT);
        assert_eq!(effect, ENE_REG_COLORS_EFFECT);
    }

    #[test]
    fn test_apply_version_layout_dimm() {
        let cfg = make_config(0, 8);
        let (count, direct, effect) = apply_version_layout("DIMM_LED-0102", &cfg);
        assert_eq!(count, 8); // max(cfg[0x02]=0, cfg[0x03]=8) = 8
        assert_eq!(direct, ENE_REG_COLORS_DIRECT);
        assert_eq!(effect, ENE_REG_COLORS_EFFECT);
    }

    #[test]
    fn test_apply_version_layout_gpu() {
        let cfg = make_config(0, 4);
        let (count, direct, effect) = apply_version_layout("AUMA0-E6K5-0107", &cfg);
        assert_eq!(count, 4); // GPU uses cfg[0x03]
        assert_eq!(direct, ENE_REG_COLORS_DIRECT_V2);
        assert_eq!(effect, ENE_REG_COLORS_EFFECT_V2);
    }

    #[test]
    fn test_apply_version_layout_v2_mb() {
        let cfg = make_config(6, 0);
        let (count, direct, effect) = apply_version_layout("AUMA0-E6K5-0105", &cfg);
        assert_eq!(count, 6);
        assert_eq!(direct, ENE_REG_COLORS_DIRECT_V2);
        assert_eq!(effect, ENE_REG_COLORS_EFFECT_V2);
    }

    #[test]
    fn test_apply_version_layout_unknown_defaults_v1() {
        let cfg = make_config(3, 0);
        let (count, direct, effect) = apply_version_layout("UNKNOWN-FIRMWARE", &cfg);
        assert_eq!(count, 3);
        assert_eq!(direct, ENE_REG_COLORS_DIRECT);
        assert_eq!(effect, ENE_REG_COLORS_EFFECT);
    }

    #[test]
    fn test_apply_version_layout_led_count_max() {
        // Takes max of both fields
        let cfg = make_config(5, 8);
        let (count, _, _) = apply_version_layout("LED-0116", &cfg);
        assert_eq!(count, 8);
    }

    #[test]
    fn test_apply_version_layout_led_count_clamp() {
        let cfg = make_config(100, 100);
        let (count, _, _) = apply_version_layout("LED-0116", &cfg);
        assert_eq!(count, 30);
    }

    #[test]
    fn test_reg_addr_bytes() {
        // ENE_REG_MODE = 0x8021 should encode as 0x2180
        assert_eq!(reg_addr_bytes(0x8021), 0x2180);
        // ENE_REG_APPLY = 0x80A0 → 0xA080
        assert_eq!(reg_addr_bytes(0x80A0), 0xA080);
        // ENE_REG_DEVICE_NAME = 0x1000 → 0x0010
        assert_eq!(reg_addr_bytes(0x1000), 0x0010);
    }

    #[test]
    fn test_write_frame_colors_no_mode_registers() {
        // ene_write_frame_colors must emit ONLY address-set + block-data ops —
        // no writes to cmd 0x01 (ENE_REG_DIRECT / ENE_REG_APPLY).
        let mut ops = MockSmBusOps::new(true);
        let colors: Vec<[u8; 3]> = vec![[0xFF, 0x00, 0x00]; 10];
        let info = EneDeviceInfo {
            version: "LED-0116".to_string(),
            led_count: 10,
            direct_reg: ENE_REG_COLORS_DIRECT,
            effect_reg: ENE_REG_COLORS_EFFECT,
        };
        let buf = build_ene_color_buffer(&colors, info.led_count);
        write_reg_block(&mut ops, 0x70, info.direct_reg, &buf).unwrap();

        let byte_writes = ops
            .ops
            .iter()
            .filter(|o| matches!(o, Op::WriteByteData { cmd: 0x01, .. }))
            .count();
        assert_eq!(
            byte_writes, 0,
            "frame write must not touch ENE_REG_DIRECT or ENE_REG_APPLY"
        );

        let block_writes = ops
            .ops
            .iter()
            .filter(|o| matches!(o, Op::WriteBlockData { .. }))
            .count();
        let addr_writes = ops
            .ops
            .iter()
            .filter(|o| matches!(o, Op::WriteWordData { cmd: 0x00, .. }))
            .count();
        assert_eq!(
            addr_writes, block_writes,
            "every block chunk needs exactly one address set"
        );
    }

    #[test]
    fn build_ene_color_buffer_uses_bgr_wire_order() {
        // ENE uses R, B, G (green and blue swapped).
        let buf = build_ene_color_buffer(&[[0xAA, 0xBB, 0xCC], [0x11, 0x22, 0x33]], 2);
        assert_eq!(buf, vec![0xAA, 0xCC, 0xBB, 0x11, 0x33, 0x22]);
    }

    #[test]
    fn build_ene_color_buffer_pads_short_input_with_black() {
        let buf = build_ene_color_buffer(&[[0xAA, 0xBB, 0xCC]], 3);
        assert_eq!(buf, vec![0xAA, 0xCC, 0xBB, 0, 0, 0, 0, 0, 0]);
    }

    #[test]
    fn build_ene_color_buffer_truncates_excess_input() {
        let buf = build_ene_color_buffer(&[[1, 2, 3], [4, 5, 6], [7, 8, 9]], 1);
        assert_eq!(buf, vec![1, 3, 2]);
    }
}
