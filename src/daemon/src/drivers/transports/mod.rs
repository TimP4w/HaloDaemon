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

    async fn feature_exchange(&self, data: &[u8], _response_size: usize) -> Result<Vec<u8>> {
        let _ = data;
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

    async fn read_matching<F>(&self, size: usize, predicate: F, max_tries: usize) -> Option<Vec<u8>>
    where
        F: Fn(&[u8]) -> bool + Send,
    {
        for _ in 0..max_tries {
            let msg = self.read(size).await.unwrap_or_default();
            if predicate(&msg) {
                return Some(msg);
            }
        }
        None
    }
}

