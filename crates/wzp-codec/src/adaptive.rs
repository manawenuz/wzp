//! Adaptive codec that wraps both Opus and Codec2, switching on the fly.
//!
//! `AdaptiveEncoder` and `AdaptiveDecoder` present a unified `AudioEncoder` /
//! `AudioDecoder` interface while transparently delegating to the appropriate
//! inner codec based on the current `QualityProfile`.
//!
//! Callers always work with 48 kHz PCM.  When Codec2 is the active codec the
//! adaptive layer handles the 48 kHz ↔ 8 kHz resampling internally.

use tracing::debug;
use wzp_proto::{AudioDecoder, AudioEncoder, CodecError, CodecId, QualityProfile};

use crate::codec2_dec::Codec2Decoder;
use crate::codec2_enc::Codec2Encoder;
use crate::opus_dec::OpusDecoder;
use crate::opus_enc::OpusEncoder;
use crate::resample::{Downsampler48to8, Upsampler8to48};

// ─── Helpers ─────────────────────────────────────────────────────────────────

/// Returns `true` when the codec operates at 8 kHz (i.e. a Codec2 variant).
fn is_codec2(codec: CodecId) -> bool {
    matches!(codec, CodecId::Codec2_3200 | CodecId::Codec2_1200)
}

/// Build a `QualityProfile` that only contains Opus-relevant fields.
fn opus_profile(profile: QualityProfile) -> QualityProfile {
    // Clamp to Opus24k if the caller somehow passes a Codec2 profile.
    let codec = if is_codec2(profile.codec) {
        CodecId::Opus24k
    } else {
        profile.codec
    };
    QualityProfile { codec, ..profile }
}

/// Build a `QualityProfile` that only contains Codec2-relevant fields.
fn codec2_profile(profile: QualityProfile) -> QualityProfile {
    let codec = if is_codec2(profile.codec) {
        profile.codec
    } else {
        CodecId::Codec2_3200
    };
    QualityProfile { codec, ..profile }
}

// ─── AdaptiveEncoder ─────────────────────────────────────────────────────────

/// Adaptive encoder that delegates to either Opus or Codec2.
///
/// Input PCM is always 48 kHz mono.  When Codec2 is selected the encoder
/// downsamples to 8 kHz before encoding.
pub struct AdaptiveEncoder {
    opus: OpusEncoder,
    codec2: Codec2Encoder,
    active: CodecId,
    downsampler: Downsampler48to8,
}

impl AdaptiveEncoder {
    /// Create a new adaptive encoder starting at the given profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        let opus = OpusEncoder::new(opus_profile(profile))?;
        let codec2 = Codec2Encoder::new(codec2_profile(profile))?;

        Ok(Self {
            opus,
            codec2,
            active: profile.codec,
            downsampler: Downsampler48to8::new(),
        })
    }
}

impl AudioEncoder for AdaptiveEncoder {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError> {
        if is_codec2(self.active) {
            // Downsample 48 kHz → 8 kHz then encode via Codec2.
            let pcm_8k = self.downsampler.process(pcm);
            self.codec2.encode(&pcm_8k, out)
        } else {
            self.opus.encode(pcm, out)
        }
    }

    fn codec_id(&self) -> CodecId {
        self.active
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        let prev = self.active;
        self.active = profile.codec;

        if is_codec2(profile.codec) {
            debug!(from = ?prev, to = ?profile.codec, "adaptive encoder → Codec2");
            self.codec2.set_profile(profile)
        } else {
            debug!(from = ?prev, to = ?profile.codec, "adaptive encoder → Opus");
            self.opus.set_profile(profile)
        }
    }

    fn max_frame_bytes(&self) -> usize {
        if is_codec2(self.active) {
            self.codec2.max_frame_bytes()
        } else {
            self.opus.max_frame_bytes()
        }
    }

    fn set_inband_fec(&mut self, enabled: bool) {
        self.opus.set_inband_fec(enabled);
        // No-op for Codec2 (per trait doc).
    }

    fn set_dtx(&mut self, enabled: bool) {
        self.opus.set_dtx(enabled);
    }
}

// ─── AdaptiveDecoder ─────────────────────────────────────────────────────────

/// Adaptive decoder that delegates to either Opus or Codec2.
///
/// Output PCM is always 48 kHz mono.  When Codec2 is selected the decoder
/// upsamples the 8 kHz output to 48 kHz before returning.
pub struct AdaptiveDecoder {
    opus: OpusDecoder,
    codec2: Codec2Decoder,
    active: CodecId,
    upsampler: Upsampler8to48,
}

impl AdaptiveDecoder {
    /// Create a new adaptive decoder starting at the given profile.
    pub fn new(profile: QualityProfile) -> Result<Self, CodecError> {
        let opus = OpusDecoder::new(opus_profile(profile))?;
        let codec2 = Codec2Decoder::new(codec2_profile(profile))?;

        Ok(Self {
            opus,
            codec2,
            active: profile.codec,
            upsampler: Upsampler8to48::new(),
        })
    }
}

impl AudioDecoder for AdaptiveDecoder {
    fn decode(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError> {
        if is_codec2(self.active) {
            // Decode into a temporary 8 kHz buffer, then upsample.
            let c2_samples = self.codec2_frame_samples();
            let mut buf_8k = vec![0i16; c2_samples];
            let n = self.codec2.decode(encoded, &mut buf_8k)?;
            let pcm_48k = self.upsampler.process(&buf_8k[..n]);
            let out_len = pcm_48k.len().min(pcm.len());
            pcm[..out_len].copy_from_slice(&pcm_48k[..out_len]);
            Ok(out_len)
        } else {
            self.opus.decode(encoded, pcm)
        }
    }

    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError> {
        if is_codec2(self.active) {
            let c2_samples = self.codec2_frame_samples();
            let mut buf_8k = vec![0i16; c2_samples];
            let n = self.codec2.decode_lost(&mut buf_8k)?;
            let pcm_48k = self.upsampler.process(&buf_8k[..n]);
            let out_len = pcm_48k.len().min(pcm.len());
            pcm[..out_len].copy_from_slice(&pcm_48k[..out_len]);
            Ok(out_len)
        } else {
            self.opus.decode_lost(pcm)
        }
    }

    fn codec_id(&self) -> CodecId {
        self.active
    }

    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError> {
        let prev = self.active;
        self.active = profile.codec;

        if is_codec2(profile.codec) {
            debug!(from = ?prev, to = ?profile.codec, "adaptive decoder → Codec2");
            self.codec2.set_profile(profile)
        } else {
            debug!(from = ?prev, to = ?profile.codec, "adaptive decoder → Opus");
            self.opus.set_profile(profile)
        }
    }
}

impl AdaptiveDecoder {
    /// Number of 8 kHz samples expected for the current Codec2 frame.
    fn codec2_frame_samples(&self) -> usize {
        self.codec2.frame_samples()
    }
}

// ─── Tests ───────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_starts_with_correct_codec() {
        let enc = AdaptiveEncoder::new(QualityProfile::GOOD).unwrap();
        assert_eq!(enc.codec_id(), CodecId::Opus24k);
    }

    #[test]
    fn decoder_starts_with_correct_codec() {
        let dec = AdaptiveDecoder::new(QualityProfile::GOOD).unwrap();
        assert_eq!(dec.codec_id(), CodecId::Opus24k);
    }

    #[test]
    fn encoder_switches_opus_to_codec2() {
        let mut enc = AdaptiveEncoder::new(QualityProfile::GOOD).unwrap();
        assert_eq!(enc.codec_id(), CodecId::Opus24k);

        enc.set_profile(QualityProfile::CATASTROPHIC).unwrap();
        assert_eq!(enc.codec_id(), CodecId::Codec2_1200);

        // Max frame bytes should reflect Codec2 now.
        assert!(enc.max_frame_bytes() <= 16);
    }

    #[test]
    fn encoder_switches_codec2_to_opus() {
        let mut enc = AdaptiveEncoder::new(QualityProfile::CATASTROPHIC).unwrap();
        assert_eq!(enc.codec_id(), CodecId::Codec2_1200);

        enc.set_profile(QualityProfile::GOOD).unwrap();
        assert_eq!(enc.codec_id(), CodecId::Opus24k);
        assert!(enc.max_frame_bytes() > 16);
    }

    #[test]
    fn decoder_switches_opus_to_codec2() {
        let mut dec = AdaptiveDecoder::new(QualityProfile::GOOD).unwrap();
        assert_eq!(dec.codec_id(), CodecId::Opus24k);

        dec.set_profile(QualityProfile::CATASTROPHIC).unwrap();
        assert_eq!(dec.codec_id(), CodecId::Codec2_1200);
    }

    #[test]
    fn decoder_codec2_plc_produces_48k_silence() {
        let mut dec = AdaptiveDecoder::new(QualityProfile::CATASTROPHIC).unwrap();
        // Codec2 1200 @ 40ms → 320 samples at 8kHz → 1920 at 48kHz
        let mut pcm = vec![0i16; 1920];
        let n = dec.decode_lost(&mut pcm).unwrap();
        assert_eq!(n, 1920);
        // PLC from Codec2 stub is silence, upsampled silence is still silence.
        assert!(pcm.iter().all(|&s| s == 0));
    }

    #[test]
    fn encoder_opus_encode_works_after_switch() {
        // Start on Codec2, switch to Opus, and encode a real frame.
        let mut enc = AdaptiveEncoder::new(QualityProfile::CATASTROPHIC).unwrap();
        enc.set_profile(QualityProfile::GOOD).unwrap();

        // 20ms at 48kHz = 960 samples
        let pcm = vec![0i16; 960];
        let mut out = vec![0u8; 512];
        let n = enc.encode(&pcm, &mut out).unwrap();
        assert!(n > 0);
    }

    #[test]
    fn encoder_roundtrip_opus() {
        let mut enc = AdaptiveEncoder::new(QualityProfile::GOOD).unwrap();
        let mut dec = AdaptiveDecoder::new(QualityProfile::GOOD).unwrap();

        let pcm_in = vec![0i16; 960]; // 20ms silence
        let mut encoded = vec![0u8; 512];
        let enc_bytes = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert!(enc_bytes > 0);

        let mut pcm_out = vec![0i16; 960];
        let dec_samples = dec.decode(&encoded[..enc_bytes], &mut pcm_out).unwrap();
        assert_eq!(dec_samples, 960);
    }
}
