//! AV1 software encoder via SVT-AV1 (shiguredo_svt_av1).

use std::num::NonZeroUsize;

use crate::av1_obu::is_keyframe_obu;
use crate::encoder::{VideoEncoder, VideoError, VideoFrame};

/// SW AV1 encoder wrapping `shiguredo_svt_av1::Encoder`.
pub struct SvtAv1Encoder {
    inner: shiguredo_svt_av1::Encoder,
    force_keyframe: bool,
}

impl SvtAv1Encoder {
    /// Create a new SVT-AV1 encoder at the given resolution.
    pub fn new(width: u32, height: u32) -> Result<Self, VideoError> {
        let mut config = shiguredo_svt_av1::EncoderConfig::new(
            width as usize,
            height as usize,
            shiguredo_svt_av1::ColorFormat::I420,
        );
        config.fps_numerator = 30;
        config.fps_denominator = 1;
        config.target_bit_rate = 2_000_000;
        config.rate_control_mode = shiguredo_svt_av1::RcMode::Cbr;
        config.enc_mode = 8; // Fast preset
        config.intra_period_length = NonZeroUsize::new(120);

        let inner = shiguredo_svt_av1::Encoder::new(config)
            .map_err(|e| VideoError::PlatformError(format!("SVT-AV1 init failed: {e}")))?;
        Ok(Self {
            inner,
            force_keyframe: false,
        })
    }
}

impl VideoEncoder for SvtAv1Encoder {
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
        let y_len = (frame.width * frame.height) as usize;
        let uv_len = y_len / 4;
        if frame.data.len() < y_len + uv_len * 2 {
            return Err(VideoError::InvalidInput(
                "frame data too small for I420".into(),
            ));
        }
        let y = &frame.data[0..y_len];
        let u = &frame.data[y_len..y_len + uv_len];
        let v = &frame.data[y_len + uv_len..y_len + uv_len * 2];

        let fd = shiguredo_svt_av1::FrameData::I420 { y, u, v };
        let options = shiguredo_svt_av1::EncodeOptions {
            force_keyframe: self.force_keyframe,
        };
        self.force_keyframe = false;

        self.inner
            .encode(&fd, &options)
            .map_err(|e| VideoError::PlatformError(format!("SVT-AV1 encode failed: {e}")))?;

        if let Some(encoded) = self.inner.next_frame() {
            Ok(encoded.data().to_vec())
        } else {
            Err(VideoError::PlatformError(
                "SVT-AV1 returned no frame".into(),
            ))
        }
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn is_keyframe(&self, packet: &[u8]) -> bool {
        is_keyframe_obu(packet)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Dav1dDecoder;

    #[test]
    fn svt_av1_encoder_instantiates() {
        let enc = SvtAv1Encoder::new(640, 480);
        assert!(enc.is_ok());
    }

    #[test]
    fn svt_av1_encoder_produces_keyframe() {
        let mut enc = SvtAv1Encoder::new(640, 480).unwrap();
        // I420 640×480 = 640*480 + 320*240 + 320*240 = 460800 bytes
        let frame = VideoFrame::new(640, 480, vec![0x80; 460_800], 0);
        let packet = enc.encode(&frame).unwrap();
        assert!(!packet.is_empty());
        assert!(enc.is_keyframe(&packet));
    }

    #[test]
    fn svt_av1_dav1d_roundtrip_10_frames() {
        use crate::decoder::VideoDecoder;

        let mut enc = SvtAv1Encoder::new(640, 480).unwrap();
        let mut dec = Dav1dDecoder::new().unwrap();

        // Encode 10 frames. SVT-AV1 produces output on every call in this
        // configuration (first frame is a keyframe, subsequent are inter).
        let mut packets: Vec<Vec<u8>> = Vec::with_capacity(10);
        for i in 0..10 {
            let frame = VideoFrame::new(640, 480, vec![0x80; 460_800], i as u64 * 33);
            let packet = enc.encode(&frame).expect("encode should succeed");
            assert!(!packet.is_empty(), "packet {} should not be empty", i);
            packets.push(packet);
        }

        // Decode each packet. The first packet contains the sequence header
        // OBU; dav1d remembers it for subsequent inter frames.
        let mut decoded = 0usize;
        for (i, packet) in packets.iter().enumerate() {
            match dec.decode(packet) {
                Ok(Some(frame)) => {
                    assert_eq!(frame.width, 640, "frame {} width mismatch", i);
                    assert_eq!(frame.height, 480, "frame {} height mismatch", i);
                    assert!(
                        !frame.data.is_empty(),
                        "frame {} data should not be empty",
                        i
                    );
                    decoded += 1;
                }
                Ok(None) => {
                    // Some frames may not produce immediate output due to decoder
                    // buffering; this is acceptable. We assert > 0 at the end.
                }
                Err(e) => panic!("decode failed at packet {}: {}", i, e),
            }
        }

        assert_eq!(decoded, 10, "expected 10 decoded frames, got {}", decoded);
    }
}
