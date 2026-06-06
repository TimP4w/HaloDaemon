/// Synchronous rusb bulk-OUT transport for devices that require a separate USB
/// bulk endpoint for large data transfers (e.g. NZXT Kraken LCD image upload).
///
/// All writes are blocking. Callers must wrap calls in `tokio::task::spawn_blocking`.
pub mod inner {
    use anyhow::{anyhow, Result};
    use rusb::{Context, DeviceHandle, Direction, TransferType, UsbContext};

    const BULK_ENDPOINT: u8 = 0x02;
    const DEFAULT_TIMEOUT_MS: u64 = 10_000;

    pub struct UsbBulkTransport {
        handle: DeviceHandle<Context>,
        interface: u8,
        endpoint: u8,
    }

    impl UsbBulkTransport {
        /// Open the device by VID/PID and claim the interface that owns the bulk-OUT
        /// endpoint at address `BULK_ENDPOINT`.
        pub fn open(vid: u16, pid: u16) -> Result<Self> {
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

            #[cfg(target_os = "linux")]
            if handle.kernel_driver_active(interface).unwrap_or(false) {
                handle.detach_kernel_driver(interface)?;
            }

            handle.claim_interface(interface)?;

            Ok(Self {
                handle,
                interface,
                endpoint: BULK_ENDPOINT,
            })
        }

        pub fn write(&self, data: &[u8]) -> Result<usize> {
            let n = self.handle.write_bulk(
                self.endpoint,
                data,
                std::time::Duration::from_millis(DEFAULT_TIMEOUT_MS),
            )?;
            Ok(n)
        }

        pub fn release(self) {
            let _ = self.handle.release_interface(self.interface);
            #[cfg(target_os = "linux")]
            let _ = self.handle.attach_kernel_driver(self.interface);
        }
    }
}

pub use inner::UsbBulkTransport;
