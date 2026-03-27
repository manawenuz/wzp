//! Codec2 decoder — real implementation via the pure-Rust `codec2` crate.
//!
//! Codec2 operates at 8 kHz mono. Resampling back to 48 kHz is handled
//! externally (see `resample.rs` and `AdaptiveCodec`).

use codec2::{Codec2 as C2, Codec2Mode};
use wzp_proto::{AudioDecoder, CodecError, CodecId, QualityProfile};

/// Maps our `CodecId` to the `codec2` crate's `Codec2Mode`.
fn mode_for(codec: CodecId) -> Result<Codec2Mode, CodecError> {
    match codec {
        CodecId::Codec2_3200 => Ok(Codec2Mode::MODE_3200),
        CodecId::Codec2_1200 => Ok(Codec2Mode::MODE_1200),
        other => Err(CodecError::DecodeFailed(format!(
            "not a Codec2 variant: {other:?}"
        ))),
    }
}

/// Codec2 decoder implementing `AudioDecoder`.
///
/// Wraps the pure-Rust `codec2` crate. Output is 8 kHz mono i16 PCM;
/// the `AdaptiveDecoder` handles 8 kHz -> 48 kHz upsampling.
pub struct Codec2Decoder {
    inner: C2,
    codec_id: CodecId,
    frame_duration_ms: u8,
}

impl Codec2Decoder {
    /// Create a new Codec2 decoder for the given quality profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        let mode = mode_for(profile.codec)?;
        Ok(Self {
            inner: C2::new(mode),
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
        })
    }

    /// Expected number of 8 kHz PCM output samples per frame.
    pub fn frame_samples(&self) -> usize {
        self.inner.samples_per_frame()
    }

    /// Number of compressed bytes per frame.
    fn bytes_per_frame(&self) -> usize {
        (self.inner.bits_per_frame() + 7) / 8
    }
}

impl AudioDecoder for Codec2Decoder {
    fn decode(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError> {
        let spf = self.inner.samples_per_frame();
        let bpf = self.bytes_per_frame();

        if encoded.len() < bpf {
            return Err(CodecError::DecodeFailed(format!(
                "need {bpf} encoded bytes, got {}",
                encoded.len()
            )));
        }
        if pcm.len() < spf {
            return Err(CodecError::DecodeFailed(format!(
                "output buffer too small: need {spf} samples, got {}",
                pcm.len()
            )));
        }

        self.inner.decode(&mut pcm[..spf], &encoded[..bpf]);
        Ok(spf)
    }

    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError> {
        // Codec2 has no built-in PLC. Fill with silence.
        let samples = self.inner.samples_per_frame();
        let n = samples.min(pcm.len());
        pcm[..n].fill(0);
        Ok(n)
    }

    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        match profile.codec {
            CodecId::Codec2_3200 | CodecId::Codec2_1200 => {
                // Recreate the inner decoder if the mode changed.
                if profile.codec != self.codec_id {
                    let mode = mode_for(profile.codec)?;
                    self.inner = C2::new(mode);
                }
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
