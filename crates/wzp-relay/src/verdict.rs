//! Shared conformance verdict enum (Tier F / Tier G).

/// Verdict produced by Tier F scoring and consumed by Tier G response policy.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum Verdict {
    /// No suspicion. Score ≥ 0.7.
    Legitimate,
    /// Tightened monitoring. 0.3 ≤ score < 0.7.
    Suspect,
    /// High confidence of abuse. Score < 0.3.
    Abusive,
}
