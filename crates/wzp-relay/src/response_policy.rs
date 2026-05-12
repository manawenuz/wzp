//! Tier G response policy — maps conformance verdicts to enforcement actions.
//!
//! Actions:
//! - `Legitimate`    → no action
//! - `Suspect`       → tighten Tier E quota, emit metric
//! - `Abusive`       → typed Hangup + 1 h fingerprint cool-down
//! - `RepeatAbusive` → relay-local block 24 h

use std::collections::HashMap;
use std::time::{Duration, Instant};

use wzp_proto::packet::{HangupReason, ViolationCode};

/// Conformance verdict produced by Tier F scoring.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No suspicion.
    Legitimate,
    /// Tightened monitoring.
    Suspect,
    /// High confidence of abuse — close session.
    Abusive,
    /// Already abusive once in the last 24 h — escalate to block.
    RepeatAbusive,
}

/// Enforcement action recommended by the response policy.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Action {
    /// Pass through unchanged.
    Allow,
    /// Throttle to tighter quota (Tier E).
    Throttle,
    /// Close the session with a typed Hangup signal.
    Close { reason: HangupReason },
    /// Block the fingerprint from joining any room for 24 h.
    Block,
}

/// Tracks fingerprint-level abuse history and applies escalation.
pub struct ResponsePolicy {
    /// `(fingerprint, violation_code)` → last abusive instant.
    cooldowns: HashMap<(String, ViolationCode), Instant>,
    /// Cool-down duration for first-time abuse.
    cooldown_duration: Duration,
    /// Block duration for repeat abuse.
    block_duration: Duration,
}

impl ResponsePolicy {
    pub fn new() -> Self {
        Self {
            cooldowns: HashMap::new(),
            cooldown_duration: Duration::from_secs(3600), // 1 h
            block_duration: Duration::from_secs(86400),   // 24 h
        }
    }

    /// Evaluate a verdict and produce the corresponding [`Action`].
    ///
    /// `fingerprint` is the participant's identity string (or IP as fallback).
    /// `code` is the specific violation type that triggered the verdict.
    pub fn evaluate(&mut self, fingerprint: &str, code: ViolationCode, verdict: Verdict) -> Action {
        match verdict {
            Verdict::Legitimate => Action::Allow,
            Verdict::Suspect => Action::Throttle,
            Verdict::Abusive => {
                let key = (fingerprint.to_string(), code);
                let now = Instant::now();

                // Check if this fingerprint was already abusive recently.
                let is_repeat = self
                    .cooldowns
                    .get(&key)
                    .map(|last| now.duration_since(*last) < self.block_duration)
                    .unwrap_or(false);

                if is_repeat {
                    Action::Block
                } else {
                    self.cooldowns.insert(key, now);
                    Action::Close {
                        reason: HangupReason::PolicyViolation {
                            code,
                            reason: format!("Tier G enforcement: {code:?}"),
                        },
                    }
                }
            }
            Verdict::RepeatAbusive => Action::Block,
        }
    }

    /// Returns true if the fingerprint is currently blocked (repeat abuse).
    pub fn is_blocked(&self, fingerprint: &str) -> bool {
        let now = Instant::now();
        self.cooldowns.iter().any(|((fp, _), last)| {
            fp == fingerprint && now.duration_since(*last) < self.block_duration
        })
    }

    /// Clean up expired cooldown entries.
    pub fn prune(&mut self) {
        let now = Instant::now();
        self.cooldowns
            .retain(|_, last| now.duration_since(*last) < self.block_duration);
    }

    /// Number of tracked cooldown entries.
    pub fn len(&self) -> usize {
        self.cooldowns.len()
    }

    pub fn is_empty(&self) -> bool {
        self.cooldowns.is_empty()
    }
}

impl Default for ResponsePolicy {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn legitimate_allowed() {
        let mut policy = ResponsePolicy::new();
        assert_eq!(
            policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Legitimate),
            Action::Allow
        );
    }

    #[test]
    fn suspect_throttled() {
        let mut policy = ResponsePolicy::new();
        assert_eq!(
            policy.evaluate("alice", ViolationCode::Entropy, Verdict::Suspect),
            Action::Throttle
        );
    }

    #[test]
    fn abusive_gets_close() {
        let mut policy = ResponsePolicy::new();
        let action = policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Abusive);
        assert!(
            matches!(action, Action::Close { .. }),
            "first-time abuse should close session"
        );
    }

    #[test]
    fn repeat_abusive_gets_block() {
        let mut policy = ResponsePolicy::new();
        // First abuse
        let _ = policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Abusive);
        // Second abuse within window → block
        let action = policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Abusive);
        assert_eq!(action, Action::Block, "repeat abuse should block");
    }

    #[test]
    fn different_violation_codes_are_independent() {
        let mut policy = ResponsePolicy::new();
        // Abuse on bitrate
        let _ = policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Abusive);
        // Abuse on entropy is treated as first-time for that code
        let action = policy.evaluate("alice", ViolationCode::Entropy, Verdict::Abusive);
        assert!(
            matches!(action, Action::Close { .. }),
            "different violation code should not trigger repeat"
        );
    }

    #[test]
    fn is_blocked_true_after_repeat() {
        let mut policy = ResponsePolicy::new();
        let _ = policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Abusive);
        let _ = policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Abusive);
        assert!(policy.is_blocked("alice"));
    }

    #[test]
    fn is_blocked_false_for_legitimate() {
        let policy = ResponsePolicy::new();
        assert!(!policy.is_blocked("alice"));
    }

    #[test]
    fn prune_removes_expired() {
        let mut policy = ResponsePolicy::new();
        let _ = policy.evaluate("alice", ViolationCode::Bitrate, Verdict::Abusive);
        assert_eq!(policy.len(), 1);
        // Manually expire by moving cooldown back
        policy.cooldowns.insert(
            ("alice".to_string(), ViolationCode::Bitrate),
            Instant::now() - Duration::from_secs(90000),
        );
        policy.prune();
        assert!(policy.is_empty());
    }

    #[test]
    fn close_reason_contains_code() {
        let mut policy = ResponsePolicy::new();
        let action = policy.evaluate("alice", ViolationCode::Entropy, Verdict::Abusive);
        match action {
            Action::Close { reason } => match reason {
                HangupReason::PolicyViolation { code, .. } => {
                    assert_eq!(code, ViolationCode::Entropy);
                }
                other => panic!("expected PolicyViolation, got {other:?}"),
            },
            other => panic!("expected Close, got {other:?}"),
        }
    }
}
