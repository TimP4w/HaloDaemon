// SPDX-License-Identifier: GPL-3.0-or-later
//! Scoped connected-UDP client backing the `halod.udp` plugin capability. The
//! socket is `connect()`ed to the single configured destination — vetted once
//! through the shared SSRF [`net_guard`] — so a plugin can only exchange
//! datagrams with that one host/port. There is deliberately no `send_to`,
//! broadcast, multicast, or discovery: those are out of scope for this release.
//!
//! [`net_guard`]: crate::domain::plugin::engine::backends::net_guard

use std::net::{SocketAddr, UdpSocket};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};

use crate::domain::plugin::engine::backends::net_guard;
use crate::domain::plugin::manifest::UdpConfig;

/// Performs the actual datagram I/O. The live implementation is a connected
/// [`UdpSocket`]; the plugin-test harness swaps in a recording backend so
/// `test.lua` can assert sends and inject received datagrams without a network.
pub trait UdpBackend: Send + Sync {
    fn send(&self, data: &[u8]) -> Result<usize>;
    /// Wait up to `timeout` for one datagram, truncated to `max_bytes`. `None`
    /// on timeout (no datagram arrived).
    fn receive(&self, timeout: Duration, max_bytes: usize) -> Result<Option<Vec<u8>>>;
}

/// A connected UDP socket bound to an ephemeral local port of the destination's
/// address family.
pub struct ConnectedUdpBackend {
    socket: UdpSocket,
}

impl ConnectedUdpBackend {
    pub fn connect(addr: SocketAddr, send_timeout: Duration) -> Result<Self> {
        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 bind")
        } else {
            "[::]:0".parse().expect("valid v6 bind")
        };
        let socket = UdpSocket::bind(bind).context("binding a local UDP socket")?;
        socket
            .connect(addr)
            .with_context(|| format!("connecting UDP to {addr}"))?;
        socket
            .set_write_timeout(Some(send_timeout))
            .context("setting UDP send timeout")?;
        Ok(Self { socket })
    }
}

impl UdpBackend for ConnectedUdpBackend {
    fn send(&self, data: &[u8]) -> Result<usize> {
        self.socket.send(data).context("UDP send failed")
    }

    fn receive(&self, timeout: Duration, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        self.socket
            .set_read_timeout(Some(timeout))
            .context("setting UDP receive timeout")?;
        let mut buf = vec![0u8; max_bytes];
        match self.socket.recv(&mut buf) {
            Ok(n) => {
                buf.truncate(n);
                Ok(Some(buf))
            }
            Err(e)
                if matches!(
                    e.kind(),
                    std::io::ErrorKind::WouldBlock | std::io::ErrorKind::TimedOut
                ) =>
            {
                Ok(None)
            }
            Err(e) => Err(anyhow::Error::from(e).context("UDP receive failed")),
        }
    }
}

/// A plugin's ready-to-use UDP capability: its declared bounds plus the backend
/// that moves datagrams, shared with the Lua worker via
/// [`crate::domain::plugin::engine::udp_api`].
#[derive(Clone)]
pub struct UdpRuntime {
    backend: Arc<dyn UdpBackend>,
    max_datagram_bytes: usize,
    recv_timeout: Duration,
}

impl UdpRuntime {
    pub fn new(
        backend: Arc<dyn UdpBackend>,
        max_datagram_bytes: usize,
        recv_timeout: Duration,
    ) -> Self {
        Self {
            backend,
            max_datagram_bytes,
            recv_timeout,
        }
    }

    /// Build the live runtime from a manifest's declared udp transport and the
    /// plugin's configured destination host/port.
    pub fn from_config(config: &UdpConfig, host: &str, port: u16) -> Result<Self> {
        let addr = net_guard::resolve_vetted_addr(host, port, config.allow_private)?;
        let backend = Arc::new(ConnectedUdpBackend::connect(
            addr,
            Duration::from_millis(config.send_timeout_ms),
        )?);
        Ok(Self::new(
            backend,
            config.max_datagram_bytes,
            Duration::from_millis(config.recv_timeout_ms),
        ))
    }

    pub fn send(&self, data: &[u8]) -> Result<usize> {
        if data.len() > self.max_datagram_bytes {
            bail!(
                "udp datagram {} exceeds the declared max_datagram_bytes {}",
                data.len(),
                self.max_datagram_bytes
            );
        }
        self.backend.send(data)
    }

    /// Receive one datagram, waiting at most the requested timeout clamped to the
    /// declared ceiling (0 or unset uses the ceiling). `None` on timeout.
    pub fn receive(&self, timeout: Option<Duration>) -> Result<Option<Vec<u8>>> {
        let timeout = match timeout {
            Some(t) if !t.is_zero() => t.min(self.recv_timeout),
            _ => self.recv_timeout,
        };
        self.backend.receive(timeout, self.max_datagram_bytes)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex;

    struct StubBackend {
        sent: Mutex<Vec<Vec<u8>>>,
        inbound: Mutex<std::collections::VecDeque<Vec<u8>>>,
    }

    impl UdpBackend for StubBackend {
        fn send(&self, data: &[u8]) -> Result<usize> {
            self.sent.lock().unwrap().push(data.to_vec());
            Ok(data.len())
        }
        fn receive(&self, _timeout: Duration, max_bytes: usize) -> Result<Option<Vec<u8>>> {
            Ok(self.inbound.lock().unwrap().pop_front().map(|mut d| {
                d.truncate(max_bytes);
                d
            }))
        }
    }

    fn runtime(inbound: Vec<Vec<u8>>) -> (UdpRuntime, Arc<StubBackend>) {
        let backend = Arc::new(StubBackend {
            sent: Mutex::new(Vec::new()),
            inbound: Mutex::new(inbound.into()),
        });
        (
            UdpRuntime::new(backend.clone(), 8, Duration::from_millis(100)),
            backend,
        )
    }

    #[test]
    fn send_enforces_the_datagram_size_cap_before_the_backend() {
        let (rt, backend) = runtime(vec![]);
        assert!(rt.send(&[0u8; 8]).is_ok());
        let err = rt.send(&[0u8; 9]).unwrap_err().to_string();
        assert!(err.contains("max_datagram_bytes"), "{err}");
        // Only the admitted send reached the backend.
        assert_eq!(backend.sent.lock().unwrap().len(), 1);
    }

    #[test]
    fn receive_returns_a_datagram_then_none_when_drained() {
        let (rt, _) = runtime(vec![vec![1, 2, 3]]);
        assert_eq!(rt.receive(None).unwrap(), Some(vec![1, 2, 3]));
        assert_eq!(rt.receive(None).unwrap(), None);
    }

    #[test]
    fn receive_truncates_to_the_datagram_cap() {
        let (rt, _) = runtime(vec![vec![0u8; 20]]);
        assert_eq!(rt.receive(None).unwrap().unwrap().len(), 8);
    }
}
