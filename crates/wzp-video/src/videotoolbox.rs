//! Apple VideoToolbox H.264 / H.265 encoder / decoder (macOS only).

use crate::decoder::VideoDecoder;
use crate::encoder::{VideoEncoder, VideoError, VideoFrame};

#[cfg(target_os = "macos")]
mod imp {
    pub use shiguredo_video_toolbox::{
        CodecConfig, DecodedFrame, Decoder, DecoderCodec, DecoderConfig, EncodeOptions, Encoder,
        EncoderConfig, FrameData, H264EncoderConfig, H264EntropyMode, H264Profile,
        HevcEncoderConfig, HevcProfile, PixelFormat,
    };
}

#[cfg(target_os = "macos")]
use imp::*;

/// macOS VideoToolbox H.264 encoder.
///
/// Wraps `VTCompressionSession`.  On non-macOS targets this is a compile-safe
/// placeholder that returns [`VideoError::NotInitialized`].
pub struct VideoToolboxEncoder {
    #[cfg(target_os = "macos")]
    inner: Encoder,
    force_keyframe: bool,
    #[cfg(not(target_os = "macos"))]
    _width: u32,
    #[cfg(not(target_os = "macos"))]
    _height: u32,
    #[cfg(not(target_os = "macos"))]
    _bitrate_bps: u32,
}

impl VideoToolboxEncoder {
    /// Create a new encoder.
    ///
    /// * `width` / `height` — frame dimensions in pixels.
    /// * `bitrate_bps` — target bitrate in bits per second.
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "macos")]
        {
            let config = EncoderConfig {
                width,
                height,
                codec: CodecConfig::H264(H264EncoderConfig {
                    profile: H264Profile::Baseline,
                    entropy_mode: H264EntropyMode::Cavlc,
                }),
                pixel_format: PixelFormat::I420,
                average_bitrate: Some(bitrate_bps as u64),
                fps_numerator: 30,
                fps_denominator: 1,
                prioritize_encoding_speed_over_quality: true,
                real_time: true,
                maximize_power_efficiency: false,
                allow_frame_reordering: false,
                allow_temporal_compression: false,
                max_key_frame_interval: std::num::NonZeroU32::new(30),
                max_key_frame_interval_duration: None,
                max_frame_delay_count: std::num::NonZeroU32::new(1),
            };
            let inner = Encoder::new(config).map_err(|e| {
                VideoError::PlatformError(format!("VTCompressionSessionCreate failed: {e}"))
            })?;
            Ok(Self {
                inner,
                force_keyframe: false,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (width, height, bitrate_bps);
            Ok(Self {
                _width: width,
                _height: height,
                _bitrate_bps: bitrate_bps,
                force_keyframe: false,
            })
        }
    }
}

impl VideoEncoder for VideoToolboxEncoder {
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
        #[cfg(target_os = "macos")]
        {
            let width = frame.width as usize;
            let height = frame.height as usize;
            let y_size = width * height;
            let uv_size = y_size / 4;
            let expected = y_size + uv_size * 2;

            if frame.data.len() < expected {
                return Err(VideoError::InvalidInput(format!(
                    "I420 frame too small: {} bytes, expected {expected}",
                    frame.data.len()
                )));
            }

            let y = &frame.data[0..y_size];
            let u = &frame.data[y_size..y_size + uv_size];
            let v = &frame.data[y_size + uv_size..y_size + uv_size * 2];

            let frame_data = FrameData::I420 { y, u, v };
            let options = EncodeOptions {
                force_key_frame: self.force_keyframe,
            };

            self.inner
                .encode(&frame_data, &options)
                .map_err(|e| VideoError::PlatformError(format!("encode failed: {e}")))?;

            // Collect encoded output.  Each `next_frame()` call yields one
            // complete access unit (AVCC format from VideoToolbox).
            let mut annex_b = Vec::new();
            let mut emitted_keyframe = false;
            while let Some(encoded) = self
                .inner
                .next_frame()
                .map_err(|e| VideoError::PlatformError(format!("next_frame failed: {e}")))?
            {
                if encoded.keyframe {
                    emitted_keyframe = true;
                }
                // Prepend SPS/PPS for keyframes (parameter sets are delivered
                // separately by the wrapper).
                for sps in &encoded.sps_list {
                    annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    annex_b.extend_from_slice(sps);
                }
                for pps in &encoded.pps_list {
                    annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    annex_b.extend_from_slice(pps);
                }
                // Convert slice NALs from AVCC (4-byte length prefix) to Annex-B.
                annex_b.extend_from_slice(&avcc_to_annexb(&encoded.data));
            }

            // Only clear the keyframe request once a keyframe has actually
            // been emitted — VideoToolbox may buffer several frames before
            // producing output.
            if emitted_keyframe {
                self.force_keyframe = false;
            }

            Ok(annex_b)
        }
        #[cfg(not(target_os = "macos"))]
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
        // NAL type 5 = IDR slice (keyframe).
        nal_type == 5
    }
}

/// Convert an AVCC blob (4-byte big-endian length prefixes) to Annex-B
/// (4-byte start codes `0x00 0x00 0x00 0x01`).
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
fn split_annex_b(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;
    while i < data.len() {
        // Skip start code.
        if i + 3 <= data.len() && data[i..i + 3] == [0x00, 0x00, 0x01] {
            i += 3;
        } else if i + 4 <= data.len() && data[i..i + 4] == [0x00, 0x00, 0x00, 0x01] {
            i += 4;
        } else {
            i += 1;
            continue;
        }
        let start = i;
        // Find next start code.
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

/// Convert Annex-B NAL units to AVCC (4-byte big-endian length prefixes).
fn annexb_to_avcc(annex_b: &[u8]) -> Vec<u8> {
    let nals = split_annex_b(annex_b);
    let mut out = Vec::with_capacity(annex_b.len());
    for nal in nals {
        let len = nal.len() as u32;
        out.extend_from_slice(&len.to_be_bytes());
        out.extend_from_slice(nal);
    }
    out
}

/// macOS VideoToolbox H.264 decoder.
///
/// Wraps `VTDecompressionSession`.  On non-macOS targets this is a compile-safe
/// placeholder that returns [`VideoError::NotInitialized`].
pub struct VideoToolboxDecoder {
    #[cfg(target_os = "macos")]
    inner: Option<Decoder>,
    #[cfg(target_os = "macos")]
    width: u32,
    #[cfg(target_os = "macos")]
    height: u32,
    #[cfg(not(target_os = "macos"))]
    _width: u32,
    #[cfg(not(target_os = "macos"))]
    _height: u32,
}

impl VideoToolboxDecoder {
    /// Create a new decoder.
    ///
    /// The actual `VTDecompressionSession` is created lazily when the first
    /// SPS/PPS parameter sets arrive in-band.
    pub fn new(width: u32, height: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                inner: None,
                width,
                height,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (width, height);
            Ok(Self {
                _width: width,
                _height: height,
            })
        }
    }

    #[cfg(target_os = "macos")]
    fn ensure_decoder(&mut self, sps: &[u8], pps: &[u8]) -> Result<(), VideoError> {
        let needs_create = self.inner.is_none();
        let needs_update = if let Some(dec) = &mut self.inner {
            // Simple heuristic: if we already have a decoder, try updating
            // its format description.  If the same SPS/PPS arrive again
            // `update_format` is a no-op.
            let codec = DecoderCodec::H264 {
                sps,
                pps,
                nalu_len_bytes: 4,
            };
            dec.update_format(codec).is_err()
        } else {
            false
        };

        if needs_create || needs_update {
            let config = DecoderConfig {
                codec: DecoderCodec::H264 {
                    sps,
                    pps,
                    nalu_len_bytes: 4,
                },
                pixel_format: PixelFormat::I420,
            };
            self.inner = Some(
                Decoder::new(config)
                    .map_err(|e| VideoError::PlatformError(format!("decoder create: {e}")))?,
            );
        }
        Ok(())
    }
}

impl VideoDecoder for VideoToolboxDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        #[cfg(target_os = "macos")]
        {
            if access_unit.is_empty() {
                return Ok(None);
            }

            // Extract parameter sets if present.
            let (sps, pps) = extract_sps_pps(access_unit);

            // Build or refresh decoder when we see new parameter sets.
            if let (Some(s), Some(p)) = (&sps, &pps) {
                self.ensure_decoder(s, p)?;
            }

            let decoder = self.inner.as_mut().ok_or(VideoError::NotInitialized)?;

            // Convert Annex-B input to AVCC (4-byte length prefixes) as
            // required by the VideoToolbox decoder wrapper.
            let avcc = annexb_to_avcc(access_unit);
            if avcc.is_empty() {
                return Ok(None);
            }

            let decoded = decoder
                .decode(&avcc)
                .map_err(|e| VideoError::PlatformError(format!("decode failed: {e}")))?;

            match decoded {
                Some(DecodedFrame::I420(frame)) => {
                    let y = frame.y_plane();
                    let u = frame.u_plane();
                    let v = frame.v_plane();
                    let mut data = Vec::with_capacity(y.len() + u.len() + v.len());
                    data.extend_from_slice(y);
                    data.extend_from_slice(u);
                    data.extend_from_slice(v);
                    Ok(Some(VideoFrame {
                        width: self.width,
                        height: self.height,
                        data,
                        timestamp_ms: 0,
                    }))
                }
                Some(DecodedFrame::Nv12(_)) => Err(VideoError::PlatformError(
                    "unexpected NV12 output from decoder".to_string(),
                )),
                None => Ok(None),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = access_unit;
            Err(VideoError::NotInitialized)
        }
    }
}

// ============================================================================
// H.265 / HEVC
// ============================================================================

/// macOS VideoToolbox H.265 encoder.
pub struct VideoToolboxHevcEncoder {
    #[cfg(target_os = "macos")]
    inner: Encoder,
    force_keyframe: bool,
    #[cfg(not(target_os = "macos"))]
    _width: u32,
    #[cfg(not(target_os = "macos"))]
    _height: u32,
    #[cfg(not(target_os = "macos"))]
    _bitrate_bps: u32,
}

impl VideoToolboxHevcEncoder {
    pub fn new(width: u32, height: u32, bitrate_bps: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "macos")]
        {
            let config = EncoderConfig {
                width,
                height,
                codec: CodecConfig::Hevc(HevcEncoderConfig {
                    profile: HevcProfile::Main,
                    allow_open_gop: false,
                }),
                pixel_format: PixelFormat::I420,
                average_bitrate: Some(bitrate_bps as u64),
                fps_numerator: 30,
                fps_denominator: 1,
                prioritize_encoding_speed_over_quality: true,
                real_time: true,
                maximize_power_efficiency: false,
                allow_frame_reordering: false,
                allow_temporal_compression: false,
                max_key_frame_interval: std::num::NonZeroU32::new(30),
                max_key_frame_interval_duration: None,
                max_frame_delay_count: std::num::NonZeroU32::new(1),
            };
            let inner = Encoder::new(config).map_err(|e| {
                VideoError::PlatformError(format!("VTCompressionSessionCreate (HEVC) failed: {e}"))
            })?;
            Ok(Self {
                inner,
                force_keyframe: false,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (width, height, bitrate_bps);
            Ok(Self {
                _width: width,
                _height: height,
                _bitrate_bps: bitrate_bps,
                force_keyframe: false,
            })
        }
    }
}

impl VideoEncoder for VideoToolboxHevcEncoder {
    fn encode(&mut self, frame: &VideoFrame) -> Result<Vec<u8>, VideoError> {
        #[cfg(target_os = "macos")]
        {
            let width = frame.width as usize;
            let height = frame.height as usize;
            let y_size = width * height;
            let uv_size = y_size / 4;
            let expected = y_size + uv_size * 2;

            if frame.data.len() < expected {
                return Err(VideoError::InvalidInput(format!(
                    "I420 frame too small: {} bytes, expected {expected}",
                    frame.data.len()
                )));
            }

            let y = &frame.data[0..y_size];
            let u = &frame.data[y_size..y_size + uv_size];
            let v = &frame.data[y_size + uv_size..y_size + uv_size * 2];

            let frame_data = FrameData::I420 { y, u, v };
            let options = EncodeOptions {
                force_key_frame: self.force_keyframe,
            };

            self.inner
                .encode(&frame_data, &options)
                .map_err(|e| VideoError::PlatformError(format!("encode failed: {e}")))?;

            let mut annex_b = Vec::new();
            let mut emitted_keyframe = false;
            while let Some(encoded) = self
                .inner
                .next_frame()
                .map_err(|e| VideoError::PlatformError(format!("next_frame failed: {e}")))?
            {
                if encoded.keyframe {
                    emitted_keyframe = true;
                }
                for vps in &encoded.vps_list {
                    annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    annex_b.extend_from_slice(vps);
                }
                for sps in &encoded.sps_list {
                    annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    annex_b.extend_from_slice(sps);
                }
                for pps in &encoded.pps_list {
                    annex_b.extend_from_slice(&[0x00, 0x00, 0x00, 0x01]);
                    annex_b.extend_from_slice(pps);
                }
                annex_b.extend_from_slice(&avcc_to_annexb(&encoded.data));
            }

            if emitted_keyframe {
                self.force_keyframe = false;
            }

            Ok(annex_b)
        }
        #[cfg(not(target_os = "macos"))]
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
        // NAL type 19 = IDR_W_RADL, 20 = IDR_N_LP.
        nal_type == 19 || nal_type == 20
    }
}

/// macOS VideoToolbox H.265 decoder.
pub struct VideoToolboxHevcDecoder {
    #[cfg(target_os = "macos")]
    inner: Option<Decoder>,
    #[cfg(target_os = "macos")]
    width: u32,
    #[cfg(target_os = "macos")]
    height: u32,
    #[cfg(not(target_os = "macos"))]
    _width: u32,
    #[cfg(not(target_os = "macos"))]
    _height: u32,
}

impl VideoToolboxHevcDecoder {
    pub fn new(width: u32, height: u32) -> Result<Self, VideoError> {
        #[cfg(target_os = "macos")]
        {
            Ok(Self {
                inner: None,
                width,
                height,
            })
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = (width, height);
            Ok(Self {
                _width: width,
                _height: height,
            })
        }
    }

    #[cfg(target_os = "macos")]
    fn ensure_decoder(&mut self, vps: &[u8], sps: &[u8], pps: &[u8]) -> Result<(), VideoError> {
        let needs_create = self.inner.is_none();
        let needs_update = if let Some(dec) = &mut self.inner {
            let codec = DecoderCodec::Hevc {
                vps,
                sps,
                pps,
                nalu_len_bytes: 4,
            };
            dec.update_format(codec).is_err()
        } else {
            false
        };

        if needs_create || needs_update {
            let config = DecoderConfig {
                codec: DecoderCodec::Hevc {
                    vps,
                    sps,
                    pps,
                    nalu_len_bytes: 4,
                },
                pixel_format: PixelFormat::I420,
            };
            self.inner = Some(
                Decoder::new(config)
                    .map_err(|e| VideoError::PlatformError(format!("decoder create: {e}")))?,
            );
        }
        Ok(())
    }
}

impl VideoDecoder for VideoToolboxHevcDecoder {
    fn decode(&mut self, access_unit: &[u8]) -> Result<Option<VideoFrame>, VideoError> {
        #[cfg(target_os = "macos")]
        {
            if access_unit.is_empty() {
                return Ok(None);
            }

            let (vps, sps, pps) = extract_vps_sps_pps(access_unit);

            if let (Some(v), Some(s), Some(p)) = (&vps, &sps, &pps) {
                self.ensure_decoder(v, s, p)?;
            }

            let decoder = self.inner.as_mut().ok_or(VideoError::NotInitialized)?;

            let avcc = annexb_to_avcc(access_unit);
            if avcc.is_empty() {
                return Ok(None);
            }

            let decoded = decoder
                .decode(&avcc)
                .map_err(|e| VideoError::PlatformError(format!("decode failed: {e}")))?;

            match decoded {
                Some(DecodedFrame::I420(frame)) => {
                    let y = frame.y_plane();
                    let u = frame.u_plane();
                    let v = frame.v_plane();
                    let mut data = Vec::with_capacity(y.len() + u.len() + v.len());
                    data.extend_from_slice(y);
                    data.extend_from_slice(u);
                    data.extend_from_slice(v);
                    Ok(Some(VideoFrame {
                        width: self.width,
                        height: self.height,
                        data,
                        timestamp_ms: 0,
                    }))
                }
                Some(DecodedFrame::Nv12(_)) => Err(VideoError::PlatformError(
                    "unexpected NV12 output from decoder".to_string(),
                )),
                None => Ok(None),
            }
        }
        #[cfg(not(target_os = "macos"))]
        {
            let _ = access_unit;
            Err(VideoError::NotInitialized)
        }
    }
}

/// Type alias for HEVC parameter-set triple returned by `extract_vps_sps_pps`.
type HevcParameterSets = (Option<Vec<u8>>, Option<Vec<u8>>, Option<Vec<u8>>);

/// Parse an Annex-B access unit and return the first VPS, SPS and PPS found (HEVC).
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

    #[test]
    fn avcc_to_annexb_roundtrip() {
        // Build a simple AVCC stream: two NALs.
        let nal1 = vec![0x67, 0x42, 0xC0, 0x1E]; // SPS
        let nal2 = vec![0x68, 0xCE, 0x3C, 0x80]; // PPS
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

        // And back.
        let avcc2 = annexb_to_avcc(&annex_b);
        assert_eq!(avcc2, avcc);
    }

    #[test]
    fn extract_sps_pps_finds_params() {
        let au = vec![
            0x00, 0x00, 0x00, 0x01, 0x67, 0x42, 0xC0, 0x1E, // SPS
            0x00, 0x00, 0x00, 0x01, 0x68, 0xCE, 0x3C, 0x80, // PPS
            0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02, // IDR
        ];
        let (sps, pps) = extract_sps_pps(&au);
        assert_eq!(sps, Some(vec![0x67, 0x42, 0xC0, 0x1E]));
        assert_eq!(pps, Some(vec![0x68, 0xCE, 0x3C, 0x80]));
    }

    // ---- H.265 / HEVC ----

    #[test]
    fn hevc_encoder_instantiates() {
        let enc = VideoToolboxHevcEncoder::new(1280, 720, 2_000_000);
        assert!(enc.is_ok());
    }

    #[test]
    fn hevc_decoder_instantiates() {
        let dec = VideoToolboxHevcDecoder::new(1280, 720);
        assert!(dec.is_ok());
    }

    #[test]
    fn hevc_is_keyframe_detects_idr() {
        let enc = VideoToolboxHevcEncoder::new(1280, 720, 2_000_000).unwrap();
        // NAL type 19 (IDR_W_RADL): first byte = 0b0_010011_0 = 0x26
        assert!(enc.is_keyframe(&[0x26, 0x01]));
        // NAL type 20 (IDR_N_LP): first byte = 0b0_010100_0 = 0x28
        assert!(enc.is_keyframe(&[0x28, 0x01]));
        // NAL type 1 (TRAIL_R): first byte = 0b0_000001_0 = 0x02
        assert!(!enc.is_keyframe(&[0x02, 0x01]));
    }

    #[test]
    fn hevc_request_keyframe_sets_flag() {
        let mut enc = VideoToolboxHevcEncoder::new(1280, 720, 2_000_000).unwrap();
        assert!(!enc.force_keyframe);
        enc.request_keyframe();
        assert!(enc.force_keyframe);
    }

    #[test]
    fn extract_vps_sps_pps_finds_hevc_params() {
        // VPS (type 32): first byte = 0b0_100000_0 = 0x40
        // SPS (type 33): first byte = 0b0_100001_0 = 0x42
        // PPS (type 34): first byte = 0b0_100010_0 = 0x44
        let au = vec![
            0x00, 0x00, 0x00, 0x01, 0x40, 0x01, 0x0C, 0x01, // VPS
            0x00, 0x00, 0x00, 0x01, 0x42, 0x01, 0x01, 0x01, // SPS
            0x00, 0x00, 0x00, 0x01, 0x44, 0x01, 0xC1, 0x72, // PPS
            0x00, 0x00, 0x00, 0x01, 0x26, 0x01, 0xAF, 0x09, // IDR
        ];
        let (vps, sps, pps) = extract_vps_sps_pps(&au);
        assert_eq!(vps, Some(vec![0x40, 0x01, 0x0C, 0x01]));
        assert_eq!(sps, Some(vec![0x42, 0x01, 0x01, 0x01]));
        assert_eq!(pps, Some(vec![0x44, 0x01, 0xC1, 0x72]));
    }
}
