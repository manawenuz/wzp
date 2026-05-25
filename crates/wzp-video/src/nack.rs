//! NACK sender / receiver state machines for video packet-loss recovery.
//!
//! The sender side caches the last 500 ms of packets so it can retransmit on
//! request.  The receiver side detects gaps and decides whether to NACK (low
//! RTT) or emit a Picture-Loss-Indication (high RTT).

use std::collections::BTreeMap;
use std::time::{Duration, Instant};

/// A packet cached for potential retransmission.
#[derive(Clone, Debug, PartialEq)]
pub struct CachedPacket {
    pub seq: u32,
    pub data: Vec<u8>,
    pub timestamp_ms: u64,
}

/// Action emitted by the receiver-side NACK state machine.
#[derive(Debug, Clone, PartialEq)]
pub enum NackAction {
    /// Request retransmission of one or more packets.
    Nack { seqs: Vec<u32> },
    /// RTT is too high for NACK to help — request a keyframe instead.
    PictureLossIndication,
}

/// Sender-side NACK handler.
///
/// Retains recently sent packets in a 500 ms ring buffer.  On `Nack` the
/// sender looks up the requested sequence numbers and returns clones of the
/// cached payloads (if they are still in the buffer).
#[derive(Debug)]
pub struct NackSender {
    buffer: Vec<(Instant, CachedPacket)>,
    max_age: Duration,
}

impl NackSender {
    pub const DEFAULT_MAX_AGE_MS: u64 = 500;

    /// Create a new sender buffer.
    pub fn new() -> Self {
        Self {
            buffer: Vec::with_capacity(1024),
            max_age: Duration::from_millis(Self::DEFAULT_MAX_AGE_MS),
        }
    }

    /// Record a packet that was just sent.
    pub fn on_send(&mut self, packet: CachedPacket, now: Instant) {
        self.buffer.push((now, packet));
    }

    /// Handle an incoming NACK — return any packets we still have.
    pub fn on_nack(&mut self, seqs: &[u32], now: Instant) -> Vec<CachedPacket> {
        self.evict(now);
        let mut out = Vec::with_capacity(seqs.len());
        for seq in seqs {
            if let Some((_, pkt)) = self.buffer.iter().find(|(_, p)| p.seq == *seq) {
                out.push(pkt.clone());
            }
        }
        out
    }

    /// Periodic housekeeping — evict stale packets.
    pub fn tick(&mut self, now: Instant) {
        self.evict(now);
    }

    fn evict(&mut self, now: Instant) {
        self.buffer
            .retain(|(t, _)| now.duration_since(*t) <= self.max_age);
    }
}

impl Default for NackSender {
    fn default() -> Self {
        Self::new()
    }
}

/// Receiver-side NACK / PLI state machine.
///
/// Tracks received sequence numbers and emits [`NackAction`]s for gaps.
///
/// Rules (from PRD-video-v1):
/// * Wait at least `frame_interval` after a gap is noticed before acting.
/// * If `RTT < 2 * frame_interval` → emit `Nack`.
/// * Otherwise → emit `PictureLossIndication`.
/// * Backoff: max 1 Nack per sequence number per `2 * RTT`.
/// * Rate cap: max 50 NACKs / second.
#[derive(Debug)]
pub struct NackReceiver {
    frame_interval: Duration,
    rtt: Duration,
    /// Missing seq → when first noticed.
    missing: BTreeMap<u32, Instant>,
    /// Seq → when last NACK sent.
    last_nack: BTreeMap<u32, Instant>,
    /// Next expected sequence number (contiguous from start).
    next_expected: u32,
    /// NACK rate cap window.
    nacks_this_sec: u32,
    sec_window: Instant,
    max_nack_rate: u32,
}

impl NackReceiver {
    pub const DEFAULT_MAX_NACK_RATE: u32 = 50;

    /// Create a new receiver state machine.
    ///
    /// * `frame_interval` — e.g. 33 ms for 30 fps.
    /// * `rtt` — initial RTT estimate.
    pub fn new(frame_interval: Duration, rtt: Duration) -> Self {
        Self {
            frame_interval,
            rtt,
            missing: BTreeMap::new(),
            last_nack: BTreeMap::new(),
            next_expected: 0,
            nacks_this_sec: 0,
            sec_window: Instant::now(),
            max_nack_rate: Self::DEFAULT_MAX_NACK_RATE,
        }
    }

    /// Update the RTT estimate (e.g. from transport feedback).
    pub fn set_rtt(&mut self, rtt: Duration) {
        self.rtt = rtt;
    }

    /// Record that a packet was received.
    pub fn on_packet(&mut self, seq: u32, now: Instant) {
        // Advance the rate window.
        if now.duration_since(self.sec_window) >= Duration::from_secs(1) {
            self.sec_window = now;
            self.nacks_this_sec = 0;
        }

        let ahead = seq.wrapping_sub(self.next_expected);
        if ahead == 0 {
            // In-order packet, no gap.
            self.next_expected = self.next_expected.wrapping_add(1);
            self.missing.remove(&seq);
            self.last_nack.remove(&seq);
        } else if ahead < u32::MAX / 2 {
            // seq >= next_expected (with wrap handling).  There is a gap.
            for offset in 0..ahead {
                let missing_seq = self.next_expected.wrapping_add(offset);
                self.missing.entry(missing_seq).or_insert(now);
            }
            self.next_expected = seq.wrapping_add(1);
            self.missing.remove(&seq);
            self.last_nack.remove(&seq);
        } else {
            // seq < next_expected — reordered or very late.  Just remove from missing.
            self.missing.remove(&seq);
            self.last_nack.remove(&seq);
        }
    }

    /// Periodic check — evaluate gaps and decide whether to NACK or PLI.
    ///
    /// Call this at roughly `frame_interval` granularity (or on a timer).
    pub fn tick(&mut self, now: Instant) -> Vec<NackAction> {
        if now.duration_since(self.sec_window) >= Duration::from_secs(1) {
            self.sec_window = now;
            self.nacks_this_sec = 0;
        }

        let threshold = self.frame_interval;
        let backoff = self.rtt.saturating_mul(2);
        let mut nack_seqs = Vec::new();

        for (&seq, &noticed_at) in &self.missing {
            if now.duration_since(noticed_at) < threshold {
                continue; // too fresh, packet may still arrive
            }
            if let Some(&last_nack_time) = self.last_nack.get(&seq) {
                if now.duration_since(last_nack_time) < backoff {
                    continue; // still in backoff
                }
            }
            nack_seqs.push(seq);
        }

        if nack_seqs.is_empty() {
            return Vec::new();
        }

        // Decide NACK vs PLI based on RTT.
        if self.rtt < self.frame_interval.saturating_mul(2) {
            // Rate cap: clamp batch to remaining budget.
            let budget = self.max_nack_rate.saturating_sub(self.nacks_this_sec) as usize;
            if budget == 0 {
                return vec![NackAction::PictureLossIndication];
            }
            nack_seqs.truncate(budget);
            self.nacks_this_sec += nack_seqs.len() as u32;
            for seq in &nack_seqs {
                self.last_nack.insert(*seq, now);
            }
            vec![NackAction::Nack { seqs: nack_seqs }]
        } else {
            vec![NackAction::PictureLossIndication]
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn ms(n: u64) -> Duration {
        Duration::from_millis(n)
    }

    #[test]
    fn sender_caches_and_retransmits() {
        let mut sender = NackSender::new();
        let now = Instant::now();

        sender.on_send(
            CachedPacket {
                seq: 10,
                data: vec![1, 2, 3],
                timestamp_ms: 100,
            },
            now,
        );
        sender.on_send(
            CachedPacket {
                seq: 11,
                data: vec![4, 5, 6],
                timestamp_ms: 133,
            },
            now,
        );

        let found = sender.on_nack(&[10, 11], now);
        assert_eq!(found.len(), 2);
        assert_eq!(found[0].seq, 10);
        assert_eq!(found[1].seq, 11);
    }

    #[test]
    fn sender_evicts_after_500ms() {
        let mut sender = NackSender::new();
        let now = Instant::now();

        sender.on_send(
            CachedPacket {
                seq: 10,
                data: vec![1],
                timestamp_ms: 0,
            },
            now,
        );

        let later = now + Duration::from_millis(501);
        let found = sender.on_nack(&[10], later);
        assert!(found.is_empty(), "packet should be evicted after 500 ms");
    }

    #[test]
    fn receiver_detects_gap_and_nacks() {
        let mut recv = NackReceiver::new(ms(33), ms(20));
        let now = Instant::now();

        recv.on_packet(0, now);
        recv.on_packet(2, now); // gap: 1 is missing

        // Immediately tick — gap is too fresh.
        let actions = recv.tick(now);
        assert!(actions.is_empty());

        // After frame_interval, should NACK.
        let later = now + ms(40);
        let actions = recv.tick(later);
        assert_eq!(actions.len(), 1);
        assert!(matches!(actions[0], NackAction::Nack { ref seqs } if seqs == &[1]));
    }

    #[test]
    fn receiver_uses_pli_when_rtt_is_high() {
        let mut recv = NackReceiver::new(ms(33), ms(100));
        let now = Instant::now();

        recv.on_packet(0, now);
        recv.on_packet(2, now); // gap: 1 is missing

        let later = now + ms(40);
        let actions = recv.tick(later);
        assert_eq!(actions.len(), 1);
        assert_eq!(actions[0], NackAction::PictureLossIndication);
    }

    #[test]
    fn receiver_backoff_respects_2x_rtt() {
        let mut recv = NackReceiver::new(ms(33), ms(20));
        let now = Instant::now();

        recv.on_packet(0, now);
        recv.on_packet(2, now); // gap: 1 is missing

        let later = now + ms(40);
        let actions = recv.tick(later);
        assert!(matches!(actions[0], NackAction::Nack { .. }));

        // Tick again immediately — should be in backoff.
        let actions2 = recv.tick(later);
        assert!(actions2.is_empty(), "should not re-nack within 2*RTT");

        // After backoff expires, should NACK again.
        let much_later = later + ms(50); // 2*RTT = 40ms
        let actions3 = recv.tick(much_later);
        assert!(matches!(actions3[0], NackAction::Nack { .. }));
    }

    #[test]
    fn receiver_late_packet_fills_gap() {
        let mut recv = NackReceiver::new(ms(33), ms(20));
        let now = Instant::now();

        recv.on_packet(0, now);
        recv.on_packet(2, now); // gap: 1 is missing

        let later = now + ms(40);
        let actions = recv.tick(later);
        assert!(matches!(actions[0], NackAction::Nack { .. }));

        // Late arrival of packet 1
        recv.on_packet(1, later);
        let actions2 = recv.tick(later + ms(1));
        assert!(
            actions2.is_empty()
                || !matches!(actions2[0], NackAction::Nack { seqs: ref s } if s.contains(&1)),
            "filled gap should not be nacked again"
        );
    }

    #[test]
    fn receiver_rate_cap_falls_back_to_pli() {
        let mut recv = NackReceiver::new(ms(33), ms(20));
        let now = Instant::now();

        // Create many gaps.
        recv.on_packet(0, now);
        recv.on_packet(100, now); // gaps 1..99

        let later = now + ms(40);
        let actions = recv.tick(later);

        // Either we got a Nack with <= max_nack_rate seqs, or we got PLI.
        match actions.first() {
            Some(NackAction::Nack { seqs }) => {
                assert!(
                    seqs.len() as u32 <= NackReceiver::DEFAULT_MAX_NACK_RATE,
                    "rate cap exceeded"
                );
            }
            Some(NackAction::PictureLossIndication) => {}
            _ => panic!("expected an action"),
        }
    }

    #[test]
    fn receiver_wraparound_ok() {
        let mut recv = NackReceiver::new(ms(33), ms(20));
        let now = Instant::now();

        recv.on_packet(u32::MAX, now);
        recv.on_packet(1, now); // gap: 0 is missing (wrap)

        let later = now + ms(40);
        let actions = recv.tick(later);
        assert!(matches!(actions[0], NackAction::Nack { ref seqs } if seqs == &[0]));
    }
}
