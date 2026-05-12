//! Priority mode for bandwidth allocation between audio and video.
//!
//! See `docs/PRD/PRD-video-quality-priority.md` for the full design.

use serde::{Deserialize, Serialize};

/// Bandwidth-allocation policy between audio and video.
///
/// Carried on [`QualityProfile`](crate::QualityProfile) and mutable at
/// runtime via [`SignalMessage::SetPriorityMode`](crate::SignalMessage).
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, Serialize, Deserialize)]
pub enum PriorityMode {
    /// Audio gets its floor first; video gets the remainder.
    /// Default for voice/video calls.
    #[default]
    AudioFirst,
    /// Video gets its floor first; audio degrades to Opus 16k floor.
    VideoFirst,
    /// Audio clamped to 16 kbps (intelligible speech); video gets remainder.
    /// Falls back to slide mode when bandwidth drops below SD floor.
    ScreenShare,
    /// Proportional split (~15 % audio, ~85 % video).
    Balanced,
}
