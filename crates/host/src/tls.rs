use std::sync::Arc;

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;
use sha2::{Digest, Sha256};

pub struct Identity {
    pub config: Arc<ServerConfig>,
    pub pin_hex: String,
}

pub fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

pub fn generate_identity() -> anyhow::Result<Identity> {
    install_ring();
    let certified = rcgen::generate_simple_self_signed(vec!["omarchy-connect".to_string()])
        .context("generate tls certificate")?;
    let cert_der = certified.cert.der().to_vec();
    let pin = Sha256::digest(&cert_der);
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der)], key)
        .context("tls server config")?;
    Ok(Identity {
        config: Arc::new(config),
        pin_hex: hex::encode(pin),
    })
}
