//! DATAGRAM frame serialization for media packets.
//!
//! Wraps `MediaPacket` serialization with MTU awareness for QUIC DATAGRAM frames.

use bytes::Bytes;
use wzp_proto::MediaPacket;

/// Serialize a `MediaPacket` into bytes suitable for a QUIC DATAGRAM frame.
pub fn serialize_media(packet: &MediaPacket) -> Bytes {
    packet.to_bytes()
}

/// Deserialize a `MediaPacket` from QUIC DATAGRAM frame bytes.
pub fn deserialize_media(data: Bytes) -> Option<MediaPacket> {
    MediaPacket::from_bytes(data)
}

/// Return the maximum payload size for a QUIC DATAGRAM on this connection.
///
/// Returns `None` if the peer does not support DATAGRAM frames.
pub fn max_datagram_payload(connection: &quinn::Connection) -> Option<usize> {
    connection.max_datagram_size()
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use wzp_proto::{CodecId, MediaHeader};

    fn test_packet() -> MediaPacket {
        MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: CodecId::Opus16k,
                has_quality_report: false,
                fec_ratio_encoded: 16,
                seq: 42,
                timestamp: 1000,
                fec_block: 1,
                fec_symbol: 0,
                reserved: 0,
                csrc_count: 0,
            },
            payload: Bytes::from_static(b"fake opus frame data"),
            quality_report: None,
        }
    }

    #[test]
    fn serialize_deserialize_roundtrip() {
        let packet = test_packet();
        let data = serialize_media(&packet);
        let decoded = deserialize_media(data).expect("deserialize should succeed");
        assert_eq!(packet.header, decoded.header);
        assert_eq!(packet.payload, decoded.payload);
        assert_eq!(packet.quality_report, decoded.quality_report);
    }

    #[test]
    fn serialize_deserialize_with_quality_report() {
        let mut packet = test_packet();
        packet.header.has_quality_report = true;
        packet.quality_report = Some(wzp_proto::QualityReport {
            loss_pct: 50,
            rtt_4ms: 75,
            jitter_ms: 10,
            bitrate_cap_kbps: 100,
        });

        let data = serialize_media(&packet);
        let decoded = deserialize_media(data).expect("deserialize should succeed");
        assert_eq!(packet.header, decoded.header);
        assert_eq!(packet.payload, decoded.payload);
        assert_eq!(packet.quality_report, decoded.quality_report);
    }

    #[test]
    fn deserialize_invalid_data_returns_none() {
        let data = Bytes::from_static(b"too short");
        assert!(deserialize_media(data).is_none());
    }
}
