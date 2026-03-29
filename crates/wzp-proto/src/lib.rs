//! WarzonePhone Protocol — shared types, traits, and core logic.
//!
//! This crate defines the contracts between all other wzp-* crates.
//! It contains:
//! - Wire format types (MediaHeader, MediaPacket, SignalMessage)
//! - Codec, FEC, crypto, and transport trait definitions
//! - Adaptive quality controller
//! - Jitter buffer
//! - Session state machine
//!
//! Compatible with the Warzone messenger identity model:
//! - Identity = 32-byte seed → HKDF → Ed25519 (signing) + X25519 (encryption)
//! - Fingerprint = SHA-256(Ed25519 pub)[:16]

pub mod bandwidth;
pub mod codec_id;
pub mod error;
pub mod jitter;
pub mod packet;
pub mod quality;
pub mod session;
pub mod traits;

// Re-export key types at crate root for convenience.
pub use codec_id::{CodecId, QualityProfile};
pub use error::*;
pub use packet::{
    HangupReason, MediaHeader, MediaPacket, MiniFrameContext, MiniHeader, QualityReport,
    SignalMessage, TrunkEntry, TrunkFrame, FRAME_TYPE_FULL, FRAME_TYPE_MINI,
};
pub use bandwidth::{BandwidthEstimator, CongestionState};
pub use quality::{AdaptiveQualityController, Tier};
pub use session::{Session, SessionEvent, SessionState};
pub use traits::*;
