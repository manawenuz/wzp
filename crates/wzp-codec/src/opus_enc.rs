//! Opus encoder wrapping the `audiopus` crate.

use audiopus::coder::Encoder;
use audiopus::{Application, Bitrate, Channels, SampleRate, Signal};
use tracing::debug;
use wzp_proto::{AudioEncoder, CodecError, CodecId, QualityProfile};

/// Opus encoder implementing `AudioEncoder`.
///
/// Operates at 48 kHz mono. Supports frame sizes of 20 ms (960 samples)
/// and 40 ms (1920 samples).
pub struct OpusEncoder {
    inner: Encoder,
    codec_id: CodecId,
    frame_duration_ms: u8,
}

// SAFETY: OpusEncoder is only used via `&mut self` methods. The inner
// audiopus Encoder contains a raw pointer that is !Sync, but we never
// share it across threads without exclusive access.
unsafe impl Sync for OpusEncoder {}

impl OpusEncoder {
    /// Create a new Opus encoder for the given quality profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        let encoder = Encoder::new(SampleRate::Hz48000, Channels::Mono, Application::Voip)
            .map_err(|e| CodecError::EncodeFailed(format!("opus encoder init: {e}")))?;

        let mut enc = Self {
            inner: encoder,
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
        };
        enc.apply_bitrate(profile.codec)?;
        enc.set_inband_fec(true);
        enc.set_dtx(true);

        // Voice signal type hint for better compression
        enc.inner
            .set_signal(Signal::Voice)
            .map_err(|e| CodecError::EncodeFailed(format!("set signal: {e}")))?;

        Ok(enc)
    }

    fn apply_bitrate(&mut self, codec: CodecId) -> Result<(), CodecError> {
        let bps = codec.bitrate_bps() as i32;
        self.inner
            .set_bitrate(Bitrate::BitsPerSecond(bps))
            .map_err(|e| CodecError::EncodeFailed(format!("set bitrate: {e}")))?;
        debug!(bitrate_bps = bps, "opus encoder bitrate set");
        Ok(())
    }

    /// Expected number of PCM samples per frame at current settings.
    pub fn frame_samples(&self) -> usize {
        (48_000 * self.frame_duration_ms as usize) / 1000
    }
}

impl AudioEncoder for OpusEncoder {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
        let expected = self.frame_samples();
        if pcm.len() != expected {
            return Err(CodecError::EncodeFailed(format!(
                "expected {expected} samples, got {}",
                pcm.len()
            )));
        }
        let n = self
            .inner
            .encode(pcm, out)
            .map_err(|e| CodecError::EncodeFailed(format!("opus encode: {e}")))?;
        Ok(n)
    }

    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        match profile.codec {
            CodecId::Opus24k | CodecId::Opus16k | CodecId::Opus6k => {
                self.codec_id = profile.codec;
                self.frame_duration_ms = profile.frame_duration_ms;
                self.apply_bitrate(profile.codec)?;
                Ok(())
            }
            other => Err(CodecError::UnsupportedTransition {
                from: self.codec_id,
                to: other,
            }),
        }
    }

    fn max_frame_bytes(&self) -> usize {
        // Opus max packet for mono voice: ~500 bytes is generous.
        // For 40ms at 24kbps: ~120 bytes typical, but we allow headroom.
        512
    }

    fn set_inband_fec(&mut self, enabled: bool) {
        let _ = self.inner.set_inband_fec(enabled);
    }

    fn set_dtx(&mut self, enabled: bool) {
        let _ = self.inner.set_dtx(enabled);
    }
}
