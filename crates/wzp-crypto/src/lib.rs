//! WarzonePhone Crypto Layer
//!
//! Implements the cryptographic primitives compatible with the Warzone messenger identity model:
//! - Identity: 32-byte seed -> HKDF -> Ed25519 (signing) + X25519 (encryption)
//! - Fingerprint: SHA-256(Ed25519 pub)[:16]
//! - Per-call: Ephemeral X25519 key exchange -> ChaCha20-Poly1305 session
//! - Nonce: Derived from session_id + seq + direction (not transmitted)
//! - Rekeying: Periodic ephemeral exchange with HKDF mixing for forward secrecy

pub mod anti_replay;
pub mod handshake;
pub mod identity;
pub mod nonce;
pub mod rekey;
pub mod session;

pub use anti_replay::AntiReplayWindow;
pub use handshake::WarzoneKeyExchange;
pub use identity::{hash_room_name, Fingerprint, IdentityKeyPair, PublicIdentity, Seed};
pub use nonce::{build_nonce, Direction};
pub use rekey::RekeyManager;
pub use session::ChaChaSession;

// Re-export trait types from wzp-proto for convenience.
pub use wzp_proto::{CryptoError, CryptoSession, KeyExchange};
