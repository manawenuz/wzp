//! Android MediaCodec H.264 / H.265 encoder / decoder (Android only).
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
/// Android color format constant: YUV 4:2:0 semiplanar (usually NV12).
#[cfg(target_os = "android")]
const COLOR_FORMAT_YUV420_SEMIPLANAR: i32 = 21;
/// Android MediaCodec CBR bitrate mode (MediaCodecInfo.EncoderCapabilities.BITRATE_MODE_CBR).
#[cfg(target_os = "android")]
const BITRATE_MODE_CBR: i32 = 2;
/// AMediaCodec keyframe buffer flag.
#[cfg(target_os = "android")]
const AMEDIACODEC_BUFFER_FLAG_KEY_FRAME: u32 = 1;
/// MediaCodec encoder parameter key for forcing the next output frame to be a sync frame.
#[cfg(target_os = "android")]
const MEDIA_CODEC_REQUEST_SYNC_FRAME: &str = "request-sync";

// AMediaCodec is thread-safe; the NonNull inside MediaCodec suppresses auto-Send.
#[cfg(target_os = "android")]
unsafe impl Send for MediaCodecEncoder {}

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
            format.set_i32("color-format", COLOR_FORMAT_YUV420_SEMIPLANAR);

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
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(mut buffer)) => {
                    if self.force_keyframe {
                        self.request_sync_frame();
                    }
                    let input =
                        i420_to_nv12(&frame.data, self.width as usize, self.height as usize)?;
                    let to_copy = {
                        let buf = buffer.buffer_mut();
                        let n = input.len().min(buf.len());
                        for (d, &s) in buf[..n].iter_mut().zip(input[..n].iter()) {
                            d.write(s);
                        }
                        n
                    };
                    self.codec
                        .queue_input_buffer(buffer, 0, to_copy, frame.timestamp_ms as u64 * 1000, 0)
                        .map_err(|e| {
                            VideoError::PlatformError(format!("queue_input_buffer failed: {e}"))
                        })?;
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
        let nals = split_annex_b(packet);
        if nals.is_empty() {
            return (packet[0] & 0x1F) == 5;
        }
        nals.iter().any(|nal| !nal.is_empty() && (nal[0] & 0x1F) == 5)
    }
}

#[cfg(target_os = "android")]
impl MediaCodecEncoder {
    fn request_sync_frame(&self) {
        let mut params = MediaFormat::new();
        params.set_i32(MEDIA_CODEC_REQUEST_SYNC_FRAME, 0);
        if let Err(e) = self.codec.set_parameters(params) {
            tracing::warn!(error = %e, "AMediaCodec request sync frame failed");
        }
    }

    /// Drain all available output buffers and convert from AVCC to Annex-B.
    fn drain_output(&mut self) -> Result<Vec<u8>, VideoError> {
        let mut output = Vec::new();
        loop {
            match self
                .codec
                .dequeue_output_buffer(std::time::Duration::from_millis(0))
            {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let is_keyframe =
                        (buffer.info().flags() & AMEDIACODEC_BUFFER_FLAG_KEY_FRAME) != 0;
                    if is_keyframe {
                        self.force_keyframe = false;
                    }
                    let data = output_buffer_payload(&buffer)?;
                    output.extend_from_slice(&avcc_to_annexb(&data));
                    self.codec
                        .release_output_buffer(buffer, false)
                        .map_err(|e| {
                            VideoError::PlatformError(format!("release_output_buffer failed: {e}"))
                        })?;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputFormatChanged,
                ) => {
                    log_media_codec_format("h264_encoder_output", &self.codec);
                    continue;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputBuffersChanged,
                ) => continue,
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

#[cfg(target_os = "android")]
unsafe impl Send for MediaCodecDecoder {}

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
                format.set_i32("color-format", COLOR_FORMAT_YUV420_PLANAR);
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
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(mut buffer)) => {
                    let to_copy = {
                        let buf = buffer.buffer_mut();
                        let n = access_unit.len().min(buf.len());
                        for (d, &s) in buf[..n].iter_mut().zip(access_unit[..n].iter()) {
                            d.write(s);
                        }
                        n
                    };
                    codec
                        .queue_input_buffer(buffer, 0, to_copy, 0, 0)
                        .map_err(|e| {
                            VideoError::PlatformError(format!(
                                "decoder queue_input_buffer failed: {e}"
                            ))
                        })?;
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
                    let data = decoded_i420_payload(codec, &buffer, self.width, self.height)?;
                    codec.release_output_buffer(buffer, false).map_err(|e| {
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
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputFormatChanged,
                ) => {
                    log_media_codec_format("h264_decoder_output", codec);
                    Ok(None)
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

    fn debug_snapshot(&self) -> Option<String> {
        #[cfg(target_os = "android")]
        {
            media_codec_debug_snapshot(self.codec.as_ref())
        }
        #[cfg(not(target_os = "android"))]
        {
            None
        }
    }
}

// ============================================================================
// H.265 / HEVC
// ============================================================================

/// Android MediaCodec H.265 encoder.
///
/// On non-Android targets this is a compile-safe placeholder.
pub struct MediaCodecHevcEncoder {
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

#[cfg(target_os = "android")]
unsafe impl Send for MediaCodecHevcEncoder {}

impl MediaCodecHevcEncoder {
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "android")]
        {
            let mut format = MediaFormat::new();
            format.set_str("mime", "video/hevc");
            format.set_i32("width", width as i32);
            format.set_i32("height", height as i32);
            format.set_i32("bitrate", bitrate_bps as i32);
            format.set_i32("frame-rate", 30);
            format.set_i32("i-frame-interval", 1);
            format.set_i32("color-format", COLOR_FORMAT_YUV420_PLANAR);

            let codec = MediaCodec::from_encoder_type("video/hevc").ok_or_else(|| {
                VideoError::PlatformError("AMediaCodec_createEncoderByType (HEVC) failed".into())
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

impl VideoEncoder for MediaCodecHevcEncoder {
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

            let mut annex_b = self.drain_output()?;

            match self
                .codec
                .dequeue_input_buffer(std::time::Duration::from_millis(10))
            {
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(mut buffer)) => {
                    let flags = if self.force_keyframe { AMEDIACODEC_BUFFER_FLAG_KEY_FRAME } else { 0 };
                    let to_copy = {
                        let buf = buffer.buffer_mut();
                        let n = frame.data.len().min(buf.len());
                        for (d, &s) in buf[..n].iter_mut().zip(frame.data[..n].iter()) {
                            d.write(s);
                        }
                        n
                    };
                    self.codec
                        .queue_input_buffer(buffer, 0, to_copy, frame.timestamp_ms as u64 * 1000, flags)
                        .map_err(|e| {
                            VideoError::PlatformError(format!("queue_input_buffer failed: {e}"))
                        })?;
                }
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::TryAgainLater) => {}
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "dequeue_input_buffer failed: {e}"
                    )));
                }
            }

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
        if packet.len() < 2 {
            return false;
        }
        let nal_type = (packet[0] >> 1) & 0x3F;
        nal_type == 19 || nal_type == 20
    }
}

/// Android MediaCodec AV1 encoder.
///
/// On non-Android targets this is a compile-safe placeholder.
pub struct MediaCodecAv1Encoder {
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

#[cfg(target_os = "android")]
unsafe impl Send for MediaCodecAv1Encoder {}

impl MediaCodecAv1Encoder {
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "android")]
        {
            let mut format = MediaFormat::new();
            format.set_str("mime", "video/av01");
            format.set_i32("width", width as i32);
            format.set_i32("height", height as i32);
            format.set_i32("bitrate", bitrate_bps as i32);
            format.set_i32("frame-rate", 30);
            format.set_i32("color-format", COLOR_FORMAT_YUV420_PLANAR);
            format.set_i32("bitrate-mode", BITRATE_MODE_CBR);
            format.set_i32("i-frame-interval", 2);

            let codec = MediaCodec::from_encoder_type("video/av01").ok_or_else(|| {
                VideoError::PlatformError("AMediaCodec_createEncoderByType (AV1) failed".into())
            })?;

            codec
                .configure(&format, None, MediaCodecDirection::Encoder)
                .map_err(|e| {
                    VideoError::PlatformError(format!("AV1 encoder configure failed: {e}"))
                })?;

            codec
                .start()
                .map_err(|e| VideoError::PlatformError(format!("AV1 encoder start failed: {e}")))?;

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

impl VideoEncoder for MediaCodecAv1Encoder {
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
        #[cfg(target_os = "android")]
        {
            let mut output = Vec::new();

            match self
                .codec
                .dequeue_input_buffer(std::time::Duration::from_millis(0))
            {
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(mut buffer)) => {
                    let flags = if self.force_keyframe { AMEDIACODEC_BUFFER_FLAG_KEY_FRAME } else { 0 };
                    let to_copy = {
                        let buf = buffer.buffer_mut();
                        let n = frame.data.len().min(buf.len());
                        for (d, &s) in buf[..n].iter_mut().zip(frame.data[..n].iter()) {
                            d.write(s);
                        }
                        n
                    };
                    self.codec
                        .queue_input_buffer(buffer, 0, to_copy, frame.timestamp_ms as u64 * 1000, flags)
                        .map_err(|e| {
                            VideoError::PlatformError(format!(
                                "AV1 encoder queue_input_buffer failed: {e}"
                            ))
                        })?;
                }
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::TryAgainLater) => {}
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "AV1 encoder dequeue_input_buffer failed: {e}"
                    )));
                }
            }

            output.extend_from_slice(&self.drain_output()?);
            Ok(output)
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
        crate::av1_obu::is_keyframe_obu(packet)
    }
}

#[cfg(target_os = "android")]
impl MediaCodecHevcEncoder {
    fn drain_output(&mut self) -> Result<Vec<u8>, VideoError> {
        let mut output = Vec::new();
        loop {
            match self
                .codec
                .dequeue_output_buffer(std::time::Duration::from_millis(0))
            {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let is_keyframe =
                        (buffer.info().flags() & AMEDIACODEC_BUFFER_FLAG_KEY_FRAME) != 0;
                    if is_keyframe {
                        self.force_keyframe = false;
                    }
                    let data = output_buffer_payload(&buffer)?;
                    output.extend_from_slice(&avcc_to_annexb(&data));
                    self.codec
                        .release_output_buffer(buffer, false)
                        .map_err(|e| {
                            VideoError::PlatformError(format!("release_output_buffer failed: {e}"))
                        })?;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputFormatChanged,
                ) => {
                    log_media_codec_format("hevc_encoder_output", &self.codec);
                    continue;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputBuffersChanged,
                ) => continue,
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

#[cfg(target_os = "android")]
impl MediaCodecAv1Encoder {
    fn drain_output(&mut self) -> Result<Vec<u8>, VideoError> {
        let mut output = Vec::new();
        loop {
            match self
                .codec
                .dequeue_output_buffer(std::time::Duration::from_millis(0))
            {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let is_keyframe =
                        (buffer.info().flags() & AMEDIACODEC_BUFFER_FLAG_KEY_FRAME) != 0;
                    if is_keyframe {
                        self.force_keyframe = false;
                    }
                    // AV1 output from MediaCodec is already in OBU format.
                    let data = output_buffer_payload(&buffer)?;
                    output.extend_from_slice(&data);
                    self.codec
                        .release_output_buffer(buffer, false)
                        .map_err(|e| {
                            VideoError::PlatformError(format!(
                                "AV1 encoder release_output_buffer failed: {e}"
                            ))
                        })?;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputFormatChanged,
                ) => {
                    log_media_codec_format("av1_encoder_output", &self.codec);
                    continue;
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputBuffersChanged,
                ) => continue,
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::TryAgainLater) => break,
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "AV1 encoder dequeue_output_buffer failed: {e}"
                    )));
                }
            }
        }
        Ok(output)
    }
}

/// Android MediaCodec H.265 decoder.
///
/// On non-Android targets this is a compile-safe placeholder.
pub struct MediaCodecHevcDecoder {
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

#[cfg(target_os = "android")]
unsafe impl Send for MediaCodecHevcDecoder {}

impl MediaCodecHevcDecoder {
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

impl VideoDecoder for MediaCodecHevcDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        #[cfg(target_os = "android")]
        {
            if access_unit.is_empty() {
                return Ok(None);
            }

            // Lazily create decoder when we see VPS/SPS/PPS.
            if self.codec.is_none() {
                let (vps, sps, pps) = extract_vps_sps_pps(access_unit);
                let (vps, sps, pps) = match (vps, sps, pps) {
                    (Some(v), Some(s), Some(p)) => (v, s, p),
                    _ => return Ok(None),
                };

                let mut format = MediaFormat::new();
                format.set_str("mime", "video/hevc");
                format.set_i32("width", self.width as i32);
                format.set_i32("height", self.height as i32);
                format.set_i32("color-format", COLOR_FORMAT_YUV420_PLANAR);
                format.set_buffer("csd-0", &vps);
                format.set_buffer("csd-1", &sps);
                format.set_buffer("csd-2", &pps);

                let codec = MediaCodec::from_decoder_type("video/hevc").ok_or_else(|| {
                    VideoError::PlatformError(
                        "AMediaCodec_createDecoderByType (HEVC) failed".into(),
                    )
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

            match codec.dequeue_input_buffer(std::time::Duration::from_millis(10)) {
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(mut buffer)) => {
                    let to_copy = {
                        let buf = buffer.buffer_mut();
                        let n = access_unit.len().min(buf.len());
                        for (d, &s) in buf[..n].iter_mut().zip(access_unit[..n].iter()) {
                            d.write(s);
                        }
                        n
                    };
                    codec
                        .queue_input_buffer(buffer, 0, to_copy, 0, 0)
                        .map_err(|e| {
                            VideoError::PlatformError(format!(
                                "decoder queue_input_buffer failed: {e}"
                            ))
                        })?;
                }
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::TryAgainLater) => {}
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "decoder dequeue_input_buffer failed: {e}"
                    )));
                }
            }

            match codec.dequeue_output_buffer(std::time::Duration::from_millis(10)) {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let data = decoded_i420_payload(codec, &buffer, self.width, self.height)?;
                    codec.release_output_buffer(buffer, false).map_err(|e| {
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
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputFormatChanged,
                ) => {
                    log_media_codec_format("hevc_decoder_output", codec);
                    Ok(None)
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

    fn debug_snapshot(&self) -> Option<String> {
        #[cfg(target_os = "android")]
        {
            media_codec_debug_snapshot(self.codec.as_ref())
        }
        #[cfg(not(target_os = "android"))]
        {
            None
        }
    }
}

/// Android MediaCodec AV1 decoder.
///
/// On non-Android targets this is a compile-safe placeholder.
pub struct MediaCodecAv1Decoder {
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

#[cfg(target_os = "android")]
unsafe impl Send for MediaCodecAv1Decoder {}

impl MediaCodecAv1Decoder {
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

impl VideoDecoder for MediaCodecAv1Decoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        #[cfg(target_os = "android")]
        {
            if access_unit.is_empty() {
                return Ok(None);
            }

            // Lazily create decoder when we see a sequence header OBU.
            if self.codec.is_none() {
                let seq_header = extract_sequence_header_obu(access_unit);
                let seq_header = match seq_header {
                    Some(sh) => sh,
                    _ => return Ok(None),
                };

                let mut format = MediaFormat::new();
                format.set_str("mime", "video/av01");
                format.set_i32("width", self.width as i32);
                format.set_i32("height", self.height as i32);
                format.set_i32("color-format", COLOR_FORMAT_YUV420_PLANAR);
                format.set_buffer("csd-0", &seq_header);

                let codec = MediaCodec::from_decoder_type("video/av01").ok_or_else(|| {
                    VideoError::PlatformError("AMediaCodec_createDecoderByType (AV1) failed".into())
                })?;

                codec
                    .configure(&format, None, MediaCodecDirection::Decoder)
                    .map_err(|e| {
                        VideoError::PlatformError(format!("AV1 decoder configure failed: {e}"))
                    })?;

                codec.start().map_err(|e| {
                    VideoError::PlatformError(format!("AV1 decoder start failed: {e}"))
                })?;

                self.codec = Some(codec);
            }

            let codec = self.codec.as_mut().ok_or(VideoError::NotInitialized)?;

            match codec.dequeue_input_buffer(std::time::Duration::from_millis(10)) {
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::Buffer(mut buffer)) => {
                    let to_copy = {
                        let buf = buffer.buffer_mut();
                        let n = access_unit.len().min(buf.len());
                        for (d, &s) in buf[..n].iter_mut().zip(access_unit[..n].iter()) {
                            d.write(s);
                        }
                        n
                    };
                    codec
                        .queue_input_buffer(buffer, 0, to_copy, 0, 0)
                        .map_err(|e| {
                            VideoError::PlatformError(format!(
                                "AV1 decoder queue_input_buffer failed: {e}"
                            ))
                        })?;
                }
                Ok(ndk::media::media_codec::DequeuedInputBufferResult::TryAgainLater) => {}
                Err(e) => {
                    return Err(VideoError::PlatformError(format!(
                        "AV1 decoder dequeue_input_buffer failed: {e}"
                    )));
                }
            }

            match codec.dequeue_output_buffer(std::time::Duration::from_millis(10)) {
                Ok(ndk::media::media_codec::DequeuedOutputBufferInfoResult::Buffer(buffer)) => {
                    let data = decoded_i420_payload(codec, &buffer, self.width, self.height)?;
                    codec.release_output_buffer(buffer, false).map_err(|e| {
                        VideoError::PlatformError(format!(
                            "AV1 decoder release_output_buffer failed: {e}"
                        ))
                    })?;
                    Ok(Some(VideoFrame {
                        width: self.width,
                        height: self.height,
                        data,
                        timestamp_ms: 0,
                    }))
                }
                Ok(
                    ndk::media::media_codec::DequeuedOutputBufferInfoResult::OutputFormatChanged,
                ) => {
                    log_media_codec_format("av1_decoder_output", codec);
                    Ok(None)
                }
                Ok(_) => Ok(None),
                Err(e) => Err(VideoError::PlatformError(format!(
                    "AV1 decoder dequeue_output_buffer failed: {e}"
                ))),
            }
        }
        #[cfg(not(target_os = "android"))]
        {
            let _ = access_unit;
            Err(VideoError::NotInitialized)
        }
    }

    fn debug_snapshot(&self) -> Option<String> {
        #[cfg(target_os = "android")]
        {
            media_codec_debug_snapshot(self.codec.as_ref())
        }
        #[cfg(not(target_os = "android"))]
        {
            None
        }
    }
}

#[cfg(target_os = "android")]
fn media_codec_debug_snapshot(codec: Option<&MediaCodec>) -> Option<String> {
    let codec = codec?;
    let format = codec.output_format();
    Some(format!(
        "color_format={:?} width={:?} height={:?} stride={:?} slice_height={:?} crop=({:?},{:?},{:?},{:?})",
        format.i32("color-format"),
        format.i32("width"),
        format.i32("height"),
        format.i32("stride"),
        format.i32("slice-height"),
        format.i32("crop-left"),
        format.i32("crop-top"),
        format.i32("crop-right"),
        format.i32("crop-bottom"),
    ))
}

#[cfg(target_os = "android")]
fn output_buffer_payload(
    buffer: &ndk::media::media_codec::OutputBuffer<'_>,
) -> Result<Vec<u8>, VideoError> {
    let info = buffer.info();
    let offset = usize::try_from(info.offset()).map_err(|_| {
        VideoError::PlatformError(format!(
            "negative MediaCodec output offset: {}",
            info.offset()
        ))
    })?;
    let size = usize::try_from(info.size()).map_err(|_| {
        VideoError::PlatformError(format!("negative MediaCodec output size: {}", info.size()))
    })?;
    let end = offset.checked_add(size).ok_or_else(|| {
        VideoError::PlatformError(format!(
            "MediaCodec output range overflow: offset={offset} size={size}"
        ))
    })?;
    let raw = buffer.buffer();
    if end > raw.len() {
        return Err(VideoError::PlatformError(format!(
            "MediaCodec output range outside buffer: offset={offset} size={size} buffer_len={}",
            raw.len()
        )));
    }
    Ok(raw[offset..end].to_vec())
}

#[cfg(target_os = "android")]
fn decoded_i420_payload(
    codec: &MediaCodec,
    buffer: &ndk::media::media_codec::OutputBuffer<'_>,
    width: u32,
    height: u32,
) -> Result<Vec<u8>, VideoError> {
    let payload = output_buffer_payload(buffer)?;
    let format = codec.output_format();
    let color_format = format
        .i32("color-format")
        .unwrap_or(COLOR_FORMAT_YUV420_PLANAR);
    let stride = positive_format_usize(&format, "stride").unwrap_or(width as usize);
    let slice_height = positive_format_usize(&format, "slice-height").unwrap_or(height as usize);

    match color_format {
        COLOR_FORMAT_YUV420_PLANAR => yuv420_planar_to_tight_i420(
            &payload,
            width as usize,
            height as usize,
            stride,
            slice_height,
        ),
        COLOR_FORMAT_YUV420_SEMIPLANAR => yuv420_semiplanar_to_tight_i420(
            &payload,
            width as usize,
            height as usize,
            stride,
            slice_height,
        ),
        _ => {
            let expected = i420_len(width as usize, height as usize)?;
            if payload.len() < expected {
                return Err(VideoError::PlatformError(format!(
                    "unsupported MediaCodec color format {color_format} produced {} bytes, expected at least {expected}",
                    payload.len()
                )));
            }
            let mut data = payload;
            data.truncate(expected);
            Ok(data)
        }
    }
}

#[cfg(target_os = "android")]
fn positive_format_usize(format: &MediaFormat, key: &str) -> Option<usize> {
    let value = format.i32(key)?;
    (value > 0).then_some(value as usize)
}

#[cfg(target_os = "android")]
fn log_media_codec_format(label: &str, codec: &MediaCodec) {
    let format = codec.output_format();
    tracing::info!(
        target: "wzp_video::mediacodec",
        label,
        color_format = format.i32("color-format"),
        width = format.i32("width"),
        height = format.i32("height"),
        stride = format.i32("stride"),
        slice_height = format.i32("slice-height"),
        crop_left = format.i32("crop-left"),
        crop_right = format.i32("crop-right"),
        crop_top = format.i32("crop-top"),
        crop_bottom = format.i32("crop-bottom"),
        "MediaCodec output format changed"
    );
}

#[cfg(target_os = "android")]
fn i420_len(width: usize, height: usize) -> Result<usize, VideoError> {
    width
        .checked_mul(height)
        .and_then(|y| y.checked_add(y / 2))
        .ok_or_else(|| {
            VideoError::InvalidInput(format!("invalid I420 dimensions {width}x{height}"))
        })
}

#[cfg(target_os = "android")]
fn i420_to_nv12(src: &[u8], width: usize, height: usize) -> Result<Vec<u8>, VideoError> {
    let y_size = width
        .checked_mul(height)
        .ok_or_else(|| VideoError::InvalidInput(format!("invalid frame dimensions {width}x{height}")))?;
    let uv_size = y_size / 4;
    let expected = y_size + uv_size * 2;
    if src.len() < expected {
        return Err(VideoError::InvalidInput(format!(
            "I420 frame too small for NV12 conversion: {} bytes, expected {expected}",
            src.len()
        )));
    }

    let mut out = vec![0u8; expected];
    out[..y_size].copy_from_slice(&src[..y_size]);
    let u = &src[y_size..y_size + uv_size];
    let v = &src[y_size + uv_size..y_size + uv_size * 2];
    for i in 0..uv_size {
        out[y_size + i * 2] = u[i];
        out[y_size + i * 2 + 1] = v[i];
    }
    Ok(out)
}

#[cfg(target_os = "android")]
fn yuv420_planar_to_tight_i420(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    slice_height: usize,
) -> Result<Vec<u8>, VideoError> {
    let y_size = width * height;
    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let chroma_stride = stride / 2;
    let chroma_slice_height = slice_height / 2;
    let padded_y_size = stride * slice_height;
    let padded_chroma_size = chroma_stride * chroma_slice_height;
    let required = padded_y_size + padded_chroma_size * 2;
    if src.len() < required {
        return Err(VideoError::PlatformError(format!(
            "planar YUV buffer too small: {} < {required} (stride={stride}, slice_height={slice_height})",
            src.len()
        )));
    }

    let mut out = vec![0u8; i420_len(width, height)?];
    for row in 0..height {
        let src_start = row * stride;
        let dst_start = row * width;
        out[dst_start..dst_start + width].copy_from_slice(&src[src_start..src_start + width]);
    }

    let src_u = padded_y_size;
    let src_v = src_u + padded_chroma_size;
    let dst_u = y_size;
    let dst_v = dst_u + chroma_width * chroma_height;
    for row in 0..chroma_height {
        let src_row = row * chroma_stride;
        let dst_row = row * chroma_width;
        out[dst_u + dst_row..dst_u + dst_row + chroma_width]
            .copy_from_slice(&src[src_u + src_row..src_u + src_row + chroma_width]);
        out[dst_v + dst_row..dst_v + dst_row + chroma_width]
            .copy_from_slice(&src[src_v + src_row..src_v + src_row + chroma_width]);
    }

    Ok(out)
}

#[cfg(target_os = "android")]
fn yuv420_semiplanar_to_tight_i420(
    src: &[u8],
    width: usize,
    height: usize,
    stride: usize,
    slice_height: usize,
) -> Result<Vec<u8>, VideoError> {
    let y_size = width * height;
    let chroma_width = width / 2;
    let chroma_height = height / 2;
    let padded_y_size = stride * slice_height;
    let required = padded_y_size + stride * chroma_height;
    if src.len() < required {
        return Err(VideoError::PlatformError(format!(
            "semiplanar YUV buffer too small: {} < {required} (stride={stride}, slice_height={slice_height})",
            src.len()
        )));
    }

    let mut out = vec![0u8; i420_len(width, height)?];
    for row in 0..height {
        let src_start = row * stride;
        let dst_start = row * width;
        out[dst_start..dst_start + width].copy_from_slice(&src[src_start..src_start + width]);
    }

    let dst_u = y_size;
    let dst_v = dst_u + chroma_width * chroma_height;
    for row in 0..chroma_height {
        let src_row = padded_y_size + row * stride;
        let dst_row = row * chroma_width;
        for col in 0..chroma_width {
            let pair = src_row + col * 2;
            out[dst_u + dst_row + col] = src[pair];
            out[dst_v + dst_row + col] = src[pair + 1];
        }
    }

    Ok(out)
}

/// Type alias for HEVC parameter-set triple returned by `extract_vps_sps_pps`.
type HevcParameterSets = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

/// Parse an Annex-B access unit and return the first VPS, SPS and PPS found (HEVC).
#[allow(dead_code)]
fn extract_vps_sps_pps(annex_b: &[u8]) -> HevcParameterSets {
    let nals = split_annex_b(annex_b);
    let mut vps = None;
    let mut sps = None;
    let mut pps = None;
    for nal in nals {
        if nal.len() < 2 {
            continue;
        }
        let nal_type = (nal[0] >> 1) & 0x3F;
        if nal_type == 32 && vps.is_none() {
            vps = Some(nal.to_vec());
        } else if nal_type == 33 && sps.is_none() {
            sps = Some(nal.to_vec());
        } else if nal_type == 34 && pps.is_none() {
            pps = Some(nal.to_vec());
        }
    }
    (vps, sps, pps)
}

/// Convert an AVCC blob (4-byte big-endian length prefixes) to Annex-B
/// (4-byte start codes `0x00 0x00 0x00 0x01`).
#[allow(dead_code)]
fn avcc_to_annexb(data: &[u8]) -> Vec<u8> {
    if starts_with_annex_b_start_code(data) {
        return data.to_vec();
    }

    let mut out = Vec::with_capacity(data.len() + data.len() / 4);
    let mut offset = 0;
    let mut saw_nal = false;
    while offset + 4 <= data.len() {
        let nal_len = u32::from_be_bytes([
            data[offset],
            data[offset + 1],
            data[offset + 2],
            data[offset + 3],
        ]) as usize;
        offset += 4;
        if offset + nal_len > data.len() {
            return if saw_nal { out } else { data.to_vec() };
        }
        out.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
        out.extend_from_slice(&data[offset..offset + nal_len]);
        offset += nal_len;
        saw_nal = true;
    }
    out
}

fn starts_with_annex_b_start_code(data: &[u8]) -> bool {
    data.starts_with(&[0x00, 0x00, 0x01]) || data.starts_with(&[0x00, 0x00, 0x00, 0x01])
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

/// Extract the first sequence header OBU from an AV1 OBU stream.
///
/// Returns the raw OBU bytes (header + size field + payload) for use as
/// Android MediaCodec `csd-0`.
#[allow(dead_code)]
fn extract_sequence_header_obu(data: &[u8]) -> Option<Vec<u8>> {
    use crate::av1_obu::{ObuHeader, read_leb128};
    let mut i = 0usize;
    while i < data.len() {
        let header = ObuHeader::from_byte(data[i]);
        i += 1;

        if header.extension_flag {
            if i >= data.len() {
                break;
            }
            i += 1;
        }

        let payload_len = if header.has_size_field {
            let (size, consumed) = read_leb128(data, i)?;
            i += consumed;
            size as usize
        } else {
            // OBU runs to end of stream — not useful for extraction.
            break;
        };

        if header.obu_type == crate::av1_obu::obu_type::SEQUENCE_HEADER {
            let obu_end = i + payload_len;
            if obu_end > data.len() {
                break;
            }
            // Return the full OBU including header, size field, and payload.
            return Some(data[..obu_end].to_vec());
        }

        i += payload_len;
    }
    None
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
        assert!(enc.is_keyframe(&[
            0x00, 0x00, 0x00, 0x01, 0x67, 0x01, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0x02, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0x03, // IDR
        ]));
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

    #[test]
    fn avcc_to_annexb_passes_through_annexb() {
        let annex_b = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, 0x00, 0x00, 0x00, 0x01, 0x65, 0x88,
            0x84, 0x21,
        ];

        assert_eq!(avcc_to_annexb(&annex_b), annex_b);
    }

    #[test]
    fn hevc_mediacodec_encoder_returns_not_initialized_on_non_android() {
        let enc = MediaCodecHevcEncoder::new(1280, 720, 2_000_000);
        assert!(matches!(enc, Err(VideoError::NotInitialized)));
    }

    #[test]
    fn hevc_mediacodec_decoder_returns_not_initialized_on_non_android() {
        let dec = MediaCodecHevcDecoder::new(1280, 720);
        assert!(matches!(dec, Err(VideoError::NotInitialized)));
    }

    #[test]
    fn hevc_is_keyframe_detects_idr() {
        let enc = MediaCodecHevcEncoder {
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
        // NAL type 19 (IDR_W_RADL): first byte = 0b0_010011_0 = 0x26
        assert!(enc.is_keyframe(&[0x26, 0x01]));
        // NAL type 20 (IDR_N_LP): first byte = 0b0_010100_0 = 0x28
        assert!(enc.is_keyframe(&[0x28, 0x01]));
        // NAL type 1 (TRAIL_R)
        assert!(!enc.is_keyframe(&[0x02, 0x01]));
    }

    #[test]
    fn av1_mediacodec_encoder_returns_not_initialized_on_non_android() {
        let enc = MediaCodecAv1Encoder::new(1280, 720, 2_000_000);
        assert!(matches!(enc, Err(VideoError::NotInitialized)));
    }

    #[test]
    fn av1_mediacodec_decoder_returns_not_initialized_on_non_android() {
        let dec = MediaCodecAv1Decoder::new(1280, 720);
        assert!(matches!(dec, Err(VideoError::NotInitialized)));
    }

    #[test]
    fn av1_is_keyframe_detects_keyframe() {
        let enc = MediaCodecAv1Encoder {
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
        // Frame header with show_existing_frame=0, frame_type=0 (KEY_FRAME)
        let mut key_obu = Vec::new();
        let header = crate::av1_obu::ObuHeader {
            obu_type: crate::av1_obu::obu_type::FRAME_HEADER,
            has_size_field: true,
            extension_flag: false,
        };
        key_obu.push(header.to_byte());
        crate::av1_obu::write_leb128(2, &mut key_obu);
        key_obu.extend_from_slice(&[0x00, 0x00]); // show_existing=0, frame_type=0
        assert!(enc.is_keyframe(&key_obu));

        // Frame header with show_existing_frame=0, frame_type=1 (INTER)
        let mut inter_obu = Vec::new();
        let header = crate::av1_obu::ObuHeader {
            obu_type: crate::av1_obu::obu_type::FRAME_HEADER,
            has_size_field: true,
            extension_flag: false,
        };
        inter_obu.push(header.to_byte());
        crate::av1_obu::write_leb128(2, &mut inter_obu);
        inter_obu.extend_from_slice(&[0x40, 0x00]); // show_existing=0, frame_type=1
        assert!(!enc.is_keyframe(&inter_obu));
    }

    #[test]
    fn extract_sequence_header_obu_finds_first_seq_header() {
        let mut data = Vec::new();
        // Sequence header OBU
        let sh_header = crate::av1_obu::ObuHeader {
            obu_type: crate::av1_obu::obu_type::SEQUENCE_HEADER,
            has_size_field: true,
            extension_flag: false,
        };
        data.push(sh_header.to_byte());
        crate::av1_obu::write_leb128(5, &mut data);
        data.extend_from_slice(&[0xAA; 5]);

        // Frame OBU
        let fh_header = crate::av1_obu::ObuHeader {
            obu_type: crate::av1_obu::obu_type::FRAME,
            has_size_field: true,
            extension_flag: false,
        };
        data.push(fh_header.to_byte());
        crate::av1_obu::write_leb128(3, &mut data);
        data.extend_from_slice(&[0xBB; 3]);

        let seq = extract_sequence_header_obu(&data).unwrap();
        // Should contain header byte + leb128(5) + 5 payload bytes
        assert_eq!(seq.len(), 1 + 1 + 5);
        assert_eq!(seq[0], sh_header.to_byte());
    }

    #[test]
    fn extract_sequence_header_obu_returns_none_without_seq_header() {
        let mut data = Vec::new();
        let fh_header = crate::av1_obu::ObuHeader {
            obu_type: crate::av1_obu::obu_type::FRAME,
            has_size_field: true,
            extension_flag: false,
        };
        data.push(fh_header.to_byte());
        crate::av1_obu::write_leb128(3, &mut data);
        data.extend_from_slice(&[0xBB; 3]);

        assert!(extract_sequence_header_obu(&data).is_none());
    }
}
