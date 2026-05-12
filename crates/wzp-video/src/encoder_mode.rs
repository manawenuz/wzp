//! Encoder operating mode — normal continuous video or slide fallback.
//!
//! See `docs/PRD/PRD-video-quality-priority.md` (ScreenShare slide-fallback).

/// Operating mode for the video encoder.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
pub enum EncoderMode {
    /// Normal continuous-frame encoding at the target fps.
    #[default]
    Normal,
    /// Slide fallback: emit one high-quality I-frame every 2–5 s,
    /// no P-frames.  Used when bandwidth is below the SD video floor
    /// during a ScreenShare session.
    SlideFallback,
}
