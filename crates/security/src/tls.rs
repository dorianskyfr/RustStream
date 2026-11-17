//! rustls configurations for RustStream's mutually-authenticated QUIC.
//!
//! There is no certificate authority. TLS 1.3 still performs the full
//! handshake signature verification (proof of possession of the private key);
//! the certificate *chain* checks are replaced by fingerprint checks:
//!
//! * the client pins the host fingerprint ([`ServerPolicy::Pinned`]), or
//!   accepts any host during first-time pairing ([`ServerPolicy::Pairing`]),
//!   where the user then compares the pairing code;
//! * the host accepts any client certificate at the TLS layer and authorises
//!   the fingerprint against its trust store right after the handshake.

use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{CryptoProvider, verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, DigitallySignedStruct, DistinguishedName, SignatureScheme};

use crate::{DeviceIdentity, Fingerprint, Result};

/// SNI / certificate subject name used by every RustStream device.
pub const SERVER_NAME: &str = "ruststream.device";

/// The crypto provider used everywhere (ring, TLS 1.3 suites only).
pub fn crypto_provider() -> Arc<CryptoProvider> {
    Arc::new(rustls::crypto::ring::default_provider())
}

/// How the client authenticates the host.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ServerPolicy {
    /// The host must present exactly this certificate.
    Pinned(Fingerprint),
    /// First connection: accept any certificate. The session is not trusted
    /// until the user confirmed the pairing code.
    Pairing,
}

fn key_der(identity: &DeviceIdentity) -> PrivateKeyDer<'static> {
    PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(identity.key_pkcs8().to_vec()))
}

fn cert_der(identity: &DeviceIdentity) -> CertificateDer<'static> {
    CertificateDer::from(identity.certificate_der().to_vec())
}

/// TLS configuration of the host endpoint.
pub fn server_config(identity: &DeviceIdentity, alpn: &[u8]) -> Result<rustls::ServerConfig> {
    let provider = crypto_provider();
    let verifier = Arc::new(AnyClientCert {
        provider: provider.clone(),
    });
    let mut cfg = rustls::ServerConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier)
        .with_single_cert(vec![cert_der(identity)], key_der(identity))?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Ok(cfg)
}

/// TLS configuration of a client connection.
pub fn client_config(
    identity: &DeviceIdentity,
    policy: ServerPolicy,
    alpn: &[u8],
) -> Result<rustls::ClientConfig> {
    let provider = crypto_provider();
    let verifier = Arc::new(FingerprintServerVerifier {
        provider: provider.clone(),
        policy,
    });
    let mut cfg = rustls::ClientConfig::builder_with_provider(provider)
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier)
        .with_client_auth_cert(vec![cert_der(identity)], key_der(identity))?;
    cfg.alpn_protocols = vec![alpn.to_vec()];
    Ok(cfg)
}

/// Fingerprint of the end-entity certificate presented by the peer.
pub fn peer_fingerprint(certs: &[CertificateDer<'_>]) -> Option<Fingerprint> {
    certs.first().map(|c| Fingerprint::of_certificate(c))
}

#[derive(Debug)]
struct FingerprintServerVerifier {
    provider: Arc<CryptoProvider>,
    policy: ServerPolicy,
}

impl ServerCertVerifier for FingerprintServerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> std::result::Result<ServerCertVerified, rustls::Error> {
        match self.policy {
            ServerPolicy::Pairing => Ok(ServerCertVerified::assertion()),
            ServerPolicy::Pinned(expected) => {
                if Fingerprint::of_certificate(end_entity) == expected {
                    Ok(ServerCertVerified::assertion())
                } else {
                    tracing::warn!("host certificate does not match the pinned fingerprint");
                    Err(rustls::Error::InvalidCertificate(
                        CertificateError::ApplicationVerificationFailure,
                    ))
                }
            }
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

/// Requires a client certificate and verifies the handshake signature, but
/// leaves authorisation of the fingerprint to the application.
#[derive(Debug)]
struct AnyClientCert {
    provider: Arc<CryptoProvider>,
}

impl ClientCertVerifier for AnyClientCert {
    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        _end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> std::result::Result<ClientCertVerified, rustls::Error> {
        Ok(ClientCertVerified::assertion())
    }

    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls12_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> std::result::Result<HandshakeSignatureValid, rustls::Error> {
        verify_tls13_signature(
            message,
            cert,
            dss,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        self.provider
            .signature_verification_algorithms
            .supported_schemes()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn configs_build() {
        let host = DeviceIdentity::generate().expect("gen");
        let client = DeviceIdentity::generate().expect("gen");
        assert!(server_config(&host, b"test").is_ok());
        assert!(client_config(&client, ServerPolicy::Pairing, b"test").is_ok());
        assert!(client_config(&client, ServerPolicy::Pinned(host.fingerprint()), b"test").is_ok());
    }

    #[test]
    fn pinned_verifier_rejects_other_certificates() {
        let host = DeviceIdentity::generate().expect("gen");
        let other = DeviceIdentity::generate().expect("gen");
        let v = FingerprintServerVerifier {
            provider: crypto_provider(),
            policy: ServerPolicy::Pinned(host.fingerprint()),
        };
        let name = ServerName::try_from(SERVER_NAME).expect("name");
        let now = UnixTime::now();
        assert!(
            v.verify_server_cert(&cert_der(&host), &[], &name, &[], now)
                .is_ok()
        );
        assert!(
            v.verify_server_cert(&cert_der(&other), &[], &name, &[], now)
                .is_err()
        );
    }
}
