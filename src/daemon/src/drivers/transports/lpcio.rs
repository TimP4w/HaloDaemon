// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]

//! LPC SuperIO bus — Windows backend over PawnIO.
//!
//! Provides the four PawnIO `LpcIO.bin` IOCTLs used by SuperIO fan control:
//! `ioctl_select_slot`, `ioctl_find_bars`, `ioctl_superio_inb/outb`, and the
//! raw `ioctl_pio_inb/outb`. The PawnIO module is obtained through
//! [`crate::drivers::transports::register_ops`], so in-process it is a direct
//! `halod-hwaccess` handle and under the service it is a broker handle — either
//! way its `select_slot`/`find_bars` state stays isolated to this `LpcIoBus`.

use anyhow::Result;

use crate::drivers::transports::register_ops;
use crate::drivers::Metered;
use halod_hwaccess::pawnio::PawnioOps;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

pub struct LpcIoBus {
    io: Metered<Box<dyn PawnioOps>>,
}

impl LpcIoBus {
    pub fn open(limit: Option<WriteRateLimit>) -> Result<Self> {
        Ok(Self {
            io: Metered::new(register_ops::open_pawnio("LpcIO.bin")?, limit),
        })
    }

    /// Tell the PawnIO module which LPC slot to drive. Slot 0 = SuperIO at
    /// 0x2E/0x2F, slot 1 = SuperIO at 0x4E/0x4F. Must be called before any
    /// I/O against that slot — without it, port reads return 0xFF.
    pub fn select_slot(&self, slot: u8) -> Result<()> {
        self.io
            .read_access()
            .execute("ioctl_select_slot", &[slot as u64])?;
        Ok(())
    }

    /// Discover runtime I/O BARs for the selected slot. Must be called while
    /// the chip is in extended-function mode. Once successful, raw
    /// `read_port`/`write_port` works against the registered BAR range for
    /// the lifetime of this `LpcIoBus` instance.
    pub fn find_bars(&self) -> Result<()> {
        self.io.read_access().execute("ioctl_find_bars", &[])?;
        Ok(())
    }

    /// Read one byte from an I/O port (raw LPC access).
    pub fn read_port(&self, port: u16) -> Result<u8> {
        let out = self
            .io
            .read_access()
            .execute("ioctl_pio_inb", &[port as u64])?;
        Ok((out.first().copied().unwrap_or(0) & 0xFF) as u8)
    }

    /// Write one byte to an I/O port. Gated by the write-rate limit — only
    /// call from a thread that's allowed to block (see
    /// `Metered::write_access_blocking`).
    pub fn write_port(&self, port: u16, value: u8) -> Result<()> {
        self.io
            .write_access_blocking(1)?
            .execute("ioctl_pio_outb", &[port as u64, value as u64])?;
        Ok(())
    }

    /// Read a SuperIO configuration register (chip must be in extended-function
    /// mode). The PawnIO module knows the index/data port pair from the most
    /// recent `select_slot` call.
    pub fn superio_inb(&self, register: u8) -> Result<u8> {
        let out = self
            .io
            .read_access()
            .execute("ioctl_superio_inb", &[register as u64])?;
        Ok((out.first().copied().unwrap_or(0) & 0xFF) as u8)
    }

    /// Write a SuperIO configuration register. Gated by the write-rate limit —
    /// only call from a thread that's allowed to block (see
    /// `Metered::write_access_blocking`).
    pub fn superio_outb(&self, register: u8, value: u8) -> Result<()> {
        self.io
            .write_access_blocking(1)?
            .execute("ioctl_superio_outb", &[register as u64, value as u64])?;
        Ok(())
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }
}
