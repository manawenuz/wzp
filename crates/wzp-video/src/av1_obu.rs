//! AV1 Open Bitstream Unit (OBU) parsing and framing.
//!
//! AV1 uses OBUs instead of NAL units. Each OBU has a 1-byte header
//! (`obu_type`, `has_size_field`, `extension_flag`) followed by an optional
//! LEB128 size field and payload.

/// OBU type codes.
pub mod obu_type {
    /// Sequence header OBU.
    pub const SEQUENCE_HEADER: u8 = 1;
    /// Temporal delimiter OBU.
    pub const TEMPORAL_DELIMITER: u8 = 2;
    /// Frame header OBU.
    pub const FRAME_HEADER: u8 = 3;
    /// Tile group OBU.
    pub const TILE_GROUP: u8 = 4;
    /// Metadata OBU.
    pub const METADATA: u8 = 5;
    /// Frame OBU (header + tile group combined).
    pub const FRAME: u8 = 6;
    /// Redundant frame header OBU.
    pub const REDUNDANT_FRAME_HEADER: u8 = 7;
    /// Tile list OBU.
    pub const TILE_LIST: u8 = 8;
    /// Padding OBU.
    pub const PADDING: u8 = 15;
}

/// Parsed OBU header.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct ObuHeader {
    /// OBU type (1–15).
    pub obu_type: u8,
    /// True if a LEB128 size field follows the header.
    pub has_size_field: bool,
    /// True if an extension header follows the main header.
    pub extension_flag: bool,
}

impl ObuHeader {
    /// Parse an OBU header from the first byte of an OBU.
    pub fn from_byte(byte: u8) -> Self {
        Self {
            obu_type: (byte >> 3) & 0x0F,
            has_size_field: ((byte >> 1) & 0x01) != 0,
            extension_flag: (byte & 0x01) != 0,
        }
    }

    /// Encode the OBU header to a single byte.
    pub fn to_byte(self) -> u8 {
        let mut b = 0u8;
        b |= (self.obu_type & 0x0F) << 3;
        if self.has_size_field {
            b |= 0x02;
        }
        if self.extension_flag {
            b |= 0x01;
        }
        b
    }
}

/// Read a LEB128-encoded value from `data` starting at `offset`.
///
/// Returns `(value, bytes_consumed)` or `None` if the encoding is invalid
/// or truncated.
pub fn read_leb128(data: &[u8], offset: usize) -> Option<(u64, usize)> {
    let mut value = 0u64;
    let mut shift = 0u32;
    let mut i = offset;
    loop {
        if i >= data.len() {
            return None;
        }
        let byte = data[i];
        i += 1;
        value |= ((byte & 0x7F) as u64) << shift;
        if (byte & 0x80) == 0 {
            return Some((value, i - offset));
        }
        shift += 7;
        if shift >= 64 {
            return None;
        }
    }
}

/// Write a value as LEB128 into `out`.
pub fn write_leb128(value: u64, out: &mut Vec<u8>) {
    let mut v = value;
    loop {
        let mut byte = (v & 0x7F) as u8;
        v >>= 7;
        if v != 0 {
            byte |= 0x80;
        }
        out.push(byte);
        if v == 0 {
            break;
        }
    }
}

/// Split a raw OBU byte stream into individual OBUs.
///
/// Returns a vector of `(header, payload)` tuples. The payload does **not**
/// include the header or size field — it is the raw OBU payload bytes.
///
/// Supports the low-overhead bitstream format (`has_size_field = true`).
/// OBUs without a size field are not supported (returns an empty vector).
pub fn split_obus(data: &[u8]) -> Vec<(ObuHeader, Vec<u8>)> {
    let mut result = Vec::new();
    let mut i = 0usize;
    while i < data.len() {
        let header = ObuHeader::from_byte(data[i]);
        i += 1;

        if header.extension_flag {
            // Extension header is 1 byte; skip it.
            if i >= data.len() {
                break;
            }
            i += 1;
        }

        let payload_len = if header.has_size_field {
            let Some((size, consumed)) = read_leb128(data, i) else {
                break;
            };
            i += consumed;
            size as usize
        } else {
            // Unsupported: OBU runs to end of stream. Stop parsing.
            break;
        };

        if i + payload_len > data.len() {
            break;
        }
        let payload = data[i..i + payload_len].to_vec();
        i += payload_len;
        result.push((header, payload));
    }
    result
}

/// Returns true if the given OBU data contains a keyframe.
///
/// Inspects `OBU_FRAME_HEADER` and `OBU_FRAME` OBUs.  In AV1, a keyframe
/// has `frame_type == 0` (KEY_FRAME) in the frame header.
///
/// `data` should be the full OBU stream (headers + payloads).
pub fn is_keyframe_obu(data: &[u8]) -> bool {
    let obus = split_obus(data);
    for (header, payload) in &obus {
        let is_frame_header =
            header.obu_type == obu_type::FRAME_HEADER || header.obu_type == obu_type::FRAME;
        if !is_frame_header || payload.is_empty() {
            continue;
        }
        // Parse the frame header. First bit is show_existing_frame.
        let mut bit_offset = 0usize;
        let show_existing = read_bit(payload, bit_offset);
        bit_offset += 1;
        if show_existing {
            continue;
        }
        // Next 2 bits are frame_type.
        let frame_type = read_bits(payload, bit_offset, 2);
        return frame_type == 0; // KEY_FRAME
    }
    false
}

/// Read a single bit from `data` at `bit_offset`.
fn read_bit(data: &[u8], bit_offset: usize) -> bool {
    let byte_idx = bit_offset / 8;
    let bit_idx = 7 - (bit_offset % 8);
    if byte_idx >= data.len() {
        return false;
    }
    ((data[byte_idx] >> bit_idx) & 1) != 0
}

/// Read `n` bits (max 8) from `data` at `bit_offset`.
fn read_bits(data: &[u8], bit_offset: usize, n: usize) -> u8 {
    debug_assert!(n <= 8);
    let mut value = 0u8;
    for i in 0..n {
        let bit = read_bit(data, bit_offset + i);
        value = (value << 1) | (bit as u8);
    }
    value
}

/// Simple OBU framer that splits an AV1 bitstream into packet-sized chunks.
pub struct Av1ObuFramer {
    max_payload: usize,
}

/// AV1 depacketizer — reassembles packet payloads into a complete OBU access unit.
pub struct Av1Depacketizer {
    buffer: Vec<u8>,
}

impl Av1Depacketizer {
    /// Create a new depacketizer.
    pub fn new() -> Self {
        Self { buffer: Vec::new() }
    }

    /// Push a packet payload into the depacketizer.
    ///
    /// Returns `Some(access_unit)` when `is_frame_end` is true and the
    /// accumulated buffer is non-empty.
    pub fn push(&mut self, payload: &[u8], is_frame_end: bool) -> Option<Vec<u8>> {
        self.buffer.extend_from_slice(payload);
        if is_frame_end && !self.buffer.is_empty() {
            let au = std::mem::take(&mut self.buffer);
            Some(au)
        } else {
            None
        }
    }

    /// Reset the internal buffer.
    pub fn reset(&mut self) {
        self.buffer.clear();
    }
}

impl Default for Av1Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

impl Av1ObuFramer {
    /// Create a new framer with the given max RTP payload size.
    pub fn new(max_payload: usize) -> Self {
        Self { max_payload }
    }

    /// Frame an AV1 access unit (one or more OBUs) into packets.
    ///
    /// Each packet contains one or more complete OBUs. OBUs larger than
    /// `max_payload` are not fragmented — the caller must set `max_payload`
    /// large enough for the largest OBU, or use a separate OBU aggregation
    /// scheme. Returns a vector of packet payloads.
    pub fn frame(&self, access_unit: &[u8]) -> Vec<Vec<u8>> {
        let obus = split_obus(access_unit);
        if obus.is_empty() {
            return Vec::new();
        }

        let mut packets = Vec::new();
        let mut current = Vec::new();

        for (header, payload) in obus {
            let mut obu_data = vec![header.to_byte()];
            write_leb128(payload.len() as u64, &mut obu_data);
            obu_data.extend_from_slice(&payload);

            if !current.is_empty() && current.len() + obu_data.len() > self.max_payload {
                packets.push(current);
                current = Vec::new();
            }
            current.extend_from_slice(&obu_data);
        }

        if !current.is_empty() {
            packets.push(current);
        }
        packets
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic OBU: header byte + LEB128 size + payload.
    fn synthetic_obu(obu_type: u8, payload: &[u8]) -> Vec<u8> {
        let mut out = Vec::new();
        let header = ObuHeader {
            obu_type,
            has_size_field: true,
            extension_flag: false,
        };
        out.push(header.to_byte());
        write_leb128(payload.len() as u64, &mut out);
        out.extend_from_slice(payload);
        out
    }

    #[test]
    fn obu_header_roundtrip() {
        for obu_type in 0..=15 {
            for has_size in [false, true] {
                for ext in [false, true] {
                    let h = ObuHeader {
                        obu_type,
                        has_size_field: has_size,
                        extension_flag: ext,
                    };
                    let byte = h.to_byte();
                    let parsed = ObuHeader::from_byte(byte);
                    assert_eq!(h, parsed, "roundtrip failed for type={obu_type}");
                }
            }
        }
    }

    #[test]
    fn leb128_roundtrip() {
        let values = [0u64, 1, 127, 128, 255, 256, 16383, 16384, 65535, 65536];
        for &v in &values {
            let mut buf = Vec::new();
            write_leb128(v, &mut buf);
            let (decoded, consumed) = read_leb128(&buf, 0).unwrap();
            assert_eq!(decoded, v, "LEB128 roundtrip failed for {v}");
            assert_eq!(consumed, buf.len());
        }
    }

    #[test]
    fn split_obus_basic() {
        let mut au = Vec::new();
        au.extend_from_slice(&synthetic_obu(obu_type::SEQUENCE_HEADER, &[0xAA; 10]));
        au.extend_from_slice(&synthetic_obu(obu_type::FRAME, &[0xBB; 20]));

        let obus = split_obus(&au);
        assert_eq!(obus.len(), 2);
        assert_eq!(obus[0].0.obu_type, obu_type::SEQUENCE_HEADER);
        assert_eq!(obus[0].1.len(), 10);
        assert_eq!(obus[1].0.obu_type, obu_type::FRAME);
        assert_eq!(obus[1].1.len(), 20);
    }

    #[test]
    fn is_keyframe_detects_keyframe() {
        // Frame header with show_existing_frame=0, frame_type=0 (KEY_FRAME)
        // Bits: 0 (show_existing) | 00 (frame_type=KEY) | ...
        // First byte: 0b0000_0000 = 0x00
        let fh = synthetic_obu(obu_type::FRAME_HEADER, &[0x00, 0x00]);
        assert!(is_keyframe_obu(&fh));
    }

    #[test]
    fn is_keyframe_rejects_inter_frame() {
        // Frame header with show_existing_frame=0, frame_type=1 (INTER)
        // Bits: 0 | 01 | ... = 0b0100_0000 = 0x40
        let fh = synthetic_obu(obu_type::FRAME_HEADER, &[0x40, 0x00]);
        assert!(!is_keyframe_obu(&fh));
    }

    #[test]
    fn av1_obu_framer_splits_access_unit() {
        let mut au = Vec::new();
        au.extend_from_slice(&synthetic_obu(obu_type::SEQUENCE_HEADER, &[0xAA; 10]));
        au.extend_from_slice(&synthetic_obu(obu_type::FRAME, &[0xBB; 20]));

        let framer = Av1ObuFramer::new(100);
        let packets = framer.frame(&au);
        assert_eq!(packets.len(), 1);

        // Verify roundtrip: split the packet back into OBUs
        let obus = split_obus(&packets[0]);
        assert_eq!(obus.len(), 2);
    }
}
