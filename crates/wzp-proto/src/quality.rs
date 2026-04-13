//! See also: [`crate::dred_tuner`] for continuous DRED tuning within a tier.

use std::collections::VecDeque;
use std::time::{Duration, Instant};

use crate::packet::QualityReport;
use crate::traits::QualityController;
use crate::QualityProfile;

/// Network quality tier — drives codec and FEC selection.
///
/// 5-tier range from studio quality down to catastrophic:
/// Studio64k > Studio48k > Studio32k > Good > Degraded > Catastrophic
#[derive(Clone, Copy, Debug, PartialEq, Eq, PartialOrd, Ord)]
pub enum Tier {
    /// loss >= 15% OR RTT >= 200ms — Codec2 1.2k
    Catastrophic = 0,
    /// loss < 15% AND RTT < 200ms — Opus 6k
    Degraded = 1,
    /// loss < 5% AND RTT < 100ms — Opus 24k
    Good = 2,
    /// loss < 2% AND RTT < 80ms — Opus 32k
    Studio32k = 3,
    /// loss < 1% AND RTT < 50ms — Opus 48k
    Studio48k = 4,
    /// loss < 1% AND RTT < 30ms — Opus 64k
    Studio64k = 5,
}

impl Tier {
    pub fn profile(self) -> QualityProfile {
        match self {
            Self::Studio64k => QualityProfile::STUDIO_64K,
            Self::Studio48k => QualityProfile::STUDIO_48K,
            Self::Studio32k => QualityProfile::STUDIO_32K,
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
                // Tighter thresholds for cellular — no studio tiers
                if loss > 25.0 || rtt > 500 {
                    Self::Catastrophic
                } else if loss > 8.0 || rtt > 300 {
                    Self::Degraded
                } else {
                    Self::Good
                }
            }
            NetworkContext::WiFi | NetworkContext::Unknown => {
                if loss >= 15.0 || rtt >= 200 {
                    Self::Catastrophic
                } else if loss >= 5.0 || rtt >= 100 {
                    Self::Degraded
                } else if loss >= 2.0 || rtt >= 80 {
                    Self::Good
                } else if loss >= 1.0 || rtt >= 50 {
                    Self::Studio32k
                } else if rtt >= 30 {
                    Self::Studio48k
                } else {
                    Self::Studio64k
                }
            }
        }
    }

    /// Return the next lower (worse) tier, or None if already at the worst.
    pub fn downgrade(self) -> Option<Tier> {
        match self {
            Self::Studio64k => Some(Self::Studio48k),
            Self::Studio48k => Some(Self::Studio32k),
            Self::Studio32k => Some(Self::Good),
            Self::Good => Some(Self::Degraded),
            Self::Degraded => Some(Self::Catastrophic),
            Self::Catastrophic => None,
        }
    }

    /// Whether this is a studio tier (above Good).
    pub fn is_studio(self) -> bool {
        matches!(self, Self::Studio64k | Self::Studio48k | Self::Studio32k)
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
    /// Probing state: when Some, we're actively testing a higher tier.
    probe: Option<ProbeState>,
    /// Time spent stable at the current tier (for probe trigger).
    stable_since: Option<Instant>,
}

/// Threshold for downgrading (fast reaction to degradation).
const DOWNGRADE_THRESHOLD: u32 = 3;
/// Threshold for downgrading on cellular networks (even faster).
const CELLULAR_DOWNGRADE_THRESHOLD: u32 = 2;
/// Threshold for upgrading from Catastrophic/Degraded to Good.
const UPGRADE_THRESHOLD: u32 = 5;
/// Threshold for upgrading into studio tiers (very conservative).
const STUDIO_UPGRADE_THRESHOLD: u32 = 10;
/// Maximum history window size.
const HISTORY_SIZE: usize = 20;
/// Default FEC boost amount during handoff recovery.
const DEFAULT_FEC_BOOST: f32 = 0.2;
/// Duration of FEC boost after a network handoff.
const FEC_BOOST_DURATION_SECS: u64 = 10;
/// Minimum time stable at current tier before probing upward (30 seconds).
const PROBE_STABLE_SECS: u64 = 30;
/// Duration of a probe window (5 seconds — ~25 quality reports at 1/s).
const PROBE_DURATION_SECS: u64 = 5;
/// Maximum bad reports during probe before aborting (1 out of ~5 = 20%).
const PROBE_MAX_BAD: u32 = 1;
/// Cooldown after a failed probe before trying again (60 seconds).
const PROBE_COOLDOWN_SECS: u64 = 60;

/// Active bandwidth probe state.
struct ProbeState {
    /// The tier we're probing (one step above current).
    target_tier: Tier,
    /// Profile to apply during probe.
    target_profile: QualityProfile,
    /// When the probe started.
    started: Instant,
    /// Reports observed during probe.
    probe_reports: u32,
    /// Bad reports during probe (loss/RTT exceeded target tier thresholds).
    bad_reports: u32,
}

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
            probe: None,
            stable_since: None,
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

        // Cancel any active probe
        self.probe = None;
        self.stable_since = None;

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
        self.probe = None;
        self.stable_since = None;
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

        let is_worse = observed_tier < self.current_tier;

        if is_worse {
            self.consecutive_up = 0;
            self.consecutive_down += 1;
            if self.consecutive_down >= self.downgrade_threshold() {
                // Jump directly to the observed tier (don't step one-at-a-time on downgrade)
                self.current_tier = observed_tier;
                self.current_profile = observed_tier.profile();
                self.consecutive_down = 0;
                return Some(self.current_profile);
            }
        } else {
            // Better conditions
            self.consecutive_down = 0;
            self.consecutive_up += 1;
            // Studio tiers require more consecutive good reports
            let threshold = if self.current_tier >= Tier::Good {
                STUDIO_UPGRADE_THRESHOLD
            } else {
                UPGRADE_THRESHOLD
            };
            if self.consecutive_up >= threshold {
                // Only upgrade one step at a time
                if let Some(next_tier) = self.upgrade_one_step() {
                    self.current_tier = next_tier;
                    self.current_profile = next_tier.profile();
                    self.consecutive_up = 0;
                    return Some(self.current_profile);
                }
            }
        }

        None
    }

    /// Check whether to start, continue, or conclude a bandwidth probe.
    ///
    /// Called from `observe()` when no hysteresis transition fired.
    fn check_probe(&mut self, observed_tier: Tier) -> Option<QualityProfile> {
        // Don't probe if forced, or if already at highest tier, or on cellular
        if self.forced || self.current_tier == Tier::Studio64k {
            return None;
        }
        if matches!(
            self.network_context,
            NetworkContext::CellularLte | NetworkContext::Cellular5g | NetworkContext::Cellular3g
        ) {
            return None;
        }

        // If we have an active probe, evaluate it
        if let Some(ref mut probe) = self.probe {
            probe.probe_reports += 1;

            // Check if the observed tier meets the probe target
            if observed_tier < probe.target_tier {
                probe.bad_reports += 1;
            }

            // Probe failed: too many bad reports
            if probe.bad_reports > PROBE_MAX_BAD {
                let _failed_probe = self.probe.take();
                // Reset stable_since to trigger cooldown
                self.stable_since =
                    Some(Instant::now() + Duration::from_secs(PROBE_COOLDOWN_SECS));
                return None; // stay at current tier
            }

            // Probe succeeded: enough good reports within the window
            if probe.started.elapsed() >= Duration::from_secs(PROBE_DURATION_SECS) {
                let target = probe.target_tier;
                let profile = probe.target_profile;
                self.probe.take();
                self.current_tier = target;
                self.current_profile = profile;
                self.consecutive_up = 0;
                self.stable_since = Some(Instant::now());
                return Some(profile);
            }

            return None; // probe still running
        }

        // No active probe — check if we should start one
        if observed_tier >= self.current_tier {
            // Track stability
            if self.stable_since.is_none() {
                self.stable_since = Some(Instant::now());
            }

            if let Some(stable_since) = self.stable_since {
                if stable_since.elapsed() >= Duration::from_secs(PROBE_STABLE_SECS) {
                    // Stable long enough — start probing
                    if let Some(next) = self.upgrade_one_step() {
                        self.probe = Some(ProbeState {
                            target_tier: next,
                            target_profile: next.profile(),
                            started: Instant::now(),
                            probe_reports: 0,
                            bad_reports: 0,
                        });
                        // Return the probe profile so the encoder switches
                        return Some(next.profile());
                    }
                }
            }
        } else {
            // Conditions degraded — reset stability timer
            self.stable_since = None;
        }

        None
    }

    fn upgrade_one_step(&self) -> Option<Tier> {
        match self.current_tier {
            Tier::Catastrophic => Some(Tier::Degraded),
            Tier::Degraded => Some(Tier::Good),
            Tier::Good => Some(Tier::Studio32k),
            Tier::Studio32k => Some(Tier::Studio48k),
            Tier::Studio48k => Some(Tier::Studio64k),
            Tier::Studio64k => None,
        }
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

        // First check for downgrades/upgrades via hysteresis
        if let Some(profile) = self.try_transition(observed) {
            // Cancel any active probe on tier change
            self.probe.take();
            self.stable_since = None;
            return Some(profile);
        }

        // Then check probing
        self.check_probe(observed)
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

        // 4 good reports — not enough (threshold is 5)
        let good = make_report(0.5, 20); // studio-quality report
        for _ in 0..4 {
            assert!(ctrl.observe(&good).is_none());
        }
        assert_eq!(ctrl.tier(), Tier::Catastrophic);

        // 5th good report triggers upgrade (one step: Catastrophic → Degraded)
        let result = ctrl.observe(&good);
        assert!(result.is_some());
        assert_eq!(ctrl.tier(), Tier::Degraded);

        // Another 5 to go from Degraded → Good
        for _ in 0..4 {
            assert!(ctrl.observe(&good).is_none());
        }
        let result = ctrl.observe(&good);
        assert!(result.is_some());
        assert_eq!(ctrl.tier(), Tier::Good);

        // Studio upgrades need 10 consecutive — Good → Studio32k
        for _ in 0..9 {
            assert!(ctrl.observe(&good).is_none());
        }
        let result = ctrl.observe(&good);
        assert!(result.is_some());
        assert_eq!(ctrl.tier(), Tier::Studio32k);
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
        // Studio tiers
        assert_eq!(Tier::classify(&make_report(0.5, 20)), Tier::Studio64k);
        assert_eq!(Tier::classify(&make_report(0.5, 40)), Tier::Studio48k);
        assert_eq!(Tier::classify(&make_report(1.5, 60)), Tier::Studio32k);
        // Good/Degraded/Catastrophic
        assert_eq!(Tier::classify(&make_report(3.0, 90)), Tier::Good);
        assert_eq!(Tier::classify(&make_report(6.0, 120)), Tier::Degraded);
        assert_eq!(Tier::classify(&make_report(16.0, 120)), Tier::Catastrophic);
        assert_eq!(Tier::classify(&make_report(5.0, 200)), Tier::Catastrophic);
    }

    #[test]
    fn studio_tier_boundaries() {
        // loss < 1% AND RTT < 30ms → Studio64k
        assert_eq!(Tier::classify(&make_report(0.9, 28)), Tier::Studio64k);
        // loss < 1% AND RTT 30-49ms → Studio48k
        assert_eq!(Tier::classify(&make_report(0.9, 32)), Tier::Studio48k);
        // loss < 2% AND RTT < 80ms → Studio32k (but loss >= 1%)
        assert_eq!(Tier::classify(&make_report(1.5, 40)), Tier::Studio32k);
        // loss >= 2% → Good (use 2.5 to survive u8 quantization)
        assert_eq!(Tier::classify(&make_report(2.5, 40)), Tier::Good);
        // RTT 80ms → Good
        assert_eq!(Tier::classify(&make_report(0.5, 80)), Tier::Good);
    }

    // ---------------------------------------------------------------
    // Network context tests
    // ---------------------------------------------------------------

    #[test]
    fn cellular_tighter_thresholds() {
        // 9% loss: Degraded on both WiFi (>=5%) and cellular (>=8%)
        let report = make_report(9.0, 80);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Degraded
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::CellularLte),
            Tier::Degraded
        );

        // 6% loss, low RTT: Degraded on WiFi (>=5%), Good on cellular (<8%)
        let report = make_report(6.0, 80);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Degraded
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::CellularLte),
            Tier::Good
        );

        // 30% loss: Catastrophic on WiFi (>=15%), Catastrophic on cellular (>=25%)
        let report = make_report(30.0, 80);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Catastrophic
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::Cellular3g),
            Tier::Catastrophic
        );
    }

    #[test]
    fn cellular_rtt_thresholds() {
        // RTT 150ms: Degraded on WiFi (>=100ms), Good on cellular (<300ms and loss<8%)
        let report = make_report(2.0, 148);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Degraded
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::CellularLte),
            Tier::Good
        );
    }

    #[test]
    fn cellular_no_studio_tiers() {
        // Even with perfect network, cellular stays at Good (no studio)
        let report = make_report(0.0, 10);
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::CellularLte),
            Tier::Good
        );
        assert_eq!(
            Tier::classify_with_context(&report, NetworkContext::WiFi),
            Tier::Studio64k
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
        assert_eq!(Tier::Studio64k.downgrade(), Some(Tier::Studio48k));
        assert_eq!(Tier::Studio48k.downgrade(), Some(Tier::Studio32k));
        assert_eq!(Tier::Studio32k.downgrade(), Some(Tier::Good));
        assert_eq!(Tier::Good.downgrade(), Some(Tier::Degraded));
        assert_eq!(Tier::Degraded.downgrade(), Some(Tier::Catastrophic));
        assert_eq!(Tier::Catastrophic.downgrade(), None);
    }

    #[test]
    fn network_context_default() {
        assert_eq!(NetworkContext::default(), NetworkContext::Unknown);
    }

    // ---------------------------------------------------------------
    // Bandwidth probing tests
    // ---------------------------------------------------------------

    #[test]
    fn probe_triggers_after_stable_period() {
        let mut ctrl = AdaptiveQualityController::new();
        let excellent = make_report(0.3, 20); // would classify as Studio64k

        // Starts at Good. Fast-forward stability by setting stable_since directly.
        ctrl.stable_since = Some(Instant::now() - Duration::from_secs(31));

        // One excellent report should trigger a probe (Good → Studio32k)
        let result = ctrl.observe(&excellent);
        assert!(result.is_some(), "should start probe after 30s stable");
        assert!(ctrl.probe.is_some(), "probe should be active");
        assert_eq!(ctrl.probe.as_ref().unwrap().target_tier, Tier::Studio32k);
    }

    #[test]
    fn probe_succeeds_after_window() {
        let mut ctrl = AdaptiveQualityController::new();
        ctrl.stable_since = Some(Instant::now() - Duration::from_secs(31));

        let excellent = make_report(0.3, 20);

        // Trigger probe start
        let result = ctrl.observe(&excellent);
        assert!(result.is_some());

        // Simulate probe window elapsed by backdating started
        ctrl.probe.as_mut().unwrap().started =
            Instant::now() - Duration::from_secs(PROBE_DURATION_SECS);

        // Next good report should finalize the probe
        let result = ctrl.observe(&excellent);
        assert!(result.is_some(), "probe should succeed");
        assert_eq!(ctrl.current_tier, Tier::Studio32k);
        assert!(ctrl.probe.is_none(), "probe should be cleared");
    }

    #[test]
    fn probe_fails_on_bad_reports() {
        let mut ctrl = AdaptiveQualityController::new();
        // Put controller at Studio32k, pretend we've been stable
        ctrl.current_tier = Tier::Studio32k;
        ctrl.current_profile = Tier::Studio32k.profile();
        ctrl.stable_since = Some(Instant::now() - Duration::from_secs(31));

        // Start a probe to Studio48k
        let excellent = make_report(0.3, 20);
        let result = ctrl.observe(&excellent);
        assert!(result.is_some()); // probe started
        assert_eq!(ctrl.probe.as_ref().unwrap().target_tier, Tier::Studio48k);

        // Feed bad reports (loss too high for Studio48k)
        let degraded = make_report(3.0, 100);
        ctrl.observe(&degraded); // first bad
        ctrl.observe(&degraded); // second bad — exceeds PROBE_MAX_BAD (1)

        // Probe should be cancelled
        assert!(ctrl.probe.is_none(), "probe should be cancelled after bad reports");
        // Should still be at Studio32k (not upgraded)
        assert_eq!(ctrl.current_tier, Tier::Studio32k);
    }

    #[test]
    fn no_probe_on_cellular() {
        let mut ctrl = AdaptiveQualityController::new();
        ctrl.signal_network_change(NetworkContext::CellularLte);
        ctrl.current_tier = Tier::Good;
        ctrl.current_profile = Tier::Good.profile();
        ctrl.stable_since = Some(Instant::now() - Duration::from_secs(60));

        let good = make_report(0.5, 40);
        let result = ctrl.observe(&good);
        // Should NOT probe on cellular
        assert!(ctrl.probe.is_none(), "should not probe on cellular");
        assert!(result.is_none() || ctrl.current_tier == Tier::Good);
    }

    #[test]
    fn no_probe_at_highest_tier() {
        let mut ctrl = AdaptiveQualityController::new();
        ctrl.current_tier = Tier::Studio64k;
        ctrl.current_profile = Tier::Studio64k.profile();
        ctrl.stable_since = Some(Instant::now() - Duration::from_secs(60));

        let excellent = make_report(0.1, 10);
        let result = ctrl.observe(&excellent);
        assert!(result.is_none(), "should not probe when already at Studio64k");
    }
}
