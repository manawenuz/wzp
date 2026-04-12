use std::collections::VecDeque;
use std::time::{Duration, Instant};

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

    /// Determine which tier a quality report belongs to (default/WiFi thresholds).
    pub fn classify(report: &QualityReport) -> Self {
        Self::classify_with_context(report, NetworkContext::Unknown)
    }

    /// Classify with network-context-aware thresholds.
    pub fn classify_with_context(report: &QualityReport, context: NetworkContext) -> Self {
        let loss = report.loss_percent();
        let rtt = report.rtt_ms();

        match context {
            NetworkContext::CellularLte
            | NetworkContext::Cellular5g
            | NetworkContext::Cellular3g => {
                // Tighter thresholds for cellular networks
                if loss > 25.0 || rtt > 500 {
                    Self::Catastrophic
                } else if loss > 8.0 || rtt > 300 {
                    Self::Degraded
                } else {
                    Self::Good
                }
            }
            NetworkContext::WiFi | NetworkContext::Unknown => {
                // Original thresholds
                if loss > 40.0 || rtt > 600 {
                    Self::Catastrophic
                } else if loss > 10.0 || rtt > 400 {
                    Self::Degraded
                } else {
                    Self::Good
                }
            }
        }
    }

    /// Return the next lower (worse) tier, or None if already at the worst.
    pub fn downgrade(self) -> Option<Tier> {
        match self {
            Self::Good => Some(Self::Degraded),
            Self::Degraded => Some(Self::Catastrophic),
            Self::Catastrophic => None,
        }
    }
}

/// Describes the network transport type for context-aware quality decisions.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum NetworkContext {
    WiFi,
    CellularLte,
    Cellular5g,
    Cellular3g,
    Unknown,
}

impl Default for NetworkContext {
    fn default() -> Self {
        Self::Unknown
    }
}

/// Adaptive quality controller with hysteresis to prevent tier flapping.
///
/// - Downgrade: 3 consecutive reports in a worse tier (2 on cellular)
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
    /// Current network context for threshold selection.
    network_context: NetworkContext,
    /// FEC boost expiry time (set during network handoff).
    fec_boost_until: Option<Instant>,
    /// FEC boost amount to add during handoff recovery window.
    fec_boost_amount: f32,
}

/// Threshold for downgrading (fast reaction to degradation).
const DOWNGRADE_THRESHOLD: u32 = 3;
/// Threshold for downgrading on cellular networks (even faster).
const CELLULAR_DOWNGRADE_THRESHOLD: u32 = 2;
/// Threshold for upgrading (slow, cautious improvement).
const UPGRADE_THRESHOLD: u32 = 10;
/// Maximum history window size.
const HISTORY_SIZE: usize = 20;
/// Default FEC boost amount during handoff recovery.
const DEFAULT_FEC_BOOST: f32 = 0.2;
/// Duration of FEC boost after a network handoff.
const FEC_BOOST_DURATION_SECS: u64 = 10;

impl AdaptiveQualityController {
    pub fn new() -> Self {
        Self {
            current_tier: Tier::Good,
            current_profile: QualityProfile::GOOD,
            consecutive_up: 0,
            consecutive_down: 0,
            history: VecDeque::with_capacity(HISTORY_SIZE),
            forced: false,
            network_context: NetworkContext::default(),
            fec_boost_until: None,
            fec_boost_amount: DEFAULT_FEC_BOOST,
        }
    }

    /// Get the current tier.
    pub fn tier(&self) -> Tier {
        self.current_tier
    }

    /// Get the current network context.
    pub fn network_context(&self) -> NetworkContext {
        self.network_context
    }

    /// Signal a network transport change (e.g., WiFi to cellular handoff).
    ///
    /// When switching from WiFi to any cellular type, this preemptively
    /// downgrades one quality tier and activates a temporary FEC boost.
    pub fn signal_network_change(&mut self, new_context: NetworkContext) {
        let old = self.network_context;
        self.network_context = new_context;

        let new_is_cellular = matches!(
            new_context,
            NetworkContext::CellularLte | NetworkContext::Cellular5g | NetworkContext::Cellular3g
        );

        // If switching from WiFi to cellular, preemptively downgrade one tier
        if old == NetworkContext::WiFi && new_is_cellular {
            if let Some(lower_tier) = self.current_tier.downgrade() {
                self.current_tier = lower_tier;
                self.current_profile = lower_tier.profile();
            }
            // Reset counters to avoid stale hysteresis state
            self.consecutive_up = 0;
            self.consecutive_down = 0;
            // Un-force so adaptive logic resumes
            self.forced = false;
        }

        // Activate FEC boost for any network change
        self.fec_boost_until = Some(Instant::now() + Duration::from_secs(FEC_BOOST_DURATION_SECS));
    }

    /// Returns the FEC boost amount if within the handoff recovery window, 0.0 otherwise.
    ///
    /// Callers should add this to their base FEC ratio during the boost window.
    pub fn fec_boost(&self) -> f32 {
        if let Some(until) = self.fec_boost_until {
            if Instant::now() < until {
                return self.fec_boost_amount;
            }
        }
        0.0
    }

    /// Reset the hysteresis counters.
    pub fn reset_counters(&mut self) {
        self.consecutive_up = 0;
        self.consecutive_down = 0;
    }

    /// Get the effective downgrade threshold based on network context.
    fn downgrade_threshold(&self) -> u32 {
        match self.network_context {
            NetworkContext::CellularLte
            | NetworkContext::Cellular5g
            | NetworkContext::Cellular3g => CELLULAR_DOWNGRADE_THRESHOLD,
            _ => DOWNGRADE_THRESHOLD,
        }
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
            if self.consecutive_down >= self.downgrade_threshold() {
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

        let observed = Tier::classify_with_context(report, self.network_context);
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

    // ---------------------------------------------------------------
    // Network context tests
    // ---------------------------------------------------------------

    #[test]
    fn cellular_tighter_thresholds() {
        // 12% loss: Good on WiFi, Degraded on cellular
        let report = make_report(12.0, 200);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Degraded
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::CellularLte),
            Tier::Degraded
        );

        // 9% loss: Good on WiFi, Degraded on cellular
        let report = make_report(9.0, 200);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Good
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::CellularLte),
            Tier::Degraded
        );

        // 30% loss: Degraded on WiFi, Catastrophic on cellular
        let report = make_report(30.0, 200);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Degraded
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::Cellular3g),
            Tier::Catastrophic
        );
    }

    #[test]
    fn cellular_rtt_thresholds() {
        // RTT 350ms: Good on WiFi, Degraded on cellular
        let report = make_report(2.0, 348); // rtt_4ms rounds so use 348
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Good
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::CellularLte),
            Tier::Degraded
        );
    }

    #[test]
    fn cellular_faster_downgrade() {
        let mut ctrl = AdaptiveQualityController::new();
        ctrl.signal_network_change(NetworkContext::CellularLte);
        // Reset tier back to Good for testing downgrade threshold
        ctrl.current_tier = Tier::Good;
        ctrl.current_profile = Tier::Good.profile();

        // On cellular, downgrade threshold is 2 instead of 3
        let bad = make_report(50.0, 200);
        assert!(ctrl.observe(&bad).is_none()); // 1st bad
        let result = ctrl.observe(&bad); // 2nd bad — should trigger on cellular
        assert!(result.is_some());
    }

    #[test]
    fn signal_network_change_preemptive_downgrade() {
        let mut ctrl = AdaptiveQualityController::new();
        assert_eq!(ctrl.tier(), Tier::Good);

        // Switch from WiFi to cellular
        ctrl.network_context = NetworkContext::WiFi;
        ctrl.signal_network_change(NetworkContext::CellularLte);

        // Should have downgraded one tier: Good -> Degraded
        assert_eq!(ctrl.tier(), Tier::Degraded);
    }

    #[test]
    fn signal_network_change_fec_boost() {
        let mut ctrl = AdaptiveQualityController::new();
        assert_eq!(ctrl.fec_boost(), 0.0);

        ctrl.signal_network_change(NetworkContext::CellularLte);

        // FEC boost should be active
        assert!(ctrl.fec_boost() > 0.0);
        assert_eq!(ctrl.fec_boost(), DEFAULT_FEC_BOOST);
    }

    #[test]
    fn tier_downgrade() {
        assert_eq!(Tier::Good.downgrade(), Some(Tier::Degraded));
        assert_eq!(Tier::Degraded.downgrade(), Some(Tier::Catastrophic));
        assert_eq!(Tier::Catastrophic.downgrade(), None);
    }

    #[test]
    fn network_context_default() {
        assert_eq!(NetworkContext::default(), NetworkContext::Unknown);
    }
}
