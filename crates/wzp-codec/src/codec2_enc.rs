//! Codec2 encoder — stub implementation.
//!
//! Codec2 operates at 8 kHz mono. Resampling from 48 kHz is handled
//! externally (see `resample.rs` and `AdaptiveCodec`).
//!
//! This is a stub that returns an error on encode. When `codec2-sys`
//! is linked, replace the body of `encode()` with actual FFI calls.

use wzp_proto::{AudioEncoder, CodecError, CodecId, QualityProfile};

/// Stub Codec2 encoder implementing `AudioEncoder`.
///
/// Currently returns `CodecError::EncodeFailed` for all encode operations.
/// The structure is ready for drop-in replacement once `codec2-sys` is available.
pub struct Codec2Encoder {
    codec_id: CodecId,
    frame_duration_ms: u8,
}

impl Codec2Encoder {
    /// Create a new stub Codec2 encoder.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        Ok(Self {
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
        })
    }

    /// Expected number of 8 kHz PCM samples per frame.
    pub fn frame_samples(&self) -> usize {
        (8_000 * self.frame_duration_ms as usize) / 1000
    }
}

impl AudioEncoder for Codec2Encoder {
    fn encode(&mut self, _pcm: &[i16], _out: &mut [u8]) -> Result<usize, CodecError> {
        Err(CodecError::EncodeFailed(
            "codec2-sys not yet linked".to_string(),
        ))
    }

    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        match profile.codec {
            CodecId::Codec2_3200 | CodecId::Codec2_1200 => {
                self.codec_id = profile.codec;
                self.frame_duration_ms = profile.frame_duration_ms;
                Ok(())
            }
            other => Err(CodecError::UnsupportedTransition {
                from: self.codec_id,
                to: other,
            }),
        }
    }

    fn max_frame_bytes(&self) -> usize {
        // Codec2 3200bps @ 20ms = 64 bits = 8 bytes
        // Codec2 1200bps @ 40ms = 48 bits = 6 bytes
        // Allow generous headroom.
        16
    }
}
