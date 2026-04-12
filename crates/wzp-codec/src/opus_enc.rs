//! Opus encoder wrapping the `opusic-c` crate (libopus 1.5.2).
//!
//! Phase 1 of the DRED integration: encoder-side DRED is enabled on every
//! Opus profile with a tiered duration (studio 100 ms / normal 200 ms /
//! degraded 500 ms), and Opus inband FEC (LBRR) is disabled because DRED
//! is the stronger mechanism for the same failure mode. The legacy behavior
//! is preserved behind the `AUDIO_USE_LEGACY_FEC` environment variable as a
//! runtime escape hatch for rollout. See `docs/PRD-dred-integration.md`.
//!
//! # DRED duration policy
//!
//! Rationale from the PRD:
//! - Studio tiers (Opus 32k/48k/64k): 100 ms — loss is rare on high-quality
//!   networks; short window keeps decoder CPU modest.
//! - Normal tiers (Opus 16k/24k): 200 ms — balanced baseline covering common
//!   VoIP loss patterns (20–150 ms bursts from wifi roam, transient congestion).
//! - Degraded tier (Opus 6k): 1040 ms — users on 6k are by definition on a
//!   bad link; the maximum libopus DRED window buys the best burst resilience
//!   where it matters. The RDO-VAE naturally degrades quality at longer offsets.
//!
//! # Why the 15% packet loss floor
//!
//! libopus 1.5's DRED emitter is gated on `OPUS_SET_PACKET_LOSS_PERC` and
//! scales the emitted window proportionally to the assumed loss:
//!
//! ```text
//!   loss_pct  samples_available    effective_ms
//!   5%         720                   15
//!   10%        2640                  55
//!   15%        4560                  95
//!   20%        6480                 135
//!   25%+       8400 (capped)        175  (≈ 87% of the 200ms configured max)
//! ```
//!
//! Measured empirically against libopus 1.5.2 on Opus 24k / 200 ms DRED
//! duration during Phase 3b. At 5% loss the window is only 15 ms — too
//! small to even reconstruct a single 20 ms Opus frame. 15% gives 95 ms
//! (enough for single-frame recovery plus modest burst margin) while
//! keeping the bitrate overhead modest compared to 25%. Real measurements
//! from the quality adapter override upward when loss exceeds the floor.

use std::sync::OnceLock;

use opusic_c::{Application, Bitrate, Channels, Encoder, InbandFec, SampleRate, Signal};
use tracing::{debug, info, warn};
use wzp_proto::{AudioEncoder, CodecError, CodecId, QualityProfile};

/// Logged exactly once per process the first time an OpusEncoder is built.
/// Confirms that libopus 1.5.2 (the version with DRED) is actually linked
/// at runtime — invaluable when chasing "is the new codec loaded?"
/// regressions on Android, where the only debug surface is logcat.
static LIBOPUS_VERSION_LOGGED: OnceLock<()> = OnceLock::new();

/// Minimum `OPUS_SET_PACKET_LOSS_PERC` value used in DRED mode. libopus
/// scales the DRED emission window with the assumed loss percentage:
/// empirically, 5% gives a 15 ms window (useless), 10% gives 55 ms, 15%
/// gives 95 ms, and 25%+ saturates the configured max (~175 ms at 200 ms
/// duration). 15% is the minimum value that produces a DRED window larger
/// than a single 20 ms frame, making it the minimum floor that actually
/// gives DRED something useful to reconstruct. Real loss measurements from
/// the quality adapter override this upward.
const DRED_LOSS_FLOOR_PCT: u8 = 15;

/// Environment variable that reverts Phase 1 behavior to Phase 0 (inband FEC
/// on, DRED off, no loss floor). Read once per encoder construction.
const LEGACY_FEC_ENV: &str = "AUDIO_USE_LEGACY_FEC";

/// Returns the DRED duration in 10 ms frame units for a given Opus codec.
///
/// Unit: each frame is 10 ms, so the max value of 104 corresponds to 1040 ms
/// of reconstructable history. Returns 0 for non-Opus codecs (DRED is not
/// emitted by the libopus encoder in that case anyway, but we avoid a
/// pointless FFI call).
///
/// See the DRED duration policy in the module docs for per-tier rationale.
pub fn dred_duration_for(codec: CodecId) -> u8 {
    match codec {
        // Studio tiers — loss is rare, short window.
        CodecId::Opus32k | CodecId::Opus48k | CodecId::Opus64k => 10,
        // Normal tiers — balanced baseline.
        CodecId::Opus16k | CodecId::Opus24k => 20,
        // Degraded tier — maximum burst resilience. 104 × 10 ms = 1040 ms,
        // the highest value libopus 1.5 supports. Users on 6k are on a bad
        // link by definition; the RDO-VAE naturally degrades quality at longer
        // offsets, so the extra window costs only ~1-2 kbps additional overhead
        // while buying substantially better burst resilience (up from 500 ms).
        CodecId::Opus6k => 104,
        // Non-Opus (Codec2 / CN): DRED is N/A.
        CodecId::Codec2_1200 | CodecId::Codec2_3200 | CodecId::ComfortNoise => 0,
    }
}

/// Returns whether the legacy-FEC escape hatch is active.
///
/// Read from `AUDIO_USE_LEGACY_FEC`. Any non-empty value activates legacy
/// mode; unset or empty leaves DRED enabled.
fn read_legacy_fec_env() -> bool {
    match std::env::var(LEGACY_FEC_ENV) {
        Ok(v) => !v.is_empty() && v != "0" && v.to_ascii_lowercase() != "false",
        Err(_) => false,
    }
}

/// Opus encoder implementing `AudioEncoder`.
///
/// Operates at 48 kHz mono. Supports 20 ms and 40 ms frames via the active
/// `QualityProfile`.
pub struct OpusEncoder {
    inner: Encoder,
    codec_id: CodecId,
    frame_duration_ms: u8,
    /// When `true`, revert to the Phase 0 behavior: inband FEC Mode1, DRED
    /// disabled, no loss floor. Captured at construction time and not
    /// re-read mid-call.
    legacy_fec_mode: bool,
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

        let legacy_fec_mode = read_legacy_fec_env();
        if legacy_fec_mode {
            warn!(
                "AUDIO_USE_LEGACY_FEC active — reverting Opus encoder to Phase 0 \
                 behavior (inband FEC Mode1, no DRED)"
            );
        }

        let mut enc = Self {
            inner: encoder,
            codec_id: profile.codec,
            frame_duration_ms: profile.frame_duration_ms,
            legacy_fec_mode,
        };

        // Common setup — bitrate, DTX, signal hint, complexity. These are
        // identical regardless of the protection mode below.
        enc.apply_bitrate(profile.codec)?;
        enc.set_dtx(true);
        enc.inner
            .set_signal(Signal::Voice)
            .map_err(|e| CodecError::EncodeFailed(format!("set signal: {e:?}")))?;
        enc.inner
            .set_complexity(7)
            .map_err(|e| CodecError::EncodeFailed(format!("set complexity: {e:?}")))?;

        // Protection mode: DRED (Phase 1 default) or legacy inband FEC.
        enc.apply_protection_mode(profile.codec)?;

        Ok(enc)
    }

    /// Configure the protection mode for the active codec.
    ///
    /// In DRED mode (default): disable inband FEC, set DRED duration for the
    /// codec tier, clamp packet_loss to the 5% floor so DRED stays active.
    ///
    /// In legacy mode: enable inband FEC Mode1 (Phase 0 behavior), leave
    /// DRED and packet_loss at libopus defaults.
    fn apply_protection_mode(&mut self, codec: CodecId) -> Result<(), CodecError> {
        if self.legacy_fec_mode {
            self.inner
                .set_inband_fec(InbandFec::Mode1)
                .map_err(|e| CodecError::EncodeFailed(format!("set inband FEC: {e:?}")))?;
            // Leave DRED at 0 and packet_loss at default — matches Phase 0.
            return Ok(());
        }

        // DRED path: disable the overlapping inband FEC, enable DRED with
        // per-profile duration, floor packet_loss so DRED emits.
        self.inner
            .set_inband_fec(InbandFec::Off)
            .map_err(|e| CodecError::EncodeFailed(format!("set inband FEC off: {e:?}")))?;

        let dred_frames = dred_duration_for(codec);
        self.inner
            .set_dred_duration(dred_frames)
            .map_err(|e| CodecError::EncodeFailed(format!("set DRED duration: {e:?}")))?;

        self.inner
            .set_packet_loss(DRED_LOSS_FLOOR_PCT)
            .map_err(|e| CodecError::EncodeFailed(format!("set packet loss floor: {e:?}")))?;

        // Both of these are gated behind the GUI debug toggle so logcat
        // stays clean in normal mode. Flip "DRED verbose logs" in the
        // settings panel to see the per-encoder config + libopus version.
        if crate::dred_verbose_logs() {
            info!(
                codec = ?codec,
                dred_frames,
                dred_ms = dred_frames as u32 * 10,
                loss_floor_pct = DRED_LOSS_FLOOR_PCT,
                "opus encoder: DRED enabled"
            );

            // One-shot logging of the linked libopus version so we can
            // confirm at a glance that opusic-c (libopus 1.5.2) is loaded.
            // Pre-Phase-0 audiopus shipped libopus 1.3 which has no DRED;
            // if this log says "libopus 1.3" something is very wrong.
            LIBOPUS_VERSION_LOGGED.get_or_init(|| {
                info!(libopus_version = %opusic_c::version(), "linked libopus version");
            });
        }

        Ok(())
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
    /// In DRED mode, the value is floored at `DRED_LOSS_FLOOR_PCT` so the
    /// encoder never drops DRED emission even on a perfect network. Real
    /// loss measurements from the quality adapter override upward.
    ///
    /// In legacy mode, the value is passed through unchanged (min 0, max 100).
    pub fn set_expected_loss(&mut self, loss_pct: u8) {
        let clamped = if self.legacy_fec_mode {
            loss_pct.min(100)
        } else {
            loss_pct.max(DRED_LOSS_FLOOR_PCT).min(100)
        };
        let _ = self.inner.set_packet_loss(clamped);
    }

    /// Set the DRED duration in 10 ms frame units (0 disables, max 104).
    ///
    /// No-op in legacy mode. Normally driven automatically by the active
    /// quality profile via `apply_protection_mode`; this setter exists for
    /// tests and for the rare case where a caller needs to override the
    /// per-profile default.
    pub fn set_dred_duration(&mut self, frames: u8) {
        if self.legacy_fec_mode {
            return;
        }
        let _ = self.inner.set_dred_duration(frames.min(104));
    }

    /// Test/introspection accessor: whether legacy FEC mode is active.
    pub fn is_legacy_fec_mode(&self) -> bool {
        self.legacy_fec_mode
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
                // Refresh DRED duration for the new tier. apply_protection_mode
                // is idempotent and handles the legacy-vs-DRED branch correctly.
                self.apply_protection_mode(profile.codec)?;
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
        // In DRED mode, ignore external requests to re-enable inband FEC —
        // running both mechanisms wastes bitrate on overlapping protection
        // and opusic-c's own docs recommend disabling inband FEC when DRED
        // is on. Trait callers that genuinely want classical FEC should set
        // `AUDIO_USE_LEGACY_FEC=1` and re-create the encoder.
        if !self.legacy_fec_mode {
            debug!(
                enabled,
                "set_inband_fec ignored: DRED mode is active (set AUDIO_USE_LEGACY_FEC to revert)"
            );
            return;
        }
        let mode = if enabled { InbandFec::Mode1 } else { InbandFec::Off };
        let _ = self.inner.set_inband_fec(mode);
    }

    fn set_dtx(&mut self, enabled: bool) {
        let _ = self.inner.set_dtx(enabled);
    }

    fn set_expected_loss(&mut self, loss_pct: u8) {
        OpusEncoder::set_expected_loss(self, loss_pct);
    }

    fn set_dred_duration(&mut self, frames: u8) {
        OpusEncoder::set_dred_duration(self, frames);
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
        let mut enc = OpusEncoder::new(QualityProfile::GOOD).unwrap();
        let mut dec = crate::opus_dec::OpusDecoder::new(QualityProfile::GOOD).unwrap();
        let pcm_in = vec![0i16; 960]; // 20 ms silence
        let mut encoded = vec![0u8; 512];
        let n = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert!(n > 0);
        let mut pcm_out = vec![0i16; 960];
        let samples = dec.decode(&encoded[..n], &mut pcm_out).unwrap();
        assert_eq!(samples, 960);
    }

    // ─── Phase 1 — DRED duration policy ─────────────────────────────────────

    #[test]
    fn dred_duration_for_studio_tiers_is_100ms() {
        assert_eq!(dred_duration_for(CodecId::Opus32k), 10);
        assert_eq!(dred_duration_for(CodecId::Opus48k), 10);
        assert_eq!(dred_duration_for(CodecId::Opus64k), 10);
    }

    #[test]
    fn dred_duration_for_normal_tiers_is_200ms() {
        assert_eq!(dred_duration_for(CodecId::Opus16k), 20);
        assert_eq!(dred_duration_for(CodecId::Opus24k), 20);
    }

    #[test]
    fn dred_duration_for_degraded_tier_is_1040ms() {
        assert_eq!(dred_duration_for(CodecId::Opus6k), 104);
    }

    #[test]
    fn dred_duration_for_codec2_is_zero() {
        assert_eq!(dred_duration_for(CodecId::Codec2_3200), 0);
        assert_eq!(dred_duration_for(CodecId::Codec2_1200), 0);
        assert_eq!(dred_duration_for(CodecId::ComfortNoise), 0);
    }

    // ─── Phase 1 — Legacy escape hatch ──────────────────────────────────────

    /// By default (env var unset), legacy mode is off.
    ///
    /// This test does NOT manipulate the environment to avoid flakiness
    /// when the full suite runs in parallel. It only asserts on a freshly
    /// created encoder in the ambient environment.
    #[test]
    fn default_mode_is_dred_not_legacy() {
        // SAFETY: only run if the ambient env hasn't set the var externally.
        if std::env::var(LEGACY_FEC_ENV).is_ok() {
            return; // don't assert — someone set the env for a reason.
        }
        let enc = OpusEncoder::new(QualityProfile::GOOD).unwrap();
        assert!(!enc.is_legacy_fec_mode());
    }

    // ─── Phase 1 — Behavioral regression: roundtrip still works ─────────────

    #[test]
    fn dred_mode_roundtrip_voice_pattern() {
        // Use a realistic voice-like input (sine wave at speech frequencies)
        // so the encoder emits meaningful DRED data rather than trivially
        // compressible silence.
        let mut enc = OpusEncoder::new(QualityProfile::GOOD).unwrap();
        let mut dec = crate::opus_dec::OpusDecoder::new(QualityProfile::GOOD).unwrap();

        let mut total_encoded_bytes = 0usize;
        // Run 50 frames (1 second) so DRED fills up and starts emitting.
        for frame_idx in 0..50 {
            let pcm_in: Vec<i16> = (0..960)
                .map(|i| {
                    let t = (frame_idx * 960 + i) as f64 / 48_000.0;
                    (8000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
                })
                .collect();
            let mut encoded = vec![0u8; 512];
            let n = enc.encode(&pcm_in, &mut encoded).unwrap();
            assert!(n > 0);
            total_encoded_bytes += n;

            let mut pcm_out = vec![0i16; 960];
            let samples = dec.decode(&encoded[..n], &mut pcm_out).unwrap();
            assert_eq!(samples, 960);
        }

        // Effective bitrate after 1 second of encoding.
        // Opus 24k base + ~1 kbps DRED ≈ 25 kbps ≈ 3125 bytes/sec.
        // Allow generous headroom (2000 lower bound, 8000 upper bound) —
        // this is a behavioral regression check, not a tight bitrate assertion.
        // The exact value is printed with --nocapture for diagnostic use.
        eprintln!(
            "[phase1 bitrate probe] legacy_fec_mode={} total_encoded={} bytes/sec",
            enc.is_legacy_fec_mode(),
            total_encoded_bytes
        );
        assert!(
            total_encoded_bytes > 2000,
            "encoder output too small: {total_encoded_bytes} bytes/sec (DRED likely not emitting)"
        );
        assert!(
            total_encoded_bytes < 8000,
            "encoder output too large: {total_encoded_bytes} bytes/sec"
        );
    }

    // ─── Phase 1 — set_profile updates DRED duration on tier switch ─────────

    #[test]
    fn profile_switch_refreshes_dred_duration() {
        // Start on GOOD (Opus 24k, DRED 20 frames), switch to DEGRADED
        // (Opus 6k, DRED 50 frames). The encoder should accept both profile
        // changes without error. We can't directly observe the DRED duration
        // inside libopus, but apply_protection_mode returns Ok for both.
        let mut enc = OpusEncoder::new(QualityProfile::GOOD).unwrap();
        assert_eq!(enc.codec_id, CodecId::Opus24k);

        enc.set_profile(QualityProfile::DEGRADED).unwrap();
        assert_eq!(enc.codec_id, CodecId::Opus6k);

        enc.set_profile(QualityProfile::STUDIO_64K).unwrap();
        assert_eq!(enc.codec_id, CodecId::Opus64k);
    }

    // ─── Phase 1 — Trait set_inband_fec is a no-op in DRED mode ─────────────

    #[test]
    fn set_inband_fec_noop_in_dred_mode() {
        if std::env::var(LEGACY_FEC_ENV).is_ok() {
            return;
        }
        let mut enc = OpusEncoder::new(QualityProfile::GOOD).unwrap();
        // Should not error, should not re-enable inband FEC internally.
        enc.set_inband_fec(true);
        // We can't directly query libopus's inband FEC state through opusic-c,
        // but the call must not panic and the encoder must still work.
        let pcm_in = vec![0i16; 960];
        let mut encoded = vec![0u8; 512];
        let n = enc.encode(&pcm_in, &mut encoded).unwrap();
        assert!(n > 0);
    }
}
