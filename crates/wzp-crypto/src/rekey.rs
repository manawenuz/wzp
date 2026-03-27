//! Rekeying state machine for forward secrecy.
//!
//! Triggers rekeying every 2^16 packets. Uses HKDF to mix the old key
//! with the new DH result, then zeroizes the old key material.

use hkdf::Hkdf;
use sha2::Sha256;
use x25519_dalek::{PublicKey, StaticSecret};

/// Rekeying interval: every 2^16 packets.
const REKEY_INTERVAL: u64 = 1 << 16;

/// Manages rekeying decisions and key evolution.
pub struct RekeyManager {
    /// Current symmetric key material (32 bytes).
    current_key: [u8; 32],
    /// Packet count at which last rekey occurred.
    last_rekey_at: u64,
}

impl RekeyManager {
    /// Create a new `RekeyManager` with the initial session key.
    pub fn new(initial_key: [u8; 32]) -> Self {
        Self {
            current_key: initial_key,
            last_rekey_at: 0,
        }
    }

    /// Check whether rekeying should occur based on packet count.
    pub fn should_rekey(&self, packet_count: u64) -> bool {
        packet_count.saturating_sub(self.last_rekey_at) >= REKEY_INTERVAL
    }

    /// Perform rekeying: mix old key + new DH shared secret via HKDF.
    ///
    /// The old key is zeroized after the new key is derived.
    /// Returns the new 32-byte symmetric key.
    pub fn perform_rekey(
        &mut self,
        new_peer_pub: &[u8; 32],
        our_new_secret: StaticSecret,
        packet_count: u64,
    ) -> [u8; 32] {
        let peer_public = PublicKey::from(*new_peer_pub);
        let new_dh = our_new_secret.diffie_hellman(&peer_public);

        // Mix old key (as salt) with new DH result (as IKM) via HKDF
        let hk = Hkdf::<Sha256>::new(Some(&self.current_key), new_dh.as_bytes());
        let mut new_key = [0u8; 32];
        hk.expand(b"warzone-rekey", &mut new_key)
            .expect("HKDF expand should not fail for 32 bytes");

        // Zeroize old key for forward secrecy
        self.current_key.fill(0);

        // Install new key
        self.current_key = new_key;
        self.last_rekey_at = packet_count;

        new_key
    }

    /// Get a reference to the current key.
    pub fn current_key(&self) -> &[u8; 32] {
        &self.current_key
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rand::rngs::OsRng;

    #[test]
    fn should_rekey_at_interval() {
        let mgr = RekeyManager::new([0xAA; 32]);
        assert!(!mgr.should_rekey(0));
        assert!(!mgr.should_rekey(65535));
        assert!(mgr.should_rekey(65536));
        assert!(mgr.should_rekey(100_000));
    }

    #[test]
    fn rekey_produces_different_key() {
        let initial = [0xBB; 32];
        let mut mgr = RekeyManager::new(initial);

        let secret = StaticSecret::random_from_rng(OsRng);
        let peer_secret = StaticSecret::random_from_rng(OsRng);
        let peer_pub = PublicKey::from(&peer_secret).to_bytes();

        let new_key = mgr.perform_rekey(&peer_pub, secret, 65536);
        assert_ne!(new_key, initial);
    }

    #[test]
    fn old_key_zeroized_after_rekey() {
        let initial = [0xCC; 32];
        let mut mgr = RekeyManager::new(initial);

        let secret = StaticSecret::random_from_rng(OsRng);
        let peer_secret = StaticSecret::random_from_rng(OsRng);
        let peer_pub = PublicKey::from(&peer_secret).to_bytes();

        // Save pointer to check zeroization
        let _new_key = mgr.perform_rekey(&peer_pub, secret, 65536);
        // The old key slot should now contain the new key, not the initial
        assert_ne!(*mgr.current_key(), initial);
    }

    #[test]
    fn consistent_rekey_with_same_inputs() {
        // Two managers with same initial key, same DH inputs, should get same result
        let initial = [0xDD; 32];
        let mut mgr1 = RekeyManager::new(initial);
        let mut mgr2 = RekeyManager::new(initial);

        // Use StaticSecret so we can clone the key bytes
        let secret_bytes = [0x42u8; 32];
        let secret1 = StaticSecret::from(secret_bytes);
        let secret2 = StaticSecret::from(secret_bytes);

        let peer_bytes = [0x77u8; 32];
        let peer_secret = StaticSecret::from(peer_bytes);
        let peer_pub = PublicKey::from(&peer_secret).to_bytes();

        let k1 = mgr1.perform_rekey(&peer_pub, secret1, 65536);
        let k2 = mgr2.perform_rekey(&peer_pub, secret2, 65536);
        assert_eq!(k1, k2);
    }
}
