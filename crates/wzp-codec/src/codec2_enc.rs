//! Codec2 encoder — real implementation via the pure-Rust `codec2` crate.
//!
//! Codec2 operates at 8 kHz mono. Resampling from 48 kHz is handled
//! externally (see `resample.rs` and `AdaptiveCodec`).

use codec2::{Codec2 as C2, Codec2Mode};
use wzp_proto::{AudioEncoder, CodecError, CodecId, QualityProfile};

/// Maps our `CodecId` to the `codec2` crate's `Codec2Mode`.
fn mode_for(codec: CodecId) -> Result<Codec2Mode, CodecError> {
    match codec {
        CodecId::Codec2_3200 => Ok(Codec2Mode::MODE_3200),
        CodecId::Codec2_1200 => Ok(Codec2Mode::MODE_1200),
        other => Err(CodecError::EncodeFailed(format!(
            "not a Codec2 variant: {other:?}"
        ))),
    }
}

/// Codec2 encoder implementing `AudioEncoder`.
///
/// Wraps the pure-Rust `codec2` crate. Input is 8 kHz mono i16 PCM;
/// the `AdaptiveEncoder` handles 48 kHz -> 8 kHz resampling.
pub struct Codec2Encoder {
    inner: C2,
    codec_id: CodecId,
    frame_duration_ms: u8,
}

impl Codec2Encoder {
    /// Create a new Codec2 encoder for the given quality profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        let mode = mode_for(profile.codec)?;
        Ok(Self {
            inner: C2::new(mode),
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
        })
    }

    /// Expected number of 8 kHz PCM samples per frame.
    pub fn frame_samples(&self) -> usize {
        self.inner.samples_per_frame()
    }

    /// Number of compressed bytes per frame.
    fn bytes_per_frame(&self) -> usize {
        (self.inner.bits_per_frame() + 7) / 8
    }
}

impl AudioEncoder for Codec2Encoder {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
        let spf = self.inner.samples_per_frame();
        let bpf = self.bytes_per_frame();

        if pcm.len() < spf {
            return Err(CodecError::EncodeFailed(format!(
                "need {spf} samples, got {}",
                pcm.len()
            )));
        }
        if out.len() < bpf {
            return Err(CodecError::EncodeFailed(format!(
                "output buffer too small: need {bpf} bytes, got {}",
                out.len()
            )));
        }

        self.inner.encode(&mut out[..bpf], &pcm[..spf]);
        Ok(bpf)
    }

    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        match profile.codec {
            CodecId::Codec2_3200 | CodecId::Codec2_1200 => {
                // Recreate the inner encoder if the mode changed.
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

    fn max_frame_bytes(&self) -> usize {
        // Codec2 3200bps @ 20ms = 64 bits = 8 bytes
        // Codec2 1200bps @ 40ms = 48 bits = 6 bytes
        // Allow generous headroom.
        16
    }
}
