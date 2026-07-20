// SPDX-License-Identifier: GPL-3.0-or-later

use std::io::{Read, Write};
use std::sync::Arc;

use anyhow::{Context, Result};
use base64::Engine as _;
use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::client::WebPkiServerVerifier;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{CertificateError, DigitallySignedStruct, Error, SignatureScheme};
use ureq::unversioned::transport::Buffers as _;

use super::NetGuardResolver;

pub(super) fn agent(
    config: ureq::config::Config,
    resolver: NetGuardResolver,
    root_der_base64: &str,
) -> Result<ureq::Agent> {
    use ureq::unversioned::transport::{Connector as _, TcpConnector};

    let connector = ().chain(TcpConnector::default()).chain(SubjectCnConnector {
        config: Arc::new(tls_config(root_der_base64)?),
    });
    Ok(ureq::Agent::with_parts(config, connector, resolver))
}

#[derive(Debug)]
struct SubjectCnVerifier {
    webpki: Arc<WebPkiServerVerifier>,
}

impl ServerCertVerifier for SubjectCnVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        intermediates: &[CertificateDer<'_>],
        server_name: &ServerName<'_>,
        ocsp_response: &[u8],
        now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, Error> {
        match self.webpki.verify_server_cert(
            end_entity,
            intermediates,
            server_name,
            ocsp_response,
            now,
        ) {
            Ok(verified) => Ok(verified),
            Err(error) if is_name_error(&error) && subject_cn_matches(end_entity, server_name) => {
                Ok(ServerCertVerified::assertion())
            }
            Err(error) => Err(error),
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        self.webpki.verify_tls12_signature(message, cert, signature)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, Error> {
        self.webpki.verify_tls13_signature(message, cert, signature)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.webpki.supported_verify_schemes()
    }
}

fn is_name_error(error: &Error) -> bool {
    matches!(
        error,
        Error::InvalidCertificate(
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

fn tls_config(root_der_base64: &str) -> Result<rustls::ClientConfig> {
    let der = base64::engine::general_purpose::STANDARD
        .decode(root_der_base64)
        .context("decoding custom TLS root CA")?;
    let mut roots = rustls::RootCertStore::empty();
    roots
        .add(CertificateDer::from(der))
        .context("loading custom TLS root CA")?;
    let provider: Arc<rustls::crypto::CryptoProvider> =
        rustls::crypto::ring::default_provider().into();
    let webpki =
        WebPkiServerVerifier::builder_with_provider(Arc::new(roots), Arc::clone(&provider))
            .build()?;
    Ok(rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS12, &rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SubjectCnVerifier { webpki }))
        .with_no_client_auth())
}

#[derive(Debug)]
struct SubjectCnConnector {
    config: Arc<rustls::ClientConfig>,
}

impl<In> ureq::unversioned::transport::Connector<In> for SubjectCnConnector
where
    In: ureq::unversioned::transport::Transport,
{
    type Out = ureq::unversioned::transport::Either<In, SubjectCnTransport<In>>;

    fn connect(
        &self,
        details: &ureq::unversioned::transport::ConnectionDetails<'_>,
        chained: Option<In>,
    ) -> std::result::Result<Option<Self::Out>, ureq::Error> {
        let transport = chained.expect("SubjectCnConnector requires a TCP transport");
        if !details.needs_tls() || transport.is_tls() {
            return Ok(Some(ureq::unversioned::transport::Either::A(transport)));
        }

        let server_name: ServerName<'_> = details
            .uri
            .authority()
            .expect("HTTPS URI has an authority")
            .host()
            .try_into()
            .map_err(|_| ureq::Error::Tls("invalid DNS name for rustls"))?;
        let connection =
            rustls::ClientConnection::new(Arc::clone(&self.config), server_name.to_owned())?;
        let stream = rustls::StreamOwned {
            conn: connection,
            sock: ureq::unversioned::transport::TransportAdapter::new(transport),
        };
        let buffers = ureq::unversioned::transport::LazyBuffers::new(
            details.config.input_buffer_size(),
            details.config.output_buffer_size(),
        );
        Ok(Some(ureq::unversioned::transport::Either::B(
            SubjectCnTransport { buffers, stream },
        )))
    }
}

struct SubjectCnTransport<In: ureq::unversioned::transport::Transport> {
    buffers: ureq::unversioned::transport::LazyBuffers,
    stream: rustls::StreamOwned<
        rustls::ClientConnection,
        ureq::unversioned::transport::TransportAdapter<In>,
    >,
}

impl<In> std::fmt::Debug for SubjectCnTransport<In>
where
    In: ureq::unversioned::transport::Transport,
{
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        formatter.debug_struct("SubjectCnTransport").finish()
    }
}

impl<In> ureq::unversioned::transport::Transport for SubjectCnTransport<In>
where
    In: ureq::unversioned::transport::Transport,
{
    fn buffers(&mut self) -> &mut dyn ureq::unversioned::transport::Buffers {
        &mut self.buffers
    }

    fn transmit_output(
        &mut self,
        amount: usize,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> std::result::Result<(), ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        self.stream.write_all(&self.buffers.output()[..amount])?;
        Ok(())
    }

    fn await_input(
        &mut self,
        timeout: ureq::unversioned::transport::NextTimeout,
    ) -> std::result::Result<bool, ureq::Error> {
        self.stream.get_mut().set_timeout(timeout);
        let input = self.buffers.input_append_buf();
        let amount = self.stream.read(input)?;
        self.buffers.input_appended(amount);
        Ok(amount > 0)
    }

    fn is_open(&mut self) -> bool {
        self.stream.get_mut().get_mut().is_open()
    }

    fn is_tls(&self) -> bool {
        true
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn only_certificate_name_errors_enable_the_fallback() {
        assert!(is_name_error(&Error::InvalidCertificate(
            CertificateError::NotValidForName
        )));
        assert!(!is_name_error(&Error::InvalidCertificate(
            CertificateError::Expired
        )));
        assert!(!is_name_error(&Error::General("failure".into())));
    }

    #[test]
    fn malformed_certificates_never_match() {
        let server_name = ServerName::try_from("bridge.example").unwrap();
        assert!(!subject_cn_matches(
            &CertificateDer::from(vec![0, 1, 2, 3]),
            &server_name
        ));
    }
}
