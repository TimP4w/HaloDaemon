// SPDX-License-Identifier: GPL-3.0-or-later

use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, Instant};

use anyhow::{anyhow, bail, Context, Result};
use base64::Engine as _;
use parking_lot::Mutex;
use tokio::net::UdpSocket;
use tokio::runtime::Handle;
use webrtc_dtls::cipher_suite::CipherSuiteId;
use webrtc_dtls::config::Config;
use webrtc_dtls::conn::DTLSConn;

use super::UdpBackend;
use crate::domain::plugin::manifest::UdpDtls;

const CLOSE_TIMEOUT: Duration = Duration::from_millis(250);

#[derive(Clone)]
pub struct DtlsPskParams {
    identity: Vec<u8>,
    psk: Vec<u8>,
    ciphers: Vec<CipherSuiteId>,
    session_idle: Option<Duration>,
}

impl DtlsPskParams {
    pub fn from_config(dtls: &UdpDtls, identity: &str, psk: &str) -> Result<Option<Self>> {
        if identity.is_empty() || psk.is_empty() {
            return Ok(None);
        }
        Ok(Some(Self {
            identity: identity.as_bytes().to_vec(),
            psk: decode_psk(&dtls.psk_encoding, psk)?,
            ciphers: dtls
                .ciphers
                .iter()
                .map(|name| cipher_suite(name))
                .collect::<Result<_>>()?,
            session_idle: dtls.session_idle_ms.map(Duration::from_millis),
        }))
    }
}

fn cipher_suite(name: &str) -> Result<CipherSuiteId> {
    match name {
        "TLS_PSK_WITH_AES_128_GCM_SHA256" => Ok(CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256),
        "TLS_PSK_WITH_AES_128_CCM" => Ok(CipherSuiteId::Tls_Psk_With_Aes_128_Ccm),
        "TLS_PSK_WITH_AES_128_CCM_8" => Ok(CipherSuiteId::Tls_Psk_With_Aes_128_Ccm_8),
        other => bail!("dtls cipher '{other}' is not available"),
    }
}

fn decode_psk(encoding: &str, value: &str) -> Result<Vec<u8>> {
    let key = match encoding {
        "hex" => decode_hex(value)?,
        "base64" => base64::engine::general_purpose::STANDARD
            .decode(value)
            .map_err(|_| anyhow!("dtls psk is not valid base64"))?,
        "raw" => value.as_bytes().to_vec(),
        other => bail!("dtls psk_encoding '{other}' is not available"),
    };
    if key.is_empty() {
        bail!("dtls psk is empty");
    }
    Ok(key)
}

fn decode_hex(value: &str) -> Result<Vec<u8>> {
    if !value.len().is_multiple_of(2) || !value.bytes().all(|b| b.is_ascii_hexdigit()) {
        bail!("dtls psk is not valid hex");
    }
    Ok((0..value.len())
        .step_by(2)
        .map(|i| u8::from_str_radix(&value[i..i + 2], 16).expect("validated hex"))
        .collect())
}

struct Session {
    conn: DTLSConn,
    last_send: Instant,
}

pub struct DtlsPskBackend {
    addr: SocketAddr,
    params: DtlsPskParams,
    send_timeout: Duration,
    handle: Handle,
    session: Mutex<Option<Session>>,
}

impl DtlsPskBackend {
    pub fn new(
        addr: SocketAddr,
        params: DtlsPskParams,
        send_timeout: Duration,
        handle: Handle,
    ) -> Self {
        Self {
            addr,
            params,
            send_timeout,
            handle,
            session: Mutex::new(None),
        }
    }

    fn handshake(&self) -> Result<DTLSConn> {
        let psk = self.params.psk.clone();
        let config = Config {
            psk: Some(Arc::new(move |_| Ok(psk.clone()))),
            psk_identity_hint: Some(self.params.identity.clone()),
            cipher_suites: self.params.ciphers.clone(),
            ..Default::default()
        };
        let addr = self.addr;
        let bind: SocketAddr = if addr.is_ipv4() {
            "0.0.0.0:0".parse().expect("valid v4 bind")
        } else {
            "[::]:0".parse().expect("valid v6 bind")
        };
        self.handle.block_on(async {
            let socket = UdpSocket::bind(bind)
                .await
                .context("binding a local UDP socket")?;
            socket
                .connect(addr)
                .await
                .with_context(|| format!("connecting UDP to {addr}"))?;
            tokio::time::timeout(
                self.send_timeout,
                DTLSConn::new(Arc::new(socket), config, true, None),
            )
            .await
            .map_err(|_| anyhow!("DTLS handshake with {addr} timed out"))?
            .map_err(|e| anyhow!("DTLS handshake with {addr} failed: {e}"))
        })
    }

    fn end(&self, session: Option<Session>) {
        if let Some(session) = session {
            self.handle.block_on(async {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, session.conn.close()).await;
            });
        }
    }

    fn idle_expired(&self, session: &Session) -> bool {
        self.params
            .session_idle
            .is_some_and(|idle| session.last_send.elapsed() > idle)
    }
}

impl UdpBackend for DtlsPskBackend {
    fn send(&self, data: &[u8]) -> Result<usize> {
        let mut slot = self.session.lock();
        if slot.as_ref().is_some_and(|s| self.idle_expired(s)) {
            self.end(slot.take());
        }
        let session = match slot.take() {
            Some(session) => session,
            None => Session {
                conn: self.handshake()?,
                last_send: Instant::now(),
            },
        };
        match self
            .handle
            .block_on(session.conn.write(data, Some(self.send_timeout)))
        {
            Ok(sent) => {
                *slot = Some(Session {
                    last_send: Instant::now(),
                    ..session
                });
                Ok(sent)
            }
            Err(e) => {
                self.end(Some(session));
                Err(anyhow!("DTLS send failed: {e}"))
            }
        }
    }

    fn receive(&self, timeout: Duration, max_bytes: usize) -> Result<Option<Vec<u8>>> {
        let mut slot = self.session.lock();
        let Some(session) = slot.as_ref() else {
            return Ok(None);
        };
        let mut buf = vec![0u8; super::MAX_UDP_PAYLOAD];
        match self
            .handle
            .block_on(session.conn.read(&mut buf, Some(timeout)))
        {
            Ok(n) => {
                buf.truncate(n.min(max_bytes));
                Ok(Some(buf))
            }
            Err(webrtc_dtls::Error::ErrDeadlineExceeded) => Ok(None),
            Err(e) => {
                self.end(slot.take());
                Err(anyhow!("DTLS receive failed: {e}"))
            }
        }
    }
}

impl Drop for DtlsPskBackend {
    fn drop(&mut self) {
        if let Some(session) = self.session.get_mut().take() {
            self.handle.spawn(async move {
                let _ = tokio::time::timeout(CLOSE_TIMEOUT, session.conn.close()).await;
            });
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};
    use webrtc_util::conn::Listener;

    const CIPHER: &str = "TLS_PSK_WITH_AES_128_GCM_SHA256";

    fn dtls(session_idle_ms: Option<u64>) -> UdpDtls {
        UdpDtls {
            profile: "psk".into(),
            identity: "identity".into(),
            psk: "psk".into(),
            psk_encoding: "hex".into(),
            ciphers: vec![CIPHER.into()],
            session_idle_ms,
        }
    }

    struct EchoServer {
        addr: SocketAddr,
        sessions: Arc<AtomicUsize>,
        identities: Arc<Mutex<Vec<Vec<u8>>>>,
    }

    fn echo_server(runtime: &tokio::runtime::Runtime, psk: Vec<u8>) -> EchoServer {
        let sessions = Arc::new(AtomicUsize::new(0));
        let identities = Arc::new(Mutex::new(Vec::new()));
        let seen = identities.clone();
        let config = Config {
            psk: Some(Arc::new(move |identity: &[u8]| {
                seen.lock().push(identity.to_vec());
                Ok(psk.clone())
            })),
            psk_identity_hint: Some(b"server".to_vec()),
            cipher_suites: vec![CipherSuiteId::Tls_Psk_With_Aes_128_Gcm_Sha256],
            ..Default::default()
        };
        let listener = runtime
            .block_on(webrtc_dtls::listener::listen("127.0.0.1:0", config))
            .unwrap();
        let addr = runtime.block_on(listener.addr()).unwrap();
        let accepted = sessions.clone();
        runtime.spawn(async move {
            while let Ok((conn, _)) = listener.accept().await {
                accepted.fetch_add(1, Ordering::SeqCst);
                tokio::spawn(async move {
                    let mut buf = vec![0u8; 2048];
                    while let Ok(n) = conn.recv(&mut buf).await {
                        if conn.send(&buf[..n]).await.is_err() {
                            break;
                        }
                    }
                });
            }
        });
        EchoServer {
            addr,
            sessions,
            identities,
        }
    }

    fn runtime() -> tokio::runtime::Runtime {
        tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .enable_all()
            .build()
            .unwrap()
    }

    fn backend(
        runtime: &tokio::runtime::Runtime,
        addr: SocketAddr,
        psk_hex: &str,
        session_idle_ms: Option<u64>,
        send_timeout: Duration,
    ) -> DtlsPskBackend {
        let params = DtlsPskParams::from_config(&dtls(session_idle_ms), "client-identity", psk_hex)
            .unwrap()
            .unwrap();
        DtlsPskBackend::new(addr, params, send_timeout, runtime.handle().clone())
    }

    #[test]
    fn psk_decodes_in_each_declared_encoding() {
        assert_eq!(decode_psk("hex", "00ffA1").unwrap(), vec![0, 255, 161]);
        assert_eq!(decode_psk("base64", "AP+h").unwrap(), vec![0, 255, 161]);
        assert_eq!(decode_psk("raw", "ab").unwrap(), b"ab".to_vec());
        assert!(decode_psk("hex", "0").is_err());
        assert!(decode_psk("hex", "zz").is_err());
        assert!(decode_psk("base64", "!").is_err());
    }

    #[test]
    fn missing_credentials_yield_no_params() {
        assert!(DtlsPskParams::from_config(&dtls(None), "", "00")
            .unwrap()
            .is_none());
        assert!(DtlsPskParams::from_config(&dtls(None), "id", "")
            .unwrap()
            .is_none());
        assert!(DtlsPskParams::from_config(&dtls(None), "id", "xyz").is_err());
    }

    #[test]
    fn receive_before_any_send_reports_no_datagram() {
        let runtime = runtime();
        let server = echo_server(&runtime, vec![1; 16]);
        let backend = backend(
            &runtime,
            server.addr,
            &"01".repeat(16),
            None,
            Duration::from_secs(5),
        );
        assert_eq!(
            backend.receive(Duration::from_millis(10), 64).unwrap(),
            None
        );
        assert_eq!(server.sessions.load(Ordering::SeqCst), 0);
    }

    #[test]
    fn first_send_handshakes_and_datagrams_round_trip() {
        let runtime = runtime();
        let server = echo_server(&runtime, vec![1; 16]);
        let backend = backend(
            &runtime,
            server.addr,
            &"01".repeat(16),
            None,
            Duration::from_secs(5),
        );
        assert_eq!(backend.send(b"frame-1").unwrap(), 7);
        assert_eq!(
            backend.receive(Duration::from_secs(5), 64).unwrap(),
            Some(b"frame-1".to_vec())
        );
        backend.send(b"frame-2").unwrap();
        assert_eq!(
            backend.receive(Duration::from_secs(5), 5).unwrap(),
            Some(b"frame".to_vec())
        );
        assert_eq!(
            backend.receive(Duration::from_millis(20), 64).unwrap(),
            None
        );
        assert_eq!(server.sessions.load(Ordering::SeqCst), 1);
        assert_eq!(*server.identities.lock(), vec![b"client-identity".to_vec()]);
    }

    #[test]
    fn a_send_after_the_idle_limit_opens_a_new_session() {
        let runtime = runtime();
        let server = echo_server(&runtime, vec![1; 16]);
        let backend = backend(
            &runtime,
            server.addr,
            &"01".repeat(16),
            Some(50),
            Duration::from_secs(5),
        );
        backend.send(b"a").unwrap();
        backend.send(b"b").unwrap();
        assert_eq!(server.sessions.load(Ordering::SeqCst), 1);
        std::thread::sleep(Duration::from_millis(120));
        backend.send(b"c").unwrap();
        assert_eq!(server.sessions.load(Ordering::SeqCst), 2);
    }

    #[test]
    fn a_wrong_key_fails_every_send_without_keeping_a_session() {
        let runtime = runtime();
        let server = echo_server(&runtime, vec![1; 16]);
        let backend = backend(
            &runtime,
            server.addr,
            &"02".repeat(16),
            None,
            Duration::from_millis(1500),
        );
        assert!(backend.send(b"a").is_err());
        assert!(backend.session.lock().is_none());
        assert!(backend.send(b"a").is_err());
        assert_eq!(server.sessions.load(Ordering::SeqCst), 0);
    }
}
