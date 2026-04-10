//! Opus decoder built on top of the raw opusic-sys `DecoderHandle`.
//!
//! Phase 0 of the DRED integration: we went straight to a custom
//! `DecoderHandle` instead of `opusic_c::Decoder` because the latter's
//! inner pointer is `pub(crate)` and we need to reach it in Phase 3 for
//! `opus_decoder_dred_decode`. See `dred_ffi.rs` for the rationale and
//! `docs/PRD-dred-integration.md` for the full plan.

use crate::dred_ffi::{DecoderHandle, DredState};
use wzp_proto::{AudioDecoder, CodecError, CodecId, QualityProfile};

/// Opus decoder implementing [`AudioDecoder`].
///
/// Operates at 48 kHz mono output. 20 ms and 40 ms frames supported via
/// the active `QualityProfile`. Behavior is intentionally identical to
/// the pre-swap audiopus-based decoder at this phase — DRED reconstruction
/// lands in Phase 3.
pub struct OpusDecoder {
    inner: DecoderHandle,
    codec_id: CodecId,
    frame_duration_ms: u8,
}

impl OpusDecoder {
    /// Create a new Opus decoder for the given quality profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        let inner = DecoderHandle::new()?;
        Ok(Self {
            inner,
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
        })
    }

    /// Expected number of output PCM samples per frame.
    pub fn frame_samples(&self) -> usize {
        (48_000 * self.frame_duration_ms as usize) / 1000
    }

    /// Reconstruct a lost frame from a previously parsed `DredState`.
    ///
    /// Phase 3b entry point: callers (CallDecoder / engine.rs) use this to
    /// synthesize audio for gaps detected by the jitter buffer when DRED
    /// side-channel state from a later-arriving packet covers the gap's
    /// sample offset. `offset_samples` is measured backward from the anchor
    /// packet that produced `state`. See `DecoderHandle::reconstruct_from_dred`
    /// for the full semantics.
    pub fn reconstruct_from_dred(
        &mut self,
        state: &DredState,
        offset_samples: i32,
        output: &mut [i16],
    ) -> Result<usize, CodecError> {
        self.inner
            .reconstruct_from_dred(state, offset_samples, output)
    }
}

impl AudioDecoder for OpusDecoder {
    fn decode(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError> {
        let expected = self.frame_samples();
        if pcm.len() < expected {
            return Err(CodecError::DecodeFailed(format!(
                "output buffer too small: need {expected}, got {}",
                pcm.len()
            )));
        }
        self.inner.decode(encoded, pcm)
    }

    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError> {
        let expected = self.frame_samples();
        if pcm.len() < expected {
            return Err(CodecError::DecodeFailed(format!(
                "output buffer too small: need {expected}, got {}",
                pcm.len()
            )));
        }
        self.inner.decode_lost(pcm)
    }

    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        match profile.codec {
            c if c.is_opus() => {
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
