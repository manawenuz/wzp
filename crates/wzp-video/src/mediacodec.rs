//! Android MediaCodec H.264 encoder / decoder (Android only).

use crate::decoder::VideoDecoder;
use crate::encoder::{VideoEncoder, VideoError, VideoFrame};

/// Android MediaCodec H.264 encoder.
///
/// Full implementation requires JNI and an Android build environment.
/// On non-Android targets this is a compile-safe placeholder.
pub struct MediaCodecEncoder {
    _width: u32,
    _height: u32,
    _bitrate_bps: u32,
}

impl MediaCodecEncoder {
    /// Create a new encoder.
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "android")]
        {
            Ok(Self {
                _width: width,
                _height: height,
                _bitrate_bps: bitrate_bps,
            })
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (width, height, bitrate_bps);
            Err(VideoError::NotInitialized)
        }
    }
}

impl VideoEncoder for MediaCodecEncoder {
    fn encode(&mut self, _frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
        #[cfg(target_os = "android")]
        {
            // TODO(T4.3): Wire MediaCodec via JNI.
            Ok(Vec::new())
        }
        #[cfg(not(target_os = "android"))]
        {
            Err(VideoError::NotInitialized)
        }
    }

    fn request_keyframe(&mut self) {
        // TODO(T4.3)
    }

    fn is_keyframe(&self, packet: &[u8]) -> bool {
        if packet.is_empty() {
            return false;
        }
        let nal_type = packet[0] & 0x1F;
        nal_type == 5
    }
}

/// Android MediaCodec H.264 decoder.
///
/// Full implementation requires JNI and an Android build environment.
pub struct MediaCodecDecoder {
    _width: u32,
    _height: u32,
}

impl MediaCodecDecoder {
    /// Create a new decoder.
    pub fn new(width: u32, height: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "android")]
        {
            Ok(Self {
                _width: width,
                _height: height,
            })
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = (width, height);
            Err(VideoError::NotInitialized)
        }
    }
}

impl VideoDecoder for MediaCodecDecoder {
    fn decode(&mut self, _access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        #[cfg(target_os = "android")]
        {
            // TODO(T4.3): Wire MediaCodec via JNI.
            Ok(None)
        }
        #[cfg(not(target_os = "android"))]
        {
            Err(VideoError::NotInitialized)
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn mediacodec_encoder_returns_not_initialized_on_non_android() {
        let enc = MediaCodecEncoder::new(1280, 720, 2_000_000);
        assert!(matches!(enc, Err(VideoError::NotInitialized)));
    }

    #[test]
    fn mediacodec_decoder_returns_not_initialized_on_non_android() {
        let dec = MediaCodecDecoder::new(1280, 720);
        assert!(matches!(dec, Err(VideoError::NotInitialized)));
    }

    #[test]
    fn is_keyframe_detects_idr() {
        let enc = MediaCodecEncoder {
            _width: 1280,
            _height: 720,
            _bitrate_bps: 2_000_000,
        };
        assert!(enc.is_keyframe(&[0x65, 0x01]));
        assert!(!enc.is_keyframe(&[0x41, 0x01]));
    }
}
