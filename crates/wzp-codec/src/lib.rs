//! WarzonePhone Codec Layer
//!
//! Provides audio encoding/decoding with adaptive codec switching:
//! - Opus (24kbps / 16kbps / 6kbps) for normal to degraded conditions
//! - Codec2 (3200bps / 1200bps) via the pure-Rust `codec2` crate for catastrophic conditions
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
pub mod silence;

pub use adaptive::{AdaptiveDecoder, AdaptiveEncoder};
pub use silence::{ComfortNoise, SilenceDetector};
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

#[cfg(test)]
mod codec2_tests {
    use super::*;
    use crate::codec2_dec::Codec2Decoder;
    use crate::codec2_enc::Codec2Encoder;

    fn c2_3200_profile() -> QualityProfile {
        QualityProfile {
            codec: CodecId::Codec2_3200,
            fec_ratio: 0.5,
            frame_duration_ms: 20,
            frames_per_block: 5,
        }
    }

    fn c2_1200_profile() -> QualityProfile {
        QualityProfile::CATASTROPHIC
    }

    // ── Frame size tests ────────────────────────────────────────────────

    #[test]
    fn codec2_3200_frame_sizes() {
        let enc = Codec2Encoder::new(c2_3200_profile()).unwrap();
        // 3200bps: 160 samples/frame @ 8kHz (20ms), 8 bytes output
        assert_eq!(enc.frame_samples(), 160);
    }

    #[test]
    fn codec2_1200_frame_sizes() {
        let enc = Codec2Encoder::new(c2_1200_profile()).unwrap();
        // 1200bps: 320 samples/frame @ 8kHz (40ms), 6 bytes output
        assert_eq!(enc.frame_samples(), 320);
    }

    // ── Encode/Decode roundtrip tests ───────────────────────────────────

    #[test]
    fn codec2_3200_encode_decode_roundtrip() {
        let mut enc = Codec2Encoder::new(c2_3200_profile()).unwrap();
        let mut dec = Codec2Decoder::new(c2_3200_profile()).unwrap();

        // 160 samples of silence at 8kHz
        let pcm_in = vec![0i16; 160];
        let mut encoded = vec![0u8; 16];
        let enc_bytes = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert_eq!(enc_bytes, 8, "3200bps should produce 8 bytes per frame");

        let mut pcm_out = vec![0i16; 160];
        let dec_samples = dec.decode(&encoded[..enc_bytes], &mut pcm_out).unwrap();
        assert_eq!(dec_samples, 160, "3200bps should decode to 160 samples");
    }

    #[test]
    fn codec2_1200_encode_decode_roundtrip() {
        let mut enc = Codec2Encoder::new(c2_1200_profile()).unwrap();
        let mut dec = Codec2Decoder::new(c2_1200_profile()).unwrap();

        // 320 samples of silence at 8kHz
        let pcm_in = vec![0i16; 320];
        let mut encoded = vec![0u8; 16];
        let enc_bytes = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert_eq!(enc_bytes, 6, "1200bps should produce 6 bytes per frame");

        let mut pcm_out = vec![0i16; 320];
        let dec_samples = dec.decode(&encoded[..enc_bytes], &mut pcm_out).unwrap();
        assert_eq!(dec_samples, 320, "1200bps should decode to 320 samples");
    }

    #[test]
    fn codec2_3200_encode_produces_bytes() {
        let mut enc = Codec2Encoder::new(c2_3200_profile()).unwrap();

        // Feed a non-silent signal to ensure encoding produces non-trivial output.
        let pcm_in: Vec<i16> = (0..160).map(|i| (i * 100) as i16).collect();
        let mut encoded = vec![0u8; 16];
        let n = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert_eq!(n, 8);
        // At least some non-zero bytes in the output.
        assert!(encoded[..n].iter().any(|&b| b != 0));
    }

    #[test]
    fn codec2_1200_encode_produces_bytes() {
        let mut enc = Codec2Encoder::new(c2_1200_profile()).unwrap();

        let pcm_in: Vec<i16> = (0..320).map(|i| (i * 50) as i16).collect();
        let mut encoded = vec![0u8; 16];
        let n = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert_eq!(n, 6);
        assert!(encoded[..n].iter().any(|&b| b != 0));
    }

    // ── Error handling tests ────────────────────────────────────────────

    #[test]
    fn codec2_encode_rejects_short_input() {
        let mut enc = Codec2Encoder::new(c2_3200_profile()).unwrap();
        let pcm_in = vec![0i16; 10]; // too few samples
        let mut out = vec![0u8; 16];
        assert!(enc.encode(&pcm_in, &mut out).is_err());
    }

    #[test]
    fn codec2_decode_rejects_short_input() {
        let mut dec = Codec2Decoder::new(c2_3200_profile()).unwrap();
        let encoded = vec![0u8; 2]; // too few bytes
        let mut pcm = vec![0i16; 160];
        assert!(dec.decode(&encoded, &mut pcm).is_err());
    }

    // ── Adaptive switching: Opus → Codec2 → Opus roundtrip ─────────────

    #[test]
    fn adaptive_opus_to_codec2_to_opus_roundtrip() {
        let mut enc = AdaptiveEncoder::new(QualityProfile::GOOD).unwrap();
        let mut dec = AdaptiveDecoder::new(QualityProfile::GOOD).unwrap();

        // Step 1: Encode/decode with Opus (20ms @ 48kHz = 960 samples).
        let pcm_48k = vec![0i16; 960];
        let mut encoded = vec![0u8; 512];
        let n = enc.encode(&pcm_48k, &mut encoded).unwrap();
        assert!(n > 0);
        let mut pcm_out = vec![0i16; 960];
        let samples = dec.decode(&encoded[..n], &mut pcm_out).unwrap();
        assert_eq!(samples, 960);

        // Step 2: Switch to Codec2 1200.
        enc.set_profile(QualityProfile::CATASTROPHIC).unwrap();
        dec.set_profile(QualityProfile::CATASTROPHIC).unwrap();
        assert_eq!(enc.codec_id(), CodecId::Codec2_1200);

        // Codec2 1200 @ 40ms needs 1920 samples at 48kHz (resampled internally to 320 @ 8kHz).
        let pcm_48k_c2 = vec![0i16; 1920];
        let mut encoded_c2 = vec![0u8; 16];
        let n_c2 = enc.encode(&pcm_48k_c2, &mut encoded_c2).unwrap();
        assert_eq!(n_c2, 6, "Codec2 1200 should produce 6 bytes");

        let mut pcm_out_c2 = vec![0i16; 1920];
        let samples_c2 = dec.decode(&encoded_c2[..n_c2], &mut pcm_out_c2).unwrap();
        assert_eq!(samples_c2, 1920, "should get 1920 samples at 48kHz after upsample");

        // Step 3: Switch back to Opus.
        enc.set_profile(QualityProfile::GOOD).unwrap();
        dec.set_profile(QualityProfile::GOOD).unwrap();
        assert_eq!(enc.codec_id(), CodecId::Opus24k);

        let n_opus = enc.encode(&pcm_48k, &mut encoded).unwrap();
        assert!(n_opus > 0);
        let samples_opus = dec.decode(&encoded[..n_opus], &mut pcm_out).unwrap();
        assert_eq!(samples_opus, 960);
    }

    // ── PLC (decode_lost) test ──────────────────────────────────────────

    #[test]
    fn codec2_decode_lost_produces_silence() {
        let mut dec = Codec2Decoder::new(c2_3200_profile()).unwrap();
        let mut pcm = vec![1i16; 160];
        let n = dec.decode_lost(&mut pcm).unwrap();
        assert_eq!(n, 160);
        assert!(pcm.iter().all(|&s| s == 0));
    }

    // ── Mode switching within Codec2 ────────────────────────────────────

    #[test]
    fn codec2_encoder_switches_3200_to_1200() {
        let mut enc = Codec2Encoder::new(c2_3200_profile()).unwrap();
        assert_eq!(enc.frame_samples(), 160);

        enc.set_profile(c2_1200_profile()).unwrap();
        assert_eq!(enc.frame_samples(), 320);

        let pcm_in = vec![0i16; 320];
        let mut out = vec![0u8; 16];
        let n = enc.encode(&pcm_in, &mut out).unwrap();
        assert_eq!(n, 6);
    }
}
