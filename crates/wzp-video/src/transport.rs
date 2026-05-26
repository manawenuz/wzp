//! Video packet serialization and reassembly on top of [`MediaHeaderV2`].
//!
//! A single encoded video frame may be far larger than one QUIC datagram
//! (~1200 bytes after header and AEAD overhead).  This module fragments
//! frames into `MediaPacket`s on the send side and reassembles them on the
//! receive side.
//!
//! ## Wire layout
//!
//! Each fragment uses a standard `MediaHeaderV2` with:
//! - `media_type = Video`
//! - `codec_id`  = the negotiated video codec
//! - `FLAG_KEYFRAME` set on all fragments of a keyframe
//! - `FLAG_FRAME_END` set on the last fragment of a frame
//! - `seq`       = monotonic packet sequence number (wrapping u32)
//! - `fec_block` = `(fragment_index as u8) << 8 | (fragment_count as u8)`
//!                 where fragment_count = total fragments in this frame (1-based)
//!
//! Max fragments per frame: 255 → max frame size ≈ 255 × 1150 ≈ 293 KB,
//! which covers 1080p keyframes at reasonable quality.

use std::collections::HashMap;

use bytes::{Bytes, BytesMut};
use wzp_proto::{CodecId, MediaHeaderV2, MediaPacket, MediaType};

/// Maximum video payload bytes per QUIC datagram.
/// 1200 (QUIC MTU) − 16 (MediaHeaderV2) − 16 (AEAD tag) = 1168.
pub const VIDEO_MAX_PAYLOAD: usize = 1168;

const VIDEO_FRAME_META_MAGIC: [u8; 4] = *b"WZV1";
const VIDEO_FRAME_META_LEN: usize = 8;

#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct VideoFrameMeta {
    pub width: u16,
    pub height: u16,
}

#[derive(Clone, Debug, PartialEq, Eq)]
pub struct ReassembledVideoFrame {
    pub codec_id: CodecId,
    pub is_keyframe: bool,
    pub width: Option<u16>,
    pub height: Option<u16>,
    pub data: Vec<u8>,
}

/// Fragments one encoded video frame into a sequence of [`MediaPacket`]s.
///
/// Pass each `MediaPacket` to `transport.send_media()`.
pub fn packetize_video_frame(
    frame: &[u8],
    codec_id: CodecId,
    is_keyframe: bool,
    seq: &mut u32,
    timestamp_ms: u32,
    width: u32,
    height: u32,
) -> Vec<MediaPacket> {
    if frame.is_empty() {
        return vec![];
    }

    let mut framed = Vec::with_capacity(VIDEO_FRAME_META_LEN + frame.len());
    framed.extend_from_slice(&VIDEO_FRAME_META_MAGIC);
    framed.extend_from_slice(&(width.min(u16::MAX as u32) as u16).to_be_bytes());
    framed.extend_from_slice(&(height.min(u16::MAX as u32) as u16).to_be_bytes());
    framed.extend_from_slice(frame);

    let chunks: Vec<&[u8]> = framed.chunks(VIDEO_MAX_PAYLOAD).collect();
    let total = chunks.len().min(255);
    let mut packets = Vec::with_capacity(total);

    for (i, chunk) in chunks.iter().enumerate().take(255) {
        let is_last = i + 1 == total;
        let mut flags = 0u8;
        if is_keyframe {
            flags |= MediaHeaderV2::FLAG_KEYFRAME;
        }
        if is_last {
            flags |= MediaHeaderV2::FLAG_FRAME_END;
        }

        let fec_block = ((i as u16) << 8) | (total as u16);

        let header = MediaHeaderV2 {
            version: MediaHeaderV2::VERSION,
            flags,
            media_type: MediaType::Video,
            codec_id,
            // Legacy relays default receivers to video layer 0. Use video stream
            // 0 for the single-layer room-video path so packets are forwarded
            // before any receiver quality state exists. Audio is separated by
            // media_type, so stream_id 0 does not collide with audio packets.
            stream_id: 0,
            fec_ratio: 0,
            seq: *seq,
            timestamp: timestamp_ms,
            fec_block,
        };
        *seq = seq.wrapping_add(1);

        let mut buf = BytesMut::with_capacity(MediaHeaderV2::WIRE_SIZE + chunk.len());
        header.write_to(&mut buf);
        buf.extend_from_slice(chunk);

        packets.push(MediaPacket {
            header,
            payload: Bytes::copy_from_slice(chunk),
            quality_report: None,
        });
    }

    packets
}

/// State for one partially-reassembled video frame.
#[derive(Default)]
struct PendingFrame {
    fragments: HashMap<u8, Vec<u8>>,
    total_fragments: u8,
    is_keyframe: bool,
    saw_frame_end: bool,
    codec_id: Option<CodecId>,
}

/// Reassembles fragmented [`MediaPacket`]s back into complete video frames.
///
/// Call [`VideoReassembler::push`] for every received video `MediaPacket`.
/// It returns a complete frame only when the last fragment (`FLAG_FRAME_END`)
/// of a frame arrives and all prior fragments are present.
pub struct VideoReassembler {
    /// Keyed by the timestamp of the frame being assembled.
    pending: HashMap<u32, PendingFrame>,
}

impl VideoReassembler {
    pub fn new() -> Self {
        Self {
            pending: HashMap::new(),
        }
    }

    /// Push one received video packet.
    ///
    /// Returns `Some(frame)` when a complete frame is ready, `None` otherwise.
    pub fn push(&mut self, pkt: &MediaPacket) -> Option<ReassembledVideoFrame> {
        let hdr = &pkt.header;
        let fragment_index = (hdr.fec_block >> 8) as u8;
        let fragment_count = (hdr.fec_block & 0xFF) as u8;
        let is_keyframe = hdr.is_keyframe();
        let is_frame_end = hdr.is_frame_end();

        // Use the packet timestamp as the frame identifier.
        let entry = self.pending.entry(hdr.timestamp).or_default();
        entry.fragments.insert(fragment_index, pkt.payload.to_vec());
        if fragment_count > 0 {
            entry.total_fragments = fragment_count;
        }
        if is_keyframe {
            entry.is_keyframe = true;
        }
        if is_frame_end {
            entry.saw_frame_end = true;
        }
        entry.codec_id = Some(hdr.codec_id);

        // Attempt reassembly once we know the frame end has arrived. The end
        // fragment can arrive before earlier fragments on QUIC/datagram paths,
        // so retry on every later fragment instead of only on the end packet.
        if !entry.saw_frame_end {
            return None;
        }

        let total = entry.total_fragments as usize;
        if total == 0 || entry.fragments.len() < total {
            // Haven't received all fragments yet; keep waiting.
            return None;
        }

        // All fragments present — reassemble in order.
        let pending = self.pending.remove(&hdr.timestamp)?;
        let codec_id = pending.codec_id?;
        let mut frame = Vec::new();
        for i in 0..total as u8 {
            frame.extend_from_slice(pending.fragments.get(&i)?);
        }
        let (meta, data) = split_video_frame_payload(frame);
        Some(ReassembledVideoFrame {
            codec_id,
            is_keyframe: pending.is_keyframe,
            width: meta.map(|m| m.width),
            height: meta.map(|m| m.height),
            data,
        })
    }

    /// Evict stale pending frames older than `max_age_ms` milliseconds.
    ///
    /// Call periodically (e.g. every 2s) to prevent accumulation of frames
    /// whose first or middle fragments were lost.
    pub fn evict_stale(&mut self, current_timestamp_ms: u32, max_age_ms: u32) {
        self.pending
            .retain(|&ts, _| current_timestamp_ms.wrapping_sub(ts) <= max_age_ms);
    }
}

fn split_video_frame_payload(mut frame: Vec<u8>) -> (Option<VideoFrameMeta>, Vec<u8>) {
    if frame.len() < VIDEO_FRAME_META_LEN || frame[..4] != VIDEO_FRAME_META_MAGIC {
        return (None, frame);
    }

    let width = u16::from_be_bytes([frame[4], frame[5]]);
    let height = u16::from_be_bytes([frame[6], frame[7]]);
    frame.drain(..VIDEO_FRAME_META_LEN);
    (Some(VideoFrameMeta { width, height }), frame)
}

impl Default for VideoReassembler {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn make_frame(size: usize) -> Vec<u8> {
        (0..size).map(|i| (i & 0xFF) as u8).collect()
    }

    #[test]
    fn single_fragment_roundtrip() {
        let frame = make_frame(100);
        let mut seq = 0u32;
        let pkts = packetize_video_frame(&frame, CodecId::Av1Main, true, &mut seq, 1000, 640, 480);
        assert_eq!(pkts.len(), 1);
        assert!(pkts[0].header.is_keyframe());
        assert!(pkts[0].header.is_frame_end());
        assert_eq!(pkts[0].header.media_type, MediaType::Video);
        assert_eq!(pkts[0].header.stream_id, 0);

        let mut reassembler = VideoReassembler::new();
        let result = reassembler.push(&pkts[0]);
        assert!(result.is_some());
        let result = result.unwrap();
        assert_eq!(result.codec_id, CodecId::Av1Main);
        assert!(result.is_keyframe);
        assert_eq!(result.width, Some(640));
        assert_eq!(result.height, Some(480));
        assert_eq!(result.data, frame);
    }

    #[test]
    fn multi_fragment_roundtrip() {
        let frame = make_frame(VIDEO_MAX_PAYLOAD * 3 + 50);
        let mut seq = 0u32;
        let pkts = packetize_video_frame(
            &frame,
            CodecId::H264Baseline,
            false,
            &mut seq,
            2000,
            960,
            540,
        );
        assert_eq!(pkts.len(), 4);
        assert!(!pkts[0].header.is_frame_end());
        assert!(pkts[3].header.is_frame_end());
        assert!(!pkts[0].header.is_keyframe());

        let mut reassembler = VideoReassembler::new();
        let mut result = None;
        for pkt in &pkts {
            result = reassembler.push(pkt);
        }
        let result = result.unwrap();
        assert_eq!(result.codec_id, CodecId::H264Baseline);
        assert!(!result.is_keyframe);
        assert_eq!(result.width, Some(960));
        assert_eq!(result.height, Some(540));
        assert_eq!(result.data, frame);
    }

    #[test]
    fn out_of_order_delivery() {
        let frame = make_frame(VIDEO_MAX_PAYLOAD * 2 + 100);
        let mut seq = 0u32;
        let pkts = packetize_video_frame(&frame, CodecId::Av1Main, false, &mut seq, 3000, 320, 240);
        assert_eq!(pkts.len(), 3);

        let mut reassembler = VideoReassembler::new();
        // Deliver out of order: 2, 0, 1
        assert!(reassembler.push(&pkts[2]).is_none()); // last arrives first — no total_fragments yet
        assert!(reassembler.push(&pkts[0]).is_none());
        let result = reassembler
            .push(&pkts[1])
            .expect("last missing fragment completes frame");
        assert_eq!(result.codec_id, CodecId::Av1Main);
        assert!(!result.is_keyframe);
        assert_eq!(result.width, Some(320));
        assert_eq!(result.height, Some(240));
        assert_eq!(result.data, frame);
    }

    #[test]
    fn empty_frame_produces_no_packets() {
        let mut seq = 0u32;
        let pkts = packetize_video_frame(&[], CodecId::Av1Main, false, &mut seq, 0, 640, 480);
        assert!(pkts.is_empty());
    }

    #[test]
    fn old_payload_without_meta_still_reassembles() {
        let payload = Bytes::copy_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65]);
        let pkt = MediaPacket {
            header: MediaHeaderV2 {
                version: MediaHeaderV2::VERSION,
                flags: MediaHeaderV2::FLAG_KEYFRAME | MediaHeaderV2::FLAG_FRAME_END,
                media_type: MediaType::Video,
                codec_id: CodecId::H264Baseline,
                stream_id: 0,
                fec_ratio: 0,
                seq: 7,
                timestamp: 123,
                fec_block: 1,
            },
            payload: payload.clone(),
            quality_report: None,
        };

        let mut reassembler = VideoReassembler::new();
        let frame = reassembler.push(&pkt).unwrap();
        assert_eq!(frame.codec_id, CodecId::H264Baseline);
        assert_eq!(frame.width, None);
        assert_eq!(frame.height, None);
        assert_eq!(frame.data, payload.to_vec());
    }
}
