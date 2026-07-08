// SPDX-License-Identifier: GPL-2.0-or-later
// SPDX-FileCopyrightText: Adam Honse (CalcProgrammer1) — OpenRGB project

use anyhow::Result;
use std::{sync::Arc, thread::sleep, time::Duration};

use crate::drivers::transports::smbus::{SmBusDevice, SmBusSyncOps};
use halod_shared::types::RgbColor;

pub const ZOTAC_ADDR: u8 = 0x4B;

const REG_BASE: u8 = 0x20;
const REG_DETECT: u8 = 0x10;
const REG_COMMIT: u8 = 0x17;
const COMMIT_VAL: u8 = 0x01;
const FIXED_VAL: u8 = 0x00;

const WRITE_DELAY: Duration = Duration::from_micros(3000);
const COMMIT_DELAY: Duration = Duration::from_micros(10000);

/// Effect mode ids (register `0x22`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Mode {
    Static = 0x01,
    Breathe = 0x02,
    Fade = 0x03,
    Wink = 0x04,
    Glide = 0x08,
    Prism = 0x09,
    Bokeh = 0x0A,
    Beacon = 0x0B,
    Tandem = 0x18,
    Tidal = 0x19,
    Astra = 0x20,
    Cosmic = 0x21,
    Volta = 0x22,
}

/// Animation direction (register `0x2B`).
#[repr(u8)]
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Direction {
    Left = 0x00,
    Right = 0x01,
}

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ZoneFrame {
    pub zone: u8,
    pub mode: Mode,
    pub color1: RgbColor,
    pub color2: RgbColor,
    pub brightness: u8,
    pub speed: u8,
    pub direction: Direction,
}

impl ZoneFrame {
    /// Encode into the 16 staging registers `0x20`..=`0x2F` (index `i` = register `0x20 + i`).
    pub fn to_regs(self) -> [u8; 16] {
        let mut r = [0u8; 16];
        r[0] = FIXED_VAL; // 0x20 fixed header
        r[1] = self.zone; // 0x21 zone index
        r[2] = self.mode as u8; // 0x22 mode
        r[3] = self.color1.r; // 0x23
        r[4] = self.color1.g; // 0x24
        r[5] = self.color1.b; // 0x25
        r[6] = self.color2.r; // 0x26
        r[7] = self.color2.g; // 0x27
        r[8] = self.color2.b; // 0x28
        r[9] = self.brightness; // 0x29
        r[10] = self.speed; // 0x2A
        r[11] = self.direction as u8; // 0x2B
                                      // 0x2C..=0x2F reserved, left zero.
        r
    }
}

fn stage_and_commit(ops: &mut dyn SmBusSyncOps, addr: u8, regs: &[u8; 16]) -> Result<()> {
    for (i, &val) in regs.iter().enumerate() {
        ops.write_byte_data(addr, REG_BASE + i as u8, val)?;
        sleep(WRITE_DELAY);
    }
    ops.write_byte_data(addr, REG_COMMIT, COMMIT_VAL)?;
    sleep(COMMIT_DELAY);
    Ok(())
}

pub struct ZotacBlackwellProtocol {
    pub(crate) bus: Arc<SmBusDevice>,
    addr: u8,
}

impl ZotacBlackwellProtocol {
    pub fn new(bus: Arc<SmBusDevice>, addr: u8) -> Self {
        Self { bus, addr }
    }

    pub fn bus_number(&self) -> u8 {
        self.bus.bus_number
    }

    pub fn addr(&self) -> u8 {
        self.addr
    }

    /// Returns true if a controller ACKs a read of the detection register.
    pub async fn detect(&self) -> bool {
        let addr = self.addr;
        self.bus
            .run_batch(move |ops| Ok(ops.read_byte_data(addr, REG_DETECT).is_ok()))
            .await
            .unwrap_or(false)
    }

    pub async fn apply_zones(&self, frames: &[ZoneFrame]) -> Result<()> {
        let addr = self.addr;
        let blocks: Vec<[u8; 16]> = frames.iter().map(|f| f.to_regs()).collect();
        self.bus
            .run_batch(move |ops| {
                for regs in &blocks {
                    stage_and_commit(ops, addr, regs)?;
                }
                Ok(())
            })
            .await
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use proptest::prelude::*;

    fn color(r: u8, g: u8, b: u8) -> RgbColor {
        RgbColor { r, g, b }
    }

    #[test]
    fn regs_layout_matches_protocol() {
        let f = ZoneFrame {
            zone: 0x02,
            mode: Mode::Breathe,
            color1: color(0x11, 0x22, 0x33),
            color2: color(0x44, 0x55, 0x66),
            brightness: 100,
            speed: 50,
            direction: Direction::Right,
        };
        let r = f.to_regs();
        assert_eq!(r[0], 0x00, "0x20 fixed header");
        assert_eq!(r[1], 0x02, "0x21 zone");
        assert_eq!(r[2], 0x02, "0x22 mode = Breathe");
        assert_eq!(&r[3..6], &[0x11, 0x22, 0x33], "0x23-0x25 color1");
        assert_eq!(&r[6..9], &[0x44, 0x55, 0x66], "0x26-0x28 color2");
        assert_eq!(r[9], 100, "0x29 brightness");
        assert_eq!(r[10], 50, "0x2A speed");
        assert_eq!(r[11], 0x01, "0x2B direction = right");
        assert_eq!(&r[12..16], &[0, 0, 0, 0], "0x2C-0x2F reserved");
    }

    #[test]
    fn mode_values_match_firestorm() {
        assert_eq!(Mode::Static as u8, 0x01);
        assert_eq!(Mode::Volta as u8, 0x22);
        assert_eq!(Mode::Prism as u8, 0x09);
        assert_eq!(Mode::Tandem as u8, 0x18);
    }

    proptest! {
        /// Every field round-trips through the register block at its fixed offset.
        #[test]
        fn frame_roundtrips_through_regs(
            zone in 0u8..=2,
            c1 in any::<(u8, u8, u8)>(),
            c2 in any::<(u8, u8, u8)>(),
            brightness in 0u8..=100,
            speed in 0u8..=100,
            right in any::<bool>(),
        ) {
            let direction = if right { Direction::Right } else { Direction::Left };
            let f = ZoneFrame {
                zone,
                mode: Mode::Static,
                color1: color(c1.0, c1.1, c1.2),
                color2: color(c2.0, c2.1, c2.2),
                brightness,
                speed,
                direction,
            };
            let r = f.to_regs();
            prop_assert_eq!(r[1], zone);
            prop_assert_eq!(r[2], Mode::Static as u8);
            prop_assert_eq!((r[3], r[4], r[5]), c1);
            prop_assert_eq!((r[6], r[7], r[8]), c2);
            prop_assert_eq!(r[9], brightness);
            prop_assert_eq!(r[10], speed);
            prop_assert_eq!(r[11], direction as u8);
        }
    }
}
