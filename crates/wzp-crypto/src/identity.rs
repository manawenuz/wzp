//! featherChat-compatible identity module.
//!
//! Mirrors `warzone-protocol/src/identity.rs` and `warzone-protocol/src/mnemonic.rs`
//! from featherChat. Same seed → same keys → same fingerprint in both codebases.
//!
//! Source of truth: deps/featherchat/warzone/crates/warzone-protocol/src/identity.rs

use ed25519_dalek::{SigningKey, VerifyingKey};
use hkdf::Hkdf;
use sha2::{Digest, Sha256};
use x25519_dalek::StaticSecret;

/// The root secret — 32 bytes from which all keys are derived.
/// Displayed to users as a BIP39 mnemonic (24 words).
///
/// Mirrors: `warzone-protocol::identity::Seed`
pub struct Seed(pub [u8; 32]);

impl Seed {
    /// Generate a new random seed.
    pub fn generate() -> Self {
        let mut bytes = [0u8; 32];
        rand::RngCore::fill_bytes(&mut rand::rngs::OsRng, &mut bytes);
        Seed(bytes)
    }

    /// Create seed from raw bytes.
    pub fn from_bytes(bytes: [u8; 32]) -> Self {
        Seed(bytes)
    }

    /// Create seed from hex string (64 hex chars).
    pub fn from_hex(hex_str: &str) -> Result<Self, String> {
        let bytes = hex::decode(hex_str).map_err(|e| format!("invalid hex: {e}"))?;
        if bytes.len() != 32 {
            return Err(format!("expected 32 bytes, got {}", bytes.len()));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&bytes);
        Ok(Seed(seed))
    }

    /// Derive the full identity keypair from this seed.
    ///
    /// Uses identical HKDF derivation as featherChat:
    /// - Ed25519: `HKDF(seed, salt=None, info="warzone-ed25519")`
    /// - X25519:  `HKDF(seed, salt=None, info="warzone-x25519")`
    pub fn derive_identity(&self) -> IdentityKeyPair {
        let hk = Hkdf::<Sha256>::new(None, &self.0);

        let mut ed_bytes = [0u8; 32];
        hk.expand(b"warzone-ed25519", &mut ed_bytes)
            .expect("HKDF expand for Ed25519");
        let signing = SigningKey::from_bytes(&ed_bytes);
        ed_bytes.fill(0);

        let mut x_bytes = [0u8; 32];
        hk.expand(b"warzone-x25519", &mut x_bytes)
            .expect("HKDF expand for X25519");
        let encryption = StaticSecret::from(x_bytes);
        x_bytes.fill(0);

        IdentityKeyPair {
            signing,
            encryption,
        }
    }

    /// Convert to BIP39 mnemonic (24 words).
    ///
    /// Mirrors: `warzone-protocol::mnemonic::seed_to_mnemonic`
    pub fn to_mnemonic(&self) -> String {
        let mnemonic =
            bip39::Mnemonic::from_entropy(&self.0).expect("32 bytes is valid BIP39 entropy");
        mnemonic.to_string()
    }

    /// Recover seed from BIP39 mnemonic (24 words).
    ///
    /// Mirrors: `warzone-protocol::mnemonic::mnemonic_to_seed`
    pub fn from_mnemonic(words: &str) -> Result<Self, String> {
        let mnemonic: bip39::Mnemonic = words.parse().map_err(|e| format!("invalid mnemonic: {e}"))?;
        let entropy = mnemonic.to_entropy();
        if entropy.len() != 32 {
            return Err(format!("expected 32 bytes entropy, got {}", entropy.len()));
        }
        let mut seed = [0u8; 32];
        seed.copy_from_slice(&entropy);
        Ok(Seed(seed))
    }
}

impl Drop for Seed {
    fn drop(&mut self) {
        self.0.fill(0); // zeroize on drop
    }
}

/// The full identity keypair derived from a seed.
///
/// Mirrors: `warzone-protocol::identity::IdentityKeyPair`
pub struct IdentityKeyPair {
    pub signing: SigningKey,
    pub encryption: StaticSecret,
}

impl IdentityKeyPair {
    /// Get the public identity (safe to share).
    pub fn public_identity(&self) -> PublicIdentity {
        let verifying = self.signing.verifying_key();
        let encryption_pub = x25519_dalek::PublicKey::from(&self.encryption);
        let fingerprint = Fingerprint::from_verifying_key(&verifying);

        PublicIdentity {
            signing: verifying,
            encryption: encryption_pub,
            fingerprint,
        }
    }
}

/// Truncated SHA-256 hash of the Ed25519 public key (16 bytes).
/// Displayed as `xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx`.
///
/// Mirrors: `warzone-protocol::types::Fingerprint`
#[derive(Clone, Copy, PartialEq, Eq, Hash)]
pub struct Fingerprint(pub [u8; 16]);

impl Fingerprint {
    pub fn from_verifying_key(key: &VerifyingKey) -> Self {
        let hash = Sha256::digest(key.as_bytes());
        let mut fp = [0u8; 16];
        fp.copy_from_slice(&hash[..16]);
        Fingerprint(fp)
    }

    /// Parse from hex string (with or without colons).
    pub fn from_hex(s: &str) -> Result<Self, String> {
        let clean: String = s.chars().filter(|c| c.is_ascii_hexdigit()).collect();
        let bytes = hex::decode(&clean).map_err(|e| format!("invalid hex: {e}"))?;
        if bytes.len() < 16 {
            return Err("fingerprint too short".to_string());
        }
        let mut fp = [0u8; 16];
        fp.copy_from_slice(&bytes[..16]);
        Ok(Fingerprint(fp))
    }

    /// As raw bytes.
    pub fn as_bytes(&self) -> &[u8; 16] {
        &self.0
    }

    /// As hex string without colons.
    pub fn to_hex(&self) -> String {
        hex::encode(self.0)
    }
}

impl std::fmt::Display for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(
            f,
            "{:04x}:{:04x}:{:04x}:{:04x}:{:04x}:{:04x}:{:04x}:{:04x}",
            u16::from_be_bytes([self.0[0], self.0[1]]),
            u16::from_be_bytes([self.0[2], self.0[3]]),
            u16::from_be_bytes([self.0[4], self.0[5]]),
            u16::from_be_bytes([self.0[6], self.0[7]]),
            u16::from_be_bytes([self.0[8], self.0[9]]),
            u16::from_be_bytes([self.0[10], self.0[11]]),
            u16::from_be_bytes([self.0[12], self.0[13]]),
            u16::from_be_bytes([self.0[14], self.0[15]]),
        )
    }
}

impl std::fmt::Debug for Fingerprint {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "Fingerprint({})", self)
    }
}

/// The public portion of an identity — safe to share with anyone.
pub struct PublicIdentity {
    pub signing: VerifyingKey,
    pub encryption: x25519_dalek::PublicKey,
    pub fingerprint: Fingerprint,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_derivation() {
        let seed = Seed::from_bytes([42u8; 32]);
        let id1 = seed.derive_identity();
        let id2 = seed.derive_identity();
        assert_eq!(
            id1.signing.verifying_key().as_bytes(),
            id2.signing.verifying_key().as_bytes(),
        );
    }

    #[test]
    fn mnemonic_roundtrip() {
        let seed = Seed::generate();
        let words = seed.to_mnemonic();
        let word_count = words.split_whitespace().count();
        assert_eq!(word_count, 24);
        let recovered = Seed::from_mnemonic(&words).unwrap();
        assert_eq!(seed.0, recovered.0);
    }

    #[test]
    fn hex_roundtrip() {
        let seed = Seed::generate();
        let hex_str = hex::encode(seed.0);
        let recovered = Seed::from_hex(&hex_str).unwrap();
        assert_eq!(seed.0, recovered.0);
    }

    #[test]
    fn fingerprint_format() {
        let seed = Seed::generate();
        let id = seed.derive_identity();
        let pub_id = id.public_identity();
        let fp_str = pub_id.fingerprint.to_string();
        // Format: xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx
        assert_eq!(fp_str.len(), 39);
        assert_eq!(fp_str.chars().filter(|c| *c == ':').count(), 7);
    }

    #[test]
    fn matches_handshake_derivation() {
        use wzp_proto::KeyExchange;
        // Verify identity module matches the KeyExchange trait implementation
        let seed = [99u8; 32];
        let id = Seed::from_bytes(seed).derive_identity();
        let kx = crate::WarzoneKeyExchange::from_identity_seed(&seed);

        assert_eq!(
            id.signing.verifying_key().as_bytes(),
            &kx.identity_public_key(),
        );
        assert_eq!(
            id.public_identity().fingerprint.as_bytes(),
            &kx.fingerprint(),
        );
    }
}
