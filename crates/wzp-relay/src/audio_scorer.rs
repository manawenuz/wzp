//! Tier F audio scorer — behavioural entropy detection for abuse mitigation.
//!
//! Computes a `legitimacy ∈ [0, 1]` score over a 10–30 s observation window.
//! Features: IAT CoV, payload-size bimodality, silence fraction, bitrate
//! deviation, and Q-flag cadence.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use wzp_proto::{CodecId, MediaHeader, MediaType};

/// Maximum samples kept in rolling windows.
const MAX_IAT_SAMPLES: usize = 200;
const MAX_SIZE_SAMPLES: usize = 200;
const MAX_Q_INTERVALS: usize = 32;

/// Silence threshold: payload below this many bytes is treated as silence / CN.
const SILENCE_SIZE_THRESHOLD: usize = 16;

/// Observation window for bitrate tracking.
const BITRATE_WINDOW_SECS: u64 = 30;

// Number of payload-size histogram bins.
// (SIZE_BINS reserved for future histogram-based bimodality)

/// Verdict produced by the scorer after sufficient observation.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No suspicion. Score ≥ 0.7.
    Legitimate,
    /// Tightened monitoring. 0.3 ≤ score < 0.7.
    Suspect,
    /// High confidence of abuse. Score < 0.3.
    Abusive,
}

/// Audio-specific behavioural scorer (Tier F).
pub struct AudioScorer {
    /// Rolling inter-arrival times.
    iat_samples: VecDeque<Duration>,
    last_arrival: Option<Instant>,

    /// Rolling payload sizes.
    size_samples: VecDeque<usize>,

    /// Count of packets below silence threshold.
    silence_packets: u32,
    /// Total packets observed in current window.
    total_packets: u32,

    /// Bitrate window.
    window_start: Instant,
    window_bytes: u64,

    /// Q-flag arrival intervals.
    q_intervals: VecDeque<Duration>,
    last_q_flag: Option<Instant>,

    /// Codec declared at first packet (used for nominal bitrate baseline).
    declared_codec: Option<CodecId>,
}

impl AudioScorer {
    pub fn new() -> Self {
        Self {
            iat_samples: VecDeque::with_capacity(MAX_IAT_SAMPLES),
            last_arrival: None,
            size_samples: VecDeque::with_capacity(MAX_SIZE_SAMPLES),
            silence_packets: 0,
            total_packets: 0,
            window_start: Instant::now(),
            window_bytes: 0,
            q_intervals: VecDeque::with_capacity(MAX_Q_INTERVALS),
            last_q_flag: None,
            declared_codec: None,
        }
    }

    /// Feed one packet into the scorer.
    pub fn observe(&mut self, header: &MediaHeader, payload_len: usize, now: Instant) {
        // Ignore non-audio traffic.
        if header.media_type != MediaType::Audio {
            return;
        }

        if self.declared_codec.is_none() {
            self.declared_codec = Some(header.codec_id);
        }

        // IAT
        if let Some(last) = self.last_arrival {
            let iat = now.saturating_duration_since(last);
            self.iat_samples.push_back(iat);
            if self.iat_samples.len() > MAX_IAT_SAMPLES {
                self.iat_samples.pop_front();
            }
        }
        self.last_arrival = Some(now);

        // Payload size
        self.size_samples.push_back(payload_len);
        if self.size_samples.len() > MAX_SIZE_SAMPLES {
            self.size_samples.pop_front();
        }

        // Silence fraction
        self.total_packets += 1;
        if payload_len <= SILENCE_SIZE_THRESHOLD {
            self.silence_packets += 1;
        }

        // Bitrate window
        if now.duration_since(self.window_start) >= Duration::from_secs(BITRATE_WINDOW_SECS) {
            self.window_start = now;
            self.window_bytes = 0;
        }
        self.window_bytes += (MediaHeader::WIRE_SIZE + payload_len) as u64;

        // Q-flag cadence
        if header.has_quality() {
            if let Some(last) = self.last_q_flag {
                let interval = now.saturating_duration_since(last);
                self.q_intervals.push_back(interval);
                if self.q_intervals.len() > MAX_Q_INTERVALS {
                    self.q_intervals.pop_front();
                }
            }
            self.last_q_flag = Some(now);
        }
    }

    /// Compute legitimacy score ∈ [0, 1].
    ///
    /// Higher = more legitimate.  Returns `None` when insufficient samples
    /// have been collected (< 20 packets).
    pub fn legitimacy(&self) -> Option<f32> {
        if self.total_packets < 20 {
            return None;
        }

        let mut score = 1.0f32;

        // 1. IAT CoV penalty
        if let Some(cov) = self.iat_cov() {
            if cov > 0.4 {
                let penalty = ((cov - 0.4) / 0.6).min(1.0) * 0.25;
                score -= penalty as f32;
            }
        }

        // 2. Silence fraction penalty
        let silence_fraction = self.silence_fraction();
        if silence_fraction < 0.02 {
            let penalty = ((0.02 - silence_fraction) / 0.02).min(1.0) * 0.25;
            score -= penalty as f32;
        } else if silence_fraction > 0.60 {
            // Too much silence can also be suspicious (stuffed payloads)
            let penalty = ((silence_fraction - 0.60) / 0.40).min(1.0) * 0.15;
            score -= penalty as f32;
        }

        // 3. Bitrate deviation penalty
        if let Some(ratio) = self.bitrate_ratio() {
            if ratio > 1.20 {
                let penalty = ((ratio - 1.20) / 0.80).min(1.0) * 0.25;
                score -= penalty as f32;
            }
        }

        // 4. Q-flag cadence penalty
        if let Some(cv) = self.q_flag_cv() {
            // High variability in Q-flag spacing = suspicious
            if cv > 0.5 {
                let penalty = ((cv - 0.5) / 0.5).min(1.0) * 0.15;
                score -= penalty as f32;
            }
        } else {
            // No Q flags seen at all — mildly suspicious after many packets
            if self.total_packets > 100 {
                score -= 0.10;
            }
        }

        // 5. Payload-size bimodality bonus/penalty
        if let Some(bimodality) = self.size_bimodality() {
            // Bimodality score: 0 = unimodal, 1 = strongly bimodal
            // Legitimate audio is bimodal (speech + silence)
            if bimodality < 0.2 {
                score -= 0.10;
            }
        }

        Some(score.clamp(0.0, 1.0))
    }

    /// Map legitimacy score to a [`Verdict`].
    pub fn verdict(&self) -> Option<Verdict> {
        self.legitimacy().map(|s| {
            if s >= 0.7 {
                Verdict::Legitimate
            } else if s >= 0.3 {
                Verdict::Suspect
            } else {
                Verdict::Abusive
            }
        })
    }

    // ------------------------------------------------------------------
    // Feature extractors
    // ------------------------------------------------------------------

    /// Coefficient of variation of inter-arrival times.
    fn iat_cov(&self) -> Option<f64> {
        if self.iat_samples.len() < 10 {
            return None;
        }
        let mean = self
            .iat_samples
            .iter()
            .map(|d| d.as_secs_f64())
            .sum::<f64>()
            / self.iat_samples.len() as f64;
        if mean == 0.0 {
            return None;
        }
        let variance = self
            .iat_samples
            .iter()
            .map(|d| {
                let diff = d.as_secs_f64() - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.iat_samples.len() as f64;
        let std = variance.sqrt();
        Some(std / mean)
    }

    /// Fraction of packets that are silence / comfort-noise sized.
    fn silence_fraction(&self) -> f64 {
        if self.total_packets == 0 {
            return 0.0;
        }
        self.silence_packets as f64 / self.total_packets as f64
    }

    /// Ratio of observed bitrate to nominal bitrate over the 30 s window.
    fn bitrate_ratio(&self) -> Option<f64> {
        let codec = self.declared_codec?;
        let nominal_bps = codec.bitrate_bps() as f64;
        if nominal_bps == 0.0 {
            return None;
        }
        let observed_bps = self.window_bytes as f64 * 8.0 / BITRATE_WINDOW_SECS as f64;
        Some(observed_bps / nominal_bps)
    }

    /// Coefficient of variation of Q-flag intervals.
    fn q_flag_cv(&self) -> Option<f64> {
        if self.q_intervals.len() < 3 {
            return None;
        }
        let mean = self
            .q_intervals
            .iter()
            .map(|d| d.as_secs_f64())
            .sum::<f64>()
            / self.q_intervals.len() as f64;
        if mean == 0.0 {
            return None;
        }
        let variance = self
            .q_intervals
            .iter()
            .map(|d| {
                let diff = d.as_secs_f64() - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.q_intervals.len() as f64;
        let std = variance.sqrt();
        Some(std / mean)
    }

    /// Simple bimodality score based on a 2-bin histogram.
    ///
    /// Splits payload sizes into "small" (≤ threshold) and "large" bins.
    /// Returns a score in [0, 1] where 1 = strongly bimodal.
    fn size_bimodality(&self) -> Option<f64> {
        if self.size_samples.len() < 20 {
            return None;
        }
        let small = self
            .size_samples
            .iter()
            .filter(|&&s| s <= SILENCE_SIZE_THRESHOLD)
            .count();
        let large = self.size_samples.len() - small;
        let total = self.size_samples.len() as f64;
        let p_small = small as f64 / total;
        let _p_large = large as f64 / total;
        // Max bimodality when both bins are equally populated (~0.5 each)
        let bimodality = 1.0 - (p_small - 0.5).abs() * 2.0;
        Some(bimodality)
    }
}

impl Default for AudioScorer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn audio_header(payload_len: usize, has_quality: bool) -> MediaHeader {
        MediaHeader {
            version: 2,
            flags: if has_quality { 0x40 } else { 0 },
            media_type: MediaType::Audio,
            codec_id: CodecId::Opus24k,
            stream_id: 0,
            fec_ratio: 0,
            seq: 0,
            timestamp: 0,
            fec_block: 0,
        }
    }

    #[test]
    fn audio_scorer_ignores_video() {
        let mut scorer = AudioScorer::new();
        let mut h = audio_header(100, false);
        h.media_type = MediaType::Video;
        scorer.observe(&h, 100, Instant::now());
        assert_eq!(scorer.total_packets, 0);
    }

    #[test]
    fn audio_scorer_counts_packets() {
        let mut scorer = AudioScorer::new();
        for i in 0..25 {
            let h = audio_header(100, false);
            scorer.observe(&h, 100, Instant::now() + Duration::from_millis(i * 20));
        }
        assert_eq!(scorer.total_packets, 25);
        assert!(scorer.legitimacy().is_some());
    }

    #[test]
    fn audio_scorer_legitimate_traffic() {
        let mut scorer = AudioScorer::new();
        let base = Instant::now();
        // Simulate 200 packets of legitimate audio:
        // ~20 ms IAT, mixed speech (100 B) and silence (8 B), periodic Q flags.
        for i in 0..200 {
            let payload = if i % 3 == 0 { 8 } else { 100 };
            let has_q = i % 10 == 0;
            let h = audio_header(payload, has_q);
            scorer.observe(&h, payload, base + Duration::from_millis(i * 20));
        }
        let leg = scorer.legitimacy().unwrap();
        assert!(
            leg >= 0.7,
            "legitimate traffic should score ≥ 0.7, got {leg}"
        );
        assert_eq!(scorer.verdict(), Some(Verdict::Legitimate));
    }

    #[test]
    fn audio_scorer_abusive_uniform_iat() {
        let mut scorer = AudioScorer::new();
        let base = Instant::now();
        // Uniform IAT (no jitter), all same size, no Q flags — tunnel-like
        for i in 0..200 {
            let h = audio_header(200, false);
            scorer.observe(&h, 200, base + Duration::from_millis(i * 20));
        }
        let leg = scorer.legitimacy().unwrap();
        assert!(
            leg < 0.6,
            "uniform tunnel-like traffic should score < 0.6, got {leg}"
        );
    }

    #[test]
    fn audio_scorer_abusive_no_silence() {
        let mut scorer = AudioScorer::new();
        let base = Instant::now();
        // No silence packets at all, very regular IAT
        for i in 0..200 {
            let h = audio_header(150, false);
            scorer.observe(&h, 150, base + Duration::from_millis(i * 20));
        }
        let leg = scorer.legitimacy().unwrap();
        assert!(
            leg < 0.6,
            "no-silence traffic should score < 0.6, got {leg}"
        );
    }

    #[test]
    fn audio_scorer_insufficient_samples() {
        let scorer = AudioScorer::new();
        assert_eq!(scorer.legitimacy(), None);
        assert_eq!(scorer.verdict(), None);
    }

    #[test]
    fn silence_fraction_computed_correctly() {
        let mut scorer = AudioScorer::new();
        let base = Instant::now();
        for i in 0..100 {
            let payload = if i < 30 { 8 } else { 100 };
            let h = audio_header(payload, false);
            scorer.observe(&h, payload, base + Duration::from_millis(i * 20));
        }
        assert!((scorer.silence_fraction() - 0.30).abs() < 0.01);
    }

    #[test]
    fn bitrate_ratio_saturates_when_no_codec() {
        let scorer = AudioScorer::new();
        assert_eq!(scorer.bitrate_ratio(), None);
    }

    #[test]
    fn q_flag_cv_regular_spacing() {
        let mut scorer = AudioScorer::new();
        let base = Instant::now();
        for i in 0..50 {
            let has_q = i % 5 == 0;
            let h = audio_header(100, has_q);
            scorer.observe(&h, 100, base + Duration::from_millis(i * 20));
        }
        let cv = scorer.q_flag_cv().unwrap();
        assert!(
            cv < 0.1,
            "regular Q-flag spacing should have CV < 0.1, got {cv}"
        );
    }

    #[test]
    fn size_bimodality_for_mixed_traffic() {
        let mut scorer = AudioScorer::new();
        let base = Instant::now();
        for i in 0..100 {
            let payload = if i % 2 == 0 { 8 } else { 120 };
            let h = audio_header(payload, false);
            scorer.observe(&h, payload, base + Duration::from_millis(i * 20));
        }
        let bim = scorer.size_bimodality().unwrap();
        assert!(
            bim > 0.8,
            "perfectly mixed small/large should be highly bimodal, got {bim}"
        );
    }

    #[test]
    fn size_bimodality_for_uniform_traffic() {
        let mut scorer = AudioScorer::new();
        let base = Instant::now();
        for i in 0..100 {
            let h = audio_header(100, false);
            scorer.observe(&h, 100, base + Duration::from_millis(i * 20));
        }
        let bim = scorer.size_bimodality().unwrap();
        assert!(
            bim < 0.3,
            "uniform size traffic should be unimodal, got {bim}"
        );
    }
}
