// SPDX-License-Identifier: GPL-3.0-or-later
//! Scoped HTTP client backing the `halod.http` plugin capability. Every request
//! is checked against the plugin's declared [`HttpPolicy`] (origin allowlist,
//! method, size and timeout caps) *before* a socket opens, and DNS resolution is
//! funnelled through the shared SSRF [`net_guard`] so a rebind can't redirect the
//! connection off its vetted address. Redirects are disabled — a 3xx is returned
//! as-is rather than silently followed off-origin.
//!
//! [`net_guard`]: crate::plugin::runtime::backends::net_guard

use std::collections::BTreeMap;
use std::io::Read;
use std::net::SocketAddr;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::Arc;
use std::time::Duration;

use anyhow::{bail, Context, Result};
use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error as RustlsError, SignatureScheme};

use crate::plugin::manifest::HttpConfig;
use crate::plugin::runtime::backends::net_guard;

/// Ceiling on distinct response headers surfaced to Lua, so a hostile server
/// can't exhaust memory through header count.
const MAX_RESPONSE_HEADERS: usize = 100;
/// Request metadata is bounded independently from the body. In particular,
/// plugins may not use framing or authority headers to change the request that
/// was admitted by [`HttpPolicy`].
const MAX_REQUEST_HEADERS: usize = 100;
const MAX_REQUEST_HEADER_BYTES: usize = 64 * 1024;
const FORBIDDEN_REQUEST_HEADERS: &[&str] = &[
    "connection",
    "content-length",
    "host",
    "keep-alive",
    "proxy-authorization",
    "proxy-connection",
    "te",
    "trailer",
    "transfer-encoding",
    "upgrade",
];

/// One bounded request a plugin asks the host to make.
#[derive(Debug, Clone)]
pub struct HttpRequest {
    pub method: String,
    pub origin: String,
    pub path: String,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
    pub timeout: Duration,
}

/// The host's response, already truncated to the plugin's declared caps.
#[derive(Debug, Clone)]
pub struct HttpResponse {
    pub status: u16,
    pub headers: Vec<(String, String)>,
    pub body: Vec<u8>,
}

/// The declared authority a plugin's `halod.http` is scoped to.
#[derive(Debug, Clone)]
pub struct HttpPolicy {
    origins: Vec<String>,
    methods: Vec<String>,
    max_request_bytes: usize,
    max_response_bytes: usize,
    max_timeout: Duration,
    allow_private: bool,
    tls_profile: String,
    tls_ca_der_base64: Option<String>,
    tls_identity: Option<String>,
    tls_connect_host: Option<String>,
    tls_certificate_identity: String,
}

impl HttpPolicy {
    /// Build the effective policy, resolving any `{host}` origin placeholder from
    /// the plugin's configured device address. A placeholder origin with no host
    /// configured is dropped, leaving an empty allowlist that rejects every
    /// request — safer than reaching an unintended address.
    pub fn from_config(config: &HttpConfig, host: Option<&str>, identity: Option<&str>) -> Self {
        let host = host.map(str::trim).filter(|h| !h.is_empty());
        let origins = config
            .origins
            .iter()
            .filter_map(|origin| {
                if origin.contains("{host}") {
                    host.map(|h| origin.replace("{host}", h))
                } else {
                    Some(origin.clone())
                }
            })
            .collect();
        Self {
            origins,
            methods: config.methods.clone(),
            max_request_bytes: config.max_request_bytes,
            max_response_bytes: config.max_response_bytes,
            max_timeout: Duration::from_millis(config.max_timeout_ms),
            allow_private: config.allow_private,
            tls_profile: config
                .tls
                .as_ref()
                .map(|tls| tls.profile.clone())
                .unwrap_or_else(|| "default".to_owned()),
            tls_ca_der_base64: config
                .tls
                .as_ref()
                .and_then(|tls| tls.ca_der_base64.clone()),
            tls_identity: identity.map(str::to_owned),
            tls_connect_host: host.map(str::to_owned),
            tls_certificate_identity: config
                .tls
                .as_ref()
                .map(|tls| tls.certificate_identity.clone())
                .unwrap_or_else(|| "webpki".to_owned()),
        }
    }

    /// Reject anything outside the declared scope before a request runs. Returns
    /// the request with its timeout clamped to the declared ceiling.
    pub fn admit(&self, mut req: HttpRequest) -> Result<HttpRequest> {
        if !self.origins.iter().any(|o| o == &req.origin) {
            bail!(
                "http origin '{}' is not in the declared allowlist",
                req.origin
            );
        }
        if !self.methods.iter().any(|m| m == &req.method) {
            bail!("http method '{}' is not declared", req.method);
        }
        if req.body.len() > self.max_request_bytes {
            bail!(
                "http request body {} exceeds the declared max_request_bytes {}",
                req.body.len(),
                self.max_request_bytes
            );
        }
        validate_request_headers(&req.headers)?;
        if req.timeout.is_zero() || req.timeout > self.max_timeout {
            req.timeout = self.max_timeout;
        }
        if self.tls_profile == "custom-ca" {
            let identity = self
                .tls_identity
                .as_deref()
                .ok_or_else(|| anyhow::anyhow!("custom TLS identity is not configured"))?;
            req.origin = format!("https://{identity}");
        }
        Ok(req)
    }

    pub fn max_response_bytes(&self) -> usize {
        self.max_response_bytes
    }
    pub fn allow_private(&self) -> bool {
        self.allow_private
    }
    pub fn tls_profile(&self) -> &str {
        &self.tls_profile
    }
}

fn validate_request_headers(headers: &[(String, String)]) -> Result<()> {
    if headers.len() > MAX_REQUEST_HEADERS {
        bail!("http request has too many headers (maximum {MAX_REQUEST_HEADERS})");
    }
    let mut total_bytes = 0usize;
    for (name, value) in headers {
        if name.is_empty() || !name.bytes().all(is_http_token_byte) {
            bail!("http request contains an invalid header name");
        }
        let lower = name.to_ascii_lowercase();
        if FORBIDDEN_REQUEST_HEADERS.contains(&lower.as_str()) {
            bail!("http request header '{name}' is controlled by the host");
        }
        // ureq 2.x only serializes horizontal tab and visible ASCII in values.
        // Validate here instead of allowing malformed values to be silently
        // dropped, and explicitly exclude CR/LF request injection.
        if !value
            .bytes()
            .all(|byte| byte == b'\t' || (0x20..=0x7e).contains(&byte))
        {
            bail!("http request header '{name}' has an invalid value");
        }
        total_bytes = total_bytes
            .checked_add(name.len())
            .and_then(|n| n.checked_add(value.len()))
            .ok_or_else(|| anyhow::anyhow!("http request headers exceed the size limit"))?;
        if total_bytes > MAX_REQUEST_HEADER_BYTES {
            bail!("http request headers exceed the {MAX_REQUEST_HEADER_BYTES}-byte size limit");
        }
    }
    Ok(())
}

fn is_http_token_byte(byte: u8) -> bool {
    byte.is_ascii_alphanumeric()
        || matches!(
            byte,
            b'!' | b'#'
                | b'$'
                | b'%'
                | b'&'
                | b'\''
                | b'*'
                | b'+'
                | b'-'
                | b'.'
                | b'^'
                | b'_'
                | b'`'
                | b'|'
                | b'~'
        )
}

/// Performs the actual request. The live implementation is [`UreqBackend`]; the
/// plugin-test harness swaps in a recording backend so `test.lua` can assert
/// requests and inject responses without touching the network.
pub trait HttpBackend: Send + Sync {
    fn request(&self, req: &HttpRequest, max_response_bytes: usize) -> Result<HttpResponse>;
}

/// Resolves every hostname through the SSRF guard, returning the single vetted
/// address so the connection can't be rebound off it between check and connect.
struct NetGuardResolver {
    allow_private: bool,
    host_override: Option<(String, String)>,
}

impl ureq::Resolver for NetGuardResolver {
    fn resolve(&self, netloc: &str) -> std::io::Result<Vec<SocketAddr>> {
        let (host, port) = netloc.rsplit_once(':').ok_or_else(|| {
            std::io::Error::new(std::io::ErrorKind::InvalidInput, "missing port in netloc")
        })?;
        let port: u16 = port
            .parse()
            .map_err(|_| std::io::Error::new(std::io::ErrorKind::InvalidInput, "invalid port"))?;
        let host = self
            .host_override
            .as_ref()
            .filter(|(identity, _)| identity.eq_ignore_ascii_case(host))
            .map(|(_, connect_host)| connect_host.as_str())
            .unwrap_or(host);
        let addr = net_guard::resolve_vetted_addr(host, port, self.allow_private).map_err(|e| {
            std::io::Error::new(std::io::ErrorKind::PermissionDenied, e.to_string())
        })?;
        Ok(vec![addr])
    }
}

/// Live HTTP over a `ureq` agent: redirects off, SSRF-vetted resolution, TLS
/// always verified against the host trust profile.
pub struct UreqBackend {
    agent: ureq::Agent,
}

impl UreqBackend {
    pub fn new(policy: &HttpPolicy) -> Result<Self> {
        // `default` = standard public-CA (webpki) verification, which ureq's tls
        // feature already enforces. Keep a defensive runtime rejection in case a
        // policy is constructed without going through manifest validation.
        let mut builder = ureq::AgentBuilder::new()
            .redirects(0)
            .timeout(policy.max_timeout)
            .resolver(NetGuardResolver {
                allow_private: policy.allow_private(),
                host_override: policy
                    .tls_identity
                    .clone()
                    .zip(policy.tls_connect_host.clone()),
            });
        match policy.tls_profile() {
            "default" => {}
            "custom-ca" => {
                let ca = policy
                    .tls_ca_der_base64
                    .as_deref()
                    .context("custom TLS root CA is not configured")?;
                builder = builder.tls_config(Arc::new(custom_ca_tls_config(
                    ca,
                    &policy.tls_certificate_identity,
                )?));
            }
            other => bail!("http tls profile '{other}' has no live client"),
        }
        let agent = builder.build();
        Ok(Self { agent })
    }
}

#[derive(Debug)]
struct SubjectCnCertVerifier {
    webpki: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for SubjectCnCertVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, RustlsError> {
        match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(error)
                if is_certificate_name_error(&error)
                    && subject_cn_matches(end_entity, server_name) =>
            {
                // WebPKI has already checked the configured CA chain, validity
                // period and signatures. This mode replaces only its SAN
                // identity representation with one exact Subject CN.
                Ok(ServerCertVerified::assertion())
            }
            Err(error) => Err(error),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        self.webpki.verify_tls12_signature(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, RustlsError> {
        self.webpki.verify_tls13_signature(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

fn is_certificate_name_error(error: &RustlsError) -> bool {
    matches!(
        error,
        RustlsError::InvalidCertificate(
            CertificateError::NotValidForName | CertificateError::NotValidForNameContext { .. }
        )
    )
}

fn subject_cn_matches(cert_der: &CertificateDer<'_>, server_name: &ServerName<'_>) -> bool {
    let ServerName::DnsName(expected) = server_name else {
        return false;
    };
    let Ok((remaining, cert)) = x509_parser::parse_x509_certificate(cert_der.as_ref()) else {
        return false;
    };
    if !remaining.is_empty() {
        return false;
    }
    let mut common_names = cert.subject().iter_common_name();
    let Some(common_name) = common_names.next() else {
        return false;
    };
    common_names.next().is_none()
        && common_name
            .as_str()
            .is_ok_and(|name| name.eq_ignore_ascii_case(expected.as_ref()))
}

fn custom_ca_tls_config(
    root_der_base64: &str,
    certificate_identity: &str,
) -> Result<rustls::ClientConfig> {
    let der = base64::engine::general_purpose::STANDARD
        .decode(root_der_base64)
        .context("decoding custom TLS root CA")?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(rustls::pki_types::CertificateDer::from(der))
        .context("loading custom TLS root CA")?;
    let provider: Arc<rustls::crypto::CryptoProvider> =
        rustls::crypto::ring::default_provider().into();
    let builder = rustls::ClientConfig::builder_with_provider(Arc::clone(&provider))
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])?;
    match certificate_identity {
        "webpki" => Ok(builder.with_root_certificates(roots).with_no_client_auth()),
        "subject-cn" => {
            let webpki =
                WebPkiServerVerifier::builder_with_provider(Arc::new(roots), provider).build()?;
            Ok(builder
                .dangerous()
                .with_custom_certificate_verifier(Arc::new(SubjectCnCertVerifier { webpki }))
                .with_no_client_auth())
        }
        other => bail!("custom TLS certificate identity '{other}' is not implemented"),
    }
}

impl HttpBackend for UreqBackend {
    fn request(&self, req: &HttpRequest, max_response_bytes: usize) -> Result<HttpResponse> {
        let url = format!("{}{}", req.origin, req.path);
        let mut builder = self.agent.request(&req.method, &url).timeout(req.timeout);
        for (name, value) in &req.headers {
            builder = builder.set(name, value);
        }
        let result = if req.body.is_empty() {
            builder.call()
        } else {
            builder.send_bytes(&req.body)
        };
        let response = match result {
            Ok(response) => response,
            // ureq surfaces a non-2xx status as an error; that is still a valid
            // HTTP response the plugin should observe, not a transport failure.
            Err(ureq::Error::Status(_, response)) => response,
            Err(ureq::Error::Transport(t)) => {
                return Err(anyhow::anyhow!("http request failed: {t}"));
            }
        };
        read_response(response, max_response_bytes)
    }
}

fn read_response(response: ureq::Response, max_response_bytes: usize) -> Result<HttpResponse> {
    let status = response.status();
    // Dedup + bound the header set before handing it to Lua.
    let mut headers: BTreeMap<String, String> = BTreeMap::new();
    for name in response
        .headers_names()
        .into_iter()
        .take(MAX_RESPONSE_HEADERS)
    {
        if let Some(value) = response.header(&name) {
            headers.insert(name, value.to_owned());
        }
    }
    let mut body = Vec::new();
    response
        .into_reader()
        .take(max_response_bytes as u64 + 1)
        .read_to_end(&mut body)
        .context("reading http response body")?;
    if body.len() > max_response_bytes {
        bail!("http response exceeds the declared max_response_bytes {max_response_bytes}");
    }
    Ok(HttpResponse {
        status,
        headers: headers.into_iter().collect(),
        body,
    })
}

/// A plugin's ready-to-use HTTP capability: its policy plus the backend that
/// runs requests, shared with the Lua worker via [`crate::plugin::runtime::http_api`].
#[derive(Clone)]
pub struct HttpRuntime {
    policy: Arc<HttpPolicy>,
    backend: Arc<dyn HttpBackend>,
    inflight: Arc<AtomicUsize>,
    max_concurrency: usize,
}

impl HttpRuntime {
    pub fn new(policy: HttpPolicy, backend: Arc<dyn HttpBackend>, max_concurrency: usize) -> Self {
        Self {
            policy: Arc::new(policy),
            backend,
            inflight: Arc::new(AtomicUsize::new(0)),
            max_concurrency: max_concurrency.max(1),
        }
    }

    /// Build the live runtime from a manifest's declared http transport, resolving
    /// a `{host}` origin from the plugin's configured device address (`host_key`).
    pub fn from_config(
        config: &HttpConfig,
        host: Option<&str>,
        identity: Option<&str>,
    ) -> Result<Self> {
        let policy = HttpPolicy::from_config(config, host, identity);
        let backend = Arc::new(UreqBackend::new(&policy)?);
        Ok(Self::new(policy, backend, config.max_concurrency))
    }

    pub fn request(&self, req: HttpRequest) -> Result<HttpResponse> {
        let req = self.policy.admit(req)?;
        // Non-blocking concurrency cap: a worker is single-threaded, so this only
        // bites when one plugin drives several device workers at once.
        let prev = self.inflight.fetch_add(1, Ordering::SeqCst);
        let _guard = InflightGuard(&self.inflight);
        if prev >= self.max_concurrency {
            bail!("http concurrency limit ({}) reached", self.max_concurrency);
        }
        self.backend.request(&req, self.policy.max_response_bytes())
    }
}

struct InflightGuard<'a>(&'a AtomicUsize);
impl Drop for InflightGuard<'_> {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::SeqCst);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn policy() -> HttpPolicy {
        HttpPolicy {
            origins: vec!["https://api.example.com".into()],
            methods: vec!["POST".into()],
            max_request_bytes: 1024,
            max_response_bytes: 1024,
            max_timeout: Duration::from_secs(5),
            allow_private: false,
            tls_profile: "default".into(),
            tls_ca_der_base64: None,
            tls_identity: None,
            tls_connect_host: None,
            tls_certificate_identity: "webpki".into(),
        }
    }

    fn request(headers: Vec<(String, String)>) -> HttpRequest {
        HttpRequest {
            method: "POST".into(),
            origin: "https://api.example.com".into(),
            path: "/v1".into(),
            headers,
            body: Vec::new(),
            timeout: Duration::from_secs(1),
        }
    }

    #[test]
    fn request_headers_reject_injection_and_host_controlled_fields() {
        for headers in [
            vec![("X-Test\r\nHost".into(), "internal".into())],
            vec![("X-Test".into(), "ok\r\nHost: internal".into())],
            vec![("Host".into(), "internal".into())],
            vec![("Content-Length".into(), "0".into())],
            vec![("Transfer-Encoding".into(), "chunked".into())],
        ] {
            assert!(policy().admit(request(headers)).is_err());
        }
    }

    #[test]
    fn request_headers_accept_normal_metadata() {
        let admitted = policy().admit(request(vec![
            ("Content-Type".into(), "application/json".into()),
            ("Authorization".into(), "Bearer secret".into()),
        ]));
        assert!(admitted.is_ok());
    }

    #[test]
    fn request_timeout_is_preserved_or_clamped_to_policy() {
        let admitted = policy().admit(request(Vec::new())).unwrap();
        assert_eq!(admitted.timeout, Duration::from_secs(1));

        let mut long = request(Vec::new());
        long.timeout = Duration::from_secs(30);
        let admitted = policy().admit(long).unwrap();
        assert_eq!(admitted.timeout, Duration::from_secs(5));
    }
}
