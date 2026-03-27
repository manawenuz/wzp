//! WarzonePhone Codec Layer
//!
//! Provides audio encoding/decoding with adaptive codec switching:
//! - Opus (24kbps / 16kbps / 6kbps) for normal to degraded conditions
//! - Codec2 (3200bps / 1200bps) via C bindings for catastrophic conditions
//!
//! ## Usage
//!
//! Use the factory functions [`create_encoder`] and [`create_decoder`] to get
//! trait-object encoders/decoders that handle adaptive switching internally.

pub mod adaptive;
pub mod codec2_dec;
pub mod codec2_enc;
pub mod opus_dec;
pub mod opus_enc;
pub mod resample;

pub use adaptive::{AdaptiveDecoder, AdaptiveEncoder};
pub use wzp_proto::{AudioDecoder, AudioEncoder, CodecId, QualityProfile};

/// Create an adaptive encoder starting at the given quality profile.
///
/// The returned encoder accepts 48 kHz mono PCM regardless of the active
/// codec; resampling is handled internally when Codec2 is selected.
pub fn create_encoder(profile: QualityProfile) -> Box<dyn AudioEncoder> {
    Box::new(
        AdaptiveEncoder::new(profile)
            .expect("failed to create adaptive encoder"),
    )
}

/// Create an adaptive decoder starting at the given quality profile.
///
/// The returned decoder always produces 48 kHz mono PCM; upsampling from
/// Codec2's native 8 kHz is handled internally.
pub fn create_decoder(profile: QualityProfile) -> Box<dyn AudioDecoder> {
    Box::new(
        AdaptiveDecoder::new(profile)
            .expect("failed to create adaptive decoder"),
    )
}
