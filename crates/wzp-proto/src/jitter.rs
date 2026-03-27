use std::collections::BTreeMap;

use crate::packet::MediaPacket;

/// Adaptive jitter buffer that reorders packets by sequence number.
///
/// Designed for the lossy relay link with up to 5 seconds of buffering depth.
/// Manages packet reordering, gap detection, and signals when PLC is needed.
pub struct JitterBuffer {
    /// Packets waiting to be consumed, ordered by sequence number.
    buffer: BTreeMap<u16, MediaPacket>,
    /// Next sequence number expected for playout.
    next_playout_seq: u16,
    /// Maximum buffer depth in number of packets.
    max_depth: usize,
    /// Target buffer depth (adaptive, based on jitter).
    target_depth: usize,
    /// Minimum buffer depth.
    min_depth: usize,
    /// Whether we have received the first packet and initialized.
    initialized: bool,
    /// Statistics.
    stats: JitterStats,
}

/// Jitter buffer statistics.
#[derive(Clone, Debug, Default)]
pub struct JitterStats {
    pub packets_received: u64,
    pub packets_played: u64,
    pub packets_lost: u64,
    pub packets_late: u64,
    pub packets_duplicate: u64,
    pub current_depth: usize,
}

/// Result of attempting to get the next packet for playout.
#[derive(Debug)]
pub enum PlayoutResult {
    /// A packet is available for playout.
    Packet(MediaPacket),
    /// The expected packet is missing — decoder should generate PLC.
    Missing { seq: u16 },
    /// Buffer is empty or not yet filled to target depth.
    NotReady,
}

impl JitterBuffer {
    /// Create a new jitter buffer.
    ///
    /// - `target_depth`: initial target buffer depth in packets
    /// - `max_depth`: absolute maximum (e.g., 250 packets = 5s at 20ms/frame)
    /// - `min_depth`: minimum depth before playout begins
    pub fn new(target_depth: usize, max_depth: usize, min_depth: usize) -> Self {
        Self {
            buffer: BTreeMap::new(),
            next_playout_seq: 0,
            max_depth,
            target_depth,
            min_depth,
            initialized: false,
            stats: JitterStats::default(),
        }
    }

    /// Create with default settings for 5-second max buffer at 20ms frames.
    pub fn default_5s() -> Self {
        Self::new(
            50,  // target: 1 second
            250, // max: 5 seconds
            25,  // min: 0.5 seconds before starting playout
        )
    }

    /// Push a received packet into the buffer.
    pub fn push(&mut self, packet: MediaPacket) {
        let seq = packet.header.seq;
        self.stats.packets_received += 1;

        if !self.initialized {
            self.next_playout_seq = seq;
            self.initialized = true;
        }

        // Check for duplicates
        if self.buffer.contains_key(&seq) {
            self.stats.packets_duplicate += 1;
            return;
        }

        // Check if packet is too old (already played out)
        if self.stats.packets_played > 0 && seq_before(seq, self.next_playout_seq) {
            self.stats.packets_late += 1;
            return;
        }

        // If we haven't started playout yet, adjust next_playout_seq to earliest known
        if self.stats.packets_played == 0 && seq_before(seq, self.next_playout_seq) {
            self.next_playout_seq = seq;
        }

        self.buffer.insert(seq, packet);

        // Evict oldest if over max depth
        while self.buffer.len() > self.max_depth {
            if let Some((&oldest_seq, _)) = self.buffer.first_key_value() {
                self.buffer.remove(&oldest_seq);
                // Advance playout seq past evicted packet
                if seq_before(self.next_playout_seq, oldest_seq.wrapping_add(1)) {
                    self.next_playout_seq = oldest_seq.wrapping_add(1);
                    self.stats.packets_lost += 1;
                }
            }
        }

        self.stats.current_depth = self.buffer.len();
    }

    /// Get the next packet for playout.
    ///
    /// Call this at the codec's frame rate (e.g., every 20ms).
    pub fn pop(&mut self) -> PlayoutResult {
        if !self.initialized {
            return PlayoutResult::NotReady;
        }

        // Wait until we have enough buffered
        if self.buffer.len() < self.min_depth {
            // But only wait if we haven't started playing yet
            if self.stats.packets_played == 0 {
                return PlayoutResult::NotReady;
            }
        }

        let seq = self.next_playout_seq;
        self.next_playout_seq = seq.wrapping_add(1);

        if let Some(packet) = self.buffer.remove(&seq) {
            self.stats.packets_played += 1;
            self.stats.current_depth = self.buffer.len();
            PlayoutResult::Packet(packet)
        } else {
            self.stats.packets_lost += 1;
            self.stats.current_depth = self.buffer.len();
            PlayoutResult::Missing { seq }
        }
    }

    /// Current buffer depth (number of packets stored).
    pub fn depth(&self) -> usize {
        self.buffer.len()
    }

    /// Get current statistics.
    pub fn stats(&self) -> &JitterStats {
        &self.stats
    }

    /// Reset the buffer (e.g., on call restart).
    pub fn reset(&mut self) {
        self.buffer.clear();
        self.initialized = false;
        self.stats = JitterStats::default();
    }

    /// Adjust target depth based on observed jitter.
    pub fn set_target_depth(&mut self, depth: usize) {
        self.target_depth = depth.min(self.max_depth);
    }
}

/// Sequence number comparison with wrapping (RFC 1982 serial number arithmetic).
/// Returns true if `a` comes before `b` in sequence space.
fn seq_before(a: u16, b: u16) -> bool {
    let diff = b.wrapping_sub(a);
    diff > 0 && diff < 0x8000
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::packet::{MediaHeader, MediaPacket};
    use bytes::Bytes;
    use crate::CodecId;

    fn make_packet(seq: u16) -> MediaPacket {
        MediaPacket {
            header: MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: CodecId::Opus24k,
                has_quality_report: false,
                fec_ratio_encoded: 0,
                seq,
                timestamp: seq as u32 * 20,
                fec_block: 0,
                fec_symbol: 0,
                reserved: 0,
                csrc_count: 0,
            },
            payload: Bytes::from(vec![0u8; 60]),
            quality_report: None,
        }
    }

    #[test]
    fn basic_ordered_playout() {
        let mut jb = JitterBuffer::new(3, 100, 2);

        // Push 3 packets in order
        jb.push(make_packet(0));
        jb.push(make_packet(1));
        jb.push(make_packet(2));

        // Should get them in order
        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 0),
            other => panic!("expected packet, got {:?}", other),
        }
        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 1),
            other => panic!("expected packet, got {:?}", other),
        }
    }

    #[test]
    fn reorders_out_of_order_packets() {
        let mut jb = JitterBuffer::new(3, 100, 2);

        jb.push(make_packet(2));
        jb.push(make_packet(0));
        jb.push(make_packet(1));

        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 0),
            other => panic!("expected packet 0, got {:?}", other),
        }
        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 1),
            other => panic!("expected packet 1, got {:?}", other),
        }
        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 2),
            other => panic!("expected packet 2, got {:?}", other),
        }
    }

    #[test]
    fn reports_missing_packets() {
        let mut jb = JitterBuffer::new(2, 100, 1);

        // Push packet 0 and 2 (skip 1)
        jb.push(make_packet(0));
        jb.push(make_packet(2));

        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 0),
            other => panic!("expected packet 0, got {:?}", other),
        }
        match jb.pop() {
            PlayoutResult::Missing { seq } => assert_eq!(seq, 1),
            other => panic!("expected missing 1, got {:?}", other),
        }
        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 2),
            other => panic!("expected packet 2, got {:?}", other),
        }
    }

    #[test]
    fn drops_duplicates() {
        let mut jb = JitterBuffer::new(2, 100, 1);
        jb.push(make_packet(0));
        jb.push(make_packet(0)); // duplicate
        assert_eq!(jb.stats().packets_duplicate, 1);
        assert_eq!(jb.depth(), 1);
    }

    #[test]
    fn seq_before_wrapping() {
        assert!(seq_before(0, 1));
        assert!(seq_before(65534, 65535));
        assert!(seq_before(65535, 0)); // wrap
        assert!(!seq_before(1, 0));
        assert!(!seq_before(5, 5)); // equal
    }

    #[test]
    fn not_ready_until_min_depth() {
        let mut jb = JitterBuffer::new(5, 100, 3);
        jb.push(make_packet(0));
        jb.push(make_packet(1));

        // Only 2 packets, min_depth is 3
        match jb.pop() {
            PlayoutResult::NotReady => {}
            other => panic!("expected NotReady, got {:?}", other),
        }

        jb.push(make_packet(2));
        // Now we have 3, should be ready
        match jb.pop() {
            PlayoutResult::Packet(p) => assert_eq!(p.header.seq, 0),
            other => panic!("expected packet 0, got {:?}", other),
        }
    }
}
