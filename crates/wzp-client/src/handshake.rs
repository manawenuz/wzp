//! Client-side cryptographic handshake.
//!
//! Performs the caller role of the WarzonePhone key exchange:
//! send `CallOffer` → recv `CallAnswer` → derive shared `CryptoSession`.

use wzp_crypto::{CryptoSession, KeyExchange, WarzoneKeyExchange};
use wzp_proto::{
    CodecId, HangupReason, MediaTransport, QualityProfile, SignalMessage, default_signal_version,
};

const SUPPORTED_VIDEO_CODECS: &[CodecId] = &[CodecId::H264Baseline];

/// Result of a successful client-side handshake.
pub struct HandshakeResult {
    pub session: Box<dyn CryptoSession>,
    /// Video codec agreed with the relay. `None` if peer is audio-only.
    pub video_codec: Option<CodecId>,
}

/// Errors that can occur during the client-side cryptographic handshake.
#[derive(Debug)]
pub enum HandshakeError {
    ConnectionClosed,
    ProtocolVersionMismatch { server_supported: Vec<u8> },
    UnexpectedSignal(&'static str),
    SignatureVerificationFailed,
    KeyDerivation(String),
    Transport(wzp_proto::TransportError),
}

impl std::fmt::Display for HandshakeError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::ConnectionClosed => write!(f, "connection closed before receiving CallAnswer"),
            Self::ProtocolVersionMismatch { server_supported } => {
                write!(
                    f,
                    "protocol version mismatch: server supports {server_supported:?}"
                )
            }
            Self::UnexpectedSignal(expected) => write!(f, "expected CallAnswer, got {expected}"),
            Self::SignatureVerificationFailed => write!(f, "callee signature verification failed"),
            Self::KeyDerivation(msg) => write!(f, "key derivation failed: {msg}"),
            Self::Transport(e) => write!(f, "transport error: {e}"),
        }
    }
}

impl std::error::Error for HandshakeError {
    fn source(&self) -> Option<&(dyn std::error::Error + 'static)> {
        match self {
            Self::Transport(e) => Some(e),
            _ => None,
        }
    }
}

impl From<wzp_proto::TransportError> for HandshakeError {
    fn from(e: wzp_proto::TransportError) -> Self {
        Self::Transport(e)
    }
}

/// Perform the client (caller) side of the cryptographic handshake.
///
/// 1. Derive identity from `seed`
/// 2. Generate ephemeral X25519 keypair
/// 3. Sign `(ephemeral_pub || "call-offer")` with identity key
/// 4. Send `CallOffer` with identity_pub, ephemeral_pub, signature
/// 5. Receive `CallAnswer`, verify callee signature
/// 6. Derive shared ChaCha20-Poly1305 session
pub async fn perform_handshake(
    transport: &dyn MediaTransport,
    seed: &[u8; 32],
    alias: Option<&str>,
) -> Result<HandshakeResult, HandshakeError> {
    // 1. Create key exchange from identity seed
    let mut kx = WarzoneKeyExchange::from_identity_seed(seed);
    let identity_pub = kx.identity_public_key();

    // 2. Generate ephemeral key
    let ephemeral_pub = kx.generate_ephemeral();

    // 3. Sign (ephemeral_pub || "call-offer")
    let mut sign_data = Vec::with_capacity(32 + 10);
    sign_data.extend_from_slice(&ephemeral_pub);
    sign_data.extend_from_slice(b"call-offer");
    let signature = kx.sign(&sign_data);

    // 4. Send CallOffer
    let offer = SignalMessage::CallOffer {
        version: default_signal_version(),
        identity_pub,
        ephemeral_pub,
        signature,
        supported_profiles: vec![
            QualityProfile::STUDIO_64K,
            QualityProfile::STUDIO_48K,
            QualityProfile::STUDIO_32K,
            QualityProfile::GOOD,
            QualityProfile::DEGRADED,
            QualityProfile::CATASTROPHIC,
        ],
        alias: alias.map(|s| s.to_string()),
        protocol_version: 2,
        supported_versions: vec![2],
        video_codecs: SUPPORTED_VIDEO_CODECS.to_vec(),
    };
    transport
        .send_signal(&offer)
        .await
        .map_err(HandshakeError::Transport)?;

    // 5. Wait for CallAnswer — 10s timeout guards against relay not responding.
    let answer = tokio::time::timeout(
        std::time::Duration::from_secs(10),
        transport.recv_signal(),
    )
    .await
    .map_err(|_| HandshakeError::Transport(wzp_proto::TransportError::Timeout { ms: 10_000 }))?
    .map_err(HandshakeError::Transport)?
    .ok_or(HandshakeError::ConnectionClosed)?;

    let (callee_identity_pub, callee_ephemeral_pub, callee_signature, _chosen_profile, video_codec) =
        match answer {
            SignalMessage::CallAnswer {
                identity_pub,
                ephemeral_pub,
                signature,
                chosen_profile,
                video_codec,
                ..
            } => (identity_pub, ephemeral_pub, signature, chosen_profile, video_codec),
            SignalMessage::Hangup {
                reason: HangupReason::ProtocolVersionMismatch { server_supported },
                ..
            } => {
                return Err(HandshakeError::ProtocolVersionMismatch { server_supported });
            }
            _ => {
                return Err(HandshakeError::UnexpectedSignal("CallAnswer"));
            }
        };

    // 6. Verify callee's signature over (ephemeral_pub || "call-answer")
    let mut verify_data = Vec::with_capacity(32 + 11);
    verify_data.extend_from_slice(&callee_ephemeral_pub);
    verify_data.extend_from_slice(b"call-answer");
    if !WarzoneKeyExchange::verify(&callee_identity_pub, &verify_data, &callee_signature) {
        return Err(HandshakeError::SignatureVerificationFailed);
    }

    // 7. Derive session
    let session = kx
        .derive_session(&callee_ephemeral_pub)
        .map_err(|e| HandshakeError::KeyDerivation(e.to_string()))?;

    Ok(HandshakeResult { session, video_codec })
}

#[cfg(test)]
mod tests {
    use super::*;

    // Integration test lives in tests/ — unit-level coverage relies on wzp-crypto tests.
    #[test]
    fn sign_data_format() {
        let kx = WarzoneKeyExchange::from_identity_seed(&[0xAA; 32]);
        let eph = [0x11u8; 32];
        let mut data = Vec::new();
        data.extend_from_slice(&eph);
        data.extend_from_slice(b"call-offer");
        let sig = kx.sign(&data);
        assert!(WarzoneKeyExchange::verify(
            &kx.identity_public_key(),
            &data,
            &sig,
        ));
    }

    #[test]
    fn handshake_result_carries_video_codec() {
        // Verify that HandshakeResult has both fields accessible and that
        // None is the correct default for audio-only peers.
        let mut kx = WarzoneKeyExchange::from_identity_seed(&[0x55; 32]);
        kx.generate_ephemeral();
        let session = kx.derive_session(&[0u8; 32]).unwrap();
        let hs = HandshakeResult { session, video_codec: None };
        assert!(hs.video_codec.is_none());

        let mut kx2 = WarzoneKeyExchange::from_identity_seed(&[0x66; 32]);
        kx2.generate_ephemeral();
        let session2 = kx2.derive_session(&[0u8; 32]).unwrap();
        let hs2 = HandshakeResult {
            session: session2,
            video_codec: Some(CodecId::H264Baseline),
        };
        assert_eq!(hs2.video_codec, Some(CodecId::H264Baseline));
    }

    #[test]
    fn offer_contains_h264_only() {
        // Keep room video on the common denominator until Android AV1/HEVC
        // send paths are proven in-device.
        assert_eq!(SUPPORTED_VIDEO_CODECS, &[CodecId::H264Baseline]);
    }
}
