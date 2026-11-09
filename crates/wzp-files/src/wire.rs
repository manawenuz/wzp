//! Wire framing and end-to-end encryption for wzp-files streams.
//!
//! Every wzp-files QUIC unidirectional stream starts with a 5-byte preamble
//! `MAGIC(4) || WIRE_VERSION(1)` followed by a 16-byte random salt. After that
//! the stream is a sequence of frames `len: u32 BE || ciphertext`, where the
//! ciphertext is XChaCha20-Poly1305 over the frame plaintext with
//! `nonce = salt(16) || frame_counter(u64 BE)` and `aad = MAGIC`.
//!
//! The control stream (`WZPC`) is special only in that a single plaintext,
//! signed [`Hello`] follows the preamble before encrypted frames begin; the
//! salt is carried inside the Hello.
//!
//! Keys are per direction (one key per sender, derived from the sender's
//! ephemeral X25519 key), so the two peers never share a (key, nonce) pair.

use chacha20poly1305::aead::{Aead, KeyInit, Payload};
use chacha20poly1305::{XChaCha20Poly1305, XNonce};
use hkdf::Hkdf;
use rand::RngCore;
use sha2::{Digest, Sha256};
use wzp_crypto::{KeyExchange, WarzoneKeyExchange};

use crate::FileError;

pub(crate) const MAGIC_CONTROL: [u8; 4] = *b"WZPC";
pub(crate) const MAGIC_DATA: [u8; 4] = *b"WZPD";
pub(crate) const WIRE_VERSION: u8 = 1;

/// Max plaintext per data frame.
pub(crate) const DATA_FRAME_MAX: usize = 64 * 1024;
/// Max ciphertext frame accepted on a data stream.
pub(crate) const DATA_FRAME_LIMIT: usize = DATA_FRAME_MAX + 64;
/// Max ciphertext frame accepted on the control stream (the chunk-hash
/// manifest is the largest control frame: 32 B × MAX_CHUNKS + header).
pub(crate) const CONTROL_FRAME_LIMIT: usize = 32 * crate::MAX_CHUNKS as usize + 1024;

const HELLO_LABEL: &[u8] = b"wzp-files/v1/hello";
pub(crate) const HELLO_LEN: usize = 32 + 32 + 16 + 64;

/// Signed handshake message sent in clear on the control stream.
#[derive(Clone, Debug)]
pub(crate) struct Hello {
    pub identity_pub: [u8; 32],
    pub ephemeral_pub: [u8; 32],
    pub salt: [u8; 16],
    pub signature: [u8; 64],
}

pub(crate) fn context_hash(context: &str) -> [u8; 32] {
    let mut h = Sha256::new();
    h.update(b"wzp-files/v1/context");
    h.update(context.as_bytes());
    h.finalize().into()
}

fn hello_signed_bytes(ctx: &[u8; 32], eph: &[u8; 32], salt: &[u8; 16]) -> Vec<u8> {
    let mut v = Vec::with_capacity(HELLO_LABEL.len() + 32 + 32 + 16);
    v.extend_from_slice(HELLO_LABEL);
    v.extend_from_slice(ctx);
    v.extend_from_slice(eph);
    v.extend_from_slice(salt);
    v
}

impl Hello {
    pub fn new(kx: &WarzoneKeyExchange, ephemeral_pub: [u8; 32], ctx: &[u8; 32]) -> Self {
        let mut salt = [0u8; 16];
        rand::thread_rng().fill_bytes(&mut salt);
        let sig = kx.sign(&hello_signed_bytes(ctx, &ephemeral_pub, &salt));
        let mut signature = [0u8; 64];
        signature.copy_from_slice(&sig);
        Self {
            identity_pub: kx.identity_public_key(),
            ephemeral_pub,
            salt,
            signature,
        }
    }

    pub fn verify(&self, ctx: &[u8; 32]) -> bool {
        WarzoneKeyExchange::verify(
            &self.identity_pub,
            &hello_signed_bytes(ctx, &self.ephemeral_pub, &self.salt),
            &self.signature,
        )
    }

    pub fn encode(&self) -> [u8; HELLO_LEN] {
        let mut out = [0u8; HELLO_LEN];
        out[..32].copy_from_slice(&self.identity_pub);
        out[32..64].copy_from_slice(&self.ephemeral_pub);
        out[64..80].copy_from_slice(&self.salt);
        out[80..].copy_from_slice(&self.signature);
        out
    }

    pub fn decode(b: &[u8; HELLO_LEN]) -> Self {
        let mut h = Hello {
            identity_pub: [0; 32],
            ephemeral_pub: [0; 32],
            salt: [0; 16],
            signature: [0; 64],
        };
        h.identity_pub.copy_from_slice(&b[..32]);
        h.ephemeral_pub.copy_from_slice(&b[32..64]);
        h.salt.copy_from_slice(&b[64..80]);
        h.signature.copy_from_slice(&b[80..]);
        h
    }
}

/// Fingerprint as used everywhere in WZP: SHA-256(Ed25519 pub)[:16].
pub(crate) fn fingerprint_of(identity_pub: &[u8; 32]) -> [u8; 16] {
    let h = Sha256::digest(identity_pub);
    let mut fp = [0u8; 16];
    fp.copy_from_slice(&h[..16]);
    fp
}

/// Derived session keys.
pub(crate) struct SessionKeys {
    pub send: [u8; 32],
    pub recv: [u8; 32],
    pub sas: u32,
}

pub(crate) fn derive_keys(
    shared: &[u8; 32],
    ctx: &[u8; 32],
    my_eph: &[u8; 32],
    peer_eph: &[u8; 32],
) -> SessionKeys {
    let (lo, hi) = if my_eph <= peer_eph {
        (my_eph, peer_eph)
    } else {
        (peer_eph, my_eph)
    };
    let mut t = Sha256::new();
    t.update(b"wzp-files/v1/transcript");
    t.update(ctx);
    t.update(lo);
    t.update(hi);
    let transcript: [u8; 32] = t.finalize().into();
    let hk = Hkdf::<Sha256>::new(Some(&transcript), shared);
    let key_for = |eph: &[u8; 32]| {
        let mut info = b"wzp-files/v1/key/".to_vec();
        info.extend_from_slice(eph);
        let mut k = [0u8; 32];
        hk.expand(&info, &mut k).expect("hkdf expand");
        k
    };
    let mut sas = [0u8; 4];
    hk.expand(b"wzp-files/v1/sas", &mut sas)
        .expect("hkdf expand");
    SessionKeys {
        send: key_for(my_eph),
        recv: key_for(peer_eph),
        sas: u32::from_be_bytes(sas) % 10000,
    }
}

fn nonce(salt: &[u8; 16], ctr: u64) -> XNonce {
    let mut n = [0u8; 24];
    n[..16].copy_from_slice(salt);
    n[16..].copy_from_slice(&ctr.to_be_bytes());
    XNonce::from(n)
}

pub(crate) fn random_salt() -> [u8; 16] {
    let mut s = [0u8; 16];
    rand::thread_rng().fill_bytes(&mut s);
    s
}

/// Encrypting frame writer over a QUIC send stream.
pub(crate) struct SealWriter {
    stream: quinn::SendStream,
    cipher: XChaCha20Poly1305,
    salt: [u8; 16],
    magic: [u8; 4],
    ctr: u64,
}

impl SealWriter {
    pub fn new(stream: quinn::SendStream, key: &[u8; 32], salt: [u8; 16], magic: [u8; 4]) -> Self {
        Self {
            stream,
            cipher: XChaCha20Poly1305::new_from_slice(key).expect("32-byte key"),
            salt,
            magic,
            ctr: 0,
        }
    }

    pub async fn send(&mut self, plaintext: &[u8]) -> Result<(), FileError> {
        let ct = self
            .cipher
            .encrypt(
                &nonce(&self.salt, self.ctr),
                Payload {
                    msg: plaintext,
                    aad: &self.magic,
                },
            )
            .map_err(|_| FileError::Protocol("encrypt failed".into()))?;
        self.ctr += 1;
        let mut buf = Vec::with_capacity(4 + ct.len());
        buf.extend_from_slice(&(ct.len() as u32).to_be_bytes());
        buf.extend_from_slice(&ct);
        self.stream
            .write_all(&buf)
            .await
            .map_err(|e| FileError::Transport(format!("stream write: {e}")))
    }

    pub fn finish(&mut self) {
        let _ = self.stream.finish();
    }

    pub fn reset(&mut self, code: u32) {
        let _ = self.stream.reset(quinn::VarInt::from_u32(code));
    }

    /// Wait until the peer stops the stream; returns its error code.
    #[cfg(test)]
    pub async fn stopped(&mut self) -> Option<u32> {
        match self.stream.stopped().await {
            Ok(Some(code)) => Some(code.into_inner() as u32),
            _ => None,
        }
    }
}

/// Decrypting frame reader over a QUIC recv stream.
pub(crate) struct OpenReader {
    stream: quinn::RecvStream,
    cipher: XChaCha20Poly1305,
    salt: [u8; 16],
    magic: [u8; 4],
    ctr: u64,
    limit: usize,
}

impl OpenReader {
    pub fn new(
        stream: quinn::RecvStream,
        key: &[u8; 32],
        salt: [u8; 16],
        magic: [u8; 4],
        limit: usize,
    ) -> Self {
        Self {
            stream,
            cipher: XChaCha20Poly1305::new_from_slice(key).expect("32-byte key"),
            salt,
            magic,
            ctr: 0,
            limit,
        }
    }

    /// Next decrypted frame; `Ok(None)` on clean end of stream at a frame
    /// boundary.
    pub async fn recv(&mut self) -> Result<Option<Vec<u8>>, FileError> {
        let mut len = [0u8; 4];
        match self.stream.read_exact(&mut len).await {
            Ok(()) => {}
            Err(quinn::ReadExactError::FinishedEarly(0)) => return Ok(None),
            Err(e) => return Err(FileError::Transport(format!("stream read: {e}"))),
        }
        let len = u32::from_be_bytes(len) as usize;
        if len > self.limit || len < 16 {
            return Err(FileError::Protocol(format!("bad frame length {len}")));
        }
        let mut ct = vec![0u8; len];
        self.stream
            .read_exact(&mut ct)
            .await
            .map_err(|e| FileError::Transport(format!("stream read: {e}")))?;
        let pt = self
            .cipher
            .decrypt(
                &nonce(&self.salt, self.ctr),
                Payload {
                    msg: &ct,
                    aad: &self.magic,
                },
            )
            .map_err(|_| FileError::Protocol("frame authentication failed".into()))?;
        self.ctr += 1;
        Ok(Some(pt))
    }

    pub fn stop(&mut self, code: u32) {
        let _ = self.stream.stop(quinn::VarInt::from_u32(code));
    }
}

/// Application error codes used on QUIC stream stop/reset.
pub(crate) mod codes {
    pub const UNSOLICITED: u32 = 0x5701;
    pub const PROTOCOL: u32 = 0x5702;
    pub const CANCELLED: u32 = 0x5703;
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn keys_are_mirrored() {
        let shared = [5u8; 32];
        let ctx = context_hash("room");
        let a = [1u8; 32];
        let b = [2u8; 32];
        let ka = derive_keys(&shared, &ctx, &a, &b);
        let kb = derive_keys(&shared, &ctx, &b, &a);
        assert_eq!(ka.send, kb.recv);
        assert_eq!(ka.recv, kb.send);
        assert_ne!(ka.send, ka.recv);
        assert_eq!(ka.sas, kb.sas);
    }

    #[test]
    fn hello_sign_verify() {
        let kx = WarzoneKeyExchange::from_identity_seed(&[3u8; 32]);
        let ctx = context_hash("x");
        let h = Hello::new(&kx, [9u8; 32], &ctx);
        let d = Hello::decode(&h.encode());
        assert!(d.verify(&ctx));
        assert!(!d.verify(&context_hash("y")));
        let mut bad = d.clone();
        bad.ephemeral_pub[0] ^= 1;
        assert!(!bad.verify(&ctx));
    }
}
