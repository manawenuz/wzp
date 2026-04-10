//! Opus encoder wrapping the `opusic-c` crate (libopus 1.5.2).
//!
//! Phase 0 of the DRED integration: swapped FFI backend from audiopus
//! (dead, libopus 1.3) to opusic-c (live, libopus 1.5.2). Behavior is
//! intentionally unchanged from the audiopus-based encoder — inband FEC
//! stays ON, DRED stays at duration 0. Phase 1 enables DRED and disables
//! inband FEC. See docs/PRD-dred-integration.md.

use opusic_c::{
    Application, Bitrate, Channels, Encoder, InbandFec, SampleRate, Signal,
};
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
// opusic-c Encoder wraps a non-null pointer that is !Sync by default,
// but we never share it across threads without exclusive access.
unsafe impl Sync for OpusEncoder {}

impl OpusEncoder {
    /// Create a new Opus encoder for the given quality profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        // opusic-c argument order: (Channels, SampleRate, Application)
        // — different from audiopus's (SampleRate, Channels, Application).
        let encoder = Encoder::new(Channels::Mono, SampleRate::Hz48000, Application::Voip)
            .map_err(|e| CodecError::EncodeFailed(format!("opus encoder init: {e:?}")))?;

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
            .map_err(|e| CodecError::EncodeFailed(format!("set signal: {e:?}")))?;

        // Default complexity 7 — good quality/CPU trade-off for VoIP
        enc.inner
            .set_complexity(7)
            .map_err(|e| CodecError::EncodeFailed(format!("set complexity: {e:?}")))?;

        Ok(enc)
    }

    fn apply_bitrate(&mut self, codec: CodecId) -> Result<(), CodecError> {
        let bps = codec.bitrate_bps();
        self.inner
            .set_bitrate(Bitrate::Value(bps))
            .map_err(|e| CodecError::EncodeFailed(format!("set bitrate: {e:?}")))?;
        debug!(bitrate_bps = bps, "opus encoder bitrate set");
        Ok(())
    }

    /// Expected number of PCM samples per frame at current settings.
    pub fn frame_samples(&self) -> usize {
        (48_000 * self.frame_duration_ms as usize) / 1000
    }

    /// Set the encoder complexity (0-10). Higher values produce better quality
    /// at the cost of more CPU. Default is 7.
    pub fn set_complexity(&mut self, complexity: i32) {
        let c = (complexity as u8).min(10);
        let _ = self.inner.set_complexity(c);
    }

    /// Hint the encoder about expected packet loss percentage (0-100).
    ///
    /// Higher values cause the encoder to use more redundancy to survive
    /// packet loss, at the expense of slightly higher bitrate.
    pub fn set_expected_loss(&mut self, loss_pct: u8) {
        let _ = self.inner.set_packet_loss(loss_pct.min(100));
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
        // opusic-c takes &[u16] for the sample input. Bit pattern is
        // identical to i16 — the cast is zero-cost and the encoder
        // interprets the bytes the same way as libopus internally.
        let pcm_u16: &[u16] = bytemuck::cast_slice(pcm);
        let n = self
            .inner
            .encode_to_slice(pcm_u16, out)
            .map_err(|e| CodecError::EncodeFailed(format!("opus encode: {e:?}")))?;
        Ok(n)
    }

    fn codec_id(&self) -> CodecId {
        self.codec_id
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        match profile.codec {
            c if c.is_opus() => {
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
        // opusic-c replaces the audiopus bool with an enum that distinguishes
        // Mode1 (classical LBRR, equivalent to libopus 1.3's inband FEC) and
        // Mode2 (newer, higher-quality variant added in 1.5). Phase 0 preserves
        // pre-swap behavior by using Mode1, which is the direct equivalent of
        // audiopus's `set_inband_fec(true)`. Phase 1 flips this to Off when
        // DRED is enabled.
        let mode = if enabled { InbandFec::Mode1 } else { InbandFec::Off };
        let _ = self.inner.set_inband_fec(mode);
    }

    fn set_dtx(&mut self, enabled: bool) {
        let _ = self.inner.set_dtx(enabled);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wzp_proto::AudioDecoder;

    /// Phase 0 acceptance gate: fail loudly if the linked libopus is not 1.5.x.
    /// DRED (Phase 1+) only exists in libopus ≥ 1.5, so running against an
    /// older version would silently regress the entire DRED integration.
    #[test]
    fn linked_libopus_is_1_5() {
        let version = opusic_c::version();
        assert!(
            version.contains("1.5"),
            "expected libopus 1.5.x, got: {version}"
        );
    }

    #[test]
    fn encoder_creates_at_good_profile() {
        let enc = OpusEncoder::new(QualityProfile::GOOD).expect("opus encoder init");
        assert_eq!(enc.codec_id, CodecId::Opus24k);
        assert_eq!(enc.frame_samples(), 960); // 20 ms @ 48 kHz
    }

    #[test]
    fn encoder_roundtrip_silence() {
        use crate::opus_dec::OpusDecoder;
        let mut enc = OpusEncoder::new(QualityProfile::GOOD).unwrap();
        let mut dec = OpusDecoder::new(QualityProfile::GOOD).unwrap();
        let pcm_in = vec![0i16; 960]; // 20 ms silence
        let mut encoded = vec![0u8; 512];
        let n = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert!(n > 0);
        let mut pcm_out = vec![0i16; 960];
        let samples = dec.decode(&encoded[..n], &mut pcm_out).unwrap();
        assert_eq!(samples, 960);
    }
}
