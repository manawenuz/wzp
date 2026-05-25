//! H.264 NAL depacketizer — reassembles packets into access units.
//!
//! Supports Single-NAL and FU-A (Fragmentation Unit type A) per RFC 6184.

/// H.264 depacketizer state machine.
///
/// Push individual packet payloads via [`push`](Self::push).  When a complete
/// access unit is ready (all NALs received and `is_frame_end` seen), the
/// depacketizer returns the reconstructed Annex-B byte slice (start codes
/// inserted between NAL units).
pub struct H264Depacketizer {
    /// Accumulated NAL data for the current access unit.
    buffer: Vec<u8>,
    /// True while we are in the middle of accumulating FU-A fragments.
    in_fragment: bool,
    /// Reconstructed NAL header byte for the current FU-A fragment sequence.
    frag_header: u8,
}

/// Annex-B start code prefix.
const START_CODE: &[u8] = &[0x00, 0x00, 0x01];

impl H264Depacketizer {
    pub fn new() -> Self {
        Self {
            buffer: Vec::new(),
            in_fragment: false,
            frag_header: 0,
        }
    }

    /// Feed one packet payload.
    ///
    /// * `payload` — the packet payload (excluding any transport headers).
    /// * `is_frame_end` — true when this is the last packet of the access unit.
    ///
    /// Returns the complete access unit when `is_frame_end` is true and no
    /// fragmentation is in progress.
    pub fn push(&mut self, payload: &[u8], is_frame_end: bool) -> Option<Vec<u8>> {
        if payload.is_empty() {
            return self.maybe_emit(is_frame_end);
        }

        let nal_type = payload[0] & 0x1F;

        if nal_type == 28 {
            // FU-A fragmentation.
            if payload.len() < 2 {
                // Malformed — drop the fragment and abort current NAL.
                self.in_fragment = false;
                return self.maybe_emit(is_frame_end);
            }

            let fu_header = payload[1];
            let is_start = (fu_header & 0x80) != 0;
            let is_end = (fu_header & 0x40) != 0;

            if is_start {
                // First fragment: reconstruct the original NAL header.
                self.frag_header = (payload[0] & 0xE0) | (fu_header & 0x1F);
                self.start_nal();
                self.buffer.push(self.frag_header);
                self.in_fragment = true;
            }

            if self.in_fragment {
                // Append payload data (skip the 2-byte FU-A headers).
                self.buffer.extend_from_slice(&payload[2..]);
            }

            if is_end {
                self.in_fragment = false;
            }
        } else {
            // Single-NAL packet.
            if self.in_fragment {
                // Unexpected single NAL while fragmenting — abort fragment.
                self.in_fragment = false;
            }
            self.start_nal();
            self.buffer.extend_from_slice(payload);
        }

        self.maybe_emit(is_frame_end)
    }

    fn start_nal(&mut self) {
        self.buffer.extend_from_slice(START_CODE);
    }

    fn maybe_emit(&mut self, is_frame_end: bool) -> Option<Vec<u8>> {
        if is_frame_end && !self.in_fragment {
            if self.buffer.is_empty() {
                None
            } else {
                let au = std::mem::take(&mut self.buffer);
                Some(au)
            }
        } else {
            None
        }
    }
}

impl Default for H264Depacketizer {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn depacketize_single_nal() {
        let mut dep = H264Depacketizer::new();
        let au = dep.push(&[0x65, 0x01, 0x02], true);
        assert_eq!(au, Some(vec![0x00, 0x00, 0x01, 0x65, 0x01, 0x02]));
    }

    #[test]
    fn depacketize_multi_nal_access_unit() {
        let mut dep = H264Depacketizer::new();
        dep.push(&[0x65, 0x01], false);
        let au = dep.push(&[0x41, 0x02, 0x03], true);
        assert_eq!(
            au,
            Some(vec![
                0x00, 0x00, 0x01, 0x65, 0x01, 0x00, 0x00, 0x01, 0x41, 0x02, 0x03
            ])
        );
    }

    #[test]
    fn depacketize_fu_a_fragments() {
        let mut dep = H264Depacketizer::new();
        // Original NAL: 0x65 + [0xAA; 20]
        // Fragmented into 3 FU-A packets.
        let fu_indicator = 0x65 & 0x60 | 28;

        // Start fragment.
        let frag1 = vec![
            fu_indicator,
            0x80 | 0x05,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
        ];
        dep.push(&frag1, false);

        // Middle fragment.
        let frag2 = vec![
            fu_indicator,
            0x05,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
            0xAA,
        ];
        dep.push(&frag2, false);

        // End fragment.
        let frag3 = vec![fu_indicator, 0x40 | 0x05, 0xAA, 0xAA, 0xAA, 0xAA];
        let au = dep.push(&frag3, true);

        let mut expected = vec![0x00, 0x00, 0x01, 0x65];
        expected.extend(std::iter::repeat_n(0xAA, 20));
        assert_eq!(au, Some(expected));
    }

    #[test]
    fn depacketize_empty_payload_no_emit() {
        let mut dep = H264Depacketizer::new();
        let au = dep.push(&[], false);
        assert!(au.is_none());
    }

    #[test]
    fn depacketize_frame_end_without_data_no_emit() {
        let mut dep = H264Depacketizer::new();
        let au = dep.push(&[], true);
        assert!(au.is_none());
    }

    #[test]
    fn depacketize_malformed_fu_a_resets() {
        let mut dep = H264Depacketizer::new();
        // FU-A indicator with no FU header.
        let au = dep.push(&[0x7C], true);
        assert!(au.is_none());
    }
}
