//! H.264 NAL framer — splits access units into MTU-sized packets.
//!
//! Supports Single-NAL and FU-A (Fragmentation Unit type A) per RFC 6184.

/// One framed packet emitted by [`H264Framer`].
#[derive(Clone, Debug, PartialEq, Eq)]
pub struct FramedPacket {
    pub payload: Vec<u8>,
    /// True when this is the last packet of the access unit.
    pub is_frame_end: bool,
}

/// H.264 access-unit framer.
///
/// Parses NAL units from a raw access unit and emits either Single-NAL
/// packets or FU-A fragments so that every payload fits in `max_payload_size`.
pub struct H264Framer {
    max_payload_size: usize,
}

impl H264Framer {
    /// Create a framer with the given maximum payload size per packet.
    ///
    /// Typical value: `MTU - MediaHeader::WIRE_SIZE - AEAD_TAG_SIZE`.
    pub fn new(max_payload_size: usize) -> Self {
        Self { max_payload_size }
    }

    /// Frame one access unit into a sequence of packets.
    ///
    /// The input may contain one or more NAL units separated by H.264 start
    /// codes (`0x000001` or `0x00000001`).  The last emitted packet has
    /// `is_frame_end = true`.
    pub fn frame(&self, access_unit: &[u8]) -> Vec<FramedPacket> {
        let nals = split_nals(access_unit);
        if nals.is_empty() {
            return Vec::new();
        }

        let mut packets = Vec::new();
        let nal_count = nals.len();

        for (idx, nal) in nals.iter().enumerate() {
            let is_last_nal = idx + 1 == nal_count;

            if nal.len() <= self.max_payload_size {
                // Single-NAL packet.
                packets.push(FramedPacket {
                    payload: nal.to_vec(),
                    is_frame_end: is_last_nal,
                });
            } else {
                // FU-A fragmentation.
                let original_header = nal[0];
                let nal_type = original_header & 0x1F;
                let nri = original_header & 0x60;

                // FU indicator: same as original header but with type = 28.
                let fu_indicator = nri | 28;

                let payload = &nal[1..];
                let mut offset = 0;
                let mut frag_idx = 0;
                let total_frags = payload.len().div_ceil(self.max_payload_size - 2);

                while offset < payload.len() {
                    let remaining = payload.len() - offset;
                    let frag_data_len = remaining.min(self.max_payload_size.saturating_sub(2));
                    let is_first = frag_idx == 0;
                    let is_last = frag_idx + 1 == total_frags;

                    let fu_header = (if is_first { 0x80 } else { 0 })
                        | (if is_last { 0x40 } else { 0 })
                        | nal_type;

                    let mut pkt = Vec::with_capacity(2 + frag_data_len);
                    pkt.push(fu_indicator);
                    pkt.push(fu_header);
                    pkt.extend_from_slice(&payload[offset..offset + frag_data_len]);

                    packets.push(FramedPacket {
                        payload: pkt,
                        is_frame_end: is_last_nal && is_last,
                    });

                    offset += frag_data_len;
                    frag_idx += 1;
                }
            }
        }

        packets
    }
}

/// Split a byte slice into individual NAL units.
///
/// NAL units are separated by start codes (`0x000001` or `0x00000001`).
/// Each returned slice starts with the NAL header byte and contains no
/// start-code prefix.
fn split_nals(data: &[u8]) -> Vec<&[u8]> {
    let mut nals = Vec::new();
    let mut i = 0;

    while i < data.len() {
        // Skip leading zeros.
        while i < data.len() && data[i] == 0 {
            i += 1;
        }
        // Need at least one more byte for the 0x01 marker.
        if i >= data.len() || data[i] != 1 {
            break;
        }
        i += 1; // skip the 0x01

        let start = i;
        // Find the next start code or end of data.
        while i + 3 < data.len() {
            if data[i] == 0
                && data[i + 1] == 0
                && (data[i + 2] == 1
                    || (data[i + 2] == 0 && i + 4 < data.len() && data[i + 3] == 1))
            {
                break;
            }
            i += 1;
        }
        // If no more start codes were found, consume to the end.
        if i + 3 >= data.len() {
            i = data.len();
        }
        let end = i;
        if start < end {
            nals.push(&data[start..end]);
        }
    }

    nals
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a synthetic access unit with two NAL units.
    fn make_access_unit() -> Vec<u8> {
        let mut au = Vec::new();
        // Start code + NAL 1 (IDR slice, type 5)
        au.extend_from_slice(&[0x00, 0x00, 0x00, 0x01, 0x65, 0x01, 0x02, 0x03]);
        // Start code + NAL 2 (non-IDR slice, type 1)
        au.extend_from_slice(&[0x00, 0x00, 0x01, 0x41, 0x04, 0x05]);
        au
    }

    #[test]
    fn frame_single_nal_roundtrip() {
        let framer = H264Framer::new(100);
        let au = make_access_unit();
        let packets = framer.frame(&au);
        assert_eq!(packets.len(), 2);
        assert_eq!(packets[0].payload, vec![0x65, 0x01, 0x02, 0x03]);
        assert!(!packets[0].is_frame_end);
        assert_eq!(packets[1].payload, vec![0x41, 0x04, 0x05]);
        assert!(packets[1].is_frame_end);
    }

    #[test]
    fn frame_empty_input() {
        let framer = H264Framer::new(100);
        let packets = framer.frame(&[]);
        assert!(packets.is_empty());
    }

    #[test]
    fn frame_fu_a_fragmentation() {
        let framer = H264Framer::new(10);
        // One NAL unit: header 0x65 (IDR) + 20 bytes payload.
        let mut au = vec![0x00, 0x00, 0x01];
        au.push(0x65);
        au.extend_from_slice(&[0xAA; 20]);

        let packets = framer.frame(&au);
        // max_payload_size = 10, so each fragment can carry 8 bytes of data
        // (2 bytes FU-A header + 8 data = 10).
        // 20 bytes payload → 3 fragments (8 + 8 + 4).
        assert_eq!(packets.len(), 3);

        // First fragment.
        assert_eq!(packets[0].payload[0], 0x65 & 0x60 | 28); // FU indicator
        assert_eq!(packets[0].payload[1], 0x80 | 0x05); // S=1, E=0, type=5
        assert_eq!(packets[0].payload.len(), 10);
        assert!(!packets[0].is_frame_end);

        // Middle fragment.
        assert_eq!(packets[1].payload[1], 0x05); // S=0, E=0, type=5
        assert_eq!(packets[1].payload.len(), 10);
        assert!(!packets[1].is_frame_end);

        // Last fragment.
        assert_eq!(packets[2].payload[1], 0x40 | 0x05); // S=0, E=1, type=5
        assert_eq!(packets[2].payload.len(), 6); // 2 header + 4 data
        assert!(packets[2].is_frame_end);
    }

    #[test]
    fn frame_fu_a_exact_fit() {
        let framer = H264Framer::new(12);
        // NAL: 1 header + 10 payload = 11 bytes total → fits in 12, no FU-A.
        let mut au = vec![0x00, 0x00, 0x01];
        au.push(0x41);
        au.extend_from_slice(&[0xBB; 10]);

        let packets = framer.frame(&au);
        assert_eq!(packets.len(), 1);
        assert_eq!(packets[0].payload.len(), 11);
        assert!(packets[0].is_frame_end);
    }
}
