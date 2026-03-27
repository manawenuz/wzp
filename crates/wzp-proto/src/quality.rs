use std::collections::VecDeque;

use crate::packet::QualityReport;
use crate::traits::QualityController;
use crate::QualityProfile;

/// Network quality tier — drives codec and FEC selection.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Tier {
    /// loss < 10%, RTT < 400ms
    Good,
    /// loss 10-40% OR RTT 400-600ms
    Degraded,
    /// loss > 40% OR RTT > 600ms
    Catastrophic,
}

impl Tier {
    pub fn profile(self) -> QualityProfile {
        match self {
            Self::Good => QualityProfile::GOOD,
            Self::Degraded => QualityProfile::DEGRADED,
            Self::Catastrophic => QualityProfile::CATASTROPHIC,
        }
    }

    /// Determine which tier a quality report belongs to.
    pub fn classify(report: &QualityReport) -> Self {
        let loss = report.loss_percent();
        let rtt = report.rtt_ms();

        if loss > 40.0 || rtt > 600 {
            Self::Catastrophic
        } else if loss > 10.0 || rtt > 400 {
            Self::Degraded
        } else {
            Self::Good
        }
    }
}

/// Adaptive quality controller with hysteresis to prevent tier flapping.
///
/// - Downgrade: 3 consecutive reports in a worse tier
/// - Upgrade: 10 consecutive reports in a better tier
pub struct AdaptiveQualityController {
    current_tier: Tier,
    current_profile: QualityProfile,
    /// Count of consecutive reports suggesting a higher (better) tier.
    consecutive_up: u32,
    /// Count of consecutive reports suggesting a lower (worse) tier.
    consecutive_down: u32,
    /// Sliding window of recent reports for smoothing.
    history: VecDeque<QualityReport>,
    /// Whether the profile was manually forced (disables adaptive logic).
    forced: bool,
}

/// Threshold for downgrading (fast reaction to degradation).
const DOWNGRADE_THRESHOLD: u32 = 3;
/// Threshold for upgrading (slow, cautious improvement).
const UPGRADE_THRESHOLD: u32 = 10;
/// Maximum history window size.
const HISTORY_SIZE: usize = 20;

impl AdaptiveQualityController {
    pub fn new() -> Self {
        Self {
            current_tier: Tier::Good,
            current_profile: QualityProfile::GOOD,
            consecutive_up: 0,
            consecutive_down: 0,
            history: VecDeque::with_capacity(HISTORY_SIZE),
            forced: false,
        }
    }

    /// Get the current tier.
    pub fn tier(&self) -> Tier {
        self.current_tier
    }

    fn try_transition(&mut self, observed_tier: Tier) -> Option<QualityProfile> {
        if observed_tier == self.current_tier {
            self.consecutive_up = 0;
            self.consecutive_down = 0;
            return None;
        }

        let is_worse = match (self.current_tier, observed_tier) {
            (Tier::Good, Tier::Degraded | Tier::Catastrophic) => true,
            (Tier::Degraded, Tier::Catastrophic) => true,
            _ => false,
        };

        if is_worse {
            self.consecutive_up = 0;
            self.consecutive_down += 1;
            if self.consecutive_down >= DOWNGRADE_THRESHOLD {
                self.current_tier = observed_tier;
                self.current_profile = observed_tier.profile();
                self.consecutive_down = 0;
                return Some(self.current_profile);
            }
        } else {
            // Better conditions
            self.consecutive_down = 0;
            self.consecutive_up += 1;
            if self.consecutive_up >= UPGRADE_THRESHOLD {
                // Only upgrade one step at a time
                let next_tier = match self.current_tier {
                    Tier::Catastrophic => Tier::Degraded,
                    Tier::Degraded => Tier::Good,
                    Tier::Good => return None,
                };
                self.current_tier = next_tier;
                self.current_profile = next_tier.profile();
                self.consecutive_up = 0;
                return Some(self.current_profile);
            }
        }

        None
    }
}

impl Default for AdaptiveQualityController {
    fn default() -> Self {
        Self::new()
    }
}

impl QualityController for AdaptiveQualityController {
    fn observe(&mut self, report: &QualityReport) -> Option<QualityProfile> {
        // Store in history
        if self.history.len() >= HISTORY_SIZE {
            self.history.pop_front();
        }
        self.history.push_back(*report);

        if self.forced {
            return None;
        }

        let observed = Tier::classify(report);
        self.try_transition(observed)
    }

    fn force_profile(&mut self, profile: QualityProfile) {
        self.current_profile = profile;
        self.forced = true;
        self.consecutive_up = 0;
        self.consecutive_down = 0;
    }

    fn current_profile(&self) -> QualityProfile {
        self.current_profile
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_report(loss_pct_f: f32, rtt_ms: u16) -> QualityReport {
        QualityReport {
            loss_pct: (loss_pct_f / 100.0 * 255.0) as u8,
            rtt_4ms: (rtt_ms / 4) as u8,
            jitter_ms: 10,
            bitrate_cap_kbps: 200,
        }
    }

    #[test]
    fn starts_at_good() {
        let ctrl = AdaptiveQualityController::new();
        assert_eq!(ctrl.tier(), Tier::Good);
        assert_eq!(ctrl.current_profile().codec, crate::CodecId::Opus24k);
    }

    #[test]
    fn downgrades_after_threshold() {
        let mut ctrl = AdaptiveQualityController::new();

        // 2 bad reports — not enough
        let bad = make_report(50.0, 300);
        assert!(ctrl.observe(&bad).is_none());
        assert!(ctrl.observe(&bad).is_none());
        assert_eq!(ctrl.tier(), Tier::Good);

        // 3rd bad report triggers downgrade
        let result = ctrl.observe(&bad);
        assert!(result.is_some());
        assert_eq!(ctrl.tier(), Tier::Catastrophic);
    }

    #[test]
    fn upgrades_slowly() {
        let mut ctrl = AdaptiveQualityController::new();

        // Force to catastrophic
        let bad = make_report(50.0, 300);
        for _ in 0..3 {
            ctrl.observe(&bad);
        }
        assert_eq!(ctrl.tier(), Tier::Catastrophic);

        // 9 good reports — not enough
        let good = make_report(2.0, 100);
        for _ in 0..9 {
            assert!(ctrl.observe(&good).is_none());
        }
        assert_eq!(ctrl.tier(), Tier::Catastrophic);

        // 10th good report triggers upgrade (one step: Catastrophic → Degraded)
        let result = ctrl.observe(&good);
        assert!(result.is_some());
        assert_eq!(ctrl.tier(), Tier::Degraded);

        // Need another 10 to go from Degraded → Good
        for _ in 0..9 {
            assert!(ctrl.observe(&good).is_none());
        }
        let result = ctrl.observe(&good);
        assert!(result.is_some());
        assert_eq!(ctrl.tier(), Tier::Good);
    }

    #[test]
    fn forced_profile_disables_adaptive() {
        let mut ctrl = AdaptiveQualityController::new();
        ctrl.force_profile(QualityProfile::CATASTROPHIC);

        // Bad reports don't trigger transitions when forced
        let bad = make_report(50.0, 300);
        for _ in 0..10 {
            assert!(ctrl.observe(&bad).is_none());
        }
    }

    #[test]
    fn tier_classification() {
        assert_eq!(Tier::classify(&make_report(5.0, 200)), Tier::Good);
        assert_eq!(Tier::classify(&make_report(15.0, 200)), Tier::Degraded);
        assert_eq!(Tier::classify(&make_report(5.0, 500)), Tier::Degraded);
        assert_eq!(Tier::classify(&make_report(50.0, 200)), Tier::Catastrophic);
        assert_eq!(Tier::classify(&make_report(5.0, 700)), Tier::Catastrophic);
    }
}
