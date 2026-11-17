// SPDX-License-Identifier: GPL-3.0-or-later
//! A TCP stream transport for plugins that talk to a network service (e.g. an
//! OpenRGB SDK server) instead of a hardware bus.
//!
//! Unlike HID's report-based `read`, a TCP byte stream has no message
//! framing of its own: `read(size)` here is **read-exact** — it returns
//! exactly `size` bytes or errors on timeout/EOF, never a short read. Wire
//! protocols built on this transport (e.g. `builtins/openrgb.lua`) rely on
//! that to read a fixed header, then exactly `data_length` bytes of payload.

use std::io::{Read, Write};
use std::net::{SocketAddr, TcpStream};
use std::time::Duration;

use anyhow::{Context, Result};
use halod_shared::types::{WriteRateLimit, WriteRateStatus};
use parking_lot::Mutex;

use crate::infrastructure::drivers::Metered;

use super::Transport;

const DEFAULT_TIMEOUT: Duration = Duration::from_secs(5);

pub struct TcpTransport {
    stream: Metered<Mutex<TcpStream>>,
}

fn resolve_timeout(timeout_ms: u64) -> Duration {
    if timeout_ms == 0 {
        DEFAULT_TIMEOUT
    } else {
        Duration::from_millis(timeout_ms)
    }
}

impl TcpTransport {
    /// Connect to an already-resolved address, bounding the connect and every
    /// read/write by `timeout_ms` (5 s when `0`). Callers that vet the
    /// destination IP (the plugin SSRF guard) resolve once and connect here, so
    /// the socket lands on exactly the vetted address with no second lookup a
    /// DNS rebind could redirect.
    pub fn connect(addr: SocketAddr, timeout_ms: u64) -> Result<Self> {
        let timeout = resolve_timeout(timeout_ms);
        let stream = TcpStream::connect_timeout(&addr, timeout)
            .with_context(|| format!("connecting to {addr}"))?;
        stream.set_nodelay(true).ok();
        stream
            .set_read_timeout(Some(timeout))
            .context("setting the read timeout")?;
        stream
            .set_write_timeout(Some(timeout))
            .context("setting the write timeout")?;
        Ok(Self {
            stream: Metered::new(Mutex::new(stream), None),
        })
    }
}

impl Transport for TcpTransport {
    fn write(&self, data: &[u8]) -> Result<()> {
        self.stream
            .write_access(data.len())?
            .lock()
            .write_all(data)
            .context("tcp write failed")
    }

    /// Read-exact: returns exactly `size` bytes or errors (never a short read).
    fn read(&self, size: usize) -> Result<Vec<u8>> {
        let mut buf = vec![0u8; size];
        self.stream
            .read_access()
            .lock()
            .read_exact(&mut buf)
            .context("tcp read failed (timed out or connection closed)")?;
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
    use std::net::TcpListener;

    fn loopback_pair() -> (TcpTransport, TcpStream) {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpTransport::connect(listener.local_addr().unwrap(), 1000).unwrap();
        let (server, _) = listener.accept().unwrap();
        (client, server)
    }

    #[test]
    fn write_then_read_round_trips_over_loopback() {
        let (client, mut server) = loopback_pair();
        let server_thread = std::thread::spawn(move || {
            let mut buf = [0u8; 4];
            server.read_exact(&mut buf).unwrap();
            server.write_all(&[9, 8, 7]).unwrap();
        });
        client.write(&[1, 2, 3, 4]).unwrap();
        assert_eq!(client.read(3).unwrap(), vec![9, 8, 7]);
        server_thread.join().unwrap();
    }

    #[test]
    fn read_is_exact_not_short() {
        let (client, mut server) = loopback_pair();
        let server_thread = std::thread::spawn(move || {
            server.write_all(&[1, 2, 3]).unwrap();
            std::thread::sleep(Duration::from_millis(20));
            server.write_all(&[4, 5, 6]).unwrap();
        });
        assert_eq!(client.read(6).unwrap(), vec![1, 2, 3, 4, 5, 6]);
        server_thread.join().unwrap();
    }

    #[test]
    fn read_past_eof_errors_instead_of_returning_short() {
        let (client, server) = loopback_pair();
        drop(server);
        assert!(client.read(4).is_err());
    }

    #[test]
    fn read_times_out_when_the_peer_stays_silent() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let client = TcpTransport::connect(listener.local_addr().unwrap(), 50).unwrap();
        let _server = listener.accept().unwrap();
        assert!(client.read(1).is_err());
    }

    #[test]
    fn connect_to_a_closed_port_errors() {
        let listener = TcpListener::bind("127.0.0.1:0").unwrap();
        let addr = listener.local_addr().unwrap();
        drop(listener);
        assert!(TcpTransport::connect(addr, 300).is_err());
    }
}
