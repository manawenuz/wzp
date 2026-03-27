//! Codec2 decoder — stub implementation.
//!
//! Codec2 operates at 8 kHz mono. Resampling back to 48 kHz is handled
//! externally (see `resample.rs` and `AdaptiveCodec`).
//!
//! This is a stub that returns an error on decode. When `codec2-sys`
//! is linked, replace the body of `decode()` with actual FFI calls.

use wzp_proto::{AudioDecoder, CodecError, CodecId, QualityProfile};

/// Stub Codec2 decoder implementing `AudioDecoder`.
///
/// Currently returns `CodecError::DecodeFailed` for decode operations.
/// PLC fills output with silence (zeros).
pub struct Codec2Decoder {
    codec_id: CodecId,
    frame_duration_ms: u8,
}

impl Codec2Decoder {
    /// Create a new stub Codec2 decoder.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        Ok(Self {
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
        })
    }

    /// Expected number of 8 kHz PCM output samples per frame.
    pub fn frame_samples(&self) -> usize {
        (8_000 * self.frame_duration_ms as usize) / 1000
    }
}

impl AudioDecoder for Codec2Decoder {
    fn decode(&mut self, _encoded: &[u8], _pcm: &mut [i16]) -> Result<usize, CodecError> {
        Err(CodecError::DecodeFailed(
            "codec2-sys not yet linked".to_string(),
        ))
    }

    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError> {
        let samples = self.frame_samples();
        let n = samples.min(pcm.len());
        // Fill with silence as basic PLC
        pcm[..n].fill(0);
        Ok(n)
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
}
