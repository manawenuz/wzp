//! Adaptive FEC configuration — maps `QualityProfile` to FEC encoder parameters.

use wzp_proto::QualityProfile;

use crate::encoder::RaptorQFecEncoder;

/// Adaptive FEC configuration derived from a `QualityProfile`.
#[derive(Clone, Debug)]
pub struct AdaptiveFec {
    /// Frames per FEC block.
    pub frames_per_block: usize,
    /// Repair ratio (0.0 = none, 1.0 = 100% overhead).
    pub repair_ratio: f32,
    /// Symbol size in bytes.
    pub symbol_size: u16,
}

impl AdaptiveFec {
    /// Default symbol size for adaptive configuration.
    const DEFAULT_SYMBOL_SIZE: u16 = 256;

    /// Create an adaptive FEC configuration from a quality profile.
    ///
    /// Maps quality tiers:
    /// - GOOD: 5 frames/block, 20% repair
    /// - DEGRADED: 10 frames/block, 50% repair
    /// - CATASTROPHIC: 8 frames/block, 100% repair
    pub fn from_profile(profile: &QualityProfile) -> Self {
        Self {
            frames_per_block: profile.frames_per_block as usize,
            repair_ratio: profile.fec_ratio,
            symbol_size: Self::DEFAULT_SYMBOL_SIZE,
        }
    }

    /// Build a configured FEC encoder from this adaptive configuration.
    pub fn build_encoder(&self) -> RaptorQFecEncoder {
        RaptorQFecEncoder::new(self.frames_per_block, self.symbol_size)
    }

    /// Get the repair ratio for use with `FecEncoder::generate_repair()`.
    pub fn ratio(&self) -> f32 {
        self.repair_ratio
    }

    /// Estimated overhead factor (1.0 + repair_ratio).
    pub fn overhead_factor(&self) -> f32 {
        1.0 + self.repair_ratio
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use wzp_proto::FecEncoder;

    #[test]
    fn good_profile() {
        let cfg = AdaptiveFec::from_profile(&QualityProfile::GOOD);
        assert_eq!(cfg.frames_per_block, 5);
        assert!((cfg.repair_ratio - 0.2).abs() < f32::EPSILON);
    }

    #[test]
    fn degraded_profile() {
        let cfg = AdaptiveFec::from_profile(&QualityProfile::DEGRADED);
        assert_eq!(cfg.frames_per_block, 10);
        assert!((cfg.repair_ratio - 0.5).abs() < f32::EPSILON);
    }

    #[test]
    fn catastrophic_profile() {
        let cfg = AdaptiveFec::from_profile(&QualityProfile::CATASTROPHIC);
        assert_eq!(cfg.frames_per_block, 8);
        assert!((cfg.repair_ratio - 1.0).abs() < f32::EPSILON);
    }

    #[test]
    fn build_encoder_from_profile() {
        let cfg = AdaptiveFec::from_profile(&QualityProfile::DEGRADED);
        let encoder = cfg.build_encoder();
        assert_eq!(encoder.current_block_size(), 0);
        assert_eq!(wzp_proto::FecEncoder::current_block_id(&encoder), 0);
    }

    #[test]
    fn overhead_factor() {
        let cfg = AdaptiveFec::from_profile(&QualityProfile::CATASTROPHIC);
        assert!((cfg.overhead_factor() - 2.0).abs() < f32::EPSILON);
    }
}
