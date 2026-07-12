// SPDX-License-Identifier: GPL-3.0-or-later
//! A TCP stream transport for plugins that talk to a network service (e.g. an
//! OpenRGB SDK server) instead of a hardware bus.
//!
//! Unlike HID's report-based `read`, a TCP byte stream has no message
//! framing of its own: `read(size)` here is **read-exact** — it returns
//! exactly `size` bytes or errors on timeout/EOF, never a short read. Wire
//! protocols built on this transport (e.g. `builtins/openrgb.lua`) rely on
//! that to read a fixed header, then exactly `data_length` bytes of payload.

use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;
use halod_shared::types::{WriteRateLimit, WriteRateStatus};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;
use tokio::sync::Mutex as AsyncMutex;

use crate::drivers::Metered;

use super::Transport;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TcpTransport {
    stream: Metered<AsyncMutex<TcpStream>>,
    timeout: Duration,
}

fn resolve_timeout(timeout_ms: u64) -> Duration {
    if timeout_ms == 0 {
        DEFAULT_TIMEOUT
    } else {
        Duration::from_millis(timeout_ms)
    }
}

impl TcpTransport {
    /// Connect to `host:port`, bounding both the connect attempt and every
    /// subsequent read/write by `timeout_ms` (falls back to a 5s default when
    /// `0`, so a plugin can't accidentally hang the discovery pass forever).
    pub async fn connect(host: &str, port: u16, timeout_ms: u64) -> Result<Self> {
        let timeout = resolve_timeout(timeout_ms);
        let stream = tokio::time::timeout(timeout, TcpStream::connect((host, port)))
            .await
            .with_context(|| format!("connecting to {host}:{port} timed out"))?
            .with_context(|| format!("connecting to {host}:{port}"))?;
        stream.set_nodelay(true).ok();
        Ok(Self {
            stream: Metered::new(AsyncMutex::new(stream), None),
            timeout,
        })
    }

    /// Blocking twin of [`Self::connect`]: uses `std::net::TcpStream` so it can
    /// be called from a synchronous context (a `PluginTransportDescriptor::open`
    /// fn pointer) without needing `.await`. Registers the resulting socket
    /// with the *current* Tokio reactor via `TcpStream::from_std`, so this
    /// still requires an active runtime context — it just doesn't need to be
    /// polled as a future itself.
    ///
    /// Callers on a shared async runtime should run this inside
    /// `tokio::task::spawn_blocking` (a real network connect can block for the
    /// full timeout) rather than calling it directly from an async task.
    pub fn connect_blocking(host: &str, port: u16, timeout_ms: u64) -> Result<Self> {
        use std::net::ToSocketAddrs;

        let addr = (host, port)
            .to_socket_addrs()
            .with_context(|| format!("resolving {host}:{port}"))?
            .next()
            .ok_or_else(|| anyhow::anyhow!("{host}:{port} resolved to no addresses"))?;
        Self::connect_addr_blocking(addr, timeout_ms)
    }

    /// Blocking connect to an already-resolved [`SocketAddr`]. Callers that must
    /// vet the destination IP (the plugin SSRF guard) resolve the hostname once,
    /// check the address, and connect here — so the socket lands on exactly the
    /// address that was vetted, with no second name resolution a DNS rebind could
    /// redirect. Same runtime-registration contract as [`Self::connect_blocking`].
    pub fn connect_addr_blocking(addr: std::net::SocketAddr, timeout_ms: u64) -> Result<Self> {
        let timeout = resolve_timeout(timeout_ms);
        let std_stream = std::net::TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        std_stream.set_nodelay(true).ok();
        std_stream
            .set_nonblocking(true)
            .context("setting socket non-blocking")?;
        let stream =
            TcpStream::from_std(std_stream).context("registering socket with the async runtime")?;
        Ok(Self {
            stream: Metered::new(AsyncMutex::new(stream), None),
            timeout,
        })
    }
}

#[async_trait]
impl Transport for TcpTransport {
    async fn write(&self, data: &[u8]) -> Result<()> {
        let guard = self.stream.write_access(data.len()).await?;
        let mut stream = guard.lock().await;
        tokio::time::timeout(self.timeout, stream.write_all(data))
            .await
            .context("tcp write timed out")?
            .context("tcp write failed")
    }

    /// Read-exact: returns exactly `size` bytes or errors (never a short read).
    async fn read(&self, size: usize) -> Result<Vec<u8>> {
        let stream = self.stream.read_access();
        let mut stream = stream.lock().await;
        let mut buf = vec![0u8; size];
        tokio::time::timeout(self.timeout, stream.read_exact(&mut buf))
            .await
            .context("tcp read timed out")?
            .context("tcp read failed (connection closed?)")?;
        Ok(buf)
    }

    fn rate_status(&self) -> WriteRateStatus {
        self.stream.status()
    }

    fn set_write_rate_limit(&self, limit: Option<WriteRateLimit>) {
        self.stream.set_limit(limit);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use tokio::net::TcpListener;

    async fn loopback_pair() -> (TcpTransport, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let client = TcpTransport::connect(&addr.ip().to_string(), addr.port(), 1000)
            .await
            .unwrap();
        let (server, _) = listener.accept().await.unwrap();
        (client, server)
    }

    #[tokio::test]
    async fn write_then_read_round_trips_over_loopback() {
        let (client, mut server) = loopback_pair().await;
        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 4];
            server.read_exact(&mut buf).await.unwrap();
            server.write_all(&[9, 8, 7]).await.unwrap();
        });
        client.write(&[1, 2, 3, 4]).await.unwrap();
        let reply = client.read(3).await.unwrap();
        assert_eq!(reply, vec![9, 8, 7]);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_is_exact_not_short() {
        let (client, mut server) = loopback_pair().await;
        let server_task = tokio::spawn(async move {
            // Split the write into two chunks; read(6) on the client side
            // must still return all 6 bytes, not stop at the first chunk.
            server.write_all(&[1, 2, 3]).await.unwrap();
            tokio::time::sleep(Duration::from_millis(20)).await;
            server.write_all(&[4, 5, 6]).await.unwrap();
        });
        let reply = client.read(6).await.unwrap();
        assert_eq!(reply, vec![1, 2, 3, 4, 5, 6]);
        server_task.await.unwrap();
    }

    #[tokio::test]
    async fn read_past_eof_errors_instead_of_returning_short() {
        let (client, server) = loopback_pair().await;
        drop(server);
        assert!(client.read(4).await.is_err());
    }

    #[tokio::test]
    async fn connect_to_a_closed_port_errors() {
        // Bind and immediately drop to get a port nothing is listening on.
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        let result = TcpTransport::connect(&addr.ip().to_string(), addr.port(), 500).await;
        assert!(result.is_err());
    }

    #[tokio::test]
    async fn connect_blocking_round_trips_from_within_a_runtime() {
        let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let ip = addr.ip().to_string();
        let port = addr.port();
        let client = tokio::task::spawn_blocking(move || {
            TcpTransport::connect_blocking(&ip, port, 500).unwrap()
        })
        .await
        .unwrap();
        let (mut server, _) = listener.accept().await.unwrap();

        let server_task = tokio::spawn(async move {
            let mut buf = [0u8; 2];
            server.read_exact(&mut buf).await.unwrap();
            server.write_all(&[42]).await.unwrap();
        });
        client.write(&[5, 6]).await.unwrap();
        let reply = client.read(1).await.unwrap();
        assert_eq!(reply, vec![42]);
        server_task.await.unwrap();
    }

    #[test]
    fn connect_blocking_to_a_closed_port_errors() {
        let rt = tokio::runtime::Runtime::new().unwrap();
        rt.block_on(async {
            let listener = TcpListener::bind("127.0.0.1:0").await.unwrap();
            let addr = listener.local_addr().unwrap();
            drop(listener);
            let result = tokio::task::spawn_blocking(move || {
                TcpTransport::connect_blocking(&addr.ip().to_string(), addr.port(), 300)
            })
            .await
            .unwrap();
            assert!(result.is_err());
        });
    }
}
