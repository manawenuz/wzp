//! Video decoder trait and platform implementations.

use crate::encoder::{VideoError, VideoFrame};

/// Trait for video decoders.
///
/// Implementations are platform-specific (VideoToolbox on macOS, MediaCodec on
/// Android, OpenH264 as software fallback).
pub trait VideoDecoder: Send {
    /// Decode one H.264 access unit into a raw video frame.
    ///
    /// Returns `Ok(Some(frame))` when a frame is ready, `Ok(None)` if more
    /// data is needed (e.g., for reordering), or an error.
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError>;

    /// Compact implementation-specific state useful for field diagnostics.
    fn debug_snapshot(&self) -> Option<String> {
        None
    }
}
