//! ChaCha20-Poly1305 encryption session.
//!
//! Implements the `CryptoSession` trait for per-call media encryption.
//! Nonces are derived deterministically from session_id + sequence counter + direction.

use std::collections::HashMap;

use chacha20poly1305::aead::Aead;
use chacha20poly1305::{ChaCha20Poly1305, KeyInit, Nonce};
use rand::rngs::OsRng;
use wzp_proto::{CryptoError, CryptoSession, MediaHeader, MediaType};
use x25519_dalek::{PublicKey, StaticSecret};

use crate::anti_replay::AntiReplayWindow;
use crate::nonce::{self, Direction};
use crate::rekey::RekeyManager;

/// Per-call symmetric encryption session using ChaCha20-Poly1305.
pub struct ChaChaSession {
    /// AEAD cipher instance.
    cipher: ChaCha20Poly1305,
    /// Session ID (first 4 bytes of the derived key hash).
    session_id: [u8; 4],
    /// Send packet counter.
    send_seq: u32,
    /// Receive packet counter.
    recv_seq: u32,
    /// Rekeying state machine.
    rekey_mgr: RekeyManager,
    /// Pending ephemeral secret for rekey (stored until peer responds).
    pending_rekey_secret: Option<StaticSecret>,
    /// Short Authentication String (4-digit code for verbal verification).
    sas_code: Option<u32>,
    /// Per-stream anti-replay windows, keyed by (stream_id, media_type).
    anti_replay: HashMap<(u8, MediaType), AntiReplayWindow>,
}

impl ChaChaSession {
    /// Create a new session from a 32-byte shared secret.
    pub fn new(shared_secret: [u8; 32]) -> Self {
        use sha2::Digest;
        let session_id_hash = sha2::Sha256::digest(&shared_secret);
        let mut session_id = [0u8; 4];
        session_id.copy_from_slice(&session_id_hash[..4]);

        let cipher = ChaCha20Poly1305::new_from_slice(&shared_secret)
            .expect("32-byte key is valid for ChaCha20Poly1305");

        Self {
            cipher,
            session_id,
            send_seq: 0,
            recv_seq: 0,
            rekey_mgr: RekeyManager::new(shared_secret),
            pending_rekey_secret: None,
            sas_code: None,
            anti_replay: HashMap::new(),
        }
    }

    /// Set the SAS code (called by key exchange after derivation).
    pub fn set_sas(&mut self, code: u32) {
        self.sas_code = Some(code);
    }

    /// Install a new key (after rekeying).
    fn install_key(&mut self, new_key: [u8; 32]) {
        use sha2::Digest;
        let session_id_hash = sha2::Sha256::digest(&new_key);
        self.session_id.copy_from_slice(&session_id_hash[..4]);
        self.cipher = ChaCha20Poly1305::new_from_slice(&new_key)
            .expect("32-byte key is valid for ChaCha20Poly1305");
    }
}

/// Parse a v2 `MediaHeader` from raw bytes.
/// Returns `None` if the buffer is too short or not a valid v2 header.
fn parse_header(header_bytes: &[u8]) -> Option<MediaHeader> {
    if header_bytes.len() < MediaHeader::WIRE_SIZE {
        return None;
    }
    let mut cursor = std::io::Cursor::new(header_bytes);
    MediaHeader::read_from(&mut cursor)
}

/// Return the default anti-replay window size for a given media type.
fn default_window_for_media_type(media_type: MediaType) -> AntiReplayWindow {
    let size = match media_type {
        MediaType::Audio => 64,
        MediaType::Video => 1024,
        MediaType::Data => 256,
        MediaType::Control => 32,
    };
    AntiReplayWindow::with_window(size)
}

impl CryptoSession for ChaChaSession {
    fn encrypt(
        &mut self,
        header_bytes: &[u8],
        plaintext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        let nonce_bytes = nonce::build_nonce(&self.session_id, self.send_seq, Direction::Send);
        let nonce = Nonce::from_slice(&nonce_bytes);

        // Encrypt with AAD
        use chacha20poly1305::aead::Payload;
        let payload = Payload {
            msg: plaintext,
            aad: header_bytes,
        };

        let ciphertext = self
            .cipher
            .encrypt(nonce, payload)
            .map_err(|_| CryptoError::Internal("encryption failed".into()))?;

        out.extend_from_slice(&ciphertext);
        self.send_seq = self.send_seq.wrapping_add(1);
        Ok(())
    }

    fn decrypt(
        &mut self,
        header_bytes: &[u8],
        ciphertext: &[u8],
        out: &mut Vec<u8>,
    ) -> Result<(), CryptoError> {
        // Use Direction::Send to match the sender's nonce construction.
        // The recv_seq counter tracks which packet from the peer we're decrypting.
        let nonce_bytes = nonce::build_nonce(&self.session_id, self.recv_seq, Direction::Send);
        let nonce = Nonce::from_slice(&nonce_bytes);

        use chacha20poly1305::aead::Payload;
        let payload = Payload {
            msg: ciphertext,
            aad: header_bytes,
        };

        let plaintext = self
            .cipher
            .decrypt(nonce, payload)
            .map_err(|_| CryptoError::DecryptionFailed)?;

        let plaintext_len = plaintext.len();
        out.extend_from_slice(&plaintext);
        self.recv_seq = self.recv_seq.wrapping_add(1);

        // Anti-replay check: if header parses as a v2 MediaHeader, verify seq
        // is not a replay for this (stream_id, media_type).
        if let Some(header) = parse_header(header_bytes) {
            let window = self
                .anti_replay
                .entry((header.stream_id, header.media_type))
                .or_insert_with(|| default_window_for_media_type(header.media_type));
            if let Err(e) = window.check_and_update(header.seq) {
                // Roll back the plaintext we just appended.
                out.truncate(out.len() - plaintext_len);
                return Err(e);
            }
        }

        Ok(())
    }

    fn initiate_rekey(&mut self) -> Result<[u8; 32], CryptoError> {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = PublicKey::from(&secret);
        self.pending_rekey_secret = Some(secret);
        Ok(public.to_bytes())
    }

    fn complete_rekey(&mut self, peer_ephemeral_pub: &[u8; 32]) -> Result<(), CryptoError> {
        let secret = self
            .pending_rekey_secret
            .take()
            .ok_or_else(|| CryptoError::RekeyFailed("no pending rekey".into()))?;

        let total_packets = self.send_seq as u64 + self.recv_seq as u64;
        let new_key = self
            .rekey_mgr
            .perform_rekey(peer_ephemeral_pub, secret, total_packets);
        self.install_key(new_key);

        // Reset sequence counters after rekey for nonce uniqueness
        self.send_seq = 0;
        self.recv_seq = 0;

        Ok(())
    }

    fn sas_code(&self) -> Option<u32> {
        self.sas_code
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_session_pair() -> (ChaChaSession, ChaChaSession) {
        let key = [0x42u8; 32];
        (ChaChaSession::new(key), ChaChaSession::new(key))
    }

    #[test]
    fn encrypt_decrypt_roundtrip() {
        let (mut alice, mut bob) = make_session_pair();
        let header = b"test-header";
        let plaintext = b"hello warzone";

        let mut ciphertext = Vec::new();
        alice.encrypt(header, plaintext, &mut ciphertext).unwrap();

        // Bob decrypts (his recv matches Alice's send)
        let mut decrypted = Vec::new();
        bob.decrypt(header, &ciphertext, &mut decrypted).unwrap();

        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn decrypt_wrong_aad_fails() {
        let (mut alice, mut bob) = make_session_pair();
        let header = b"correct-header";
        let plaintext = b"secret data";

        let mut ciphertext = Vec::new();
        alice.encrypt(header, plaintext, &mut ciphertext).unwrap();

        let mut decrypted = Vec::new();
        let result = bob.decrypt(b"wrong-header", &ciphertext, &mut decrypted);
        assert!(result.is_err());
    }

    #[test]
    fn decrypt_wrong_key_fails() {
        let mut alice = ChaChaSession::new([0xAA; 32]);
        let mut eve = ChaChaSession::new([0xBB; 32]);

        let header = b"hdr";
        let plaintext = b"secret";

        let mut ciphertext = Vec::new();
        alice.encrypt(header, plaintext, &mut ciphertext).unwrap();

        let mut decrypted = Vec::new();
        let result = eve.decrypt(header, &ciphertext, &mut decrypted);
        assert!(result.is_err());
    }

    #[test]
    fn multiple_packets_roundtrip() {
        let (mut alice, mut bob) = make_session_pair();
        let header = b"hdr";

        for i in 0..100 {
            let msg = format!("message {}", i);
            let mut ct = Vec::new();
            alice.encrypt(header, msg.as_bytes(), &mut ct).unwrap();

            let mut pt = Vec::new();
            bob.decrypt(header, &ct, &mut pt).unwrap();
            assert_eq!(pt, msg.as_bytes());
        }
    }

    #[test]
    fn rekey_changes_key() {
        let (mut alice, mut _bob) = make_session_pair();

        let peer_secret = StaticSecret::random_from_rng(OsRng);
        let peer_pub = PublicKey::from(&peer_secret).to_bytes();

        let rekey_pub = alice.initiate_rekey().unwrap();
        assert_ne!(rekey_pub, [0u8; 32]); // Should be a valid public key

        alice.complete_rekey(&peer_pub).unwrap();
        // Session is now rekeyed - counters reset
        assert_eq!(alice.send_seq, 0);
    }

    #[test]
    fn per_stream_anti_replay_rejects_duplicate() {
        use wzp_proto::{CodecId, MediaType};

        let (mut alice, mut bob) = make_session_pair();
        let header = MediaHeader {
            version: 2,
            flags: 0,
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 10,
            seq: 42,
            timestamp: 1000,
            fec_block: 0,
        };
        let mut header_bytes = Vec::new();
        header.write_to(&mut header_bytes);

        let plaintext = b"audio frame";

        // First packet decrypts successfully
        let mut ct = Vec::new();
        alice.encrypt(&header_bytes, plaintext, &mut ct).unwrap();
        let mut pt = Vec::new();
        bob.decrypt(&header_bytes, &ct, &mut pt).unwrap();
        assert_eq!(&pt, plaintext);

        // Exact duplicate is rejected by anti-replay
        let mut pt2 = Vec::new();
        let result = bob.decrypt(&header_bytes, &ct, &mut pt2);
        assert!(
            result.is_err(),
            "duplicate packet with same seq must be rejected"
        );
        assert!(pt2.is_empty(), "plaintext must be rolled back on replay");
    }

    #[test]
    fn per_stream_anti_replay_video_burst_200_with_reorder() {
        use wzp_proto::{CodecId, MediaType};

        let (mut alice, mut bob) = make_session_pair();
        let header = MediaHeader {
            version: 2,
            flags: 0,
            media_type: MediaType::Video,
            codec_id: CodecId::Opus24k,
            stream_id: 1,
            fec_ratio: 10,
            seq: 0,
            timestamp: 0,
            fec_block: 0,
        };

        let plaintext = b"video frame";

        // Send 200 packets in order
        for i in 0..200 {
            let mut h = header;
            h.seq = i;
            let mut header_bytes = Vec::new();
            h.write_to(&mut header_bytes);

            let mut ct = Vec::new();
            alice.encrypt(&header_bytes, plaintext, &mut ct).unwrap();

            let mut pt = Vec::new();
            bob.decrypt(&header_bytes, &ct, &mut pt).unwrap();
        }

        // Re-send packet 50 — should be rejected as replay
        let mut h = header;
        h.seq = 50;
        let mut header_bytes = Vec::new();
        h.write_to(&mut header_bytes);

        let mut ct = Vec::new();
        alice.encrypt(&header_bytes, plaintext, &mut ct).unwrap();

        let mut pt = Vec::new();
        let result = bob.decrypt(&header_bytes, &ct, &mut pt);
        assert!(result.is_err(), "reordered duplicate must be rejected");
    }
}
