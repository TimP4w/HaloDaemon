// SPDX-License-Identifier: GPL-3.0-or-later
#![cfg(target_os = "windows")]

//! SuperIO motherboard fan-header support.
//!
//! Probes LPC ports 0x2E and 0x4E for a supported Nuvoton NCT677x chip. Each
//! detected chip owns its own [`LpcIoBus`] — PawnIO's LpcIO module registers
//! allowed-BAR ports per-instance, so giving every chip its own bus avoids
//! `select_slot` clearing the previous chip's BARs.

pub mod board;
pub mod fan;
pub mod nct677x;
pub mod sensor;

use std::sync::{Arc, Mutex};

use crate::drivers::transports::lpcio::LpcIoBus;
use crate::drivers::vendors::generic::devices::superio::fan::SuperIoFanDevice;
use crate::drivers::vendors::generic::devices::superio::sensor::SuperIoSensorDevice;
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

    /// Re-open the HWM space if the chip re-engaged its I/O lock.
    pub fn keep_io_unlocked(self, bus: &LpcIoBus) -> anyhow::Result<()> {
        match self {
            Self::Nct677x(d) => nct677x::keep_io_unlocked(bus, d),
        }
    }

    /// Current RPM for `channel`, or 0 if unavailable.
    pub fn read_rpm(self, bus: &LpcIoBus, channel: u8) -> u32 {
        match self {
            Self::Nct677x(d) => nct677x::read_rpm(bus, d, channel),
        }
    }

    /// Current PWM duty (0-100%) read back for `channel`.
    pub fn read_duty(self, bus: &LpcIoBus, channel: u8) -> u8 {
        match self {
            Self::Nct677x(d) => nct677x::read_duty(bus, d, channel),
        }
    }

    /// Read the saved fan-control mode byte for `channel`, for later restore.
    pub fn read_ctrl_mode(self, bus: &LpcIoBus, channel: u8) -> Option<u8> {
        match self {
            Self::Nct677x(d) => nct677x::read_ctrl_mode(bus, d, channel),
        }
    }

    /// Restore a previously saved fan-control mode byte on `channel`.
    pub fn restore_ctrl_mode(self, bus: &LpcIoBus, channel: u8, mode: u8) {
        match self {
            Self::Nct677x(d) => nct677x::restore_ctrl_mode(bus, d, channel, mode),
        }
    }

    /// Switch `channel` to manual control and write `duty` (0-100%).
    pub fn set_duty(self, bus: &LpcIoBus, channel: u8, duty: u8) -> anyhow::Result<()> {
        match self {
            Self::Nct677x(d) => nct677x::set_duty(bus, d, channel, duty),
        }
    }

    /// Read all temperature sensors and return their readings.
    pub fn read_temperatures(self, bus: &LpcIoBus) -> Vec<nct677x::TempReading> {
        match self {
            Self::Nct677x(d) => nct677x::read_all_temperatures(bus, d),
        }
    }
}

inventory::submit!(crate::registry::discovery::TransportScanner {
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
        // WMI, broker RPC, and PawnIO are all blocking Win32 calls. Run the
        // entire hardware probe off the async runtime; otherwise one stuck
        // driver call prevents the scanner's 30-second timeout from firing and
        // discovery never advances to HID/USB.
        let (board_info, buses) = tokio::task::spawn_blocking(|| {
            let board_info = board::read_board_info();
            // Detect each port with its own PawnIO instance so chip A's
            // find_bars registration isn't clobbered by probing chip B's slot.
            let mut buses: Vec<(DetectedChip, Arc<Mutex<LpcIoBus>>)> = Vec::new();
            for &port in PROBE_PORTS {
                let bus = match LpcIoBus::open(None) {
                    Ok(bus) => bus,
                    Err(e) => {
                        log::debug!("[SuperIO] LpcIO.bin not available: {e}");
                        return (board_info, buses);
                    }
                };

                if let Err(e) = bus.select_slot(port_slot(port)) {
                    log::debug!("[SuperIO] select_slot for port 0x{port:02X} failed: {e}");
                    continue;
                }

                match nct677x::detect(&bus, port) {
                    Ok(Some(d)) => {
                        buses.push((DetectedChip::Nct677x(d), Arc::new(Mutex::new(bus))))
                    }
                    Ok(None) => {} // bus drops, closing the PawnIO handle
                    Err(e) => log::debug!("[NCT677x] detect on port 0x{port:02X} failed: {e}"),
                }
            }
            (board_info, buses)
        })
        .await?;

        if buses.is_empty() {
            log::debug!("[SuperIO] no recognised SuperIO chip found");
            return Ok(());
        }

        for (chip, bus) in buses {
            let sensor: Arc<dyn crate::drivers::Device> =
                Arc::new(SuperIoSensorDevice::new(chip, Arc::clone(&bus)));
            crate::registry::usecases::registration::register_device(&app, sensor).await;

            for ch in 0..chip.fan_count() {
                let label = board::fan_label(&board_info, chip, ch);
                let fan: Arc<dyn crate::drivers::Device> =
                    Arc::new(SuperIoFanDevice::new(chip, ch, label, Arc::clone(&bus)));
                crate::registry::usecases::registration::register_device(&app, fan).await;
            }
        }
        Ok(())
    }
}
