//! WZP video pipeline — H.264 baseline framer and depacketizer.
//!
//! This crate lives alongside `wzp-codec` and handles video-specific
//! packetization (NAL fragmentation / reassembly).  Platform encoders and
//! decoders land in T4.2/T4.3.

pub mod controller;
pub mod decoder;
pub mod depacketizer;
pub mod encoder;
pub mod framer;
pub mod mediacodec;
pub mod nack;
pub mod videotoolbox;

pub use controller::{VideoQualityController, VideoTarget};
pub use decoder::VideoDecoder;
pub use depacketizer::H264Depacketizer;
pub use encoder::{VideoEncoder, VideoError, VideoFrame};
pub use framer::{FramedPacket, H264Framer};
pub use nack::{CachedPacket, NackAction, NackReceiver, NackSender};
pub use videotoolbox::{VideoToolboxDecoder, VideoToolboxEncoder};

#[cfg(test)]
mod tests {
    use crate::{H264Depacketizer, H264Framer};

    /// Build a synthetic H.264 access unit (Annex-B, 3-byte start codes):
    /// - NAL 1: IDR slice (type 5) with 100-byte payload
    /// - NAL 2: non-IDR slice (type 1) with 50-byte payload
    fn synthetic_access_unit() -> Vec<u8> {
        let mut au = Vec::new();
        au.extend_from_slice(&[0x00, 0x00, 0x01, 0x65]); // IDR start code
        au.extend_from_slice(&[0xCC; 100]);
        au.extend_from_slice(&[0x00, 0x00, 0x01, 0x41]); // non-IDR start code
        au.extend_from_slice(&[0xDD; 50]);
        au
    }

    #[test]
    fn roundtrip_single_nal() {
        let au = synthetic_access_unit();
        let framer = H264Framer::new(500);
        let packets = framer.frame(&au);

        let mut dep = H264Depacketizer::new();
        let mut result = None;
        for pkt in &packets {
            result = dep.push(&pkt.payload, pkt.is_frame_end);
        }

        assert_eq!(result, Some(au));
    }

    #[test]
    fn roundtrip_with_fu_a_fragmentation() {
        let au = synthetic_access_unit();
        // Max payload 30 bytes forces the 100-byte NAL into FU-A fragments.
        let framer = H264Framer::new(30);
        let packets = framer.frame(&au);

        // The 100-byte NAL (1 header + 100 payload = 101 bytes) will be
        // fragmented. 30-byte max means 28 bytes of data per fragment
        // (2 bytes FU-A header).  100 payload bytes → 4 fragments.
        // The 50-byte NAL (1 + 50 = 51) also fragments → 2 fragments.
        // Total packets = 4 + 2 = 6.
        assert_eq!(packets.len(), 6);

        let mut dep = H264Depacketizer::new();
        let mut result = None;
        for pkt in &packets {
            result = dep.push(&pkt.payload, pkt.is_frame_end);
        }

        assert_eq!(result, Some(au));
    }

    #[test]
    fn roundtrip_empty_access_unit() {
        let framer = H264Framer::new(100);
        let packets = framer.frame(&[]);
        assert!(packets.is_empty());
    }
}
