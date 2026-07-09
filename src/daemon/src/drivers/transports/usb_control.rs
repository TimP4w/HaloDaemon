// SPDX-License-Identifier: GPL-3.0-or-later
/// Synchronous rusb vendor-control transport (e.g. DDC/CI over a USB Billboard
/// hub controller). Transfers are blocking; callers on async tasks must ensure
/// the timeout is short enough not to stall the executor thread.
use anyhow::{Context as _, Result};
use std::time::Duration;

use crate::drivers::transports::{ControlTransport, UsbClaim};
use crate::drivers::Metered;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};

const DEFAULT_TIMEOUT: Duration = Duration::from_millis(50);

pub struct UsbControlTransport {
    io: Metered<UsbClaim>,
}

impl UsbControlTransport {
    pub fn open(vid: u16, pid: u16, interface: u8, limit: Option<WriteRateLimit>) -> Result<Self> {
        let claim = UsbClaim::open(vid, pid, interface)?;
        Ok(Self {
            io: Metered::new(claim, limit),
        })
    }

    pub fn write_control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        data: &[u8],
    ) -> Result<()> {
        let claim = self.io.write_access_blocking(data.len())?;
        claim
            .handle
            .write_control(
                bm_request_type,
                b_request,
                w_value,
                w_index,
                data,
                DEFAULT_TIMEOUT,
            )
            .context("USB control write failed")?;
        Ok(())
    }

    pub fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }

    pub fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }

    pub fn read_control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        buf: &mut [u8],
    ) -> Result<usize> {
        self.io
            .read_access()
            .handle
            .read_control(
                bm_request_type,
                b_request,
                w_value,
                w_index,
                buf,
                DEFAULT_TIMEOUT,
            )
            .context("USB control read failed")
    }

    pub fn release(self) {
        if let Some(claim) = self.io.into_inner() {
            claim.release();
        }
    }
}

impl ControlTransport for UsbControlTransport {
    fn write_control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        data: &[u8],
    ) -> anyhow::Result<()> {
        UsbControlTransport::write_control(self, bm_request_type, b_request, w_value, w_index, data)
    }

    fn read_control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        buf: &mut [u8],
    ) -> anyhow::Result<usize> {
        UsbControlTransport::read_control(self, bm_request_type, b_request, w_value, w_index, buf)
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.io.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.io.set_limit(limit);
    }
}
