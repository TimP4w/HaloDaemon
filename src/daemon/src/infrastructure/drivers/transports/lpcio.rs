// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]

//! LPC SuperIO bus — Windows backend over PawnIO.
//!
//! The daemon exposes only typed LPC operations. The elevated broker maps them
//! to the fixed `LpcIO.bin` PawnIO functions and keeps each module handle's
//! `select_slot`/`find_bars` state isolated.

use crate::infrastructure::drivers::transports::register_ops;
use crate::infrastructure::drivers::Metered;
use anyhow::{anyhow, bail, Result};
use halod_hwaccess::named_mutex::{self, NamedMutex};
use halod_shared::types::{WriteRateLimit, WriteRateStatus};
use parking_lot::Mutex;
use std::sync::OnceLock;

const ISA_BUS_TIMEOUT_MS: u32 = 2000;

/// Serializes Super-I/O config sequences and HWM index/data transactions with
/// other monitoring software and with this process's other LPC handles.
pub struct IsaBusGuard {
    _local: parking_lot::MutexGuard<'static, ()>,
    _named: Option<named_mutex::MutexGuard<'static>>,
}

pub fn lock_isa_bus() -> Result<IsaBusGuard> {
    static LOCAL: Mutex<()> = Mutex::new(());
    static NAMED: OnceLock<Option<NamedMutex>> = OnceLock::new();
    let local = LOCAL.lock();
    let named = NAMED.get_or_init(|| match NamedMutex::open(named_mutex::ISA_BUS) {
        Ok(mutex) => Some(mutex),
        Err(error) => {
            log::warn!(
                "ISA bus mutex unavailable, Super-I/O access is only serialized within this process: {error:#}"
            );
            None
        }
    });
    let named = named
        .as_ref()
        .map(|mutex| mutex.lock(ISA_BUS_TIMEOUT_MS))
        .transpose()?;
    Ok(IsaBusGuard {
        _local: local,
        _named: named,
    })
}

pub struct LpcIoBus {
    io: Metered<register_ops::LpcIoBrokerClient>,
}

impl LpcIoBus {
    pub fn open(limit: Option<WriteRateLimit>) -> Result<Self> {
        Ok(Self {
            io: Metered::new(register_ops::open_lpc_io()?, limit),
        })
    }

    /// Tell the PawnIO module which LPC slot to drive. Slot 0 = SuperIO at
    /// 0x2E/0x2F, slot 1 = SuperIO at 0x4E/0x4F. Must be called before any
    /// I/O against that slot — without it, port reads return 0xFF.
    pub fn select_slot(&self, slot: u8) -> Result<()> {
        self.io.read_access().select_slot(slot)
    }

    /// Discover runtime I/O BARs for the selected slot. Must be called while
    /// the chip is in extended-function mode. Once successful, raw
    /// `read_port`/`write_port` works against the registered BAR range for
    /// the lifetime of this `LpcIoBus` instance.
    pub fn find_bars(&self) -> Result<()> {
        self.io.read_access().find_bars()
    }

    /// Read one byte from an I/O port (raw LPC access).
    pub fn read_port(&self, port: u16) -> Result<u8> {
        self.io.read_access().read_port(port)
    }

    /// Write one byte to an I/O port. Gated by the write-rate limit — only
    /// call from a thread that's allowed to block (see
    /// `Metered::write_access`).
    pub fn write_port(&self, port: u16, value: u8) -> Result<()> {
        self.io.write_access(1)?.write_port(port, value)
    }

    /// Read a SuperIO configuration register (chip must be in extended-function
    /// mode). The PawnIO module knows the index/data port pair from the most
    /// recent `select_slot` call.
    pub fn superio_inb(&self, register: u8) -> Result<u8> {
        self.io.read_access().superio_inb(register)
    }

    /// Write a SuperIO configuration register. Gated by the write-rate limit —
    /// only call from a thread that's allowed to block (see
    /// `Metered::write_access`).
    pub fn superio_outb(&self, register: u8, value: u8) -> Result<()> {
        self.io.write_access(1)?.superio_outb(register, value)
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }
}

struct RestoreRegister {
    base: u16,
    register: u16,
    value: u8,
}

struct LpcIoState {
    bus: LpcIoBus,
    originals: Vec<RestoreRegister>,
    hwm_base: Option<u16>,
}

fn check_window(prepared: Option<u16>, base: u16) -> Result<()> {
    match prepared {
        Some(window) if window == base => Ok(()),
        Some(window) => bail!("HWM base {base:#06x} is not the prepared window {window:#06x}"),
        None => bail!("no HWM window prepared; call lpcio_prepare_hwm first"),
    }
}

/// Plugin-facing LPCIO service. Every HWM register changed through Lua is
/// snapshotted on first write and can be restored by the host even when the Lua
/// worker is wedged and its `close()` callback cannot run.
pub struct LpcIoTransport {
    state: Mutex<LpcIoState>,
}

impl LpcIoTransport {
    pub fn open(limit: Option<WriteRateLimit>) -> Result<Self> {
        Ok(Self {
            state: Mutex::new(LpcIoState {
                bus: LpcIoBus::open(limit)?,
                originals: Vec::new(),
                hwm_base: None,
            }),
        })
    }

    pub fn select_slot(&self, slot: u8) -> Result<()> {
        self.state.lock().bus.select_slot(slot)
    }

    pub fn find_bars(&self) -> Result<()> {
        self.state.lock().bus.find_bars()
    }

    /// Runs the Super-I/O config sequence under the ISA bus mutex and verifies
    /// that `find_bars` registered the HWM window; the PawnIO module silently
    /// drops a BAR whose two probe passes disagree.
    pub fn prepare_hwm(&self, slot: u8, unlock: bool) -> Result<()> {
        let mut state = self.state.lock();
        let state = &mut *state;
        let _bus_lock = lock_isa_bus()?;
        state.hwm_base = None;
        let bus = &state.bus;
        bus.select_slot(slot)?;
        let config_port = if slot == 0 { 0x2e } else { 0x4e };
        bus.write_port(config_port, 0x87)?;
        bus.write_port(config_port, 0x87)?;
        let result: Result<u16> = (|| {
            bus.find_bars()?;
            bus.superio_outb(0x07, 0x0b)?;
            let base = u16::from_be_bytes([bus.superio_inb(0x60)?, bus.superio_inb(0x61)?]);
            if unlock {
                let options = bus.superio_inb(0x28)?;
                if options & 0x10 != 0 {
                    bus.superio_outb(0x28, options & !0x10)?;
                }
            }
            Ok(base)
        })();
        let exit = bus.write_port(config_port, 0xaa);
        let base = result?;
        exit?;
        if base == 0 || base.checked_add(6).is_none() {
            bail!("Super-I/O slot {slot} reports no usable HWM window ({base:#06x})");
        }
        bus.read_port(base + 5).map_err(|error| {
            anyhow!(
                "HWM window {base:#06x} was not registered by find_bars (concurrent Super-I/O access?): {error:#}"
            )
        })?;
        state.hwm_base = Some(base);
        Ok(())
    }

    pub fn read_port(&self, port: u16) -> Result<u8> {
        self.state.lock().bus.read_port(port)
    }

    pub fn write_port(&self, port: u16, value: u8) -> Result<()> {
        self.state.lock().bus.write_port(port, value)
    }

    pub fn superio_inb(&self, register: u8) -> Result<u8> {
        self.state.lock().bus.superio_inb(register)
    }

    pub fn superio_outb(&self, register: u8, value: u8) -> Result<()> {
        self.state.lock().bus.superio_outb(register, value)
    }

    fn hwm_read_raw(bus: &LpcIoBus, base: u16, register: u16) -> Result<u8> {
        let addr = base + 5;
        let data = base + 6;
        bus.write_port(addr, 0x4e)?;
        bus.write_port(data, (register >> 8) as u8)?;
        bus.write_port(addr, register as u8)?;
        bus.read_port(data)
    }

    fn hwm_write_raw(bus: &LpcIoBus, base: u16, register: u16, value: u8) -> Result<()> {
        let addr = base + 5;
        let data = base + 6;
        bus.write_port(addr, 0x4e)?;
        bus.write_port(data, (register >> 8) as u8)?;
        bus.write_port(addr, register as u8)?;
        bus.write_port(data, value)
    }

    pub fn hwm_read(&self, base: u16, register: u16) -> Result<u8> {
        let state = self.state.lock();
        check_window(state.hwm_base, base)?;
        let _bus_lock = lock_isa_bus()?;
        Self::hwm_read_raw(&state.bus, base, register)
    }

    pub fn hwm_write(&self, base: u16, register: u16, value: u8) -> Result<()> {
        let mut state = self.state.lock();
        check_window(state.hwm_base, base)?;
        let _bus_lock = lock_isa_bus()?;
        if !state
            .originals
            .iter()
            .any(|entry| entry.base == base && entry.register == register)
        {
            let original = Self::hwm_read_raw(&state.bus, base, register)?;
            state.originals.push(RestoreRegister {
                base,
                register,
                value: original,
            });
        }
        Self::hwm_write_raw(&state.bus, base, register, value)
    }

    pub fn restore(&self) -> Result<()> {
        let mut state = self.state.lock();
        let _bus_lock = lock_isa_bus()?;
        let originals = std::mem::take(&mut state.originals);
        let mut first_error = None;
        for entry in originals.into_iter().rev() {
            if let Err(error) =
                Self::hwm_write_raw(&state.bus, entry.base, entry.register, entry.value)
            {
                log::error!(
                    "restoring LPCIO HWM register {:#06x}: {error:#}",
                    entry.register
                );
                if first_error.is_none() {
                    first_error = Some(error);
                }
            }
        }
        match first_error {
            Some(error) => Err(error),
            None => Ok(()),
        }
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.state.lock().bus.rate_status()
    }
}

impl Drop for LpcIoTransport {
    fn drop(&mut self) {
        if let Err(error) = self.restore() {
            log::error!("restoring LPCIO state during transport drop: {error:#}");
        }
    }
}

#[cfg(test)]
mod tests {
    use super::check_window;

    #[test]
    fn hwm_access_requires_the_prepared_window() {
        assert!(check_window(Some(0x290), 0x290).is_ok());
        let unprepared = check_window(None, 0x290).unwrap_err().to_string();
        assert!(unprepared.contains("lpcio_prepare_hwm"), "{unprepared}");
        let mismatch = check_window(Some(0x290), 0xa30).unwrap_err().to_string();
        assert!(
            mismatch.contains("0x0a30") && mismatch.contains("0x0290"),
            "{mismatch}"
        );
    }
}
