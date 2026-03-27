use bytes::{Buf, BufMut, Bytes, BytesMut};
use serde::{Deserialize, Serialize};

use crate::CodecId;

/// 12-byte media packet header for the lossy link.
///
/// Wire layout:
/// ```text
/// Byte 0:  [V:1][T:1][CodecID:4][Q:1][FecRatioHi:1]
/// Byte 1:  [FecRatioLo:6][unused:2]
/// Byte 2-3: Sequence number (big-endian u16)
/// Byte 4-7: Timestamp in ms since session start (big-endian u32)
/// Byte 8:   FEC block ID
/// Byte 9:   FEC symbol index within block
/// Byte 10:  Reserved / flags
/// Byte 11:  CSRC count
/// ```
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct MediaHeader {
    /// Protocol version (0 = v1).
    pub version: u8,
    /// true = FEC repair packet, false = source media.
    pub is_repair: bool,
    /// Codec identifier.
    pub codec_id: CodecId,
    /// Whether a QualityReport trailer is appended.
    pub has_quality_report: bool,
    /// FEC ratio as 7-bit value (0-127 maps to 0.0-1.0).
    pub fec_ratio_encoded: u8,
    /// Wrapping packet sequence number.
    pub seq: u16,
    /// Milliseconds since session start.
    pub timestamp: u32,
    /// FEC source block ID (wrapping).
    pub fec_block: u8,
    /// Symbol index within the FEC block.
    pub fec_symbol: u8,
    /// Reserved flags byte.
    pub reserved: u8,
    /// Number of contributing sources (for future mixing).
    pub csrc_count: u8,
}

impl MediaHeader {
    /// Header size in bytes on the wire.
    pub const WIRE_SIZE: usize = 12;

    /// Encode the FEC ratio float (0.0-2.0+) to a 7-bit value (0-127).
    pub fn encode_fec_ratio(ratio: f32) -> u8 {
        // Map 0.0-2.0 to 0-127, clamping at 127
        let scaled = (ratio * 63.5).round() as u8;
        scaled.min(127)
    }

    /// Decode the 7-bit FEC ratio value back to a float.
    pub fn decode_fec_ratio(encoded: u8) -> f32 {
        (encoded & 0x7F) as f32 / 63.5
    }

    /// Serialize to a 12-byte buffer.
    pub fn write_to(&self, buf: &mut impl BufMut) {
        // Byte 0: V(1) | T(1) | CodecID(4) | Q(1) | FecRatioHi(1)
        let byte0 = ((self.version & 0x01) << 7)
            | ((self.is_repair as u8) << 6)
            | ((self.codec_id.to_wire() & 0x0F) << 2)
            | ((self.has_quality_report as u8) << 1)
            | ((self.fec_ratio_encoded >> 6) & 0x01);
        buf.put_u8(byte0);

        // Byte 1: FecRatioLo(6) | unused(2)
        let byte1 = (self.fec_ratio_encoded & 0x3F) << 2;
        buf.put_u8(byte1);

        // Bytes 2-3: sequence number
        buf.put_u16(self.seq);

        // Bytes 4-7: timestamp
        buf.put_u32(self.timestamp);

        // Byte 8: FEC block
        buf.put_u8(self.fec_block);

        // Byte 9: FEC symbol
        buf.put_u8(self.fec_symbol);

        // Byte 10: reserved
        buf.put_u8(self.reserved);

        // Byte 11: CSRC count
        buf.put_u8(self.csrc_count);
    }

    /// Deserialize from a buffer. Returns None if insufficient data.
    pub fn read_from(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::WIRE_SIZE {
            return None;
        }

        let byte0 = buf.get_u8();
        let byte1 = buf.get_u8();

        let version = (byte0 >> 7) & 0x01;
        let is_repair = ((byte0 >> 6) & 0x01) != 0;
        let codec_wire = (byte0 >> 2) & 0x0F;
        let has_quality_report = ((byte0 >> 1) & 0x01) != 0;
        let fec_ratio_hi = byte0 & 0x01;
        let fec_ratio_lo = (byte1 >> 2) & 0x3F;
        let fec_ratio_encoded = (fec_ratio_hi << 6) | fec_ratio_lo;

        let codec_id = CodecId::from_wire(codec_wire)?;
        let seq = buf.get_u16();
        let timestamp = buf.get_u32();
        let fec_block = buf.get_u8();
        let fec_symbol = buf.get_u8();
        let reserved = buf.get_u8();
        let csrc_count = buf.get_u8();

        Some(Self {
            version,
            is_repair,
            codec_id,
            has_quality_report,
            fec_ratio_encoded,
            seq,
            timestamp,
            fec_block,
            fec_symbol,
            reserved,
            csrc_count,
        })
    }

    /// Serialize header to a new Bytes value.
    pub fn to_bytes(&self) -> Bytes {
        let mut buf = BytesMut::with_capacity(Self::WIRE_SIZE);
        self.write_to(&mut buf);
        buf.freeze()
    }
}

/// Quality report appended to a media packet when Q flag is set (4 bytes).
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub struct QualityReport {
    /// Observed loss percentage (0-255 maps to 0-100%).
    pub loss_pct: u8,
    /// RTT estimate in 4ms units (0-255 = 0-1020ms).
    pub rtt_4ms: u8,
    /// Jitter in milliseconds.
    pub jitter_ms: u8,
    /// Maximum receive bitrate in kbps.
    pub bitrate_cap_kbps: u8,
}

impl QualityReport {
    pub const WIRE_SIZE: usize = 4;

    pub fn loss_percent(&self) -> f32 {
        self.loss_pct as f32 / 255.0 * 100.0
    }

    pub fn rtt_ms(&self) -> u16 {
        self.rtt_4ms as u16 * 4
    }

    pub fn write_to(&self, buf: &mut impl BufMut) {
        buf.put_u8(self.loss_pct);
        buf.put_u8(self.rtt_4ms);
        buf.put_u8(self.jitter_ms);
        buf.put_u8(self.bitrate_cap_kbps);
    }

    pub fn read_from(buf: &mut impl Buf) -> Option<Self> {
        if buf.remaining() < Self::WIRE_SIZE {
            return None;
        }
        Some(Self {
            loss_pct: buf.get_u8(),
            rtt_4ms: buf.get_u8(),
            jitter_ms: buf.get_u8(),
            bitrate_cap_kbps: buf.get_u8(),
        })
    }
}

/// A complete media packet (header + payload + optional quality report).
#[derive(Clone, Debug)]
pub struct MediaPacket {
    pub header: MediaHeader,
    pub payload: Bytes,
    pub quality_report: Option<QualityReport>,
}

impl MediaPacket {
    /// Serialize the entire packet to bytes.
    pub fn to_bytes(&self) -> Bytes {
        let qr_size = if self.quality_report.is_some() {
            QualityReport::WIRE_SIZE
        } else {
            0
        };
        let total = MediaHeader::WIRE_SIZE + self.payload.len() + qr_size;
        let mut buf = BytesMut::with_capacity(total);

        self.header.write_to(&mut buf);
        buf.put(self.payload.clone());
        if let Some(ref qr) = self.quality_report {
            qr.write_to(&mut buf);
        }

        buf.freeze()
    }

    /// Deserialize from bytes. `payload_len` must be known from context
    /// (e.g., total packet size minus header minus optional QR).
    pub fn from_bytes(data: Bytes) -> Option<Self> {
        let mut cursor = &data[..];
        let header = MediaHeader::read_from(&mut cursor)?;

        let remaining = data.len() - MediaHeader::WIRE_SIZE;
        let (payload_len, quality_report) = if header.has_quality_report {
            if remaining < QualityReport::WIRE_SIZE {
                return None;
            }
            let pl = remaining - QualityReport::WIRE_SIZE;
            let qr_start = MediaHeader::WIRE_SIZE + pl;
            let mut qr_cursor = &data[qr_start..];
            let qr = QualityReport::read_from(&mut qr_cursor)?;
            (pl, Some(qr))
        } else {
            (remaining, None)
        };

        let payload = data.slice(MediaHeader::WIRE_SIZE..MediaHeader::WIRE_SIZE + payload_len);

        Some(Self {
            header,
            payload,
            quality_report,
        })
    }
}

/// Signaling messages sent over the reliable QUIC stream.
///
/// Compatible with Warzone messenger's identity model:
/// - Identity keys are Ed25519 (signing) + X25519 (encryption) derived from a 32-byte seed via HKDF
/// - Fingerprint = SHA-256(Ed25519 public key)[:16]
#[derive(Clone, Debug, Serialize, Deserialize)]
pub enum SignalMessage {
    /// Call initiation (analogous to Warzone's WireMessage::CallOffer).
    CallOffer {
        /// Caller's Ed25519 identity public key (32 bytes).
        identity_pub: [u8; 32],
        /// Ephemeral X25519 public key for this call.
        ephemeral_pub: [u8; 32],
        /// Ed25519 signature over (ephemeral_pub || callee_fingerprint).
        signature: Vec<u8>,
        /// Supported quality profiles.
        supported_profiles: Vec<crate::QualityProfile>,
    },

    /// Call acceptance (analogous to Warzone's WireMessage::CallAnswer).
    CallAnswer {
        /// Callee's Ed25519 identity public key (32 bytes).
        identity_pub: [u8; 32],
        /// Callee's ephemeral X25519 public key.
        ephemeral_pub: [u8; 32],
        /// Ed25519 signature over (ephemeral_pub || caller_fingerprint).
        signature: Vec<u8>,
        /// Chosen quality profile.
        chosen_profile: crate::QualityProfile,
    },

    /// ICE candidate for NAT traversal.
    IceCandidate {
        candidate: String,
    },

    /// Periodic rekeying (forward secrecy).
    Rekey {
        /// New ephemeral X25519 public key.
        new_ephemeral_pub: [u8; 32],
        /// Ed25519 signature over (new_ephemeral_pub || session_id).
        signature: Vec<u8>,
    },

    /// Quality/profile change request.
    QualityUpdate {
        report: QualityReport,
        recommended_profile: crate::QualityProfile,
    },

    /// Connection keepalive / RTT measurement.
    Ping { timestamp_ms: u64 },
    Pong { timestamp_ms: u64 },

    /// End the call.
    Hangup { reason: HangupReason },
}

/// Reasons for ending a call.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Serialize, Deserialize)]
pub enum HangupReason {
    Normal,
    Busy,
    Declined,
    Timeout,
    Error,
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn header_roundtrip() {
        let header = MediaHeader {
            version: 0,
            is_repair: false,
            codec_id: CodecId::Opus24k,
            has_quality_report: true,
            fec_ratio_encoded: 42,
            seq: 12345,
            timestamp: 987654,
            fec_block: 7,
            fec_symbol: 3,
            reserved: 0,
            csrc_count: 0,
        };

        let bytes = header.to_bytes();
        assert_eq!(bytes.len(), MediaHeader::WIRE_SIZE);

        let mut cursor = &bytes[..];
        let decoded = MediaHeader::read_from(&mut cursor).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn header_repair_flag() {
        let header = MediaHeader {
            version: 0,
            is_repair: true,
            codec_id: CodecId::Codec2_1200,
            has_quality_report: false,
            fec_ratio_encoded: 127,
            seq: 65535,
            timestamp: u32::MAX,
            fec_block: 255,
            fec_symbol: 255,
            reserved: 0xFF,
            csrc_count: 0,
        };

        let bytes = header.to_bytes();
        let mut cursor = &bytes[..];
        let decoded = MediaHeader::read_from(&mut cursor).unwrap();
        assert_eq!(header, decoded);
    }

    #[test]
    fn quality_report_roundtrip() {
        let qr = QualityReport {
            loss_pct: 128,
            rtt_4ms: 100,
            jitter_ms: 50,
            bitrate_cap_kbps: 200,
        };

        let mut buf = BytesMut::new();
        qr.write_to(&mut buf);
        assert_eq!(buf.len(), QualityReport::WIRE_SIZE);

        let mut cursor = &buf[..];
        let decoded = QualityReport::read_from(&mut cursor).unwrap();
        assert_eq!(qr, decoded);
    }

    #[test]
    fn media_packet_roundtrip() {
        let packet = MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: CodecId::Opus6k,
                has_quality_report: true,
                fec_ratio_encoded: 32,
                seq: 100,
                timestamp: 2000,
                fec_block: 1,
                fec_symbol: 0,
                reserved: 0,
                csrc_count: 0,
            },
            payload: Bytes::from_static(b"test audio data here"),
            quality_report: Some(QualityReport {
                loss_pct: 25,
                rtt_4ms: 75,
                jitter_ms: 10,
                bitrate_cap_kbps: 100,
            }),
        };

        let bytes = packet.to_bytes();
        let decoded = MediaPacket::from_bytes(bytes).unwrap();

        assert_eq!(packet.header, decoded.header);
        assert_eq!(packet.payload, decoded.payload);
        assert_eq!(packet.quality_report, decoded.quality_report);
    }

    #[test]
    fn fec_ratio_encode_decode() {
        let ratio = 0.5;
        let encoded = MediaHeader::encode_fec_ratio(ratio);
        let decoded = MediaHeader::decode_fec_ratio(encoded);
        assert!((decoded - ratio).abs() < 0.02);

        let ratio_max = 2.0;
        let encoded_max = MediaHeader::encode_fec_ratio(ratio_max);
        assert_eq!(encoded_max, 127);
    }
}
