use std::sync::Arc;

use anyhow::Context;
use rustls::pki_types::{CertificateDer, PrivateKeyDer, PrivatePkcs8KeyDer};
use rustls::ServerConfig;

pub fn install_ring() {
    let _ = rustls::crypto::ring::default_provider().install_default();
}

/// Ephemeral self-signed certificate. Clients do not pin it; the PIN exchange
/// is bound to this TLS session through an exporter instead.
pub fn generate_config() -> anyhow::Result<Arc<ServerConfig>> {
    install_ring();
    let certified = rcgen::generate_simple_self_signed(vec!["omarchy-connect".to_string()])
        .context("generate tls certificate")?;
    let cert_der = certified.cert.der().to_vec();
    let key = PrivateKeyDer::Pkcs8(PrivatePkcs8KeyDer::from(certified.key_pair.serialize_der()));
    let config = ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![CertificateDer::from(cert_der)], key)
        .context("tls server config")?;
    Ok(Arc::new(config))
}
