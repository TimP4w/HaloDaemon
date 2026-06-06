#![cfg(target_os = "windows")]

//! LPC SuperIO bus — Windows backend over PawnIO.
//!
//! Provides the four PawnIO `LpcIO.bin` IOCTLs used by SuperIO fan control:
//! `ioctl_select_slot`, `ioctl_find_bars`, `ioctl_superio_inb/outb`, and the
//! raw `ioctl_pio_inb/outb`. PawnIO plumbing (DLL loading, blob caching,
//! `pawnio_execute` dispatch) lives in [`super::pawnio`].

use anyhow::Result;

use super::pawnio::PawnioModule;

pub struct LpcIoBus {
    module: PawnioModule,
}

impl LpcIoBus {
    pub fn open() -> Result<Self> {
        Ok(Self {
            module: PawnioModule::open(&["LpcIO.bin"])?,
        })
    }

    /// Tell the PawnIO module which LPC slot to drive. Slot 0 = SuperIO at
    /// 0x2E/0x2F, slot 1 = SuperIO at 0x4E/0x4F. Must be called before any
    /// I/O against that slot — without it, port reads return 0xFF.
    pub fn select_slot(&self, slot: u8) -> Result<()> {
        self.module
            .exec(b"ioctl_select_slot\0", &[slot as u64], &mut [])?;
        Ok(())
    }

    /// Discover runtime I/O BARs for the selected slot. Must be called while
    /// the chip is in extended-function mode. Once successful, raw
    /// `read_port`/`write_port` works against the registered BAR range for
    /// the lifetime of this `LpcIoBus` instance.
    pub fn find_bars(&self) -> Result<()> {
        self.module.exec(b"ioctl_find_bars\0", &[], &mut [])?;
        Ok(())
    }

    /// Read one byte from an I/O port (raw LPC access).
    pub fn read_port(&self, port: u16) -> Result<u8> {
        let mut out = [0u64; 1];
        self.module
            .exec(b"ioctl_pio_inb\0", &[port as u64], &mut out)?;
        Ok((out[0] & 0xFF) as u8)
    }

    /// Write one byte to an I/O port.
    pub fn write_port(&self, port: u16, value: u8) -> Result<()> {
        self.module.exec(
            b"ioctl_pio_outb\0",
            &[port as u64, value as u64],
            &mut [],
        )?;
        Ok(())
    }

    /// Read a SuperIO configuration register (chip must be in extended-function
    /// mode). The PawnIO module knows the index/data port pair from the most
    /// recent `select_slot` call.
    pub fn superio_inb(&self, register: u8) -> Result<u8> {
        let mut out = [0u64; 1];
        self.module
            .exec(b"ioctl_superio_inb\0", &[register as u64], &mut out)?;
        Ok((out[0] & 0xFF) as u8)
    }

    /// Write a SuperIO configuration register.
    pub fn superio_outb(&self, register: u8, value: u8) -> Result<()> {
        self.module.exec(
            b"ioctl_superio_outb\0",
            &[register as u64, value as u64],
            &mut [],
        )?;
        Ok(())
    }
}
