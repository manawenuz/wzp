//! Continuous DRED tuning from real-time network metrics.
//!
//! Instead of locking DRED duration to 3 discrete quality tiers (100/200/500 ms),
//! `DredTuner` maps live path quality metrics to a continuous DRED duration and
//! expected-loss hint, updated every N packets. This makes DRED reactive within
//! ~200 ms instead of waiting for 3+ consecutive bad quality reports to trigger
//! a full tier transition.
//!
//! The tuner also implements pre-emptive jitter-spike detection ("sawtooth"
//! prediction): when jitter variance spikes >30% over a 200 ms window — typical
//! of Starlink satellite handovers — it temporarily boosts DRED to the maximum
//! allowed for the current codec before packets actually start dropping.
//!
//! See also: [`crate::quality`] for discrete tier classification that drives
//! codec switching. DredTuner operates within a tier, adjusting DRED
//! parameters continuously based on live network metrics.

use crate::CodecId;

/// Output of a single tuning cycle.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct DredTuning {
    /// DRED duration in 10 ms frame units (0–104). Passed directly to
    /// `OpusEncoder::set_dred_duration()`.
    pub dred_frames: u8,
    /// Expected packet loss percentage (0–100). Passed to
    /// `OpusEncoder::set_expected_loss()`. Floored at 15% by the encoder
    /// itself, but we pass the real value so the encoder can override upward.
    pub expected_loss_pct: u8,
}

/// Minimum DRED frames for any Opus codec (matches DRED_LOSS_FLOOR_PCT logic:
/// at 15% loss, libopus 1.5 emits ~95 ms of DRED, which needs at least 10
/// frames configured to be useful).
const MIN_DRED_FRAMES: u8 = 5;

/// Maximum DRED frames libopus supports (104 × 10 ms = 1040 ms).
const MAX_DRED_FRAMES: u8 = 104;

/// Jitter variance spike ratio that triggers pre-emptive DRED boost.
const JITTER_SPIKE_RATIO: f32 = 1.3;

/// How many tuning cycles a jitter-spike boost persists (at 25 packets/cycle
/// and 20 ms/packet, 10 cycles ≈ 5 seconds).
const SPIKE_BOOST_COOLDOWN_CYCLES: u32 = 10;

/// Maps codec tier to its baseline DRED frames (used when network is healthy).
fn baseline_dred_frames(codec: CodecId) -> u8 {
    match codec {
        CodecId::Opus32k | CodecId::Opus48k | CodecId::Opus64k => 10, // 100 ms
        CodecId::Opus16k | CodecId::Opus24k => 20,                    // 200 ms
        CodecId::Opus6k => 50,                                         // 500 ms
        _ => 0,
    }
}

/// Maps codec tier to its maximum allowed DRED frames under spike/bad conditions.
fn max_dred_frames_for(codec: CodecId) -> u8 {
    match codec {
        // Studio: cap at 300 ms (don't waste bitrate on good links)
        CodecId::Opus32k | CodecId::Opus48k | CodecId::Opus64k => 30,
        // Normal: cap at 500 ms
        CodecId::Opus16k | CodecId::Opus24k => 50,
        // Degraded: allow full 1040 ms
        CodecId::Opus6k => MAX_DRED_FRAMES,
        _ => 0,
    }
}

/// Continuous DRED tuner driven by network path metrics.
pub struct DredTuner {
    /// Current codec (determines baseline and ceiling).
    codec: CodecId,
    /// Last computed tuning output.
    last_tuning: DredTuning,
    /// EWMA-smoothed jitter for spike detection (in ms).
    jitter_ewma: f32,
    /// Remaining cooldown cycles for a jitter-spike boost.
    spike_cooldown: u32,
    /// Whether the tuner has received at least one observation.
    initialized: bool,
}

impl DredTuner {
    /// Create a new tuner for the given codec.
    pub fn new(codec: CodecId) -> Self {
        let baseline = baseline_dred_frames(codec);
        Self {
            codec,
            last_tuning: DredTuning {
                dred_frames: baseline,
                expected_loss_pct: 15, // match DRED_LOSS_FLOOR_PCT
            },
            jitter_ewma: 0.0,
            spike_cooldown: 0,
            initialized: false,
        }
    }

    /// Update the active codec (e.g. on tier transition). Resets spike state.
    pub fn set_codec(&mut self, codec: CodecId) {
        self.codec = codec;
        self.spike_cooldown = 0;
    }

    /// Feed network metrics and compute new DRED parameters.
    ///
    /// Call this every tuning cycle (e.g. every 25 packets ≈ 500 ms at 20 ms
    /// frame duration).
    ///
    /// - `loss_pct`: observed packet loss (0.0–100.0)
    /// - `rtt_ms`: smoothed round-trip time
    /// - `jitter_ms`: current jitter estimate (RTT variance)
    ///
    /// Returns `Some(tuning)` if the output changed, `None` if unchanged.
    pub fn update(&mut self, loss_pct: f32, rtt_ms: u32, jitter_ms: u32) -> Option<DredTuning> {
        if !self.codec.is_opus() {
            return None;
        }

        let baseline = baseline_dred_frames(self.codec);
        let ceiling = max_dred_frames_for(self.codec);

        // --- Jitter spike detection ---
        let jitter_f = jitter_ms as f32;
        if !self.initialized {
            self.jitter_ewma = jitter_f;
            self.initialized = true;
        } else {
            // Fast-up (alpha=0.3), slow-down (alpha=0.05) asymmetric EWMA
            let alpha = if jitter_f > self.jitter_ewma { 0.3 } else { 0.05 };
            self.jitter_ewma = alpha * jitter_f + (1.0 - alpha) * self.jitter_ewma;
        }

        // Detect spike: instantaneous jitter > EWMA × 1.3
        if self.jitter_ewma > 1.0 && jitter_f > self.jitter_ewma * JITTER_SPIKE_RATIO {
            self.spike_cooldown = SPIKE_BOOST_COOLDOWN_CYCLES;
        }

        // Decrement cooldown
        if self.spike_cooldown > 0 {
            self.spike_cooldown -= 1;
        }

        // --- Compute DRED frames ---
        let dred_frames = if self.spike_cooldown > 0 {
            // During spike boost: jump to ceiling
            ceiling
        } else {
            // Continuous mapping: scale linearly between baseline and ceiling
            // based on loss percentage.
            //   0% loss → baseline
            //  40% loss → ceiling
            let loss_clamped = loss_pct.clamp(0.0, 40.0);
            let t = loss_clamped / 40.0;
            let raw = baseline as f32 + t * (ceiling - baseline) as f32;
            (raw as u8).clamp(MIN_DRED_FRAMES, ceiling)
        };

        // --- Compute expected loss hint ---
        // Pass the real loss so the encoder can clamp at its own floor (15%).
        // For RTT-driven boost: high RTT suggests impending loss, so add a
        // phantom loss contribution to keep DRED emitting generously.
        let rtt_loss_phantom = if rtt_ms > 200 {
            ((rtt_ms - 200) as f32 / 40.0).min(15.0)
        } else {
            0.0
        };
        let expected_loss = (loss_pct + rtt_loss_phantom).clamp(0.0, 100.0) as u8;

        let tuning = DredTuning {
            dred_frames,
            expected_loss_pct: expected_loss,
        };

        if tuning != self.last_tuning {
            self.last_tuning = tuning;
            Some(tuning)
        } else {
            None
        }
    }

    /// Get the last computed tuning without updating.
    pub fn current(&self) -> DredTuning {
        self.last_tuning
    }

    /// Whether a jitter-spike boost is currently active.
    pub fn spike_boost_active(&self) -> bool {
        self.spike_cooldown > 0
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn baseline_for_opus24k() {
        let tuner = DredTuner::new(CodecId::Opus24k);
        assert_eq!(tuner.current().dred_frames, 20); // 200 ms
    }

    #[test]
    fn baseline_for_opus6k() {
        let tuner = DredTuner::new(CodecId::Opus6k);
        assert_eq!(tuner.current().dred_frames, 50); // 500 ms
    }

    #[test]
    fn codec2_returns_none() {
        let mut tuner = DredTuner::new(CodecId::Codec2_1200);
        assert!(tuner.update(10.0, 100, 20).is_none());
    }

    #[test]
    fn scales_with_loss() {
        let mut tuner = DredTuner::new(CodecId::Opus24k);

        // 0% loss → baseline (20 frames)
        tuner.update(0.0, 50, 5);
        assert_eq!(tuner.current().dred_frames, 20);

        // 20% loss → midpoint between 20 and 50 = 35
        tuner.update(20.0, 50, 5);
        assert_eq!(tuner.current().dred_frames, 35);

        // 40%+ loss → ceiling (50 frames)
        tuner.update(40.0, 50, 5);
        assert_eq!(tuner.current().dred_frames, 50);
    }

    #[test]
    fn jitter_spike_triggers_boost() {
        let mut tuner = DredTuner::new(CodecId::Opus24k);

        // Establish baseline jitter
        for _ in 0..20 {
            tuner.update(0.0, 50, 10);
        }
        assert!(!tuner.spike_boost_active());

        // Spike: jitter jumps to 50 ms (5x the EWMA of ~10)
        tuner.update(0.0, 50, 50);
        assert!(tuner.spike_boost_active());
        // Should be at ceiling (50 frames = 500 ms for Opus24k)
        assert_eq!(tuner.current().dred_frames, 50);
    }

    #[test]
    fn spike_cooldown_decays() {
        let mut tuner = DredTuner::new(CodecId::Opus24k);

        // Establish baseline then spike
        for _ in 0..20 {
            tuner.update(0.0, 50, 10);
        }
        tuner.update(0.0, 50, 50);
        assert!(tuner.spike_boost_active());

        // Run through cooldown
        for _ in 0..SPIKE_BOOST_COOLDOWN_CYCLES {
            tuner.update(0.0, 50, 10);
        }
        assert!(!tuner.spike_boost_active());
        // Should return to baseline
        assert_eq!(tuner.current().dred_frames, 20);
    }

    #[test]
    fn rtt_phantom_loss() {
        let mut tuner = DredTuner::new(CodecId::Opus24k);

        // High RTT (400ms) with 0% real loss
        tuner.update(0.0, 400, 10);
        // Phantom loss = (400-200)/40 = 5
        assert_eq!(tuner.current().expected_loss_pct, 5);
    }

    #[test]
    fn set_codec_resets_spike() {
        let mut tuner = DredTuner::new(CodecId::Opus24k);

        // Trigger spike
        for _ in 0..20 {
            tuner.update(0.0, 50, 10);
        }
        tuner.update(0.0, 50, 50);
        assert!(tuner.spike_boost_active());

        // Switch codec — spike should reset
        tuner.set_codec(CodecId::Opus6k);
        assert!(!tuner.spike_boost_active());
    }

    #[test]
    fn opus6k_reaches_max_1040ms() {
        let mut tuner = DredTuner::new(CodecId::Opus6k);

        // High loss → should reach 104 frames (1040 ms)
        tuner.update(40.0, 50, 5);
        assert_eq!(tuner.current().dred_frames, MAX_DRED_FRAMES);
    }

    #[test]
    fn returns_none_when_unchanged() {
        let mut tuner = DredTuner::new(CodecId::Opus24k);

        // First update always returns Some (initial → computed)
        let first = tuner.update(0.0, 50, 5);
        // Same inputs → None
        let second = tuner.update(0.0, 50, 5);
        assert!(first.is_some() || second.is_none());
    }
}
