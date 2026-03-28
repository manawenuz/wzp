//! Trunk batching — accumulates media packets from multiple sessions into
//! [`TrunkFrame`]s that fit inside a single QUIC datagram.

use std::time::Duration;

use bytes::Bytes;
use wzp_proto::packet::{TrunkEntry, TrunkFrame};

/// Batches individual session packets into [`TrunkFrame`]s.
///
/// A trunk frame is flushed when any of the following thresholds are hit:
/// - `max_entries` — maximum number of packets per trunk.
/// - `max_bytes` — maximum total wire size (should fit one UDP datagram).
///
/// The caller is responsible for timer-based flushing using [`flush_interval`]
/// and calling [`flush`] when the interval expires.
pub struct TrunkBatcher {
    pending: TrunkFrame,
    /// Current accumulated wire size of the pending frame.
    pending_bytes: usize,
    /// Maximum packets per trunk (default 10).
    pub max_entries: usize,
    /// Maximum total wire bytes per trunk (default 1200, fits in one UDP datagram).
    pub max_bytes: usize,
    /// Maximum wait before flushing (default 5 ms). Used by the caller for timer scheduling.
    pub flush_interval: Duration,
}

impl TrunkBatcher {
    /// Header size: the 2-byte count prefix present in every TrunkFrame.
    const FRAME_HEADER: usize = 2;

    pub fn new() -> Self {
        Self {
            pending: TrunkFrame::new(),
            pending_bytes: Self::FRAME_HEADER,
            max_entries: 10,
            max_bytes: 1200,
            flush_interval: Duration::from_millis(5),
        }
    }

    /// Push a session packet. Returns `Some(frame)` if the batch is now full
    /// and was flushed, `None` if more room remains.
    pub fn push(&mut self, session_id: [u8; 2], payload: Bytes) -> Option<TrunkFrame> {
        let entry_wire = TrunkEntry::OVERHEAD + payload.len();

        // If adding this entry would exceed limits, flush first.
        if self.should_flush_with(entry_wire) && !self.pending.is_empty() {
            let frame = self.take_pending();
            // Then start a new batch with this entry.
            self.pending.push(session_id, payload);
            self.pending_bytes += entry_wire;
            return Some(frame);
        }

        self.pending.push(session_id, payload);
        self.pending_bytes += entry_wire;

        if self.should_flush() {
            Some(self.take_pending())
        } else {
            None
        }
    }

    /// Flush the current pending frame if non-empty.
    pub fn flush(&mut self) -> Option<TrunkFrame> {
        if self.pending.is_empty() {
            None
        } else {
            Some(self.take_pending())
        }
    }

    /// Returns `true` if the pending batch has reached `max_entries` or `max_bytes`.
    pub fn should_flush(&self) -> bool {
        self.pending.len() >= self.max_entries || self.pending_bytes >= self.max_bytes
    }

    // --- private helpers ---

    /// Would adding `extra_bytes` exceed a threshold?
    fn should_flush_with(&self, extra_bytes: usize) -> bool {
        self.pending.len() + 1 > self.max_entries
            || self.pending_bytes + extra_bytes > self.max_bytes
    }

    /// Take the pending frame out, resetting state.
    fn take_pending(&mut self) -> TrunkFrame {
        let frame = std::mem::replace(&mut self.pending, TrunkFrame::new());
        self.pending_bytes = Self::FRAME_HEADER;
        frame
    }
}

impl Default for TrunkBatcher {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn trunk_batcher_fills_and_flushes() {
        let mut batcher = TrunkBatcher::new();
        batcher.max_entries = 3;
        batcher.max_bytes = 4096; // large enough to not interfere

        // First two pushes should not flush.
        assert!(batcher.push([0, 1], Bytes::from_static(b"aaa")).is_none());
        assert!(batcher.push([0, 2], Bytes::from_static(b"bbb")).is_none());
        // Third push should trigger flush (max_entries = 3).
        let frame = batcher
            .push([0, 3], Bytes::from_static(b"ccc"))
            .expect("should flush at max_entries");
        assert_eq!(frame.len(), 3);
        assert_eq!(frame.packets[0].session_id, [0, 1]);
        assert_eq!(frame.packets[2].payload, Bytes::from_static(b"ccc"));

        // Batcher is now empty.
        assert!(batcher.flush().is_none());
    }

    #[test]
    fn trunk_batcher_respects_max_bytes() {
        let mut batcher = TrunkBatcher::new();
        batcher.max_entries = 100; // won't be the trigger
        // Frame header (2) + one entry overhead (4) + 50 payload = 56
        // Two entries: 2 + 2*(4+50) = 110
        // Three entries: 2 + 3*54 = 164
        batcher.max_bytes = 120; // allow at most 2 entries of 50-byte payload

        let big = Bytes::from(vec![0xAA; 50]);
        assert!(batcher.push([0, 1], big.clone()).is_none()); // 56 bytes
        // Second push: 56 + 54 = 110 < 120, fits
        assert!(batcher.push([0, 2], big.clone()).is_none());
        // Third push would be 164 > 120, so existing batch flushes first
        let frame = batcher
            .push([0, 3], big.clone())
            .expect("should flush on max_bytes");
        assert_eq!(frame.len(), 2);

        // The third entry is now pending
        let remaining = batcher.flush().unwrap();
        assert_eq!(remaining.len(), 1);
        assert_eq!(remaining.packets[0].session_id, [0, 3]);
    }
}
