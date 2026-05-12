//! Video encoder trait and platform implementations.

/// Errors that can occur during video encoding or decoding.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum VideoError {
    /// Platform codec failed (e.g., VTCompressionSession error).
    PlatformError(String),
    /// Invalid input parameters.
    InvalidInput(String),
    /// Codec is not initialized.
    NotInitialized,
}

impl std::fmt::Display for VideoError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            VideoError::PlatformError(s) => write!(f, "platform error: {s}"),
            VideoError::InvalidInput(s) => write!(f, "invalid input: {s}"),
            VideoError::NotInitialized => write!(f, "codec not initialized"),
        }
    }
}

impl std::error::Error for VideoError {}

/// Trait for video encoders.
///
/// Implementations are platform-specific (VideoToolbox on macOS, MediaCodec on
/// Android, OpenH264 as software fallback).
pub trait VideoEncoder: Send {
    /// Encode one raw video frame into a H.264 access unit.
    ///
    /// Returns the encoded bytes (one complete access unit) or an error.
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError>;

    /// Request the next encoded frame to be an I-frame (keyframe).
    fn request_keyframe(&mut self);

    /// Returns true if the given encoded packet is a keyframe.
    fn is_keyframe(&self, packet: &[u8]) -> bool;
}

/// Raw video frame input for encoding.
#[derive(Clone, Debug)]
pub struct VideoFrame {
    /// Width in pixels.
    pub width: u32,
    /// Height in pixels.
    pub height: u32,
    /// Pixel data (NV12 or I420, depending on platform).
    pub data: Vec<u8>,
    /// Presentation timestamp in milliseconds.
    pub timestamp_ms: u64,
}

impl VideoFrame {
    pub fn new(width: u32, height: u32, data: Vec<u8>, timestamp_ms: u64) -> Self {
        Self {
            width,
            height,
            data,
            timestamp_ms,
        }
    }
}
