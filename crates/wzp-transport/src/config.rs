//! QUIC configuration tuned for lossy VoIP links.

use std::sync::Arc;
use std::time::Duration;

use quinn::crypto::rustls::QuicClientConfig;
use quinn::crypto::rustls::QuicServerConfig;

/// Create a server configuration with a self-signed certificate (for testing).
///
/// Tunes QUIC transport parameters for lossy VoIP:
/// - 30s idle timeout
/// - 5s keep-alive interval
/// - DATAGRAM extension enabled
/// - Conservative flow control for bandwidth-constrained links
pub fn server_config() -> (quinn::ServerConfig, Vec<u8>) {
    let cert_key = rcgen::generate_simple_self_signed(vec!["localhost".to_string()])
        .expect("failed to generate self-signed cert");
    let cert_der = rustls::pki_types::CertificateDer::from(cert_key.cert);
    let key_der =
        rustls::pki_types::PrivateKeyDer::try_from(cert_key.key_pair.serialize_der()).unwrap();

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
    // Allow datagrams up to 1200 bytes (conservative for lossy links).
    config.datagram_receive_buffer_size(Some(65536));

    // Conservative flow control for bandwidth-constrained links
    config.receive_window(quinn::VarInt::from_u32(256 * 1024)); // 256KB
    config.send_window(128 * 1024); // 128KB
    config.stream_receive_window(quinn::VarInt::from_u32(64 * 1024)); // 64KB per stream

    // Aggressive initial RTT estimate for high-latency links
    config.initial_rtt(Duration::from_millis(300));

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
        let (cfg, cert_der) = server_config();
        assert!(!cert_der.is_empty());
        // Verify the config was created (no panic)
        drop(cfg);
    }

    #[test]
    fn client_config_creates_without_error() {
        let cfg = client_config();
        drop(cfg);
    }
}
