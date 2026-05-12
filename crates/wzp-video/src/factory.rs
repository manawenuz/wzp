//! Video encoder/decoder factory — dispatches by [`CodecId`] with platform-aware
//! HW → SW fallback.

use wzp_proto::CodecId;

use crate::decoder::VideoDecoder;
use crate::encoder::{VideoEncoder, VideoError};

/// Create a [`VideoEncoder`] for the given codec and platform.
///
/// **Encoder dispatch:**
/// - `H264Baseline` → `VideoToolboxEncoder` (macOS) / `MediaCodecEncoder` (Android)
/// - `H265Main` → `VideoToolboxHevcEncoder` (macOS) / `MediaCodecHevcEncoder` (Android)
/// - `Av1Main` → `SvtAv1Encoder` (all platforms — universal SW fallback)
///
/// Non-video codecs return [`VideoError::InvalidInput`].
pub fn create_video_encoder(
    codec_id: CodecId,
    width: u32,
    height: u32,
    bitrate_bps: u32,
) -> Result<Box<dyn VideoEncoder>, VideoError> {
    match codec_id {
        CodecId::H264Baseline => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(crate::videotoolbox::VideoToolboxEncoder::new(
                    width,
                    height,
                    bitrate_bps,
                )?))
            }
            #[cfg(target_os = "android")]
            {
                Ok(Box::new(crate::mediacodec::MediaCodecEncoder::new(
                    width,
                    height,
                    bitrate_bps,
                )?))
            }
            #[cfg(not(any(target_os = "macos", target_os = "android")))]
            {
                let _ = (width, height, bitrate_bps);
                Err(VideoError::NotInitialized)
            }
        }
        CodecId::H265Main => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(crate::videotoolbox::VideoToolboxHevcEncoder::new(
                    width,
                    height,
                    bitrate_bps,
                )?))
            }
            #[cfg(target_os = "android")]
            {
                Ok(Box::new(crate::mediacodec::MediaCodecHevcEncoder::new(
                    width,
                    height,
                    bitrate_bps,
                )?))
            }
            #[cfg(not(any(target_os = "macos", target_os = "android")))]
            {
                let _ = (width, height, bitrate_bps);
                Err(VideoError::NotInitialized)
            }
        }
        CodecId::Av1Main => {
            // SVT-AV1 is the universal SW fallback.
            // macOS VideoToolbox has no AV1 encode. Android MediaCodec AV1
            // encode requires API 29+ and may not be available on all devices.
            let _ = bitrate_bps; // SvtAv1Encoder currently hard-codes bitrate
            Ok(Box::new(crate::svt_av1::SvtAv1Encoder::new(width, height)?))
        }
        _ => Err(VideoError::InvalidInput("not a video codec".into())),
    }
}

/// Create a [`VideoDecoder`] for the given codec and platform.
///
/// **Decoder dispatch:**
/// - `H264Baseline` → `VideoToolboxDecoder` (macOS) / `MediaCodecDecoder` (Android)
/// - `H265Main` → `VideoToolboxHevcDecoder` (macOS) / `MediaCodecHevcDecoder` (Android)
/// - `Av1Main` → `VideoToolboxAv1Decoder` (macOS M3+) → `Dav1dDecoder` (fallback, all platforms)
///
/// Non-video codecs return [`VideoError::InvalidInput`].
pub fn create_video_decoder(
    codec_id: CodecId,
    width: u32,
    height: u32,
) -> Result<Box<dyn VideoDecoder>, VideoError> {
    match codec_id {
        CodecId::H264Baseline => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(crate::videotoolbox::VideoToolboxDecoder::new(
                    width, height,
                )?))
            }
            #[cfg(target_os = "android")]
            {
                Ok(Box::new(crate::mediacodec::MediaCodecDecoder::new(
                    width, height,
                )?))
            }
            #[cfg(not(any(target_os = "macos", target_os = "android")))]
            {
                let _ = (width, height);
                Err(VideoError::NotInitialized)
            }
        }
        CodecId::H265Main => {
            #[cfg(target_os = "macos")]
            {
                Ok(Box::new(crate::videotoolbox::VideoToolboxHevcDecoder::new(
                    width, height,
                )?))
            }
            #[cfg(target_os = "android")]
            {
                Ok(Box::new(crate::mediacodec::MediaCodecHevcDecoder::new(
                    width, height,
                )?))
            }
            #[cfg(not(any(target_os = "macos", target_os = "android")))]
            {
                let _ = (width, height);
                Err(VideoError::NotInitialized)
            }
        }
        CodecId::Av1Main => {
            // Try platform HW decoders first, then fall back to dav1d.
            #[cfg(target_os = "macos")]
            {
                if let Ok(dec) = crate::videotoolbox::VideoToolboxAv1Decoder::new(width, height) {
                    return Ok(Box::new(dec));
                }
            }
            #[cfg(target_os = "android")]
            {
                if let Ok(dec) = crate::mediacodec::MediaCodecAv1Decoder::new(width, height) {
                    return Ok(Box::new(dec));
                }
            }
            Ok(Box::new(crate::dav1d::Dav1dDecoder::new()?))
        }
        _ => Err(VideoError::InvalidInput("not a video codec".into())),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn av1_encoder_factory_creates_svt_av1() {
        let enc = create_video_encoder(CodecId::Av1Main, 640, 480, 2_000_000);
        assert!(
            enc.is_ok(),
            "AV1 encoder factory should succeed on all platforms"
        );
    }

    #[test]
    fn av1_decoder_factory_creates_decoder() {
        let dec = create_video_decoder(CodecId::Av1Main, 640, 480);
        assert!(
            dec.is_ok(),
            "AV1 decoder factory should succeed on all platforms"
        );
    }

    #[test]
    fn h264_encoder_factory_not_initialized_on_non_platform() {
        #[cfg(not(any(target_os = "macos", target_os = "android")))]
        {
            let enc = create_video_encoder(CodecId::H264Baseline, 640, 480, 2_000_000);
            assert!(matches!(enc, Err(VideoError::NotInitialized)));
        }
        #[cfg(any(target_os = "macos", target_os = "android"))]
        {
            // On supported platforms the factory succeeds.
            let enc = create_video_encoder(CodecId::H264Baseline, 640, 480, 2_000_000);
            assert!(enc.is_ok());
        }
    }

    #[test]
    fn h265_encoder_factory_not_initialized_on_non_platform() {
        #[cfg(not(any(target_os = "macos", target_os = "android")))]
        {
            let enc = create_video_encoder(CodecId::H265Main, 640, 480, 2_000_000);
            assert!(matches!(enc, Err(VideoError::NotInitialized)));
        }
        #[cfg(any(target_os = "macos", target_os = "android"))]
        {
            let enc = create_video_encoder(CodecId::H265Main, 640, 480, 2_000_000);
            assert!(enc.is_ok());
        }
    }

    #[test]
    fn h264_decoder_factory_not_initialized_on_non_platform() {
        #[cfg(not(any(target_os = "macos", target_os = "android")))]
        {
            let dec = create_video_decoder(CodecId::H264Baseline, 640, 480);
            assert!(matches!(dec, Err(VideoError::NotInitialized)));
        }
        #[cfg(any(target_os = "macos", target_os = "android"))]
        {
            let dec = create_video_decoder(CodecId::H264Baseline, 640, 480);
            assert!(dec.is_ok());
        }
    }

    #[test]
    fn h265_decoder_factory_not_initialized_on_non_platform() {
        #[cfg(not(any(target_os = "macos", target_os = "android")))]
        {
            let dec = create_video_decoder(CodecId::H265Main, 640, 480);
            assert!(matches!(dec, Err(VideoError::NotInitialized)));
        }
        #[cfg(any(target_os = "macos", target_os = "android"))]
        {
            let dec = create_video_decoder(CodecId::H265Main, 640, 480);
            assert!(dec.is_ok());
        }
    }

    #[test]
    fn audio_codec_rejected_by_factory() {
        let enc = create_video_encoder(CodecId::Opus24k, 640, 480, 2_000_000);
        assert!(matches!(enc, Err(VideoError::InvalidInput(_))));

        let dec = create_video_decoder(CodecId::Opus24k, 640, 480);
        assert!(matches!(dec, Err(VideoError::InvalidInput(_))));
    }
}
