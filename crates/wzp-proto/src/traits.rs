use async_trait::async_trait;

use crate::error::*;
use crate::packet::*;
use crate::{CodecId, QualityProfile};

// ─── Audio Codec Traits ──────────────────────────────────────────────────────

/// Encodes PCM audio into compressed frames.
pub trait AudioEncoder: Send + Sync {
    /// Encode PCM samples (16-bit mono) into a compressed frame.
    ///
    /// Input sample rate depends on `codec_id()` — 48kHz for Opus, 8kHz for Codec2.
    /// Returns the number of bytes written to `out`.
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError>;

    /// Current codec identifier.
    fn codec_id(&self) -> CodecId;

    /// Switch codec/bitrate configuration on the fly.
    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError>;

    /// Maximum output bytes for a single frame at current settings.
    fn max_frame_bytes(&self) -> usize;

    /// Enable/disable Opus inband FEC (no-op for Codec2).
    fn set_inband_fec(&mut self, _enabled: bool) {}

    /// Enable/disable DTX (discontinuous transmission). No-op for Codec2.
    fn set_dtx(&mut self, _enabled: bool) {}
}

/// Decodes compressed frames back to PCM audio.
pub trait AudioDecoder: Send + Sync {
    /// Decode a compressed frame into PCM samples.
    /// Returns the number of samples written to `pcm`.
    fn decode(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError>;

    /// Generate PLC (packet loss concealment) output for a missing frame.
    /// Returns the number of samples written.
    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError>;

    /// Current codec identifier.
    fn codec_id(&self) -> CodecId;

    /// Switch codec/bitrate configuration.
    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError>;
}

// ─── FEC Traits ──────────────────────────────────────────────────────────────

/// Encodes source symbols into FEC-protected blocks using fountain codes.
pub trait FecEncoder: Send + Sync {
    /// Add a source symbol (one audio frame) to the current block.
    fn add_source_symbol(&mut self, data: &[u8]) -> Result<(), FecError>;

    /// Generate repair symbols for the current block.
    ///
    /// `ratio` is the repair overhead (e.g., 0.5 = 50% more symbols than source).
    /// Returns `(fec_symbol_index, repair_data)` pairs.
    fn generate_repair(&mut self, ratio: f32) -> Result<Vec<(u8, Vec<u8>)>, FecError>;

    /// Finalize the current block and start a new one.
    /// Returns the block ID of the finalized block.
    fn finalize_block(&mut self) -> Result<u8, FecError>;

    /// Current block ID being built.
    fn current_block_id(&self) -> u8;

    /// Number of source symbols in the current block.
    fn current_block_size(&self) -> usize;
}

/// Decodes FEC-protected blocks, recovering lost source symbols.
pub trait FecDecoder: Send + Sync {
    /// Feed a received symbol (source or repair) into the decoder.
    fn add_symbol(
        &mut self,
        block_id: u8,
        symbol_index: u8,
        is_repair: bool,
        data: &[u8],
    ) -> Result<(), FecError>;

    /// Attempt to reconstruct the source block.
    ///
    /// Returns `None` if not yet decodable (insufficient symbols).
    /// Returns `Some(Vec<source_frames>)` on success.
    fn try_decode(&mut self, block_id: u8) -> Result<Option<Vec<Vec<u8>>>, FecError>;

    /// Drop state for blocks older than `block_id`.
    fn expire_before(&mut self, block_id: u8);
}

// ─── Crypto Traits ───────────────────────────────────────────────────────────
//
// Compatible with Warzone messenger identity model:
//   Identity = 32-byte seed → HKDF → Ed25519 (signing) + X25519 (encryption)
//   Fingerprint = SHA-256(Ed25519 pub)[:16]

/// Per-call encryption session (symmetric, after key exchange).
pub trait CryptoSession: Send + Sync {
    /// Encrypt a media packet payload.
    ///
    /// `header_bytes` is used as AAD (authenticated but not encrypted).
    /// The encrypted output is written to `out` (ciphertext + 16-byte auth tag).
    fn encrypt(
        &mut self,
        header_bytes: &[u8],
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError>;

    /// Decrypt a media packet payload.
    ///
    /// `header_bytes` is the AAD used during encryption.
    /// Returns decrypted plaintext in `out`.
    fn decrypt(
        &mut self,
        header_bytes: &[u8],
        ciphertext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError>;

    /// Initiate rekeying. Returns the new ephemeral X25519 public key to send to the peer.
    fn initiate_rekey(&mut self) -> Result<[u8; 32], CryptoError>;

    /// Complete rekeying with the peer's new ephemeral public key.
    fn complete_rekey(&mut self, peer_ephemeral_pub: &[u8; 32]) -> Result<(), CryptoError>;

    /// Current encryption overhead in bytes (auth tag size).
    fn overhead(&self) -> usize {
        16 // ChaCha20-Poly1305 tag
    }

    /// Short Authentication String (SAS) — 4-digit code for verbal verification.
    /// Both peers derive the same code from the shared secret + identity keys.
    /// If a MITM relay is intercepting, the codes will differ.
    /// Returns None if SAS was not computed (e.g., relay-side sessions).
    fn sas_code(&self) -> Option<u32> {
        None
    }
}

/// Key exchange using the Warzone identity model.
///
/// The identity keypair (Ed25519 + X25519) is derived from the user's 32-byte seed
/// via HKDF. Each call generates a new ephemeral X25519 keypair.
pub trait KeyExchange: Send + Sync {
    /// Initialize from a Warzone identity seed.
    ///
    /// The seed derives:
    /// - Ed25519 signing keypair (for identity/signatures)
    /// - X25519 static keypair (for encryption, though calls use ephemeral keys)
    fn from_identity_seed(seed: &[u8; 32]) -> Self
    where
        Self: Sized;

    /// Generate a new ephemeral X25519 keypair for this call.
    /// Returns the ephemeral public key to send to the peer.
    fn generate_ephemeral(&mut self) -> [u8; 32];

    /// Get our Ed25519 identity public key.
    fn identity_public_key(&self) -> [u8; 32];

    /// Get our fingerprint (SHA-256(Ed25519 pub)[:16]).
    fn fingerprint(&self) -> [u8; 16];

    /// Sign data with our Ed25519 identity key.
    fn sign(&self, data: &[u8]) -> Vec<u8>;

    /// Verify a signature from a peer's Ed25519 public key.
    fn verify(peer_identity_pub: &[u8; 32], data: &[u8], signature: &[u8]) -> bool
    where
        Self: Sized;

    /// Derive a CryptoSession from our ephemeral secret + peer's ephemeral public key.
    ///
    /// The shared secret is computed via X25519 ECDH, then expanded via HKDF.
    fn derive_session(
        &self,
        peer_ephemeral_pub: &[u8; 32],
    ) -> Result<Box<dyn CryptoSession>, CryptoError>;
}

// ─── Transport Traits ────────────────────────────────────────────────────────

/// Transport layer for sending/receiving media and signaling.
#[async_trait]
pub trait MediaTransport: Send + Sync {
    /// Send a media packet (unreliable, via QUIC DATAGRAM frame).
    async fn send_media(&self, packet: &MediaPacket) -> Result<(), TransportError>;

    /// Receive the next media packet. Returns None on clean shutdown.
    async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError>;

    /// Send a signaling message (reliable, via QUIC stream).
    async fn send_signal(&self, msg: &SignalMessage) -> Result<(), TransportError>;

    /// Receive the next signaling message. Returns None on clean shutdown.
    async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError>;

    /// Current estimated path quality metrics.
    fn path_quality(&self) -> PathQuality;

    /// Close the transport gracefully.
    async fn close(&self) -> Result<(), TransportError>;
}

/// Observed network path quality metrics.
#[derive(Clone, Copy, Debug, Default)]
pub struct PathQuality {
    /// Estimated packet loss percentage (0.0-100.0).
    pub loss_pct: f32,
    /// Smoothed round-trip time in milliseconds.
    pub rtt_ms: u32,
    /// Jitter (RTT variance) in milliseconds.
    pub jitter_ms: u32,
    /// Estimated available bandwidth in kbps.
    pub bandwidth_kbps: u32,
}

// ─── Obfuscation Trait (Phase 2) ─────────────────────────────────────────────

/// Wraps/unwraps packets for DPI evasion on the client-relay link.
pub trait ObfuscationLayer: Send + Sync {
    /// Wrap outgoing bytes with obfuscation (padding, framing, etc.).
    fn obfuscate(
        &mut self,
        data: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), crate::error::ObfuscationError>;

    /// Unwrap incoming obfuscated bytes.
    fn deobfuscate(
        &mut self,
        data: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), crate::error::ObfuscationError>;
}

// ─── Quality Controller Trait ────────────────────────────────────────────────

/// Adaptive quality controller that selects codec/FEC parameters based on link conditions.
pub trait QualityController: Send + Sync {
    /// Feed a quality observation. Returns a new profile if a tier transition occurred.
    fn observe(&mut self, report: &QualityReport) -> Option<QualityProfile>;

    /// Force a specific profile (overrides adaptive logic).
    fn force_profile(&mut self, profile: QualityProfile);

    /// Current active quality profile.
    fn current_profile(&self) -> QualityProfile;
}
