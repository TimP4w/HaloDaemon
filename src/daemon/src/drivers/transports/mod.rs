// SPDX-License-Identifier: GPL-3.0-or-later
#[cfg(target_os = "windows")]
pub mod amd_smn;
pub mod hid;
#[cfg(target_os = "linux")]
pub mod hwmon;
#[cfg(target_os = "windows")]
pub mod lpcio;
pub mod mock;
pub mod register_ops;
pub mod smbus;
pub mod tcp;
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
    /// `true` when we detached a kernel driver in [`Self::claim`], so
    /// [`Drop`] knows to re-attach it. Kernel-driver detach/reattach is a Linux
    /// (libusb) concern only.
    #[cfg(target_os = "linux")]
    had_kernel_driver: bool,
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
        let mut had_kernel_driver = false;
        #[cfg(target_os = "linux")]
        match handle.kernel_driver_active(interface) {
            Ok(true) => {
                handle.detach_kernel_driver(interface)?;
                had_kernel_driver = true;
            }
            Ok(false) => {}
            Err(e) => log::warn!(
                "UsbClaim: kernel_driver_active({}) query failed: {e}",
                interface
            ),
        }

        handle.claim_interface(interface)?;
        Ok(Self {
            handle,
            interface,
            #[cfg(target_os = "linux")]
            had_kernel_driver,
        })
    }

    #[expect(dead_code, reason = "explicit early release for transport owners")]
    pub fn release(self) {
        // Drop runs, releasing the interface and re-attaching the kernel driver
        // on Linux. The caller has taken ownership and wants explicit cleanup;
        // letting the value drop is the correct behaviour.
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
        if self.had_kernel_driver {
            if let Err(e) = self.handle.attach_kernel_driver(self.interface) {
                log::warn!(
                    "UsbClaim: attach_kernel_driver({}) failed: {e}",
                    self.interface
                );
            }
        }
    }
}

/// Lets device code accept `impl BulkTransport` and be tested with a mock instead of real hardware.
#[async_trait]
#[expect(dead_code, reason = "plugin-facing bulk transport protocol")]
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
#[expect(dead_code, reason = "rate-limit hook is optional transport protocol")]
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

    /// Pop the next inbound report regardless of which endpoint it arrived on.
    /// Single-node transports have only one endpoint, so this defaults to
    /// `read`; multi-collection transports (HID short/long on Windows) merge
    /// both queues so protocol code can match a reply wherever it lands.
    async fn read_any(&self, size: usize) -> Result<Vec<u8>> {
        self.read(size).await
    }

    /// Hand back a report that was read but did not belong to the in-flight
    /// request, so it is delivered through the event path (`drain_events`)
    /// instead of being dropped. The transport never interprets the bytes.
    async fn defer_event(&self, _data: &[u8]) -> Result<()> {
        anyhow::bail!("defer_event not supported by this transport")
    }

    /// Write to an explicitly opened companion HID collection. Protocol code
    /// chooses the collection; the transport never interprets report IDs.
    async fn write_companion(&self, _data: &[u8]) -> Result<()> {
        anyhow::bail!("companion collection not supported by this transport")
    }

    /// Batch writes to an explicitly opened companion collection. HID
    /// protocols with split short/long collections use this for streaming.
    async fn write_many_companion(&self, packets: &[Vec<u8>]) -> Result<()> {
        for packet in packets {
            self.write_companion(packet).await?;
        }
        Ok(())
    }

    async fn read_companion(&self, _size: usize) -> Result<Vec<u8>> {
        anyhow::bail!("companion collection not supported by this transport")
    }

    async fn write_then_read_companion(&self, data: &[u8], size: usize) -> Result<Vec<u8>> {
        self.write_companion(data).await?;
        self.read_companion(size).await
    }

    fn has_companion(&self) -> bool {
        false
    }

    /// Subscribe to dispatcher wakeups for event-driven transports. Request
    /// reads use a separate input handle and never consume this event stream.
    fn event_receiver(&self) -> Option<tokio::sync::watch::Receiver<u64>> {
        None
    }

    /// Drain unsolicited input in arrival order for delivery to Lua `event()`.
    async fn drain_events(&self, _limit: usize) -> Result<Vec<TransportEvent>> {
        Ok(Vec::new())
    }

    /// Start dispatching unsolicited input reports. HID opens a dedicated
    /// event handle lazily; request/reply reads retain their own input handle.
    /// Called only when the owning plugin declares an `event()` callback.
    fn enable_event_listener(&self) -> Result<()> {
        Ok(())
    }

    /// Live write-rate limit and throughput. No default: every implementor
    /// (including test mocks) must back this with a real `Metered` gate
    /// rather than silently reporting nothing — a device generic over
    /// `T: Transport` can then rely on the limiter actually working.
    fn rate_status(&self) -> WriteRateStatus;

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>);
}

#[derive(Debug, Clone)]
pub struct TransportEvent {
    pub endpoint: &'static str,
    pub data: Vec<u8>,
}
