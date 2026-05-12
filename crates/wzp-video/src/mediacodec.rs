//! Android MediaCodec H.264 encoder / decoder (Android only).
//!
//! On Android targets this uses the `ndk` crate's safe bindings around
//! `AMediaCodec`.  On non-Android targets all methods return
//! [`VideoError::NotInitialized`].

use crate::decoder::VideoDecoder;
use crate::encoder::{VideoEncoder, VideoError, VideoFrame};

#[cfg(target_os = "android")]
mod imp {
    pub use ndk::media::media_codec::{MediaCodec, MediaCodecDirection};
    pub use ndk::media::media_format::MediaFormat;
}

#[cfg(target_os = "android")]
use imp::*;

/// Android MediaCodec H.264 encoder.
///
/// Full implementation requires an Android build environment (NDK).
/// On non-Android targets this is a compile-safe placeholder.
pub struct MediaCodecEncoder {
    #[cfg(target_os = "android")]
    codec: MediaCodec,
    #[cfg(target_os = "android")]
    width: u32,
    #[cfg(target_os = "android")]
    height: u32,
    force_keyframe: bool,
    #[cfg(not(target_os = "android"))]
    _width: u32,
    #[cfg(not(target_os = "android"))]
    _height: u32,
    #[cfg(not(target_os = "android"))]
    _bitrate_bps: u32,
}

/// Android color format constant: YUV 4:2:0 planar (I420).
#[cfg(target_os = "android")]
const COLOR_FORMAT_YUV420_PLANAR: i32 = 19;

impl MediaCodecEncoder {
    /// Create a new encoder.
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "android")]
        {
            let mut format = MediaFormat::new();
            format.set_str("mime", "video/avc");
            format.set_i32("width", width as i32);
            format.set_i32("height", height as i32);
            format.set_i32("bitrate", bitrate_bps as i32);
            format.set_i32("frame-rate", 30);
            format.set_i32("i-frame-interval", 1);
            format.set_i32("color-format", COLOR_FORMAT_YUV420_PLANAR);

            let codec = MediaCodec::from_encoder_type("video/avc").ok_or_else(|| {
                VideoError::PlatformError("AMediaCodec_createEncoderByType failed".into())
            })?;

            codec
                .configure(&format, None, MediaCodecDirection::Encoder)
                .map_err(|e| VideoError::PlatformError(format!("configure failed: {e}")))?;

            codec
                .start()
                .map_err(|e| VideoError::PlatformError(format!("start failed: {e}")))?;

            Ok(Self {
                codec,
                width,
                height,
                force_keyframe: false,
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
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
        #[cfg(target_os = "android")]
        {
            let y_size = (self.width * self.height) as usize;
            let uv_size = y_size / 4;
            let expected = y_size + uv_size * 2;
            if frame.data.len() < expected {
                return Err(VideoError::InvalidInput(format!(
                    "I420 frame too small: {} bytes, expected {expected}",
                    frame.data.len()
                )));
            }

            // Drain any pending output before feeding new input.
            let mut annex_b = self.drain_output()?;

            // Feed the new frame.
            match self
                .codec
                .dequeue_input_buffer(std::time::Duration::from_millis(10))
            {
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(buffer)) => {
                    let idx = buffer.index();
                    if let Some(input_buf) = self.codec.input_buffer(idx) {
                        let to_copy = frame.data.len().min(input_buf.len());
                        input_buf[..to_copy].copy_from_slice(&frame.data[..to_copy]);

                        let flags = if self.force_keyframe {
                            // Request a sync frame by setting the key-frame flag.
                            // The flag is cleared only after we see a keyframe in output.
                            ndk_sys::AMEDIACODEC_BUFFER_FLAG_KEY_FRAME as u32
                        } else {
                            0
                        };

                        self.codec
                            .queue_input_buffer_by_index(
                                idx,
                                0,
                                to_copy,
                                frame.timestamp_ms as u64 * 1000,
                                flags,
                            )
                            .map_err(|e| {
                                VideoError::PlatformError(format!("queue_input_buffer failed: {e}"))
                            })?;
                    }
                }
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::TryAgainLater) => {}
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "dequeue_input_buffer failed: {e}"
                    )));
                }
            }

            // Drain output again to collect the encoded frame.
            annex_b.extend_from_slice(&self.drain_output()?);
            Ok(annex_b)
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = frame;
            Err(VideoError::NotInitialized)
        }
    }

    fn request_keyframe(&mut self) {
        self.force_keyframe = true;
    }

    fn is_keyframe(&self, packet: &[u8]) -> bool {
        if packet.is_empty() {
            return false;
        }
        let nal_type = packet[0] & 0x1F;
        nal_type == 5
    }
}

#[cfg(target_os = "android")]
impl MediaCodecEncoder {
    /// Drain all available output buffers and convert from AVCC to Annex-B.
    fn drain_output(&mut self) -> Result<Vec<u8>, VideoError> {
        let mut output = Vec::new();
        loop {
            match self
                .codec
                .dequeue_output_buffer(std::time::Duration::from_millis(0))
            {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let idx = buffer.index();
                    if let Some(data) = self.codec.output_buffer(idx) {
                        // Check if this is a keyframe by looking at buffer flags.
                        let info = buffer.info();
                        let is_keyframe = (info.flags()
                            & (ndk_sys::AMEDIACODEC_BUFFER_FLAG_KEY_FRAME as u32))
                            != 0;
                        if is_keyframe {
                            self.force_keyframe = false;
                        }
                        output.extend_from_slice(&avcc_to_annexb(data));
                    }
                    self.codec
                        .release_output_buffer_by_index(idx, false)
                        .map_err(|e| {
                            VideoError::PlatformError(format!("release_output_buffer failed: {e}"))
                        })?;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputFormatChanged,
                ) => {
                    // Format change — usually happens once at start.  Continue draining.
                    continue;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputBuffersChanged,
                ) => {
                    continue;
                }
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "dequeue_output_buffer failed: {e}"
                    )));
                }
            }
        }
        Ok(output)
    }
}

/// Android MediaCodec H.264 decoder.
///
/// Full implementation requires an Android build environment (NDK).
/// On non-Android targets this is a compile-safe placeholder.
pub struct MediaCodecDecoder {
    #[cfg(target_os = "android")]
    codec: Option<MediaCodec>,
    #[cfg(target_os = "android")]
    width: u32,
    #[cfg(target_os = "android")]
    height: u32,
    #[cfg(not(target_os = "android"))]
    _width: u32,
    #[cfg(not(target_os = "android"))]
    _height: u32,
}

impl MediaCodecDecoder {
    /// Create a new decoder.
    pub fn new(width: u32, height: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "android")]
        {
            Ok(Self {
                codec: None,
                width,
                height,
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
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        #[cfg(target_os = "android")]
        {
            if access_unit.is_empty() {
                return Ok(None);
            }

            // Lazily create the decoder when we see the first SPS/PPS.
            if self.codec.is_none() {
                let (sps, pps) = extract_sps_pps(access_unit);
                let (sps, pps) = match (sps, pps) {
                    (Some(s), Some(p)) => (s, p),
                    _ => return Ok(None), // need parameter sets before we can init decoder
                };

                let mut format = MediaFormat::new();
                format.set_str("mime", "video/avc");
                format.set_i32("width", self.width as i32);
                format.set_i32("height", self.height as i32);
                format.set_buffer("csd-0", &sps);
                format.set_buffer("csd-1", &pps);

                let codec = MediaCodec::from_decoder_type("video/avc").ok_or_else(|| {
                    VideoError::PlatformError("AMediaCodec_createDecoderByType failed".into())
                })?;

                codec
                    .configure(&format, None, MediaCodecDirection::Decoder)
                    .map_err(|e| {
                        VideoError::PlatformError(format!("decoder configure failed: {e}"))
                    })?;

                codec
                    .start()
                    .map_err(|e| VideoError::PlatformError(format!("decoder start failed: {e}")))?;

                self.codec = Some(codec);
            }

            let codec = self.codec.as_mut().ok_or(VideoError::NotInitialized)?;

            // Feed input.
            match codec.dequeue_input_buffer(std::time::Duration::from_millis(10)) {
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(buffer)) => {
                    let idx = buffer.index();
                    if let Some(input_buf) = codec.input_buffer(idx) {
                        let to_copy = access_unit.len().min(input_buf.len());
                        input_buf[..to_copy].copy_from_slice(&access_unit[..to_copy]);
                        codec
                            .queue_input_buffer_by_index(idx, 0, to_copy, 0, 0)
                            .map_err(|e| {
                                VideoError::PlatformError(format!(
                                    "decoder queue_input_buffer failed: {e}"
                                ))
                            })?;
                    }
                }
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::TryAgainLater) => {}
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "decoder dequeue_input_buffer failed: {e}"
                    )));
                }
            }

            // Drain output.
            match codec.dequeue_output_buffer(std::time::Duration::from_millis(10)) {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let idx = buffer.index();
                    let data = codec.output_buffer(idx).unwrap_or(&[]).to_vec();
                    codec
                        .release_output_buffer_by_index(idx, false)
                        .map_err(|e| {
                            VideoError::PlatformError(format!(
                                "decoder release_output_buffer failed: {e}"
                            ))
                        })?;

                    Ok(Some(VideoFrame {
                        width: self.width,
                        height: self.height,
                        data,
                        timestamp_ms: 0,
                    }))
                }
                Ok(_) => Ok(None),
                Err(e) => Err(VideoError::PlatformError(format!(
                    "decoder dequeue_output_buffer failed: {e}"
                ))),
            }
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = access_unit;
            Err(VideoError::NotInitialized)
        }
    }
}

/// Convert an AVCC blob (4-byte big-endian length prefixes) to Annex-B
/// (4-byte start codes `0x00 0x00 0x00 0x01`).
#[allow(dead_code)]
fn avcc_to_annexb(data: &[u8]) -> Vec<u8> {
    let mut out = Vec::with_capacity(data.len() + data.len() / 4);
    let mut offset = 0;
    while offset + 4 <= data.len() {
        let nal_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        if offset + nal_len > data.len() {
            break;
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[offset..offset + nal_len]);
        offset += nal_len;
    }
    out
}

/// Parse an Annex-B access unit and return the first SPS and PPS found.
#[allow(dead_code)]
fn extract_sps_pps(annex_b: &[u8]) -> (Option<Vec<u8>>, Option<Vec<u8>>) {
    let nals = split_annex_b(annex_b);
    let mut sps = None;
    let mut pps = None;
    for nal in nals {
        if nal.is_empty() {
            continue;
        }
        let nal_type = nal[0] & 0x1F;
        if nal_type == 7 && sps.is_none() {
            sps = Some(nal.to_vec());
        } else if nal_type == 8 && pps.is_none() {
            pps = Some(nal.to_vec());
        }
    }
    (sps, pps)
}

/// Split an Annex-B byte stream into individual NAL units (without start codes).
#[allow(dead_code)]
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i < data.len() {
        if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
            i += 3;
        } else if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            i += 4;
        } else {
            i += 1;
            continue;
        }
        let start = i;
        while i < data.len() {
            if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
                break;
            }
            if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
                break;
            }
            i += 1;
        }
        nals.push(&data[start..i]);
    }
    nals
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
            #[cfg(target_os = "android")]
            codec: unreachable!(),
            #[cfg(target_os = "android")]
            width: 1280,
            #[cfg(target_os = "android")]
            height: 720,
            force_keyframe: false,
            #[cfg(not(target_os = "android"))]
            _width: 1280,
            #[cfg(not(target_os = "android"))]
            _height: 720,
            #[cfg(not(target_os = "android"))]
            _bitrate_bps: 2_000_000,
        };
        assert!(enc.is_keyframe(&[0x65, 0x01]));
        assert!(!enc.is_keyframe(&[0x41, 0x01]));
    }

    #[test]
    fn avcc_to_annexb_roundtrip() {
        let nal1 = vec![0x67, 0x42, 0xC0, 0x1E];
        let nal2 = vec![0x68, 0xCE, 0x3C, 0x80];
        let mut avcc = Vec::new();
        avcc.extend_from_slice(&(nal1.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal1);
        avcc.extend_from_slice(&(nal2.len() as u32).to_be_bytes());
        avcc.extend_from_slice(&nal2);

        let annex_b = avcc_to_annexb(&avcc);
        let expected = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, 0x00, 0x00, 0x00, 0x01, 0x68, 0xCE,
            0x3C, 0x80,
        ];
        assert_eq!(annex_b, expected);
    }
}
