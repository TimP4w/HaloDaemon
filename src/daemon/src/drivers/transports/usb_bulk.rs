// SPDX-License-Identifier: GPL-3.0-or-later
/// Synchronous rusb bulk-OUT transport for devices that require a separate USB
/// bulk endpoint for large data transfers (e.g. NZXT Kraken LCD image upload).
///
/// All blocking I/O is encapsulated: use [`UsbBulkTransport::write_async`] from
/// async contexts to avoid stalling the tokio executor.
use anyhow::{anyhow, Result};
use async_trait::async_trait;
use rusb::{Context, Direction, TransferType, UsbContext};

use crate::drivers::transports::{BulkTransport, UsbClaim};
use crate::drivers::Metered;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

const BULK_ENDPOINT: u8 = 0x02;
const DEFAULT_TIMEOUT_MS: u64 = 10_000;

/// `UsbBulkTransport` is cheaply cloneable: cloning shares the same underlying
/// device handle so multiple owners can issue writes without reopening the device.
#[derive(Clone)]
pub struct UsbBulkTransport {
    io: Metered<UsbClaim>,
    endpoint: u8,
}

impl UsbBulkTransport {
    /// Open the device by VID/PID and claim the interface that owns the bulk-OUT
    /// endpoint at address `BULK_ENDPOINT`.
    pub fn open(vid: u16, pid: u16, limit: Option<WriteRateLimit>) -> Result<Self> {
        let ctx = Context::new()?;
        let handle = ctx
            .open_device_with_vid_pid(vid, pid)
            .ok_or_else(|| anyhow!("USB device {:04x}:{:04x} not found", vid, pid))?;

        // Find the interface that owns the bulk-OUT endpoint.
        let device = handle.device();
        let config = device.active_config_descriptor()?;
        let mut found_intf: Option<u8> = None;
        'outer: for intf in config.interfaces() {
            for desc in intf.descriptors() {
                for ep in desc.endpoint_descriptors() {
                    if ep.direction() == Direction::Out
                        && ep.transfer_type() == TransferType::Bulk
                        && ep.address() == BULK_ENDPOINT
                    {
                        found_intf = Some(desc.interface_number());
                        break 'outer;
                    }
                }
            }
        }
        let interface = found_intf.ok_or_else(|| {
            anyhow!(
                "Bulk-OUT endpoint 0x{:02x} not found on {:04x}:{:04x}",
                BULK_ENDPOINT,
                vid,
                pid
            )
        })?;

        let claim = UsbClaim::claim(handle, interface)?;

        Ok(Self {
            io: Metered::new(claim, limit),
            endpoint: BULK_ENDPOINT,
        })
    }

    pub fn write(&self, data: &[u8]) -> Result<usize> {
        let claim = self.io.write_access_blocking(data.len())?;
        // A bulk transfer may complete short, so loop until every byte is delivered.
        let mut sent = 0;
        while sent < data.len() {
            let n = claim.handle.write_bulk(
                self.endpoint,
                &data[sent..],
                std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )?;
            if n == 0 {
                anyhow::bail!(
                    "bulk write stalled after {sent}/{} bytes on endpoint 0x{:02x}",
                    data.len(),
                    self.endpoint
                );
            }
            sent += n;
        }
        Ok(sent)
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }

    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }
}

#[async_trait]
impl BulkTransport for UsbBulkTransport {
    fn write(&self, data: &[u8]) -> Result<usize> {
        UsbBulkTransport::write(self, data)
    }

    async fn write_async(&self, data: Vec<u8>) -> Result<usize> {
        let transport = self.clone();
        tokio::task::spawn_blocking(move || transport.write(&data))
            .await
            .map_err(|e| anyhow!("spawn_blocking join: {e}"))?
    }

    fn rate_status(&self) -> WriteRateStatus {
        UsbBulkTransport::rate_status(self)
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        UsbBulkTransport::set_write_rate_limit(self, limit);
    }
}
