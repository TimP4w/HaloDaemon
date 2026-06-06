#![cfg(target_os = "windows")]

//! SuperIO motherboard fan-header support.
//!
//! Probes LPC ports 0x2E and 0x4E for a supported Nuvoton NCT677x chip. Each
//! detected chip owns its own [`LpcIoBus`] — PawnIO's LpcIO module registers
//! allowed-BAR ports per-instance, so giving every chip its own bus avoids
//! `select_slot` clearing the previous chip's BARs.
//!
//! ITE IT87xx detection lived here briefly but was removed pending a
//! correct port — the IT87xx family has nontrivial chip-specific quirks
//! (16/13-bit tach mode, bank select on newer parts, Gigabyte EC handling)
//! that need real-hardware verification before shipping. Add it back when
//! someone with that hardware can validate.

pub mod board;
pub mod fan;
pub mod nct677x;
pub mod sensor;

use std::sync::{Arc, Mutex};

use crate::drivers::vendors::generic::devices::superio::fan::SuperIoFanDevice;
use crate::drivers::vendors::generic::devices::superio::sensor::SuperIoSensorDevice;
use crate::drivers::transports::lpcio::LpcIoBus;
use crate::state::AppState;

/// LPC SuperIO probe ports — slot 0 maps to 0x2E, slot 1 to 0x4E.
pub const PROBE_PORTS: &[u16] = &[0x2E, 0x4E];

pub fn port_slot(port: u16) -> u8 {
    if port == 0x2E {
        0
    } else {
        1
    }
}

/// A SuperIO chip detected on the LPC bus. Currently only the Nuvoton
/// NCT677x family is supported; the variant wrapper keeps the door open
/// for adding more (IT87xx, Fintek) without touching the call sites.
#[derive(Debug, Clone, Copy)]
pub enum DetectedChip {
    Nct677x(nct677x::Detected),
}

impl DetectedChip {
    pub fn probe_port(self) -> u16 {
        match self {
            Self::Nct677x(d) => d.probe_port,
        }
    }

    pub fn hwm_base(self) -> u16 {
        match self {
            Self::Nct677x(d) => d.hwm_base,
        }
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Nct677x(d) => d.variant.name(),
        }
    }

    pub fn fan_count(self) -> u8 {
        match self {
            Self::Nct677x(d) => d.variant.fan_count(),
        }
    }
}

inventory::submit!(crate::discovery::TransportScanner {
    name: "SuperIO",
    platform: Some("windows"),
    scan: |app| Box::pin(async move {
        if let Err(e) = SuperIoTransport::discover(app).await {
            log::error!("SuperIO discovery failed: {}", e);
        }
    }),
});

pub struct SuperIoTransport;

impl SuperIoTransport {
    pub async fn discover(app: Arc<AppState>) -> anyhow::Result<()> {
        // Read the motherboard identity once so fan labels match the BIOS
        // silkscreen on known boards.
        let board_info = board::read_board_info();

        // Detect each port with its own PawnIO instance so chip A's
        // find_bars registration isn't clobbered by probing chip B's slot.
        let mut buses: Vec<(DetectedChip, Arc<Mutex<LpcIoBus>>)> = Vec::new();
        for &port in PROBE_PORTS {
            let bus = match LpcIoBus::open() {
                Ok(b) => b,
                Err(e) => {
                    log::debug!("[SuperIO] LpcIO.bin not available: {e}");
                    return Ok(());
                }
            };

            if let Err(e) = bus.select_slot(port_slot(port)) {
                log::debug!(
                    "[SuperIO] select_slot for port 0x{port:02X} failed: {e}"
                );
                continue;
            }

            match nct677x::detect(&bus, port) {
                Ok(Some(d)) => buses.push((DetectedChip::Nct677x(d), Arc::new(Mutex::new(bus)))),
                Ok(None) => {} // bus drops, closing the PawnIO handle
                Err(e) => log::debug!("[NCT677x] detect on port 0x{port:02X} failed: {e}"),
            }
        }

        if buses.is_empty() {
            log::debug!("[SuperIO] no recognised SuperIO chip found");
            return Ok(());
        }

        for (chip, bus) in buses {
            let sensor: Arc<dyn crate::drivers::Device> =
                Arc::new(SuperIoSensorDevice::new(chip, Arc::clone(&bus)));
            crate::usecases::registration::register_device(&app, sensor).await;

            for ch in 0..chip.fan_count() {
                let label = board::fan_label(&board_info, chip, ch);
                let fan: Arc<dyn crate::drivers::Device> =
                    Arc::new(SuperIoFanDevice::new(chip, ch, label, Arc::clone(&bus)));
                crate::usecases::registration::register_device(&app, fan).await;
            }
        }
        Ok(())
    }
}
