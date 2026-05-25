//! Tier F video scorer — behavioural detection for video abuse.
//!
//! Computes a `legitimacy ∈ [0, 1]` score over a 5–15 s observation window.
//! Features: keyframe periodicity (CoV), I/P frame ratio, BWE responsiveness.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use wzp_proto::{MediaHeader, MediaType};

use crate::verdict::Verdict;

/// Maximum keyframe inter-arrival samples kept.
const MAX_KF_SAMPLES: usize = 50;

/// Minimum packets before a legitimacy score is produced.
const MIN_PACKETS: u32 = 30;

/// Packet threshold after which zero keyframes is treated as abusive.
const NO_KEYFRAME_THRESHOLD: u32 = 120;

/// Packet threshold after which all-I-frame streams are penalised.
const ALL_I_FRAME_THRESHOLD: u32 = 30;

/// Video-specific behavioural scorer (Tier F).
pub struct VideoScorer {
    /// Rolling inter-arrival times between keyframes.
    keyframe_iat_samples: VecDeque<Duration>,
    last_keyframe_at: Option<Instant>,

    /// I-frame count in current observation window.
    i_frame_count: u32,
    /// P-frame count in current observation window.
    p_frame_count: u32,

    /// Bitrate window.
    window_start: Instant,
    window_bytes: u64,

    /// BWE responsiveness tracking.
    last_bwe_kbps: Option<u32>,
    bitrate_at_last_bwe: Option<f64>,
    responsive_count: u32,
    unresponsive_count: u32,

    /// Total video packets observed.
    total_packets: u32,
}

impl VideoScorer {
    pub fn new() -> Self {
        Self {
            keyframe_iat_samples: VecDeque::with_capacity(MAX_KF_SAMPLES),
            last_keyframe_at: None,
            i_frame_count: 0,
            p_frame_count: 0,
            window_start: Instant::now(),
            window_bytes: 0,
            last_bwe_kbps: None,
            bitrate_at_last_bwe: None,
            responsive_count: 0,
            unresponsive_count: 0,
            total_packets: 0,
        }
    }

    /// Feed one packet into the scorer.
    ///
    /// `bwe_kbps` is the most recent downstream bandwidth estimate, if any.
    pub fn observe(
        &mut self,
        header: &MediaHeader,
        payload_len: usize,
        now: Instant,
        bwe_kbps: Option<u32>,
    ) {
        // Ignore non-video traffic.
        if header.media_type != MediaType::Video {
            return;
        }

        if self.total_packets == 0 {
            self.window_start = now;
        }
        self.total_packets += 1;

        // Track keyframes vs P-frames.
        if header.is_keyframe() {
            self.i_frame_count += 1;
            if let Some(last) = self.last_keyframe_at {
                let iat = now.saturating_duration_since(last);
                self.keyframe_iat_samples.push_back(iat);
                if self.keyframe_iat_samples.len() > MAX_KF_SAMPLES {
                    self.keyframe_iat_samples.pop_front();
                }
            }
            self.last_keyframe_at = Some(now);
        } else {
            self.p_frame_count += 1;
        }

        // Track bitrate window.
        self.window_bytes += (MediaHeader::WIRE_SIZE + payload_len) as u64;

        // BWE responsiveness check.
        if let Some(bwe) = bwe_kbps {
            let current_rate = self.current_bitrate(now);
            if let Some(last_bwe) = self.last_bwe_kbps {
                let bwe_drop = if last_bwe > 0 {
                    (last_bwe as f64 - bwe as f64) / last_bwe as f64
                } else {
                    0.0
                };
                if bwe_drop > 0.30 {
                    let last_rate = self.bitrate_at_last_bwe.unwrap_or(0.0);
                    let rate_drop = if last_rate > 0.0 {
                        (last_rate - current_rate) / last_rate
                    } else {
                        0.0
                    };
                    if rate_drop >= 0.10 {
                        self.responsive_count += 1;
                    } else {
                        self.unresponsive_count += 1;
                    }
                }
            }
            self.last_bwe_kbps = Some(bwe);
            self.bitrate_at_last_bwe = Some(current_rate);
            self.window_start = now;
            self.window_bytes = 0;
        }
    }

    /// Compute legitimacy score ∈ [0, 1].
    ///
    /// Higher = more legitimate.  Returns `None` when insufficient samples
    /// have been collected (< 30 packets).
    pub fn legitimacy(&self) -> Option<f32> {
        if self.total_packets < MIN_PACKETS {
            return None;
        }

        let mut score = 1.0f32;

        // 1. Keyframe regularity (0.35 weight).
        if let Some(reg) = self.keyframe_regularity() {
            score -= (1.0 - reg as f32) * 0.35;
        } else if self.i_frame_count == 0 && self.total_packets > NO_KEYFRAME_THRESHOLD {
            score -= 0.50;
        } else {
            score -= 0.10;
        }

        // 2. I/P ratio (0.30 weight).
        if self.p_frame_count == 0 && self.total_packets > ALL_I_FRAME_THRESHOLD {
            score -= 0.60;
        } else if let Some(ip) = self.ip_ratio() {
            score -= (1.0 - ip as f32) * 0.30;
        } else {
            score -= 0.10;
        }

        // 3. BWE responsiveness (0.40 weight).
        if let Some(bwe) = self.bwe_responsiveness() {
            score -= (1.0 - bwe as f32) * 0.40;
        } else {
            score -= 0.15;
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

    /// Keyframe regularity score ∈ [0, 1] where 1 = perfectly regular.
    fn keyframe_regularity(&self) -> Option<f64> {
        if self.keyframe_iat_samples.len() < 3 {
            return None;
        }
        let mean = self
            .keyframe_iat_samples
            .iter()
            .map(|d| d.as_secs_f64())
            .sum::<f64>()
            / self.keyframe_iat_samples.len() as f64;
        if mean == 0.0 {
            return None;
        }
        let variance = self
            .keyframe_iat_samples
            .iter()
            .map(|d| {
                let diff = d.as_secs_f64() - mean;
                diff * diff
            })
            .sum::<f64>()
            / self.keyframe_iat_samples.len() as f64;
        let std = variance.sqrt();
        let cov = std / mean;
        // Map CoV to regularity: cov = 0 → 1.0, cov → ∞ → 0.0.
        Some(1.0 / (1.0 + cov))
    }

    /// I/P ratio score ∈ [0, 1] where 1 = healthy GOP, 0 = all-I-frames.
    fn ip_ratio(&self) -> Option<f64> {
        if self.i_frame_count == 0 {
            return None;
        }
        if self.p_frame_count == 0 {
            return Some(0.0);
        }
        let p_per_i = self.p_frame_count as f64 / self.i_frame_count as f64;
        // Legitimate: P-per-I ≥ 29 (GOP 30).
        // Abusive: P-per-I < 5 (too many I-frames).
        let score = if p_per_i >= 29.0 {
            1.0
        } else if p_per_i <= 5.0 {
            0.0
        } else {
            (p_per_i - 5.0) / (29.0 - 5.0)
        };
        Some(score)
    }

    /// BWE responsiveness score ∈ [0, 1] where 1 = always responsive.
    fn bwe_responsiveness(&self) -> Option<f64> {
        let total = self.responsive_count + self.unresponsive_count;
        if total == 0 {
            return None;
        }
        let responsive = self.responsive_count as f64 / total as f64;
        Some(responsive)
    }

    /// Current bitrate in kbps over the active window.
    fn current_bitrate(&self, now: Instant) -> f64 {
        let elapsed = now
            .saturating_duration_since(self.window_start)
            .as_secs_f64();
        if elapsed > 0.0 {
            self.window_bytes as f64 * 8.0 / 1000.0 / elapsed
        } else {
            0.0
        }
    }
}

impl Default for VideoScorer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wzp_proto::{CodecId, MediaType};

    fn video_header(is_keyframe: bool) -> MediaHeader {
        MediaHeader {
            version: 2,
            flags: if is_keyframe {
                MediaHeader::FLAG_KEYFRAME
            } else {
                0
            },
            media_type: MediaType::Video,
            codec_id: CodecId::H264Baseline,
            stream_id: 0,
            fec_ratio: 0,
            seq: 0,
            timestamp: 0,
            fec_block: 0,
        }
    }

    fn audio_header() -> MediaHeader {
        MediaHeader {
            version: 2,
            flags: 0,
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
    fn video_scorer_ignores_audio() {
        let mut scorer = VideoScorer::new();
        let h = audio_header();
        scorer.observe(&h, 100, Instant::now(), None);
        assert_eq!(scorer.total_packets, 0);
    }

    #[test]
    fn video_scorer_counts_packets() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();
        for i in 0..35 {
            let h = video_header(i % 30 == 0);
            scorer.observe(&h, 500, base + Duration::from_millis(i * 33), None);
        }
        assert_eq!(scorer.total_packets, 35);
        assert!(scorer.legitimacy().is_some());
    }

    #[test]
    fn video_scorer_insufficient_samples() {
        let scorer = VideoScorer::new();
        assert_eq!(scorer.legitimacy(), None);
        assert_eq!(scorer.verdict(), None);
    }

    #[test]
    fn video_scorer_legitimate_traffic() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();
        // Simulate 150 packets of legitimate 30 fps video:
        // GOP 30 (keyframe every 30 frames ≈ 1 s).
        for i in 0..150 {
            let is_kf = i % 30 == 0;
            let payload = if is_kf { 2000 } else { 500 };
            let h = video_header(is_kf);
            let now = base + Duration::from_millis(i * 33);
            let bwe = if i == 60 {
                Some(4000)
            } else if i == 120 {
                Some(4000)
            } else {
                None
            };
            scorer.observe(&h, payload, now, bwe);
        }
        let leg = scorer.legitimacy().unwrap();
        assert!(
            leg >= 0.6,
            "legitimate traffic should score ≥ 0.6, got {leg}"
        );
        assert_eq!(scorer.verdict(), Some(Verdict::Legitimate));
    }

    #[test]
    fn video_scorer_abusive_no_keyframes() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();
        // 150 packets, no keyframes at all.
        for i in 0..150 {
            let h = video_header(false);
            scorer.observe(&h, 500, base + Duration::from_millis(i * 33), None);
        }
        let leg = scorer.legitimacy().unwrap();
        assert!(
            leg < 0.3,
            "no-keyframe traffic should score < 0.3, got {leg}"
        );
        assert_eq!(scorer.verdict(), Some(Verdict::Abusive));
    }

    #[test]
    fn video_scorer_ip_ratio_out_of_range() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();
        // 100 packets, all keyframes (all-I-frame stream).
        for i in 0..100 {
            let h = video_header(true);
            scorer.observe(&h, 2000, base + Duration::from_millis(i * 33), None);
        }
        let leg = scorer.legitimacy().unwrap();
        assert!(
            leg < 0.3,
            "all-I-frame traffic should score < 0.3, got {leg}"
        );
        assert_eq!(scorer.verdict(), Some(Verdict::Abusive));
    }

    #[test]
    fn video_scorer_abusive_bwe_unresponsive() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();
        // 60 packets at constant rate.
        for i in 0..60 {
            let h = video_header(i % 30 == 0);
            let payload = if i % 30 == 0 { 2000 } else { 500 };
            scorer.observe(&h, payload, base + Duration::from_millis(i * 33), None);
        }
        // BWE = 4000 kbps.
        let h = video_header(false);
        scorer.observe(&h, 500, base + Duration::from_millis(60 * 33), Some(4000));

        // Another 60 packets at the same rate despite lower BWE.
        for i in 60..120 {
            let h = video_header(i % 30 == 0);
            let payload = if i % 30 == 0 { 2000 } else { 500 };
            scorer.observe(&h, payload, base + Duration::from_millis(i * 33), None);
        }
        // BWE drops 50 % but bitrate unchanged → unresponsive.
        let h = video_header(false);
        scorer.observe(&h, 500, base + Duration::from_millis(120 * 33), Some(2000));

        let bwe = scorer.bwe_responsiveness().unwrap();
        assert!(
            bwe < 0.5,
            "unresponsive stream should have low BWE score, got {bwe}"
        );
        let leg = scorer.legitimacy().unwrap();
        assert!(
            leg < 0.7,
            "BWE-unresponsive traffic should score < 0.7, got {leg}"
        );
    }

    #[test]
    fn keyframe_regularity_perfect_gop() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();
        // 120 packets → 4 keyframes → 3 IAT samples (needs ≥ 3).
        for i in 0..120 {
            let h = video_header(i % 30 == 0);
            scorer.observe(&h, 500, base + Duration::from_millis(i * 33), None);
        }
        let reg = scorer.keyframe_regularity().unwrap();
        assert!(
            reg > 0.9,
            "perfect GOP should have very high regularity, got {reg}"
        );
    }

    #[test]
    fn keyframe_regularity_random() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();
        // Explicitly irregular keyframe spacing.
        let kf_positions = [5, 15, 65, 80, 150, 165, 230, 260, 310];
        for i in 0..320 {
            let is_kf = kf_positions.contains(&i);
            let h = video_header(is_kf);
            scorer.observe(&h, 500, base + Duration::from_millis(i * 33), None);
        }
        let reg = scorer.keyframe_regularity().unwrap();
        assert!(
            reg < 0.8,
            "random GOP should have lower regularity, got {reg}"
        );
    }

    #[test]
    fn bwe_responsive_drop() {
        let mut scorer = VideoScorer::new();
        let base = Instant::now();

        // First window: high rate.
        for i in 0..60 {
            let h = video_header(i % 30 == 0);
            let payload = if i % 30 == 0 { 2000 } else { 1000 };
            scorer.observe(&h, payload, base + Duration::from_millis(i * 33), None);
        }
        let h = video_header(false);
        scorer.observe(&h, 1000, base + Duration::from_millis(60 * 33), Some(4000));

        // Second window: lower rate (responsive to BWE drop).
        for i in 60..120 {
            let h = video_header(i % 30 == 0);
            let payload = if i % 30 == 0 { 500 } else { 250 };
            scorer.observe(&h, payload, base + Duration::from_millis(i * 33), None);
        }
        let h = video_header(false);
        scorer.observe(&h, 250, base + Duration::from_millis(120 * 33), Some(1500));

        let bwe = scorer.bwe_responsiveness().unwrap();
        assert!(
            bwe > 0.5,
            "responsive stream should have high BWE score, got {bwe}"
        );
    }
}
