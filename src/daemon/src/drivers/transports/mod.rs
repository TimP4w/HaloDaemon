// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(target_os = "windows")]
pub mod amd_smn;
pub mod hid;
#[cfg(target_os = "linux")]
pub mod hwmon;
#[cfg(target_os = "windows")]
pub mod lpcio;
pub mod mock;
#[cfg(target_os = "windows")]
pub mod pawnio;
pub mod smbus;
pub mod usb_bulk;
pub mod usb_control;

use anyhow::Result;
use async_trait::async_trait;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};
use rusb::UsbContext;

/// Shared USB interface claim guard with automatic detach/reattach of kernel drivers on Linux.
pub struct UsbClaim {
    pub handle: rusb::DeviceHandle<rusb::Context>,
    pub interface: u8,
}

impl UsbClaim {
    /// Open a USB device by VID/PID and claim the interface.
    pub fn open(vid: u16, pid: u16, interface: u8) -> Result<Self> {
        let ctx = rusb::Context::new()?;
        let handle = ctx
            .open_device_with_vid_pid(vid, pid)
            .ok_or_else(|| anyhow::anyhow!("USB device {:04x}:{:04x} not found", vid, pid))?;
        Self::claim(handle, interface)
    }

    /// Claim an interface on an already-opened device handle.
    pub fn claim(handle: rusb::DeviceHandle<rusb::Context>, interface: u8) -> Result<Self> {
        #[cfg(target_os = "linux")]
        match handle.kernel_driver_active(interface) {
            Ok(true) => handle.detach_kernel_driver(interface)?,
            Ok(false) => {}
            Err(e) => log::warn!(
                "UsbClaim: kernel_driver_active({}) query failed: {e}",
                interface
            ),
        }

        handle.claim_interface(interface)?;
        Ok(Self { handle, interface })
    }

    pub fn release(self) {
        std::mem::forget(self);
    }
}

impl Drop for UsbClaim {
    fn drop(&mut self) {
        if let Err(e) = self.handle.release_interface(self.interface) {
            log::warn!(
                "UsbClaim: release_interface({}) failed: {e}",
                self.interface
            );
        }
        #[cfg(target_os = "linux")]
        if let Err(e) = self.handle.attach_kernel_driver(self.interface) {
            log::warn!(
                "UsbClaim: attach_kernel_driver({}) failed: {e}",
                self.interface
            );
        }
    }
}

/// Lets device code accept `impl BulkTransport` and be tested with a mock instead of real hardware.
#[async_trait]
pub trait BulkTransport: Send + Sync {
    /// Write all bytes to the bulk-OUT endpoint, looping until every byte is
    /// delivered. Returns the total number of bytes sent.
    fn write(&self, data: &[u8]) -> anyhow::Result<usize>;

    /// Async wrapper that runs [`write`] on a `spawn_blocking` thread so the
    /// tokio executor is not stalled by the blocking transfer.
    async fn write_async(&self, data: Vec<u8>) -> anyhow::Result<usize>;

    /// Live write-rate limit and throughput. No default: every implementor
    /// must back this with a real `Metered` gate rather than silently
    /// reporting nothing.
    fn rate_status(&self) -> WriteRateStatus;

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>);
}

/// Abstraction over synchronous USB vendor-control transports.
///
/// `UsbControlTransport` implements this trait. Device code that accepts
/// `impl ControlTransport` (or `dyn ControlTransport`) can be tested with a
/// mock without opening real hardware.
pub trait ControlTransport: Send + Sync {
    /// Issue a vendor control OUT transfer.
    fn write_control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        data: &[u8],
    ) -> anyhow::Result<()>;

    /// Issue a vendor control IN transfer. Returns the number of bytes read.
    fn read_control(
        &self,
        bm_request_type: u8,
        b_request: u8,
        w_value: u16,
        w_index: u16,
        buf: &mut [u8],
    ) -> anyhow::Result<usize>;

    /// Live write-rate limit and throughput. No default: every implementor
    /// must back this with a real `Metered` gate rather than silently
    /// reporting nothing.
    fn rate_status(&self) -> WriteRateStatus;

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>);
}

#[async_trait]
pub trait Transport: Send + Sync {
    async fn write(&self, data: &[u8]) -> Result<()>;
    async fn read(&self, size: usize) -> Result<Vec<u8>>;

    // Extended methods — default impls for non-HID transports / mocks.
    // HidTransport overrides these with optimized hardware-backed versions.

    async fn write_then_read(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        self.write(data).await?;
        self.read(size).await
    }

    async fn write_many(&self, packets: &[Vec<u8>]) -> Result<()> {
        for pkt in packets {
            self.write(pkt).await?;
        }
        Ok(())
    }

    /// Send a feature report and read the reply; unlike `write_then_read`,
    /// `response_size` excludes the leading report-ID byte.
    async fn feature_exchange(&self, _data: &[u8], _response_size: usize) -> Result<Vec<u8>> {
        anyhow::bail!("feature_exchange not supported by this transport")
    }

    async fn read_nonblocking(&self, size: usize) -> Result<Vec<u8>> {
        self.read(size).await
    }

    fn has_long_handle(&self) -> bool {
        false
    }

    async fn read_long(&self, size: usize) -> Result<Vec<u8>> {
        self.read(size).await
    }

    // `Self: Sized` keeps this generic method out of the vtable so `dyn
    // Transport` stays object-safe (the plugin layer holds `Arc<dyn Transport>`).
    // Concrete callers are unaffected.
    async fn read_matching<F>(&self, size: usize, predicate: F, max_tries: usize) -> Option<Vec<u8>>
    where
        F: Fn(&[u8]) -> bool + Send,
        Self: Sized,
    {
        for i in 0..max_tries {
            match self.read(size).await {
                Ok(msg) if predicate(&msg) => return Some(msg),
                Ok(_) => {}
                Err(e) => log::debug!("read_matching: read failed: {e}"),
            }
            // Backoff to avoid tight-spinning on non-blocking transports.
            if i > 0 && i % 10 == 0 {
                tokio::time::sleep(tokio::time::Duration::from_millis(1)).await;
            }
        }
        None
    }

    /// Live write-rate limit and throughput. No default: every implementor
    /// (including test mocks) must back this with a real `Metered` gate
    /// rather than silently reporting nothing — a device generic over
    /// `T: Transport` can then rely on the limiter actually working.
    fn rate_status(&self) -> WriteRateStatus;

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>);
}
