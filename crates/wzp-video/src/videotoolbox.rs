//! Apple VideoToolbox H.264 encoder / decoder (macOS only).

use crate::decoder::VideoDecoder;
use crate::encoder::{VideoEncoder, VideoError, VideoFrame};

/// macOS VideoToolbox H.264 encoder.
///
/// Wraps `VTCompressionSession`.  Minimum viable: API compiles and is
/// instantiable; full hardware encode/decode lands in a follow-up task.
pub struct VideoToolboxEncoder {
    width: u32,
    height: u32,
    bitrate_bps: u32,
    force_keyframe: bool,
}

impl VideoToolboxEncoder {
    /// Create a new encoder.
    ///
    /// * `width` / `height` — frame dimensions in pixels.
    /// * `bitrate_bps` — target bitrate in bits per second.
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        Ok(Self {
            width,
            height,
            bitrate_bps,
            force_keyframe: false,
        })
    }
}

impl VideoEncoder for VideoToolboxEncoder {
    fn encode(&mut self, _frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
        // TODO(T4.2-MVP): Wire VTCompressionSession.
        // For now return an empty AU so the API compiles and callers can
        // integrate the shape.
        Ok(Vec::new())
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn is_keyframe(&self, packet: &[u8]) -> bool {
        if packet.is_empty() {
            return false;
        }
        let nal_type = packet[0] & 0x1F;
        // NAL type 5 = IDR slice (keyframe).
        nal_type == 5
    }
}

/// macOS VideoToolbox H.264 decoder.
///
/// Wraps `VTDecompressionSession`.  Minimum viable: API compiles and is
/// instantiable.
pub struct VideoToolboxDecoder {
    width: u32,
    height: u32,
}

impl VideoToolboxDecoder {
    /// Create a new decoder.
    pub fn new(width: u32, height: u32) -> Result<Self, VideoError> {
        Ok(Self { width, height })
    }
}

impl VideoDecoder for VideoToolboxDecoder {
    fn decode(&mut self, _access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        // TODO(T4.2-MVP): Wire VTDecompressionSession.
        Ok(None)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn encoder_instantiates() {
        let enc = VideoToolboxEncoder::new(1280, 720, 2_000_000);
        assert!(enc.is_ok());
    }

    #[test]
    fn decoder_instantiates() {
        let dec = VideoToolboxDecoder::new(1280, 720);
        assert!(dec.is_ok());
    }

    #[test]
    fn is_keyframe_detects_idr() {
        let enc = VideoToolboxEncoder::new(1280, 720, 2_000_000).unwrap();
        assert!(enc.is_keyframe(&[0x65, 0x01, 0x02]));
        assert!(!enc.is_keyframe(&[0x41, 0x01, 0x02]));
    }

    #[test]
    fn request_keyframe_sets_flag() {
        let mut enc = VideoToolboxEncoder::new(1280, 720, 2_000_000).unwrap();
        assert!(!enc.force_keyframe);
        enc.request_keyframe();
        assert!(enc.force_keyframe);
    }
}
