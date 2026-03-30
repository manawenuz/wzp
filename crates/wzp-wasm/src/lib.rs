//! WarzonePhone WASM bindings.
//!
//! Exports two subsystems for browser-side usage:
//!
//! **FEC** — RaptorQ forward error correction (encode/decode).
//! Audio frames are padded to a fixed symbol size (default 256 bytes) with a
//! 2-byte little-endian length prefix, matching the native wzp-fec wire format.
//!
//! Wire format per symbol:
//!   [block_id:1][symbol_idx:1][is_repair:1][symbol_data:symbol_size]
//!
//! Encoder output: concatenated symbols in the above format when a block completes.
//! Decoder input: individual symbols in the above format.
//! Decoder output: concatenated original source data (length-prefix stripped).
//!
//! **Crypto** — X25519 key exchange + ChaCha20-Poly1305 AEAD encryption.
//! Mirrors `wzp-crypto` nonce/session/handshake logic so WASM and native
//! peers produce interoperable ciphertext.

use wasm_bindgen::prelude::*;
use raptorq::{
    EncodingPacket, ObjectTransmissionInformation, PayloadId, SourceBlockDecoder,
    SourceBlockEncoder,
};

/// Header size prepended to each symbol on the wire: block_id + symbol_idx + is_repair.
const HEADER_SIZE: usize = 3;

/// Length prefix size inside each padded symbol (u16 LE), matching wzp-fec.
const LEN_PREFIX: usize = 2;

// ---------------------------------------------------------------------------
// Encoder
// ---------------------------------------------------------------------------

#[wasm_bindgen]
pub struct WzpFecEncoder {
    block_id: u8,
    frames_per_block: usize,
    symbol_size: usize,
    source_symbols: Vec<Vec<u8>>,
}

#[wasm_bindgen]
impl WzpFecEncoder {
    /// Create a new FEC encoder.
    ///
    /// * `block_size` — number of source symbols (audio frames) per FEC block.
    /// * `symbol_size` — padded byte size of each symbol (default 256).
    #[wasm_bindgen(constructor)]
    pub fn new(block_size: usize, symbol_size: usize) -> Self {
        Self {
            block_id: 0,
            frames_per_block: block_size,
            symbol_size,
            source_symbols: Vec::with_capacity(block_size),
        }
    }

    /// Add a source symbol (audio frame).
    ///
    /// Returns encoded packets (all source + repair) when the block is complete,
    /// or `undefined` if the block is still accumulating.
    ///
    /// Each returned packet carries the 3-byte header:
    ///   `[block_id][symbol_idx][is_repair]` followed by `symbol_size` bytes.
    pub fn add_symbol(&mut self, data: &[u8]) -> Option<Vec<u8>> {
        self.source_symbols.push(data.to_vec());

        if self.source_symbols.len() >= self.frames_per_block {
            Some(self.encode_block())
        } else {
            None
        }
    }

    /// Force-flush the current (possibly partial) block.
    ///
    /// Returns all source + repair symbols with headers, or empty vec if no
    /// symbols have been accumulated.
    pub fn flush(&mut self) -> Vec<u8> {
        if self.source_symbols.is_empty() {
            return Vec::new();
        }
        self.encode_block()
    }

    /// Internal: encode accumulated source symbols into a block, generate repair,
    /// and return the concatenated wire-format output.
    fn encode_block(&mut self) -> Vec<u8> {
        let ss = self.symbol_size;
        let num_source = self.source_symbols.len();
        let block_id = self.block_id;

        // Build length-prefixed, padded block data (matches wzp-fec format).
        let block_data = self.build_block_data();

        let config =
            ObjectTransmissionInformation::with_defaults(block_data.len() as u64, ss as u16);
        let encoder = SourceBlockEncoder::new(block_id, &config, &block_data);

        // Generate source packets.
        let source_packets = encoder.source_packets();

        // Generate repair packets — 50% overhead by default.
        let num_repair = ((num_source as f32) * 0.5).ceil() as u32;
        let repair_packets = encoder.repair_packets(0, num_repair);

        // Allocate output buffer.
        let total_packets = source_packets.len() + repair_packets.len();
        let packet_wire_size = HEADER_SIZE + ss;
        let mut output = Vec::with_capacity(total_packets * packet_wire_size);

        // Write source symbols.
        for (i, pkt) in source_packets.iter().enumerate() {
            output.push(block_id);
            output.push(i as u8);
            output.push(0); // is_repair = false
            let pkt_data = pkt.data();
            let copy_len = pkt_data.len().min(ss);
            output.extend_from_slice(&pkt_data[..copy_len]);
            // Pad if shorter.
            if copy_len < ss {
                output.resize(output.len() + (ss - copy_len), 0);
            }
        }

        // Write repair symbols.
        for (i, pkt) in repair_packets.iter().enumerate() {
            output.push(block_id);
            output.push((num_source + i) as u8);
            output.push(1); // is_repair = true
            let pkt_data = pkt.data();
            let copy_len = pkt_data.len().min(ss);
            output.extend_from_slice(&pkt_data[..copy_len]);
            if copy_len < ss {
                output.resize(output.len() + (ss - copy_len), 0);
            }
        }

        // Advance block.
        self.block_id = self.block_id.wrapping_add(1);
        self.source_symbols.clear();

        output
    }

    /// Build the contiguous, length-prefixed block data buffer.
    fn build_block_data(&self) -> Vec<u8> {
        let ss = self.symbol_size;
        let mut data = vec![0u8; self.source_symbols.len() * ss];
        for (i, sym) in self.source_symbols.iter().enumerate() {
            let max_payload = ss - LEN_PREFIX;
            let payload_len = sym.len().min(max_payload);
            let offset = i * ss;
            data[offset..offset + LEN_PREFIX]
                .copy_from_slice(&(payload_len as u16).to_le_bytes());
            data[offset + LEN_PREFIX..offset + LEN_PREFIX + payload_len]
                .copy_from_slice(&sym[..payload_len]);
        }
        data
    }
}

// ---------------------------------------------------------------------------
// Decoder
// ---------------------------------------------------------------------------

/// Per-block decoder state.
struct BlockState {
    packets: Vec<EncodingPacket>,
    decoded: bool,
    result: Option<Vec<u8>>,
}

#[wasm_bindgen]
pub struct WzpFecDecoder {
    frames_per_block: usize,
    symbol_size: usize,
    blocks: Vec<(u8, BlockState)>, // poor man's map (no std HashMap in tiny WASM)
}

#[wasm_bindgen]
impl WzpFecDecoder {
    /// Create a new FEC decoder.
    ///
    /// * `block_size` — expected number of source symbols per block.
    /// * `symbol_size` — padded byte size of each symbol (must match encoder).
    #[wasm_bindgen(constructor)]
    pub fn new(block_size: usize, symbol_size: usize) -> Self {
        Self {
            frames_per_block: block_size,
            symbol_size,
            blocks: Vec::new(),
        }
    }

    /// Feed a received symbol.
    ///
    /// Returns the decoded block (concatenated original frames, unpadded) if
    /// enough symbols have been received to recover the block, or `undefined`.
    pub fn add_symbol(
        &mut self,
        block_id: u8,
        symbol_idx: u8,
        _is_repair: bool,
        data: &[u8],
    ) -> Option<Vec<u8>> {
        let ss = self.symbol_size;

        // Pad incoming data to symbol_size.
        let mut padded = vec![0u8; ss];
        let len = data.len().min(ss);
        padded[..len].copy_from_slice(&data[..len]);

        let esi = symbol_idx as u32;
        let packet = EncodingPacket::new(PayloadId::new(block_id, esi), padded);

        // Find or create block state.
        let block = self.get_or_create_block(block_id);

        if block.decoded {
            return block.result.clone();
        }

        block.packets.push(packet);

        // Attempt decode.
        self.try_decode(block_id)
    }

    /// Try to decode a block; returns the original frames if successful.
    fn try_decode(&mut self, block_id: u8) -> Option<Vec<u8>> {
        let ss = self.symbol_size;
        let num_source = self.frames_per_block;
        let block_length = (num_source as u64) * (ss as u64);

        let block = self.get_block_mut(block_id)?;
        if block.decoded {
            return block.result.clone();
        }

        let config =
            ObjectTransmissionInformation::with_defaults(block_length, ss as u16);
        let mut decoder = SourceBlockDecoder::new(block_id, &config, block_length);

        let decoded = decoder.decode(block.packets.clone());

        match decoded {
            Some(data) => {
                // Extract original frames by stripping length prefixes.
                let mut output = Vec::new();
                for i in 0..num_source {
                    let offset = i * ss;
                    if offset + LEN_PREFIX > data.len() {
                        break;
                    }
                    let payload_len = u16::from_le_bytes([
                        data[offset],
                        data[offset + 1],
                    ]) as usize;
                    let payload_start = offset + LEN_PREFIX;
                    let payload_end = (payload_start + payload_len).min(data.len());
                    output.extend_from_slice(&data[payload_start..payload_end]);
                }

                let block = self.get_block_mut(block_id).unwrap();
                block.decoded = true;
                block.result = Some(output.clone());
                Some(output)
            }
            None => None,
        }
    }

    fn get_or_create_block(&mut self, block_id: u8) -> &mut BlockState {
        if let Some(pos) = self.blocks.iter().position(|(id, _)| *id == block_id) {
            return &mut self.blocks[pos].1;
        }
        self.blocks.push((
            block_id,
            BlockState {
                packets: Vec::new(),
                decoded: false,
                result: None,
            },
        ));
        let last = self.blocks.len() - 1;
        &mut self.blocks[last].1
    }

    fn get_block_mut(&mut self, block_id: u8) -> Option<&mut BlockState> {
        self.blocks
            .iter_mut()
            .find(|(id, _)| *id == block_id)
            .map(|(_, state)| state)
    }
}

// =========================================================================
// Crypto — X25519 key exchange
// =========================================================================

/// X25519 key exchange: generate ephemeral keypair and derive shared secret.
///
/// Usage from JS:
/// ```js
/// const kx = new WzpKeyExchange();
/// const ourPub = kx.public_key();         // Uint8Array(32)
/// // ... send ourPub to peer, receive peerPub ...
/// const secret = kx.derive_shared_secret(peerPub); // Uint8Array(32)
/// const session = new WzpCryptoSession(secret);
/// ```
#[wasm_bindgen]
pub struct WzpKeyExchange {
    secret: x25519_dalek::StaticSecret,
    public: x25519_dalek::PublicKey,
}

#[wasm_bindgen]
impl WzpKeyExchange {
    /// Generate a new random X25519 keypair.
    #[wasm_bindgen(constructor)]
    pub fn new() -> Self {
        let secret = x25519_dalek::StaticSecret::random_from_rng(rand::rngs::OsRng);
        let public = x25519_dalek::PublicKey::from(&secret);
        Self { secret, public }
    }

    /// Our public key (32 bytes).
    pub fn public_key(&self) -> Vec<u8> {
        self.public.as_bytes().to_vec()
    }

    /// Derive a 32-byte session key from the peer's public key.
    ///
    /// Raw DH output is expanded via HKDF-SHA256 with info="warzone-session-key",
    /// matching `wzp-crypto::handshake::WarzoneKeyExchange::derive_session`.
    pub fn derive_shared_secret(&self, peer_public: &[u8]) -> Result<Vec<u8>, JsValue> {
        if peer_public.len() != 32 {
            return Err(JsValue::from_str("peer public key must be 32 bytes"));
        }
        let mut peer_bytes = [0u8; 32];
        peer_bytes.copy_from_slice(peer_public);
        let peer_pk = x25519_dalek::PublicKey::from(peer_bytes);

        // Rebuild secret from bytes (StaticSecret doesn't impl Clone).
        let secret_bytes = self.secret.to_bytes();
        let secret_clone = x25519_dalek::StaticSecret::from(secret_bytes);
        let shared = secret_clone.diffie_hellman(&peer_pk);

        // HKDF expand — same derivation as wzp-crypto handshake.rs
        use hkdf::Hkdf;
        use sha2::Sha256;
        let hk = Hkdf::<Sha256>::new(None, shared.as_bytes());
        let mut session_key = [0u8; 32];
        hk.expand(b"warzone-session-key", &mut session_key)
            .expect("HKDF expand should not fail for 32-byte output");

        Ok(session_key.to_vec())
    }
}

// =========================================================================
// Crypto — ChaCha20-Poly1305 AEAD session
// =========================================================================

/// Build a 12-byte nonce (mirrors `wzp-crypto::nonce::build_nonce`).
///
/// Layout: `session_id[4] || seq(u32 BE) || direction(1) || pad(3 zero)`.
fn build_nonce(session_id: &[u8; 4], seq: u32, direction: u8) -> [u8; 12] {
    let mut nonce = [0u8; 12];
    nonce[0..4].copy_from_slice(session_id);
    nonce[4..8].copy_from_slice(&seq.to_be_bytes());
    nonce[8] = direction;
    nonce
}

/// Symmetric encryption session using ChaCha20-Poly1305.
///
/// Mirrors `wzp-crypto::session::ChaChaSession` for WASM.  Nonce derivation
/// and key setup are identical so WASM and native peers interoperate.
#[wasm_bindgen]
pub struct WzpCryptoSession {
    cipher: chacha20poly1305::ChaCha20Poly1305,
    session_id: [u8; 4],
    send_seq: u32,
    recv_seq: u32,
}

#[wasm_bindgen]
impl WzpCryptoSession {
    /// Create from a 32-byte shared secret (output of `WzpKeyExchange.derive_shared_secret`).
    #[wasm_bindgen(constructor)]
    pub fn new(shared_secret: &[u8]) -> Result<WzpCryptoSession, JsValue> {
        if shared_secret.len() != 32 {
            return Err(JsValue::from_str("shared secret must be 32 bytes"));
        }

        use chacha20poly1305::KeyInit;
        use sha2::Digest;

        let session_id_hash = sha2::Sha256::digest(shared_secret);
        let mut session_id = [0u8; 4];
        session_id.copy_from_slice(&session_id_hash[..4]);

        let cipher = chacha20poly1305::ChaCha20Poly1305::new_from_slice(shared_secret)
            .map_err(|e| JsValue::from_str(&format!("invalid key: {}", e)))?;

        Ok(Self {
            cipher,
            session_id,
            send_seq: 0,
            recv_seq: 0,
        })
    }

    /// Encrypt a media payload with AAD (typically the 12-byte MediaHeader).
    ///
    /// Returns `ciphertext || poly1305_tag` (plaintext.len() + 16 bytes).
    pub fn encrypt(&mut self, header_aad: &[u8], plaintext: &[u8]) -> Result<Vec<u8>, JsValue> {
        use chacha20poly1305::aead::{Aead, Payload};
        use chacha20poly1305::Nonce;

        let nonce_bytes = build_nonce(&self.session_id, self.send_seq, 0); // 0 = Send
        let nonce = Nonce::from_slice(&nonce_bytes);

        let payload = Payload {
            msg: plaintext,
            aad: header_aad,
        };

        let ciphertext = self
            .cipher
            .encrypt(nonce, payload)
            .map_err(|_| JsValue::from_str("encryption failed"))?;

        self.send_seq = self.send_seq.wrapping_add(1);
        Ok(ciphertext)
    }

    /// Decrypt a media payload with AAD.
    ///
    /// Returns plaintext on success, or throws on auth failure.
    pub fn decrypt(&mut self, header_aad: &[u8], ciphertext: &[u8]) -> Result<Vec<u8>, JsValue> {
        use chacha20poly1305::aead::{Aead, Payload};
        use chacha20poly1305::Nonce;

        // direction=0 (Send) matches the sender's nonce — same as native code.
        let nonce_bytes = build_nonce(&self.session_id, self.recv_seq, 0);
        let nonce = Nonce::from_slice(&nonce_bytes);

        let payload = Payload {
            msg: ciphertext,
            aad: header_aad,
        };

        let plaintext = self
            .cipher
            .decrypt(nonce, payload)
            .map_err(|_| JsValue::from_str("decryption failed — bad key or corrupted data"))?;

        self.recv_seq = self.recv_seq.wrapping_add(1);
        Ok(plaintext)
    }

    /// Current send sequence number (for diagnostics / UI stats).
    pub fn send_seq(&self) -> u32 {
        self.send_seq
    }

    /// Current receive sequence number (for diagnostics / UI stats).
    pub fn recv_seq(&self) -> u32 {
        self.recv_seq
    }
}

// ---------------------------------------------------------------------------
// Tests (native only — not compiled to WASM)
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encode_decode_roundtrip() {
        let block_size = 5;
        let symbol_size = 256;

        let mut encoder = WzpFecEncoder::new(block_size, symbol_size);
        let mut decoder = WzpFecDecoder::new(block_size, symbol_size);

        // Create test frames of varying sizes.
        let frames: Vec<Vec<u8>> = (0..block_size)
            .map(|i| vec![(i as u8).wrapping_mul(37).wrapping_add(7); 80 + i * 10])
            .collect();

        // Feed frames to encoder; last one triggers block encoding.
        let mut wire_data = None;
        for frame in &frames {
            wire_data = encoder.add_symbol(frame);
        }
        let wire_data = wire_data.expect("block should be complete");

        // Parse wire packets and feed to decoder.
        let packet_size = HEADER_SIZE + symbol_size;
        assert_eq!(wire_data.len() % packet_size, 0);

        let mut result = None;
        for chunk in wire_data.chunks(packet_size) {
            let blk_id = chunk[0];
            let sym_idx = chunk[1];
            let is_repair = chunk[2] != 0;
            let sym_data = &chunk[HEADER_SIZE..];
            if let Some(decoded) = decoder.add_symbol(blk_id, sym_idx, is_repair, sym_data) {
                result = Some(decoded);
                break;
            }
        }

        let decoded_data = result.expect("should decode with all symbols");

        // Verify: decoded data should be all original frames concatenated.
        let mut expected = Vec::new();
        for frame in &frames {
            expected.extend_from_slice(frame);
        }
        assert_eq!(decoded_data, expected);
    }

    #[test]
    fn decode_with_packet_loss() {
        let block_size = 5;
        let symbol_size = 256;

        let mut encoder = WzpFecEncoder::new(block_size, symbol_size);
        let mut decoder = WzpFecDecoder::new(block_size, symbol_size);

        let frames: Vec<Vec<u8>> = (0..block_size)
            .map(|i| vec![(i as u8).wrapping_mul(37).wrapping_add(7); 100])
            .collect();

        let mut wire_data = None;
        for frame in &frames {
            wire_data = encoder.add_symbol(frame);
        }
        let wire_data = wire_data.unwrap();

        let packet_size = HEADER_SIZE + symbol_size;
        let packets: Vec<&[u8]> = wire_data.chunks(packet_size).collect();

        // Drop 2 source packets (simulate 40% source loss).
        // We have 5 source + 3 repair = 8 packets. Drop packets at index 1 and 3.
        let mut result = None;
        for (i, chunk) in packets.iter().enumerate() {
            if i == 1 || i == 3 {
                continue; // simulate loss
            }
            let blk_id = chunk[0];
            let sym_idx = chunk[1];
            let is_repair = chunk[2] != 0;
            let sym_data = &chunk[HEADER_SIZE..];
            if let Some(decoded) = decoder.add_symbol(blk_id, sym_idx, is_repair, sym_data) {
                result = Some(decoded);
                break;
            }
        }

        let decoded_data = result.expect("should recover with FEC despite 2 lost packets");

        let mut expected = Vec::new();
        for frame in &frames {
            expected.extend_from_slice(frame);
        }
        assert_eq!(decoded_data, expected);
    }

    #[test]
    fn flush_partial_block() {
        let mut encoder = WzpFecEncoder::new(5, 256);

        // Add only 3 of 5 expected symbols, then flush.
        encoder.add_symbol(&[1; 50]);
        encoder.add_symbol(&[2; 60]);
        encoder.add_symbol(&[3; 70]);

        let wire_data = encoder.flush();
        assert!(!wire_data.is_empty());

        // Verify block_id advanced.
        assert_eq!(encoder.block_id, 1);
    }

    // -- Crypto tests -------------------------------------------------------

    #[test]
    fn crypto_encrypt_decrypt_roundtrip() {
        let key = [0x42u8; 32];
        let mut alice = WzpCryptoSession::new(&key).unwrap();
        let mut bob = WzpCryptoSession::new(&key).unwrap();

        let header = b"test-header";
        let plaintext = b"hello warzone from wasm";

        let ciphertext = alice.encrypt(header, plaintext).unwrap();
        let decrypted = bob.decrypt(header, &ciphertext).unwrap();

        assert_eq!(&decrypted, plaintext);
    }

    // NOTE: crypto_wrong_aad_fails and crypto_wrong_key_fails return
    // Err(JsValue) which aborts on non-wasm32 (JsValue::from_str uses an
    // extern "C" shim that panics with "cannot unwind").  These tests are
    // gated to wasm32-only; on native the encrypt/decrypt roundtrip and
    // nonce-layout tests provide sufficient coverage.

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn crypto_wrong_aad_fails() {
        let key = [0x42u8; 32];
        let mut alice = WzpCryptoSession::new(&key).unwrap();
        let mut bob = WzpCryptoSession::new(&key).unwrap();

        let ciphertext = alice.encrypt(b"correct", b"secret").unwrap();
        let result = bob.decrypt(b"wrong", &ciphertext);
        assert!(result.is_err());
    }

    #[cfg(target_arch = "wasm32")]
    #[test]
    fn crypto_wrong_key_fails() {
        let mut alice = WzpCryptoSession::new(&[0xAA; 32]).unwrap();
        let mut eve = WzpCryptoSession::new(&[0xBB; 32]).unwrap();

        let ciphertext = alice.encrypt(b"hdr", b"secret").unwrap();
        let result = eve.decrypt(b"hdr", &ciphertext);
        assert!(result.is_err());
    }

    #[test]
    fn crypto_multiple_packets() {
        let key = [0x42u8; 32];
        let mut alice = WzpCryptoSession::new(&key).unwrap();
        let mut bob = WzpCryptoSession::new(&key).unwrap();

        for i in 0..100u32 {
            let msg = format!("message {}", i);
            let ct = alice.encrypt(b"hdr", msg.as_bytes()).unwrap();
            let pt = bob.decrypt(b"hdr", &ct).unwrap();
            assert_eq!(pt, msg.as_bytes());
        }
        assert_eq!(alice.send_seq(), 100);
        assert_eq!(bob.recv_seq(), 100);
    }

    #[test]
    fn key_exchange_roundtrip() {
        let alice_kx = WzpKeyExchange::new();
        let bob_kx = WzpKeyExchange::new();

        let alice_secret = alice_kx
            .derive_shared_secret(&bob_kx.public_key())
            .unwrap();
        let bob_secret = bob_kx
            .derive_shared_secret(&alice_kx.public_key())
            .unwrap();

        assert_eq!(alice_secret, bob_secret);
        assert_eq!(alice_secret.len(), 32);

        // Verify the derived secret actually works for encrypt/decrypt.
        let mut alice_session = WzpCryptoSession::new(&alice_secret).unwrap();
        let mut bob_session = WzpCryptoSession::new(&bob_secret).unwrap();

        let ct = alice_session.encrypt(b"hdr", b"hello").unwrap();
        let pt = bob_session.decrypt(b"hdr", &ct).unwrap();
        assert_eq!(&pt, b"hello");
    }

    #[test]
    fn nonce_layout_matches_native() {
        // Verify our build_nonce matches wzp-crypto::nonce::build_nonce layout.
        let sid = [0xAA, 0xBB, 0xCC, 0xDD];
        let seq: u32 = 0x00000100;
        let nonce = build_nonce(&sid, seq, 1); // 1 = Recv direction
        assert_eq!(&nonce[0..4], &[0xAA, 0xBB, 0xCC, 0xDD]);
        assert_eq!(&nonce[4..8], &[0x00, 0x00, 0x01, 0x00]);
        assert_eq!(nonce[8], 1);
        assert_eq!(&nonce[9..12], &[0, 0, 0]);
    }
}
