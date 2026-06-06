// SPDX-License-Identifier: MPL-2.0
// SPDX-FileCopyrightText: LibreHardwareMonitor contributors
// Derived from LibreHardwareMonitor's Nct677X.cs and LpcPort.cs
// (https://github.com/LibreHardwareMonitor/LibreHardwareMonitor).

#![cfg(target_os = "windows")]

//! Nuvoton NCT677x SuperIO chip — detection and runtime register access.
//!
//! Two register spaces:
//!  * SuperIO config space (0x2E/0x4E) — entered with the magic 0x87 0x87
//!    sequence, used to read the chip ID + revision and to look up the
//!    runtime HWM base address (LDN 0x0B, CR 0x60).
//!  * Runtime HWM space — exposed at `hwm_base + 5` (index) and
//!    `hwm_base + 6` (data) after the base address is known. Bank-addressed:
//!    write 0x4E to the index port with the bank number, then write the
//!    low-byte register to the index port and read/write the data port.

use anyhow::Result;

use crate::drivers::transports::lpcio::LpcIoBus;

const CHIP_ID_REGISTER: u8 = 0x20;
const CHIP_REVISION_REGISTER: u8 = 0x21;
const DEVICE_SELECT_REGISTER: u8 = 0x07;
const BASE_ADDRESS_REGISTER: u8 = 0x60;
const WINBOND_NUVOTON_HARDWARE_MONITOR_LDN: u8 = 0x0B;
const NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK: u8 = 0x28;

/// Runtime offsets from the HWM base address.
const ADDRESS_REGISTER_OFFSET: u16 = 0x05;
const DATA_REGISTER_OFFSET: u16 = 0x06;
const BANK_SELECT_REGISTER: u8 = 0x4E;

/// Enter Winbond/Nuvoton extended function mode (0x87 0x87 to index port).
pub fn enter(bus: &LpcIoBus, port: u16) -> Result<()> {
    bus.write_port(port, 0x87)?;
    bus.write_port(port, 0x87)
}

/// Exit extended function mode (0xAA to index port).
pub fn exit(bus: &LpcIoBus, port: u16) -> Result<()> {
    bus.write_port(port, 0xAA)
}

/// Read a runtime HWM register. `register` is a 16-bit value where the high
/// byte is the bank index and the low byte is the register inside the bank.
pub fn read_hwm(bus: &LpcIoBus, hwm_base: u16, register: u16) -> Result<u8> {
    let bank = (register >> 8) as u8;
    let reg = (register & 0xFF) as u8;
    let addr = hwm_base + ADDRESS_REGISTER_OFFSET;
    let data = hwm_base + DATA_REGISTER_OFFSET;
    bus.write_port(addr, BANK_SELECT_REGISTER)?;
    bus.write_port(data, bank)?;
    bus.write_port(addr, reg)?;
    bus.read_port(data)
}

/// Write a runtime HWM register.
pub fn write_hwm(bus: &LpcIoBus, hwm_base: u16, register: u16, value: u8) -> Result<()> {
    let bank = (register >> 8) as u8;
    let reg = (register & 0xFF) as u8;
    let addr = hwm_base + ADDRESS_REGISTER_OFFSET;
    let data = hwm_base + DATA_REGISTER_OFFSET;
    bus.write_port(addr, BANK_SELECT_REGISTER)?;
    bus.write_port(data, bank)?;
    bus.write_port(addr, reg)?;
    bus.write_port(data, value)
}

/// Detected NCT677x variant.
#[derive(Debug, Clone, Copy, PartialEq)]
pub enum Nct677xVariant {
    Nct6771F,
    Nct6776F,
    Nct6779D,
    Nct6791D,
    Nct6792D,
    Nct6792DA,
    Nct6793D,
    Nct6795D,
    Nct6796D,
    Nct6796DR,
    Nct6797D,
    Nct6798D,
    Nct6799D,
    Nct6701D,
    Nct5585D,
    Nct6683D,
    Nct6686D,
    Nct6687D,
    Nct6687DR,
    Nct610Xd,
}

/// Nuvoton vendor ID returned from the HWM space when the I/O space lock is
/// cleared.
pub const NUVOTON_VENDOR_ID: u16 = 0x5CA3;

impl Nct677xVariant {
    /// Vendor ID register addresses `(high, low)` in the HWM space. Reading
    /// these via the runtime base+5/+6 path is the canonical way to check
    /// whether the I/O space lock is currently clear.
    pub fn vendor_id_regs(self) -> (u16, u16) {
        match self {
            Self::Nct610Xd => (0x80FE, 0x00FE),
            Self::Nct6776F | Self::Nct6771F => (0x80FE, 0x00FE),
            // Modern NCT67xxD family (including NCT6701D, NCT6799D, NCT6796DR).
            _ => (0x804F, 0x004F),
        }
    }

    /// Whether this variant has the runtime-HWM I/O space lock that must be
    /// cleared after detection. Applies to the NCT679xD family on modern
    /// boards — without clearing, every runtime register reads as 0.
    pub fn needs_io_space_lock_disable(self) -> bool {
        matches!(
            self,
            Self::Nct6791D
                | Self::Nct6792D
                | Self::Nct6792DA
                | Self::Nct6793D
                | Self::Nct6795D
                | Self::Nct6796D
                | Self::Nct6796DR
                | Self::Nct6797D
                | Self::Nct6798D
                | Self::Nct6799D
                | Self::Nct6701D
                | Self::Nct5585D
        )
    }

    /// Map `(id, revision)` of CR 0x20/0x21 to a variant.
    pub fn from_id(id: u8, revision: u8) -> Option<Self> {
        let rev_hi = revision & 0xF0;
        match id {
            0xB4 if rev_hi == 0x70 => Some(Self::Nct6771F),
            0xC3 if rev_hi == 0x30 => Some(Self::Nct6776F),
            0xC4 if rev_hi == 0x50 => Some(Self::Nct610Xd),
            0xC5 if rev_hi == 0x60 => Some(Self::Nct6779D),
            0xC7 if revision == 0x32 => Some(Self::Nct6683D),
            0xC8 if revision == 0x03 => Some(Self::Nct6791D),
            0xC9 => match revision {
                0x11 => Some(Self::Nct6792D),
                0x13 => Some(Self::Nct6792DA),
                _ => None,
            },
            0xD1 if revision == 0x21 => Some(Self::Nct6793D),
            0xD3 if revision == 0x52 => Some(Self::Nct6795D),
            0xD4 => match revision {
                0x23 => Some(Self::Nct6796D),
                0x2A => Some(Self::Nct6796DR),
                0x51 => Some(Self::Nct6797D),
                0x2B => Some(Self::Nct6798D),
                0x40 | 0x41 => Some(Self::Nct6686D),
                _ => None,
            },
            0xD5 if revision == 0x92 => Some(Self::Nct6687D),
            0xD8 => match revision {
                0x02 => Some(Self::Nct6799D),
                0x06 => Some(Self::Nct6701D),
                _ => None,
            },
            _ => None,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Nct6771F => "NCT6771F",
            Self::Nct6776F => "NCT6776F",
            Self::Nct6779D => "NCT6779D",
            Self::Nct6791D => "NCT6791D",
            Self::Nct6792D => "NCT6792D",
            Self::Nct6792DA => "NCT6792DA",
            Self::Nct6793D => "NCT6793D",
            Self::Nct6795D => "NCT6795D",
            Self::Nct6796D => "NCT6796D",
            Self::Nct6796DR => "NCT6796DR",
            Self::Nct6797D => "NCT6797D",
            Self::Nct6798D => "NCT6798D",
            Self::Nct6799D => "NCT6799D",
            Self::Nct6701D => "NCT6701D",
            Self::Nct5585D => "NCT5585D",
            Self::Nct6683D => "NCT6683D",
            Self::Nct6686D => "NCT6686D",
            Self::Nct6687D => "NCT6687D",
            Self::Nct6687DR => "NCT6687DR",
            Self::Nct610Xd => "NCT610XD",
        }
    }

    /// True for the EC-style NCT668x family (different register layout: no
    /// banked addressing, separate page/index/data port scheme).
    pub fn is_ec_family(self) -> bool {
        matches!(
            self,
            Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR
        )
    }

    /// Number of fan channels.
    pub fn fan_count(self) -> u8 {
        match self {
            Self::Nct6779D => 5,
            Self::Nct6776F => 5,
            Self::Nct6771F => 4,
            Self::Nct610Xd => 3,
            Self::Nct6796DR | Self::Nct6796D | Self::Nct6797D | Self::Nct6798D
            | Self::Nct6799D | Self::Nct6701D | Self::Nct5585D => 7,
            Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR => 8,
            _ => 6,
        }
    }

    /// 16-bit fan-RPM count registers (banked). High byte at reg, low byte at reg+1.
    /// For the modern NCT67xxD family these are `0x4B0`, `0x4B2`, ..., `0x4CC`.
    pub fn fan_count_regs(self) -> &'static [u16] {
        match self {
            Self::Nct6776F | Self::Nct6771F => {
                &[0x656, 0x658, 0x65A, 0x65C, 0x65E]
            }
            Self::Nct610Xd => &[0x030, 0x032, 0x034],
            // EC-family (NCT668x) uses different access; placeholder regs.
            Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR => {
                &[0x140, 0x142, 0x144, 0x146, 0x148, 0x14A, 0x14C, 0x14E]
            }
            _ => &[0x4B0, 0x4B2, 0x4B4, 0x4B6, 0x4B8, 0x4BA, 0x4CC],
        }
    }

    /// PWM output (read-back) registers — value 0-255 ↔ 0-100% duty.
    pub fn fan_pwm_out_regs(self) -> &'static [u16] {
        match self {
            // NCT6797D/6798D/6799D/NCT5585D use the high-bank regs.
            Self::Nct6797D | Self::Nct6798D | Self::Nct6799D | Self::Nct5585D => {
                &[0x001, 0x003, 0x011, 0x013, 0x015, 0xA09, 0xB09]
            }
            Self::Nct610Xd => &[0x04A, 0x04B, 0x04C],
            Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR => {
                &[0x160, 0x161, 0x162, 0x163, 0x164, 0x165, 0x166, 0x167]
            }
            // Default: NCT6791D/6792D/6793D/6795D/6796D/6796DR/6701D/etc.
            _ => &[0x001, 0x003, 0x011, 0x013, 0x015, 0x017, 0x029],
        }
    }

    /// PWM command (write) registers — written in manual control mode.
    pub fn fan_pwm_cmd_regs(self) -> &'static [u16] {
        match self {
            Self::Nct610Xd => &[0x119, 0x129, 0x139],
            Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR => {
                &[0xA28, 0xA29, 0xA2A, 0xA2B, 0xA2C, 0xA2D, 0xA2E, 0xA2F]
            }
            _ => &[0x109, 0x209, 0x309, 0x809, 0x909, 0xA09, 0xB09],
        }
    }

    /// Control-mode registers — write 0 for manual, original for restore.
    pub fn fan_ctrl_mode_regs(self) -> &'static [u16] {
        match self {
            Self::Nct610Xd => &[0x113, 0x123, 0x133],
            Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR => {
                &[0xA00, 0xA00, 0xA00, 0xA00, 0xA00, 0xA00, 0xA00, 0xA00]
            }
            _ => &[0x102, 0x202, 0x302, 0x802, 0x902, 0xA02, 0xB02],
        }
    }

    /// Temperature reading slots for the modern NCT67xxD family.
    /// Each slot is `(int_reg, half_reg, half_bit, source_reg)`:
    /// the half-bit lives in bit 7 of the half register. The source
    /// register tells you which physical sensor is currently mapped to
    /// this slot — read it to label the temperature correctly.
    pub fn temp_slots(self) -> &'static [TempSlot] {
        match self {
            Self::Nct6776F | Self::Nct6771F => &[
                TempSlot { int_reg: 0x027, half_reg: 0x000, half_bit: 0xFF, source_reg: 0x621 },
                TempSlot { int_reg: 0x073, half_reg: 0x074, half_bit: 7, source_reg: 0x100 },
                TempSlot { int_reg: 0x075, half_reg: 0x076, half_bit: 7, source_reg: 0x200 },
                TempSlot { int_reg: 0x077, half_reg: 0x078, half_bit: 7, source_reg: 0x300 },
            ],
            Self::Nct610Xd => &[
                TempSlot { int_reg: 0x06B, half_reg: 0x000, half_bit: 0xFF, source_reg: 0x621 },
                TempSlot { int_reg: 0x010, half_reg: 0x016, half_bit: 0,    source_reg: 0x000 },
                TempSlot { int_reg: 0x011, half_reg: 0x01B, half_bit: 1,    source_reg: 0x000 },
                TempSlot { int_reg: 0x012, half_reg: 0x01B, half_bit: 2,    source_reg: 0x000 },
            ],
            // EC family uses a different access scheme entirely — skip.
            Self::Nct6683D | Self::Nct6686D | Self::Nct6687D | Self::Nct6687DR => &[],
            // Modern NCT67xxD family (default, includes NCT6701D, NCT6796DR, NCT6799D).
            _ => &[
                TempSlot { int_reg: 0x073, half_reg: 0x074, half_bit: 7, source_reg: 0x100 },
                TempSlot { int_reg: 0x075, half_reg: 0x076, half_bit: 7, source_reg: 0x200 },
                TempSlot { int_reg: 0x077, half_reg: 0x078, half_bit: 7, source_reg: 0x300 },
                TempSlot { int_reg: 0x079, half_reg: 0x07A, half_bit: 7, source_reg: 0x800 },
                TempSlot { int_reg: 0x07B, half_reg: 0x07C, half_bit: 7, source_reg: 0x900 },
                TempSlot { int_reg: 0x07D, half_reg: 0x07E, half_bit: 7, source_reg: 0xA00 },
                TempSlot { int_reg: 0x4A0, half_reg: 0x49E, half_bit: 6, source_reg: 0xB00 },
            ],
        }
    }
}

/// Detected NCT677x chip with its config probe port AND its discovered
/// runtime HWM base address.
#[derive(Debug, Clone, Copy)]
pub struct Detected {
    pub probe_port: u16,
    pub variant: Nct677xVariant,
    /// HWM base address (e.g. 0x0290). 0 for EC-family chips that don't use
    /// the standard base+5/+6 scheme.
    pub hwm_base: u16,
}

/// Probe `port` for an NCT677x chip. Returns Some(detected) on success.
///
/// Caller must have already called `bus.select_slot(slot_for_port(port))`.
pub fn detect(bus: &LpcIoBus, port: u16) -> Result<Option<Detected>> {
    enter(bus, port)?;
    let id = bus.superio_inb(CHIP_ID_REGISTER)?;
    let revision = bus.superio_inb(CHIP_REVISION_REGISTER)?;
    log::debug!(
        "[NCT677x] probe port=0x{:02X} chip_id=0x{:02X} revision=0x{:02X}",
        port, id, revision
    );

    if id == 0 || id == 0xFF {
        let _ = exit(bus, port);
        return Ok(None);
    }
    let Some(variant) = Nct677xVariant::from_id(id, revision) else {
        let _ = exit(bus, port);
        log::debug!("[NCT677x] unrecognised chip on port 0x{:02X}", port);
        return Ok(None);
    };

    // Discover HWM base address. For EC-family chips we still try, but the
    // bank-select scheme is different; the HWM read path returns no data
    // when hwm_base == 0.
    bus.find_bars()?;
    bus.superio_outb(DEVICE_SELECT_REGISTER, WINBOND_NUVOTON_HARDWARE_MONITOR_LDN)?;
    let hi = bus.superio_inb(BASE_ADDRESS_REGISTER)? as u16;
    let lo = bus.superio_inb(BASE_ADDRESS_REGISTER + 1)? as u16;
    let hwm_base = (hi << 8) | lo;

    // NCT679xD chips ship with the HWM I/O space locked. Clear bit 4 of
    // CR 0x28 here while we're still in extended-function mode — otherwise
    // every runtime register at base+5/+6 reads as 0.
    if variant.needs_io_space_lock_disable() {
        match bus.superio_inb(NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK) {
            Ok(options) if options & 0x10 != 0 => {
                let cleared = options & !0x10;
                if let Err(e) =
                    bus.superio_outb(NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK, cleared)
                {
                    log::warn!("[NCT677x] could not clear I/O space lock: {e}");
                } else {
                    log::info!(
                        "[NCT677x] cleared I/O space lock (CR 0x28: 0x{:02X} → 0x{:02X})",
                        options, cleared
                    );
                }
            }
            Ok(_) => log::debug!("[NCT677x] I/O space lock already clear"),
            Err(e) => log::warn!("[NCT677x] read CR 0x28 failed: {e}"),
        }
    }

    exit(bus, port)?;

    log::info!(
        "[NCT677x] detected {} on port 0x{:02X} (id=0x{:02X}, rev=0x{:02X}), hwm_base=0x{:04X}",
        variant.name(), port, id, revision, hwm_base
    );

    Ok(Some(Detected {
        probe_port: port,
        variant,
        hwm_base,
    }))
}

/// One temperature register slot. The chip dynamically maps physical
/// sensors (PECI agent, CPUTIN, SYSTIN, …) into these slots — you must
/// read the source register to learn what's actually being measured.
#[derive(Debug, Clone, Copy)]
pub struct TempSlot {
    pub int_reg: u16,
    pub half_reg: u16,
    /// Bit index of the half-degree value inside `half_reg`, or 0xFF
    /// for slots with no half-degree precision.
    pub half_bit: u8,
    /// Register that returns the `SourceNct67Xxd` enum byte for this slot.
    /// 0 means "the slot is fixed; use `int_reg`'s source unconditionally".
    pub source_reg: u16,
}

/// One decoded temperature reading.
#[derive(Debug, Clone, Copy)]
pub struct TempReading {
    pub source: u8,
    pub label: &'static str,
    pub temperature_c: f32,
}

/// Read every temperature slot on a modern NCT67xxD chip, dedupe by the
/// source byte the chip reports for each slot, and return the unique
/// readings labelled by their physical sensor.
pub fn read_all_temperatures(bus: &LpcIoBus, chip: Detected) -> Vec<TempReading> {
    if chip.hwm_base == 0 {
        return vec![];
    }
    let mut seen_sources: u64 = 0;
    let mut out = Vec::new();

    for slot in chip.variant.temp_slots() {
        // What physical sensor is mapped to this slot? Mask to 5 bits —
        // upper bits are reserved.
        let source = if slot.source_reg != 0 {
            match read_hwm(bus, chip.hwm_base, slot.source_reg) {
                Ok(b) => b & 0x1F,
                Err(_) => continue,
            }
        } else {
            // Fixed-source slot — skip it; we can't label without a source.
            continue;
        };

        // Source 0 means "nothing mapped here" — skip.
        if source == 0 {
            continue;
        }

        // Dedupe: skip if we've already collected a reading from this source.
        let source_bit = 1u64 << source;
        if seen_sources & source_bit != 0 {
            continue;
        }

        // Read integer °C as a signed byte, then shift left by 1 to combine
        // with the half-degree bit (gives temperature in half-degrees).
        let int_byte = match read_hwm(bus, chip.hwm_base, slot.int_reg) {
            Ok(b) => b,
            Err(_) => continue,
        };
        let mut value: i32 = (int_byte as i8 as i32) << 1;
        if slot.half_bit != 0xFF {
            let half_byte = read_hwm(bus, chip.hwm_base, slot.half_reg).unwrap_or(0);
            value |= ((half_byte >> slot.half_bit) & 0x01) as i32;
        }
        let temp = (value as f32) * 0.5;
        if !(-55.0..=125.0).contains(&temp) {
            continue;
        }

        seen_sources |= source_bit;
        out.push(TempReading {
            source,
            label: source_label(source),
            temperature_c: temp,
        });
    }
    out
}

/// Map a `SourceNct67Xxd` enum byte to a friendly label.
pub fn source_label(source: u8) -> &'static str {
    match source {
        1 => "Motherboard",          // SYSTIN
        2 => "CPU (CPUTIN)",         // CPUTIN
        3 => "Auxiliary 0",          // AUXTIN0
        4 => "Auxiliary 1",          // AUXTIN1
        5 => "Auxiliary 2",          // AUXTIN2
        6 => "Auxiliary 3",          // AUXTIN3
        7 => "Auxiliary 4",          // AUXTIN4
        8 => "SMBus Master 0",
        9 => "SMBus Master 1",
        10 => "T-Sensor",
        16 => "CPU Package (PECI Agent 0)",
        17 => "CPU (PECI Agent 1)",
        18 => "PCH Chip CPU Max",
        19 => "PCH Chip",
        20 => "PCH CPU",
        21 => "PCH MCH",
        22 => "Agent 0 DIMM 0",
        23 => "Agent 0 DIMM 1",
        24 => "Agent 1 DIMM 0",
        25 => "Agent 1 DIMM 1",
        26 => "Byte Temp 0",
        27 => "Byte Temp 1",
        28 => "PECI Agent 0 Calibrated",
        29 => "PECI Agent 1 Calibrated",
        31 => "Virtual",
        _ => "Unknown",
    }
}

/// Convert NCT677x fan-count register bytes to an RPM value, using the
/// 13-bit counter scheme `count = (high << 5) | (low & 0x1F)` that applies
/// to the modern NCT67xxD family and the NCT610XD.
pub fn rpm_from_count_bytes(high: u8, low: u8) -> u32 {
    const MAX_COUNT: u32 = 0x1FFF;
    const MIN_COUNT: u32 = 0x15;
    let count = ((high as u32) << 5) | ((low as u32) & 0x1F);
    if count >= MAX_COUNT || count < MIN_COUNT {
        0
    } else {
        1_350_000 / count
    }
}

/// Try to read the chip's vendor ID via the runtime HWM path. Returns
/// `false` on any I/O error.
pub fn is_io_unlocked(bus: &LpcIoBus, chip: Detected) -> bool {
    if chip.hwm_base == 0 {
        return false;
    }
    let (hi_reg, lo_reg) = chip.variant.vendor_id_regs();
    let Ok(hi) = read_hwm(bus, chip.hwm_base, hi_reg) else {
        return false;
    };
    let Ok(lo) = read_hwm(bus, chip.hwm_base, lo_reg) else {
        return false;
    };
    let id = ((hi as u16) << 8) | (lo as u16);
    id == NUVOTON_VENDOR_ID
}

/// Re-clear the HWM I/O space lock. If the runtime path can't read a
/// Nuvoton vendor ID, re-enter SuperIO extended-function mode and clear
/// CR 0x28 bit 4 to re-open the HWM space. Must run at the start of every
/// poll cycle — modern NCT679xD chips can re-engage the lock between
/// accesses under BIOS power management.
pub fn keep_io_unlocked(bus: &LpcIoBus, chip: Detected) -> Result<()> {
    if !chip.variant.needs_io_space_lock_disable() {
        return Ok(());
    }
    if is_io_unlocked(bus, chip) {
        return Ok(());
    }
    log::debug!(
        "[NCT677x] HWM I/O space appears locked on port 0x{:02X}, re-clearing",
        chip.probe_port
    );
    enter(bus, chip.probe_port)?;
    let options = bus.superio_inb(NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK)?;
    if options & 0x10 != 0 {
        bus.superio_outb(
            NUVOTON_HARDWARE_MONITOR_IO_SPACE_LOCK,
            options & !0x10,
        )?;
    }
    exit(bus, chip.probe_port)?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn nct6796d_recognized() {
        // CR[0x20]=0xD4, CR[0x21]=0x23 → NCT6796D.
        assert_eq!(
            Nct677xVariant::from_id(0xD4, 0x23),
            Some(Nct677xVariant::Nct6796D)
        );
    }

    #[test]
    fn nct6796dr_recognized() {
        assert_eq!(
            Nct677xVariant::from_id(0xD4, 0x2A),
            Some(Nct677xVariant::Nct6796DR)
        );
    }

    #[test]
    fn nct6799d_recognized() {
        assert_eq!(
            Nct677xVariant::from_id(0xD8, 0x02),
            Some(Nct677xVariant::Nct6799D)
        );
    }

    #[test]
    fn nct6701d_recognized() {
        assert_eq!(
            Nct677xVariant::from_id(0xD8, 0x06),
            Some(Nct677xVariant::Nct6701D)
        );
    }

    #[test]
    fn nct6776f_recognized() {
        // 0xC3, revision high nibble 0x30
        assert_eq!(
            Nct677xVariant::from_id(0xC3, 0x33),
            Some(Nct677xVariant::Nct6776F)
        );
    }

    #[test]
    fn unknown_id_returns_none() {
        assert_eq!(Nct677xVariant::from_id(0xAA, 0x00), None);
    }

    #[test]
    fn unknown_revision_returns_none() {
        // 0xD4 with unmatched revision
        assert_eq!(Nct677xVariant::from_id(0xD4, 0x99), None);
    }

    #[test]
    fn rpm_from_count_bytes_basic() {
        // count = 900 ↔ rpm = 1500. Encoded as high=28 (>>5), low=4 (count & 0x1F).
        assert_eq!(rpm_from_count_bytes(28, 4), 1500);
    }

    #[test]
    fn rpm_from_count_bytes_zero() {
        // count below MIN (0x15) → 0.
        assert_eq!(rpm_from_count_bytes(0, 0), 0);
    }

    #[test]
    fn rpm_from_count_bytes_max_or_above() {
        // count >= MAX (0x1FFF) → 0 (no fan / stalled).
        assert_eq!(rpm_from_count_bytes(0xFF, 0xFF), 0);
    }

    #[test]
    fn fan_register_arrays_cover_fan_count() {
        for v in [
            Nct677xVariant::Nct6796D,
            Nct677xVariant::Nct6796DR,
            Nct677xVariant::Nct6799D,
            Nct677xVariant::Nct6701D,
        ] {
            assert!(v.fan_count_regs().len() as u8 >= v.fan_count().min(7));
        }
    }
}

