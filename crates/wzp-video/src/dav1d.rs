//! AV1 software decoder via dav1d (shiguredo_dav1d).

use crate::decoder::VideoDecoder;
use crate::encoder::{VideoError, VideoFrame};

/// SW AV1 decoder wrapping `shiguredo_dav1d::Decoder`.
pub struct Dav1dDecoder {
    inner: shiguredo_dav1d::Decoder,
}

impl Dav1dDecoder {
    /// Create a new dav1d decoder.
    pub fn new() -> Result<Self, VideoError> {
        let config = shiguredo_dav1d::DecoderConfig::new();
        let inner = shiguredo_dav1d::Decoder::new(config)
            .map_err(|e| VideoError::PlatformError(format!("dav1d init failed: {e}")))?;
        Ok(Self { inner })
    }
}

impl VideoDecoder for Dav1dDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        self.inner
            .decode(access_unit)
            .map_err(|e| VideoError::PlatformError(format!("dav1d decode failed: {e}")))?;

        match self.inner.next_frame() {
            Ok(Some(frame)) => {
                let width = frame.width() as u32;
                let height = frame.height() as u32;
                // Copy Y plane data as a simple representation.
                // Full I420 handling would copy U/V planes too.
                let data = frame.y_plane().to_vec();
                Ok(Some(VideoFrame {
                    width,
                    height,
                    data,
                    timestamp_ms: 0,
                }))
            }
            Ok(None) => Ok(None),
            Err(e) => Err(VideoError::PlatformError(format!(
                "dav1d get_picture failed: {e}"
            ))),
        }
    }
}

impl Default for Dav1dDecoder {
    fn default() -> Self {
        Self::new().expect("dav1d default init should not fail")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dav1d_decoder_instantiates() {
        let decoder = Dav1dDecoder::new();
        assert!(decoder.is_ok());
    }
}
