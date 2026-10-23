//! TLS configuration: mutual TLS 1.3, trusted by pinned thumbprint.
//!
//! The verifiers here replace chain building entirely. They still verify the handshake signature --
//! that is what proves the peer holds the private key for the certificate it presented -- but the
//! decision about *whether* to trust that certificate is made by the pin store alone.

use std::sync::{Arc, OnceLock};

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::CryptoProvider;
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::server::danger::{ClientCertVerified, ClientCertVerifier};
use rustls::{CertificateError, ClientConfig, CommonState, DigitallySignedStruct, DistinguishedName, ServerConfig};

use crate::identity::{thumbprint_of, NodeIdentity};
use crate::pins::PinStore;

/// The crypto provider every configuration uses.
pub fn provider() -> Arc<CryptoProvider> {
    static PROVIDER: OnceLock<Arc<CryptoProvider>> = OnceLock::new();
    Arc::clone(PROVIDER.get_or_init(|| Arc::new(rustls::crypto::ring::default_provider())))
}

/// The name a client asks for. Meaningless -- certificates are pinned, not matched by name -- but
/// TLS requires one.
pub fn server_name() -> ServerName<'static> {
    ServerName::try_from("VirtualKmSwitch").expect("a valid DNS name")
}

/// The thumbprint of the certificate the far end presented.
pub fn peer_thumbprint(state: &CommonState) -> Option<String> {
    state.peer_certificates()?.first().map(|certificate| thumbprint_of(certificate))
}

/// Trusts what the pin store trusts, or -- for pairing only -- anything at all.
#[derive(Debug)]
struct PeerVerifier {
    pins: Option<Arc<PinStore>>,
    provider: Arc<CryptoProvider>,
}

impl std::fmt::Debug for PinStore {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "PinStore({} pins)", self.count())
    }
}

impl PeerVerifier {
    fn check(&self, certificate: &CertificateDer<'_>) -> Result<(), rustls::Error> {
        match &self.pins {
            // Accept-any, which only the pairing window uses. There is no pin yet -- establishing
            // one is the entire point of that exchange -- so the certificate is recorded rather
            // than judged, and the proof computed over it decides everything.
            None => Ok(()),

            Some(pins) if pins.validate(certificate).is_some() => Ok(()),

            Some(_) => Err(rustls::Error::InvalidCertificate(CertificateError::ApplicationVerificationFailure)),
        }
    }

    fn verify_tls12(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls12_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn verify_tls13(
        &self,
        message: &[u8],
        certificate: &CertificateDer<'_>,
        signature: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        rustls::crypto::verify_tls13_signature(
            message,
            certificate,
            signature,
            &self.provider.signature_verification_algorithms,
        )
    }

    fn schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.provider.signature_verification_algorithms.supported_schemes()
    }
}

impl ServerCertVerifier for PeerVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes()
    }
}

impl ClientCertVerifier for PeerVerifier {
    /// Mutual authentication is required, not optional: an unauthenticated peer could otherwise
    /// open a connection and start sending keystrokes.
    fn client_auth_mandatory(&self) -> bool {
        true
    }

    fn root_hint_subjects(&self) -> &[DistinguishedName] {
        &[]
    }

    fn verify_client_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _now: UnixTime,
    ) -> Result<ClientCertVerified, rustls::Error> {
        self.check(end_entity).map(|()| ClientCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls12(message, cert, dss)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, rustls::Error> {
        self.verify_tls13(message, cert, dss)
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        self.schemes()
    }
}

fn verifier(pins: Option<Arc<PinStore>>) -> Arc<PeerVerifier> {
    Arc::new(PeerVerifier { pins, provider: provider() })
}

fn client_config(identity: &NodeIdentity, pins: Option<Arc<PinStore>>) -> Result<Arc<ClientConfig>, rustls::Error> {
    let config = ClientConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .dangerous()
        .with_custom_certificate_verifier(verifier(pins))
        .with_client_auth_cert(vec![identity.certificate()], identity.private_key())?;

    Ok(Arc::new(config))
}

fn server_config(identity: &NodeIdentity, pins: Option<Arc<PinStore>>) -> Result<Arc<ServerConfig>, rustls::Error> {
    let config = ServerConfig::builder_with_provider(provider())
        .with_protocol_versions(&[&rustls::version::TLS13])?
        .with_client_cert_verifier(verifier(pins))
        .with_single_cert(vec![identity.certificate()], identity.private_key())?;

    Ok(Arc::new(config))
}

/// Dialling a paired machine: trusts exactly the certificates in `pins`, consulted live.
pub fn pinned_client_config(identity: &NodeIdentity, pins: Arc<PinStore>) -> Result<Arc<ClientConfig>, rustls::Error> {
    client_config(identity, Some(pins))
}

/// Accepting a paired machine: requires a client certificate, and trusts exactly those in `pins`.
pub fn pinned_server_config(identity: &NodeIdentity, pins: Arc<PinStore>) -> Result<Arc<ServerConfig>, rustls::Error> {
    server_config(identity, Some(pins))
}

/// Pairing only: any certificate is accepted, and recorded for the proof to be computed over.
pub fn accept_any_client_config(identity: &NodeIdentity) -> Result<Arc<ClientConfig>, rustls::Error> {
    client_config(identity, None)
}

/// Pairing only: as [`accept_any_client_config`], for the side with the window open.
pub fn accept_any_server_config(identity: &NodeIdentity) -> Result<Arc<ServerConfig>, rustls::Error> {
    server_config(identity, None)
}
