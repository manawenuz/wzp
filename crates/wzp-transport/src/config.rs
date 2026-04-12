//! QUIC configuration tuned for lossy VoIP links.

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;

/// Create a server configuration with a self-signed certificate (random keypair).
///
/// The certificate changes on every call. Use `server_config_from_seed` for
/// a deterministic certificate that survives relay restarts.
pub fn server_config() -> (quinn::ServerConfig, Vec<u8>) {
    let cert_key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("failed to generate self-signed cert");
    let cert_der = rustls::pki_types::CertificateDer::from(cert_key.cert);
    let key_der =
        rustls::pki_types::PrivateKeyDer::try_from(cert_key.key_pair.serialize_der()).unwrap();
    build_server_config(cert_der, key_der)
}

/// Create a server configuration with a deterministic self-signed certificate
/// derived from a 32-byte seed. Same seed = same cert = same TLS fingerprint.
pub fn server_config_from_seed(seed: &[u8; 32]) -> (quinn::ServerConfig, Vec<u8>) {
    use ed25519_dalek::pkcs8::EncodePrivateKey;
    use ed25519_dalek::SigningKey;
    use hkdf::Hkdf;
    use sha2::Sha256;

    // Derive Ed25519 key bytes from seed via HKDF
    let hk = Hkdf::<Sha256>::new(None, seed);
    let mut ed_bytes = [0u8; 32];
    hk.expand(b"wzp-tls-ed25519", &mut ed_bytes)
        .expect("HKDF expand failed");

    // Create Ed25519 signing key and export as PKCS8 DER
    let signing_key = SigningKey::from_bytes(&ed_bytes);
    let pkcs8_doc = signing_key.to_pkcs8_der()
        .expect("failed to encode Ed25519 key as PKCS8");
    let key_der_for_rcgen = rustls::pki_types::PrivateKeyDer::try_from(pkcs8_doc.as_bytes().to_vec())
        .expect("failed to wrap PKCS8 DER");

    // Create rcgen KeyPair from DER
    let key_pair = rcgen::KeyPair::from_der_and_sign_algo(
        &key_der_for_rcgen,
        &rcgen::PKCS_ED25519,
    )
    .expect("failed to create KeyPair from seed-derived Ed25519 key");

    // Build self-signed cert with this deterministic keypair
    let params = rcgen::CertificateParams::new(vec!["localhost".to_string()])
        .expect("failed to create CertificateParams");
    let cert = params.self_signed(&key_pair).expect("failed to self-sign cert");
    let cert_der = rustls::pki_types::CertificateDer::from(cert.der().to_vec());
    let key_der = rustls::pki_types::PrivateKeyDer::try_from(key_pair.serialize_der())
        .expect("failed to serialize key DER");

    build_server_config(cert_der, key_der)
}

/// Compute a hex-formatted SHA-256 fingerprint of a DER-encoded certificate.
///
/// Format: `xx:xx:xx:xx:...` (32 bytes = 64 hex chars with colons).
pub fn tls_fingerprint(cert_der: &[u8]) -> String {
    use sha2::{Sha256, Digest};
    let hash = Sha256::digest(cert_der);
    hash.iter()
        .map(|b| format!("{b:02x}"))
        .collect::<Vec<_>>()
        .join(":")
}

fn build_server_config(
    cert_der: rustls::pki_types::CertificateDer<'static>,
    key_der: rustls::pki_types::PrivateKeyDer<'static>,
) -> (quinn::ServerConfig, Vec<u8>) {
    let mut server_crypto = rustls::ServerConfig::builder()
        .with_no_client_auth()
        .with_single_cert(vec![cert_der.clone()], key_der)
        .expect("bad server cert/key");
    server_crypto.alpn_protocols = vec![b"wzp".to_vec()];

    let quic_server_config =
        QuicServerConfig::try_from(server_crypto).expect("failed to create QuicServerConfig");

    let mut server_config = quinn::ServerConfig::with_crypto(Arc::new(quic_server_config));
    let transport = transport_config();
    server_config.transport_config(Arc::new(transport));

    (server_config, cert_der.to_vec())
}

/// Create a client configuration that trusts any certificate (for testing).
///
/// Uses the same VoIP-tuned transport parameters as the server.
pub fn client_config() -> quinn::ClientConfig {
    let mut client_crypto = rustls::ClientConfig::builder()
        .dangerous()
        .with_custom_certificate_verifier(Arc::new(SkipServerVerification))
        .with_no_client_auth();
    client_crypto.alpn_protocols = vec![b"wzp".to_vec()];

    let quic_client_config =
        QuicClientConfig::try_from(client_crypto).expect("failed to create QuicClientConfig");

    let mut client_config = quinn::ClientConfig::new(Arc::new(quic_client_config));
    let transport = transport_config();
    client_config.transport_config(Arc::new(transport));

    client_config
}

/// Shared transport configuration tuned for lossy VoIP.
fn transport_config() -> quinn::TransportConfig {
    let mut config = quinn::TransportConfig::default();

    // 30 second idle timeout
    config.max_idle_timeout(Some(
        quinn::IdleTimeout::try_from(Duration::from_secs(30)).unwrap(),
    ));

    // 5 second keep-alive interval
    config.keep_alive_interval(Some(Duration::from_secs(5)));

    // Enable DATAGRAM extension for unreliable media packets.
    config.datagram_receive_buffer_size(Some(65536));

    // Conservative flow control for bandwidth-constrained links
    config.receive_window(quinn::VarInt::from_u32(256 * 1024)); // 256KB
    config.send_window(128 * 1024); // 128KB
    config.stream_receive_window(quinn::VarInt::from_u32(64 * 1024)); // 64KB per stream

    // Aggressive initial RTT estimate for high-latency links
    config.initial_rtt(Duration::from_millis(300));

    // PMTUD (Path MTU Discovery) — quinn 0.11 enables this by default but
    // with conservative bounds (initial 1200, upper 1452). We keep the safe
    // initial_mtu of 1200 so the first packets always get through, but raise
    // upper_bound so the binary search can discover larger MTUs on paths that
    // support them. Typical results:
    //   - Ethernet/fiber: discovers ~1452 (Ethernet MTU minus IP/UDP/QUIC)
    //   - WireGuard/VPN: discovers ~1380-1420
    //   - Starlink: discovers ~1400-1452
    //   - Cellular: stays at 1200-1300
    // Black hole detection automatically falls back to 1200 if probes fail.
    // This matters for future video frames which can be 1-50 KB and benefit
    // from fewer application-layer fragments per frame.
    let mut mtu_config = quinn::MtuDiscoveryConfig::default();
    mtu_config
        .upper_bound(1452)
        .interval(Duration::from_secs(300))       // re-probe every 5 min
        .black_hole_cooldown(Duration::from_secs(30)); // retry faster on lossy links
    config.mtu_discovery_config(Some(mtu_config));
    config.initial_mtu(1200); // safe starting point

    config
}

/// Certificate verifier that accepts any server certificate (testing only).
#[derive(Debug)]
struct SkipServerVerification;

impl rustls::client::danger::ServerCertVerifier for SkipServerVerification {
    fn verify_server_cert(
        &self,
        _end_entity: &rustls::pki_types::CertificateDer<'_>,
        _intermediates: &[rustls::pki_types::CertificateDer<'_>],
        _server_name: &rustls::pki_types::ServerName<'_>,
        _ocsp_response: &[u8],
        _now: rustls::pki_types::UnixTime,
    ) -> Result<rustls::client::danger::ServerCertVerified, rustls::Error> {
        Ok(rustls::client::danger::ServerCertVerified::assertion())
    }

    fn verify_tls12_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn verify_tls13_signature(
        &self,
        _message: &[u8],
        _cert: &rustls::pki_types::CertificateDer<'_>,
        _dss: &rustls::DigitallySignedStruct,
    ) -> Result<rustls::client::danger::HandshakeSignatureValid, rustls::Error> {
        Ok(rustls::client::danger::HandshakeSignatureValid::assertion())
    }

    fn supported_verify_schemes(&self) -> Vec<rustls::SignatureScheme> {
        // Support the schemes that rustls typically uses
        vec![
            rustls::SignatureScheme::ECDSA_NISTP256_SHA256,
            rustls::SignatureScheme::ECDSA_NISTP384_SHA384,
            rustls::SignatureScheme::ED25519,
            rustls::SignatureScheme::RSA_PSS_SHA256,
            rustls::SignatureScheme::RSA_PSS_SHA384,
            rustls::SignatureScheme::RSA_PSS_SHA512,
            rustls::SignatureScheme::RSA_PKCS1_SHA256,
            rustls::SignatureScheme::RSA_PKCS1_SHA384,
            rustls::SignatureScheme::RSA_PKCS1_SHA512,
        ]
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn server_config_creates_without_error() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let (cfg, cert_der) = server_config();
        assert!(!cert_der.is_empty());
        // Verify the config was created (no panic)
        drop(cfg);
    }

    #[test]
    fn client_config_creates_without_error() {
        let _ = rustls::crypto::ring::default_provider().install_default();
        let cfg = client_config();
        drop(cfg);
    }
}
