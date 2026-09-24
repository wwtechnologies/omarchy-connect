use std::sync::Arc;

use rustls::client::danger::{HandshakeSignatureValid, ServerCertVerified, ServerCertVerifier};
use rustls::crypto::{verify_tls12_signature, verify_tls13_signature};
use rustls::pki_types::{CertificateDer, ServerName, UnixTime};
use rustls::{DigitallySignedStruct, Error, SignatureScheme};
use sha2::{Digest, Sha256};

#[derive(Debug)]
pub struct PinVerifier {
    pin: [u8; 32],
}

impl PinVerifier {
    pub fn new(pin: [u8; 32]) -> Self {
        Self { pin }
    }
}

impl ServerCertVerifier for PinVerifier {
    fn verify_server_cert(
        &self,
        end_entity: &CertificateDer<'_>,
        _intermediates: &[CertificateDer<'_>],
        _server_name: &ServerName<'_>,
        _ocsp_response: &[u8],
        _now: UnixTime,
    ) -> Result<ServerCertVerified, Error> {
        let digest = Sha256::digest(end_entity.as_ref());
        if digest.as_slice() == self.pin {
            Ok(ServerCertVerified::assertion())
        } else {
            Err(Error::General("certificate pin mismatch".into()))
        }
    }

    fn verify_tls12_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let algs = signature_algorithms();
        verify_tls12_signature(message, cert, dss, &algs)
    }

    fn verify_tls13_signature(
        &self,
        message: &[u8],
        cert: &CertificateDer<'_>,
        dss: &DigitallySignedStruct,
    ) -> Result<HandshakeSignatureValid, Error> {
        let algs = signature_algorithms();
        verify_tls13_signature(message, cert, dss, &algs)
    }

    fn supported_verify_schemes(&self) -> Vec<SignatureScheme> {
        signature_algorithms().supported_schemes()
    }
}

fn signature_algorithms() -> rustls::crypto::WebPkiSupportedAlgorithms {
    rustls::crypto::ring::default_provider().signature_verification_algorithms
}

pub fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn client_config(pin: [u8; 32]) -> Arc<rustls::ClientConfig> {
    install_ring();
    let verifier = Arc::new(PinVerifier::new(pin));
    Arc::new(
        rustls::ClientConfig::builder()
            .dangerous()
            .with_custom_certificate_verifier(verifier)
            .with_no_client_auth(),
    )
}
