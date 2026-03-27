//! WarzonePhone FEC Layer
//!
//! Forward Error Correction using RaptorQ fountain codes with temporal interleaving.
//!
//! This crate provides:
//! - [`RaptorQFecEncoder`] — accumulates audio frames into FEC blocks and generates repair symbols
//! - [`RaptorQFecDecoder`] — reassembles source blocks from received source and repair symbols
//! - [`Interleaver`] — spreads symbols across blocks to mitigate burst losses
//! - [`BlockManager`](block_manager) — tracks block lifecycle on encoder and decoder sides
//! - [`AdaptiveFec`] — maps quality profiles to FEC parameters

pub mod adaptive;
pub mod block_manager;
pub mod decoder;
pub mod encoder;
pub mod interleave;

pub use adaptive::AdaptiveFec;
pub use block_manager::{DecoderBlockManager, DecoderBlockState, EncoderBlockManager, EncoderBlockState};
pub use decoder::RaptorQFecDecoder;
pub use encoder::RaptorQFecEncoder;
pub use interleave::Interleaver;

pub use wzp_proto::{FecDecoder, FecEncoder, QualityProfile};

/// Create an encoder/decoder pair configured for the given quality profile.
pub fn create_fec_pair(
    profile: &QualityProfile,
) -> (RaptorQFecEncoder, RaptorQFecDecoder) {
    let cfg = AdaptiveFec::from_profile(profile);
    let encoder = cfg.build_encoder();
    let decoder = RaptorQFecDecoder::new(cfg.frames_per_block, cfg.symbol_size);
    (encoder, decoder)
}

/// Create an encoder configured for the given quality profile.
pub fn create_encoder(profile: &QualityProfile) -> RaptorQFecEncoder {
    AdaptiveFec::from_profile(profile).build_encoder()
}

/// Create a decoder configured for the given quality profile.
pub fn create_decoder(profile: &QualityProfile) -> RaptorQFecDecoder {
    let cfg = AdaptiveFec::from_profile(profile);
    RaptorQFecDecoder::new(cfg.frames_per_block, cfg.symbol_size)
}
