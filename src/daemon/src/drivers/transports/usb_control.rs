/// Synchronous rusb vendor-control transport for devices that use USB control transfers
/// (e.g. DDC/CI over a USB Billboard hub controller).
///
/// All writes are blocking. USB control transfers are sub-millisecond, so calling
/// directly from async context without spawn_blocking is acceptable.
pub mod inner {
    use anyhow::{anyhow, Result};
    use rusb::{Context, DeviceHandle, UsbContext};
    use std::time::Duration;

    const DEFAULT_TIMEOUT: Duration = Duration::from_millis(1000);

    pub struct UsbControlTransport {
        handle: DeviceHandle<Context>,
        interface: u8,
    }

    impl UsbControlTransport {
        pub fn open(vid: u16, pid: u16, interface: u8) -> Result<Self> {
            let ctx = Context::new()?;
            let handle = ctx
                .open_device_with_vid_pid(vid, pid)
                .ok_or_else(|| anyhow!("USB device {:04x}:{:04x} not found", vid, pid))?;

            #[cfg(target_os = "linux")]
            if handle.kernel_driver_active(interface).unwrap_or(false) {
                handle.detach_kernel_driver(interface)?;
            }

            handle.claim_interface(interface)?;
            Ok(Self { handle, interface })
        }

        pub fn write_control(
            &self,
            bm_request_type: u8,
            b_request: u8,
            w_value: u16,
            w_index: u16,
            data: &[u8],
        ) -> Result<()> {
            self.handle
                .write_control(
                    bm_request_type,
                    b_request,
                    w_value,
                    w_index,
                    data,
                    DEFAULT_TIMEOUT,
                )
                .map_err(|e| anyhow!("USB control write failed: {}", e))?;
            Ok(())
        }

        pub fn read_control(
            &self,
            bm_request_type: u8,
            b_request: u8,
            w_value: u16,
            w_index: u16,
            buf: &mut [u8],
        ) -> Result<usize> {
            self.handle
                .read_control(
                    bm_request_type,
                    b_request,
                    w_value,
                    w_index,
                    buf,
                    DEFAULT_TIMEOUT,
                )
                .map_err(|e| anyhow!("USB control read failed: {}", e))
        }

        pub fn release(self) {
            let _ = self.handle.release_interface(self.interface);
            #[cfg(target_os = "linux")]
            let _ = self.handle.attach_kernel_driver(self.interface);
        }
    }
}

pub use inner::UsbControlTransport;
