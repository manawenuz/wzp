//! GCC-style bandwidth estimation and congestion control.
//!
//! Tracks available bandwidth using delay-based and loss-based signals,
//! then adjusts the sending bitrate to avoid congestion. The estimator
//! uses multiplicative decrease (15%) on congestion and additive increase
//! (5%) during underuse, following the general shape of Google Congestion
//! Control (GCC).

use std::collections::VecDeque;
use std::time::Instant;

use crate::packet::QualityReport;
use crate::QualityProfile;

/// Network congestion state derived from delay and loss signals.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum CongestionState {
    /// Network is fine, can increase bandwidth.
    Underuse,
    /// Normal operation.
    Normal,
    /// Congestion detected, should decrease bandwidth.
    Overuse,
}

/// Detects congestion from increasing RTT using an exponential moving average.
///
/// Maintains a baseline RTT (minimum observed) and compares the smoothed RTT
/// against it. If `rtt_ema > baseline * threshold_ratio`, congestion is detected.
/// The baseline slowly drifts upward to handle route changes.
struct DelayBasedDetector {
    /// Baseline RTT (minimum observed).
    baseline_rtt_ms: f64,
    /// EMA of recent RTT.
    rtt_ema: f64,
    /// EMA smoothing factor.
    alpha: f64,
    /// Threshold: if rtt_ema > baseline * threshold_ratio, congestion detected.
    threshold_ratio: f64,
    /// Current state.
    state: CongestionState,
    /// Whether we have received any RTT sample yet.
    initialized: bool,
    /// Drift factor: baseline slowly increases each update to track route changes.
    baseline_drift: f64,
}

impl DelayBasedDetector {
    fn new() -> Self {
        Self {
            baseline_rtt_ms: f64::MAX,
            rtt_ema: 0.0,
            alpha: 0.3,
            threshold_ratio: 1.5,
            state: CongestionState::Normal,
            initialized: false,
            baseline_drift: 0.001,
        }
    }

    /// Update the detector with a new RTT sample.
    fn update(&mut self, rtt_ms: f64) {
        if !self.initialized {
            self.baseline_rtt_ms = rtt_ms;
            self.rtt_ema = rtt_ms;
            self.initialized = true;
            self.state = CongestionState::Normal;
            return;
        }

        // Track minimum RTT as baseline.
        if rtt_ms < self.baseline_rtt_ms {
            self.baseline_rtt_ms = rtt_ms;
        } else {
            // Slowly drift baseline upward to handle route changes.
            self.baseline_rtt_ms += self.baseline_drift * (rtt_ms - self.baseline_rtt_ms);
        }

        // Update EMA.
        self.rtt_ema = self.alpha * rtt_ms + (1.0 - self.alpha) * self.rtt_ema;

        // Determine state.
        let overuse_threshold = self.baseline_rtt_ms * self.threshold_ratio;
        let underuse_threshold = self.baseline_rtt_ms * 1.1;

        if self.rtt_ema > overuse_threshold {
            self.state = CongestionState::Overuse;
        } else if self.rtt_ema < underuse_threshold {
            self.state = CongestionState::Underuse;
        } else {
            self.state = CongestionState::Normal;
        }
    }

    fn state(&self) -> CongestionState {
        self.state
    }
}

/// Detects congestion from packet loss using a sliding window average.
struct LossBasedDetector {
    /// Recent loss percentages (sliding window).
    loss_window: VecDeque<f64>,
    /// Maximum window size.
    window_size: usize,
    /// Loss threshold for congestion (default 5%).
    threshold_pct: f64,
}

impl LossBasedDetector {
    fn new() -> Self {
        Self {
            loss_window: VecDeque::with_capacity(10),
            window_size: 10,
            threshold_pct: 5.0,
        }
    }

    /// Add a loss percentage sample to the window.
    fn update(&mut self, loss_pct: f64) {
        if self.loss_window.len() >= self.window_size {
            self.loss_window.pop_front();
        }
        self.loss_window.push_back(loss_pct);
    }

    /// Returns true if the average loss in the window exceeds the threshold.
    fn is_congested(&self) -> bool {
        if self.loss_window.is_empty() {
            return false;
        }
        let avg = self.loss_window.iter().sum::<f64>() / self.loss_window.len() as f64;
        avg > self.threshold_pct
    }
}

// ─── BandwidthEstimator ─────────────────────────────────────────────────────

/// GCC-style bandwidth estimator that tracks available bandwidth using
/// delay-based and loss-based congestion signals.
///
/// # Algorithm
///
/// - **Overuse** (delay or loss): multiplicative decrease by 15%.
/// - **Underuse** (delay) with no loss congestion: additive increase by 5%.
/// - **Normal**: hold steady.
/// - Result is always clamped to `[min_bw_kbps, max_bw_kbps]`.
pub struct BandwidthEstimator {
    /// Current estimated bandwidth in kbps.
    estimated_bw_kbps: f64,
    /// Minimum bandwidth floor (don't go below this).
    min_bw_kbps: f64,
    /// Maximum bandwidth ceiling.
    max_bw_kbps: f64,
    /// Delay-based detector state.
    delay_detector: DelayBasedDetector,
    /// Loss-based detector state.
    loss_detector: LossBasedDetector,
    /// Last update timestamp.
    last_update: Option<Instant>,
}

/// Multiplicative decrease factor applied on congestion (15% reduction).
const DECREASE_FACTOR: f64 = 0.85;
/// Additive increase factor applied during underuse (5% of current estimate).
const INCREASE_FACTOR: f64 = 0.05;

impl BandwidthEstimator {
    /// Create a new bandwidth estimator.
    ///
    /// - `initial_bw_kbps`: starting bandwidth estimate.
    /// - `min`: minimum bandwidth floor in kbps.
    /// - `max`: maximum bandwidth ceiling in kbps.
    pub fn new(initial_bw_kbps: f64, min: f64, max: f64) -> Self {
        Self {
            estimated_bw_kbps: initial_bw_kbps,
            min_bw_kbps: min,
            max_bw_kbps: max,
            delay_detector: DelayBasedDetector::new(),
            loss_detector: LossBasedDetector::new(),
            last_update: None,
        }
    }

    /// Update the estimator with new network observations.
    ///
    /// Returns the new estimated bandwidth in kbps.
    ///
    /// - If delay overuse OR loss congested: decrease by 15% (multiplicative decrease).
    /// - If delay underuse AND not loss congested: increase by 5% (additive increase).
    /// - If normal: hold steady.
    /// - Result is clamped to `[min, max]`.
    pub fn update(&mut self, rtt_ms: f64, loss_pct: f64, _jitter_ms: f64) -> f64 {
        self.delay_detector.update(rtt_ms);
        self.loss_detector.update(loss_pct);
        self.last_update = Some(Instant::now());

        let delay_state = self.delay_detector.state();
        let loss_congested = self.loss_detector.is_congested();

        if delay_state == CongestionState::Overuse || loss_congested {
            // Multiplicative decrease.
            self.estimated_bw_kbps *= DECREASE_FACTOR;
        } else if delay_state == CongestionState::Underuse && !loss_congested {
            // Additive increase.
            self.estimated_bw_kbps += self.estimated_bw_kbps * INCREASE_FACTOR;
        }
        // Normal: hold steady — no change.

        // Clamp to [min, max].
        self.estimated_bw_kbps = self
            .estimated_bw_kbps
            .clamp(self.min_bw_kbps, self.max_bw_kbps);

        self.estimated_bw_kbps
    }

    /// Current estimated bandwidth in kbps.
    pub fn estimated_kbps(&self) -> f64 {
        self.estimated_bw_kbps
    }

    /// Current congestion state (derived from delay detector).
    pub fn congestion_state(&self) -> CongestionState {
        self.delay_detector.state()
    }

    /// Convenience method: update from a `QualityReport`.
    ///
    /// Extracts RTT, loss, and jitter from the report and feeds them into
    /// the estimator.
    pub fn from_quality_report(&mut self, report: &QualityReport) -> f64 {
        let rtt_ms = report.rtt_ms() as f64;
        let loss_pct = report.loss_percent() as f64;
        let jitter_ms = report.jitter_ms as f64;
        self.update(rtt_ms, loss_pct, jitter_ms)
    }

    /// Recommend a `QualityProfile` based on the current bandwidth estimate.
    ///
    /// - bw >= 25 kbps -> GOOD (Opus 24k + 20% FEC = ~28.8 kbps total)
    /// - bw >= 8 kbps  -> DEGRADED (Opus 6k + 50% FEC = ~9.0 kbps)
    /// - bw < 8 kbps   -> CATASTROPHIC (Codec2 1.2k + 100% FEC = ~2.4 kbps)
    pub fn recommended_profile(&self) -> QualityProfile {
        if self.estimated_bw_kbps >= 25.0 {
            QualityProfile::GOOD
        } else if self.estimated_bw_kbps >= 8.0 {
            QualityProfile::DEGRADED
        } else {
            QualityProfile::CATASTROPHIC
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_bandwidth() {
        let bwe = BandwidthEstimator::new(50.0, 2.0, 100.0);
        assert!((bwe.estimated_kbps() - 50.0).abs() < f64::EPSILON);
    }

    #[test]
    fn stable_network_holds_bandwidth() {
        let mut bwe = BandwidthEstimator::new(50.0, 2.0, 100.0);
        // Feed stable, low RTT and 0% loss — after initial sample sets baseline,
        // subsequent identical RTT should be underuse (rtt_ema < baseline * 1.1),
        // causing slow increases. The bandwidth should stay near initial or grow slightly.
        let initial = bwe.estimated_kbps();
        for _ in 0..20 {
            bwe.update(30.0, 0.0, 5.0);
        }
        // Should not have decreased significantly.
        assert!(
            bwe.estimated_kbps() >= initial,
            "bandwidth should not decrease on stable network: got {} vs initial {}",
            bwe.estimated_kbps(),
            initial
        );
    }

    #[test]
    fn high_rtt_decreases_bandwidth() {
        let mut bwe = BandwidthEstimator::new(50.0, 2.0, 100.0);
        // Establish a low baseline.
        for _ in 0..5 {
            bwe.update(20.0, 0.0, 2.0);
        }
        let before = bwe.estimated_kbps();

        // Now feed high RTT to trigger overuse.
        for _ in 0..10 {
            bwe.update(200.0, 0.0, 10.0);
        }
        assert!(
            bwe.estimated_kbps() < before,
            "bandwidth should decrease on high RTT: got {} vs before {}",
            bwe.estimated_kbps(),
            before
        );
    }

    #[test]
    fn high_loss_decreases_bandwidth() {
        let mut bwe = BandwidthEstimator::new(50.0, 2.0, 100.0);
        let before = bwe.estimated_kbps();

        // Feed 10% loss repeatedly (above the 5% threshold).
        for _ in 0..15 {
            bwe.update(20.0, 10.0, 2.0);
        }
        assert!(
            bwe.estimated_kbps() < before,
            "bandwidth should decrease on high loss: got {} vs before {}",
            bwe.estimated_kbps(),
            before
        );
    }

    #[test]
    fn recovery_increases_bandwidth() {
        let mut bwe = BandwidthEstimator::new(50.0, 2.0, 100.0);

        // Drive bandwidth down with high RTT.
        for _ in 0..5 {
            bwe.update(20.0, 0.0, 2.0);
        }
        for _ in 0..20 {
            bwe.update(200.0, 0.0, 10.0);
        }
        let low_bw = bwe.estimated_kbps();
        assert!(low_bw < 50.0, "should have decreased");

        // Now feed good conditions — low RTT should be underuse, causing increase.
        // Reset the baseline by feeding very low RTT.
        for _ in 0..30 {
            bwe.update(10.0, 0.0, 1.0);
        }
        assert!(
            bwe.estimated_kbps() > low_bw,
            "bandwidth should recover: got {} vs low {}",
            bwe.estimated_kbps(),
            low_bw
        );
    }

    #[test]
    fn bandwidth_clamped_to_min() {
        let mut bwe = BandwidthEstimator::new(10.0, 5.0, 100.0);
        // Keep feeding congestion to drive bandwidth down.
        for _ in 0..5 {
            bwe.update(20.0, 0.0, 2.0);
        }
        for _ in 0..100 {
            bwe.update(500.0, 50.0, 100.0);
        }
        assert!(
            (bwe.estimated_kbps() - 5.0).abs() < f64::EPSILON,
            "bandwidth should be clamped to min: got {}",
            bwe.estimated_kbps()
        );
    }

    #[test]
    fn bandwidth_clamped_to_max() {
        let mut bwe = BandwidthEstimator::new(90.0, 2.0, 100.0);
        // Keep feeding great conditions to drive bandwidth up.
        for _ in 0..200 {
            bwe.update(5.0, 0.0, 1.0);
        }
        assert!(
            bwe.estimated_kbps() <= 100.0,
            "bandwidth should be clamped to max: got {}",
            bwe.estimated_kbps()
        );
    }

    #[test]
    fn recommended_profile_thresholds() {
        // At boundary: >= 25 kbps => GOOD
        let bwe_good = BandwidthEstimator::new(25.0, 2.0, 100.0);
        assert_eq!(bwe_good.recommended_profile(), QualityProfile::GOOD);

        // Just below 25 => DEGRADED
        let bwe_degraded = BandwidthEstimator::new(24.9, 2.0, 100.0);
        assert_eq!(bwe_degraded.recommended_profile(), QualityProfile::DEGRADED);

        // At boundary: >= 8 kbps => DEGRADED
        let bwe_degraded2 = BandwidthEstimator::new(8.0, 2.0, 100.0);
        assert_eq!(
            bwe_degraded2.recommended_profile(),
            QualityProfile::DEGRADED
        );

        // Below 8 => CATASTROPHIC
        let bwe_cat = BandwidthEstimator::new(7.9, 2.0, 100.0);
        assert_eq!(
            bwe_cat.recommended_profile(),
            QualityProfile::CATASTROPHIC
        );

        // High bandwidth
        let bwe_high = BandwidthEstimator::new(80.0, 2.0, 100.0);
        assert_eq!(bwe_high.recommended_profile(), QualityProfile::GOOD);
    }

    #[test]
    fn from_quality_report_integration() {
        let mut bwe = BandwidthEstimator::new(50.0, 2.0, 100.0);

        // Build a QualityReport with moderate loss and RTT.
        let report = QualityReport {
            loss_pct: (10.0_f32 / 100.0 * 255.0) as u8, // ~10% loss
            rtt_4ms: 25,                                   // 100ms RTT
            jitter_ms: 10,
            bitrate_cap_kbps: 200,
        };

        let new_bw = bwe.from_quality_report(&report);
        // Should return a valid bandwidth value.
        assert!(new_bw > 0.0);
        assert!(new_bw <= 100.0);
        // The estimator should have been updated.
        assert!((bwe.estimated_kbps() - new_bw).abs() < f64::EPSILON);
    }

    // ── Additional detector unit tests ──────────────────────────────────

    #[test]
    fn delay_detector_starts_normal() {
        let det = DelayBasedDetector::new();
        assert_eq!(det.state(), CongestionState::Normal);
    }

    #[test]
    fn loss_detector_below_threshold() {
        let mut det = LossBasedDetector::new();
        for _ in 0..10 {
            det.update(2.0); // 2% loss, well below 5% threshold
        }
        assert!(!det.is_congested());
    }

    #[test]
    fn loss_detector_above_threshold() {
        let mut det = LossBasedDetector::new();
        for _ in 0..10 {
            det.update(8.0); // 8% loss, above 5% threshold
        }
        assert!(det.is_congested());
    }
}
