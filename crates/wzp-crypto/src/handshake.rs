//! Warzone identity key exchange.
//!
//! Implements the `KeyExchange` trait from `wzp-proto`:
//! - Identity: 32-byte seed -> HKDF -> Ed25519 (signing) + X25519 (encryption)
//! - Fingerprint: SHA-256(Ed25519 pub)[:16]
//! - Per-call: ephemeral X25519 -> ChaCha20-Poly1305 session

use ed25519_dalek::{Signer, SigningKey, Verifier, VerifyingKey};
use hkdf::Hkdf;
use rand::rngs::OsRng;
use sha2::{Digest, Sha256};
use x25519_dalek::{PublicKey as X25519PublicKey, StaticSecret};
use wzp_proto::{CryptoError, CryptoSession, KeyExchange};

use crate::session::ChaChaSession;

/// Warzone-compatible key exchange implementation.
pub struct WarzoneKeyExchange {
    /// Ed25519 signing key (identity).
    signing_key: SigningKey,
    /// X25519 static secret derived from identity seed. Reserved for future
    /// use in static-key federation authentication (not used in current
    /// ephemeral-only handshake protocol).
    #[allow(dead_code)]
    x25519_static_secret: StaticSecret,
    /// X25519 static public key derived from identity seed. Reserved for
    /// future use in static-key federation authentication (not used in
    /// current ephemeral-only handshake protocol).
    #[allow(dead_code)]
    x25519_static_public: X25519PublicKey,
    /// Ephemeral X25519 secret for the current call (set by generate_ephemeral).
    ephemeral_secret: Option<StaticSecret>,
}

impl KeyExchange for WarzoneKeyExchange {
    fn from_identity_seed(seed: &[u8; 32]) -> Self {
        // Derive Ed25519 signing key via HKDF
        let hk = Hkdf::<Sha256>::new(None, seed);
        let mut ed25519_bytes = [0u8; 32];
        hk.expand(b"warzone-ed25519", &mut ed25519_bytes)
            .expect("HKDF expand for Ed25519 should not fail");
        let signing_key = SigningKey::from_bytes(&ed25519_bytes);

        // Derive X25519 static key via HKDF
        let mut x25519_bytes = [0u8; 32];
        hk.expand(b"warzone-x25519", &mut x25519_bytes)
            .expect("HKDF expand for X25519 should not fail");
        let x25519_static_secret = StaticSecret::from(x25519_bytes);
        let x25519_static_public = X25519PublicKey::from(&x25519_static_secret);

        Self {
            signing_key,
            x25519_static_secret,
            x25519_static_public,
            ephemeral_secret: None,
        }
    }

    fn generate_ephemeral(&mut self) -> [u8; 32] {
        let secret = StaticSecret::random_from_rng(OsRng);
        let public = X25519PublicKey::from(&secret);
        self.ephemeral_secret = Some(secret);
        public.to_bytes()
    }

    fn identity_public_key(&self) -> [u8; 32] {
        self.signing_key.verifying_key().to_bytes()
    }

    fn fingerprint(&self) -> [u8; 16] {
        let pub_bytes = self.identity_public_key();
        let hash = Sha256::digest(pub_bytes);
        let mut fp = [0u8; 16];
        fp.copy_from_slice(&hash[..16]);
        fp
    }

    fn sign(&self, data: &[u8]) -> Vec<u8> {
        let sig = self.signing_key.sign(data);
        sig.to_bytes().to_vec()
    }

    fn verify(peer_identity_pub: &[u8; 32], data: &[u8], signature: &[u8]) -> bool {
        let Ok(verifying_key) = VerifyingKey::from_bytes(peer_identity_pub) else {
            return false;
        };
        let Ok(sig_bytes) = <[u8; 64]>::try_from(signature) else {
            return false;
        };
        let sig = ed25519_dalek::Signature::from_bytes(&sig_bytes);
        verifying_key.verify(data, &sig).is_ok()
    }

    fn derive_session(
        &self,
        peer_ephemeral_pub: &[u8; 32],
    ) -> Result<Box<dyn CryptoSession>, CryptoError> {
        let secret = self
            .ephemeral_secret
            .as_ref()
            .ok_or_else(|| {
                CryptoError::Internal("no ephemeral key generated; call generate_ephemeral first".into())
            })?;

        let peer_public = X25519PublicKey::from(*peer_ephemeral_pub);
        // Use diffie_hellman with a clone of the StaticSecret
        let secret_bytes: [u8; 32] = secret.to_bytes();
        let secret_clone = StaticSecret::from(secret_bytes);
        let shared_secret = secret_clone.diffie_hellman(&peer_public);

        // Expand shared secret via HKDF
        let hk = Hkdf::<Sha256>::new(None, shared_secret.as_bytes());
        let mut session_key = [0u8; 32];
        hk.expand(b"warzone-session-key", &mut session_key)
            .expect("HKDF expand for session key should not fail");

        // Derive SAS (Short Authentication String) from shared secret only.
        // The shared secret is identical on both sides (X25519 DH property).
        // A MITM would produce a different shared secret → different SAS.
        // We use a dedicated HKDF label so SAS is independent of the session key.
        let mut sas_key = [0u8; 4];
        hk.expand(b"warzone-sas-code", &mut sas_key)
            .expect("HKDF expand for SAS should not fail");
        let sas_code = u32::from_be_bytes(sas_key) % 10000;

        let mut session = ChaChaSession::new(session_key);
        session.set_sas(sas_code);
        Ok(Box::new(session))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn deterministic_identity_from_seed() {
        let seed = [0x42u8; 32];
        let kx1 = WarzoneKeyExchange::from_identity_seed(&seed);
        let kx2 = WarzoneKeyExchange::from_identity_seed(&seed);
        assert_eq!(kx1.identity_public_key(), kx2.identity_public_key());
        assert_eq!(kx1.fingerprint(), kx2.fingerprint());
    }

    #[test]
    fn different_seeds_different_keys() {
        let kx1 = WarzoneKeyExchange::from_identity_seed(&[0x01; 32]);
        let kx2 = WarzoneKeyExchange::from_identity_seed(&[0x02; 32]);
        assert_ne!(kx1.identity_public_key(), kx2.identity_public_key());
    }

    #[test]
    fn fingerprint_is_16_bytes_of_sha256() {
        let seed = [0x99u8; 32];
        let kx = WarzoneKeyExchange::from_identity_seed(&seed);
        let fp = kx.fingerprint();
        assert_eq!(fp.len(), 16);

        // Verify manually
        let pub_key = kx.identity_public_key();
        let hash = Sha256::digest(pub_key);
        assert_eq!(&fp[..], &hash[..16]);
    }

    #[test]
    fn sign_and_verify() {
        let seed = [0xAA; 32];
        let kx = WarzoneKeyExchange::from_identity_seed(&seed);
        let data = b"hello warzone";
        let sig = kx.sign(data);
        assert!(WarzoneKeyExchange::verify(
            &kx.identity_public_key(),
            data,
            &sig
        ));
    }

    #[test]
    fn verify_wrong_data_fails() {
        let seed = [0xAA; 32];
        let kx = WarzoneKeyExchange::from_identity_seed(&seed);
        let sig = kx.sign(b"correct data");
        assert!(!WarzoneKeyExchange::verify(
            &kx.identity_public_key(),
            b"wrong data",
            &sig
        ));
    }

    #[test]
    fn verify_wrong_key_fails() {
        let kx1 = WarzoneKeyExchange::from_identity_seed(&[0x01; 32]);
        let kx2 = WarzoneKeyExchange::from_identity_seed(&[0x02; 32]);
        let sig = kx1.sign(b"data");
        assert!(!WarzoneKeyExchange::verify(
            &kx2.identity_public_key(),
            b"data",
            &sig
        ));
    }

    #[test]
    fn full_handshake_alice_bob_same_session_key() {
        let mut alice = WarzoneKeyExchange::from_identity_seed(&[0xAA; 32]);
        let mut bob = WarzoneKeyExchange::from_identity_seed(&[0xBB; 32]);

        let alice_eph_pub = alice.generate_ephemeral();
        let bob_eph_pub = bob.generate_ephemeral();

        let mut alice_session = alice.derive_session(&bob_eph_pub).unwrap();
        let mut bob_session = bob.derive_session(&alice_eph_pub).unwrap();

        // Verify they can communicate: Alice encrypts, Bob decrypts
        let header = b"call-header";
        let plaintext = b"hello from alice";

        let mut ciphertext = Vec::new();
        alice_session
            .encrypt(header, plaintext, &mut ciphertext)
            .unwrap();

        let mut decrypted = Vec::new();
        bob_session
            .decrypt(header, &ciphertext, &mut decrypted)
            .unwrap();

        assert_eq!(&decrypted, plaintext);
    }

    #[test]
    fn sas_codes_match_between_peers() {
        let mut alice = WarzoneKeyExchange::from_identity_seed(&[0xAA; 32]);
        let mut bob = WarzoneKeyExchange::from_identity_seed(&[0xBB; 32]);

        let alice_eph_pub = alice.generate_ephemeral();
        let bob_eph_pub = bob.generate_ephemeral();

        let alice_session = alice.derive_session(&bob_eph_pub).unwrap();
        let bob_session = bob.derive_session(&alice_eph_pub).unwrap();

        let alice_sas = alice_session.sas_code();
        let bob_sas = bob_session.sas_code();

        assert!(alice_sas.is_some(), "Alice should have SAS");
        assert!(bob_sas.is_some(), "Bob should have SAS");
        assert_eq!(alice_sas, bob_sas, "SAS codes must match between peers");
        assert!(alice_sas.unwrap() < 10000, "SAS should be 4 digits");
    }

    #[test]
    fn sas_differs_for_different_peers() {
        let mut alice = WarzoneKeyExchange::from_identity_seed(&[0xAA; 32]);
        let mut bob = WarzoneKeyExchange::from_identity_seed(&[0xBB; 32]);
        let mut eve = WarzoneKeyExchange::from_identity_seed(&[0xEE; 32]);

        let alice_eph = alice.generate_ephemeral();
        let bob_eph = bob.generate_ephemeral();
        let eve_eph = eve.generate_ephemeral();

        let alice_bob_session = alice.derive_session(&bob_eph).unwrap();

        // Eve does separate handshake with Bob (MITM scenario)
        let eve_bob_session = eve.derive_session(&bob_eph).unwrap();

        // SAS codes should differ — Eve's session has different shared secret
        assert_ne!(
            alice_bob_session.sas_code(),
            eve_bob_session.sas_code(),
            "MITM session should produce different SAS"
        );
    }
}
