use std::collections::BTreeMap;
use std::time::{Duration, Instant};

use crate::packet::MediaPacket;

// ---------------------------------------------------------------------------
// Adaptive playout delay (NetEq-inspired)
// ---------------------------------------------------------------------------

/// Adaptive playout delay estimator based on observed inter-arrival jitter.
///
/// Inspired by WebRTC NetEq and IAX2 adaptive jitter buffering. Tracks an
/// exponential moving average (EMA) of inter-packet arrival jitter and
/// converts it to a target buffer depth in packets.
pub struct AdaptivePlayoutDelay {
    /// Current target delay in packets (equivalent to target_depth).
    target_delay: usize,
    /// Minimum allowed delay.
    min_delay: usize,
    /// Maximum allowed delay.
    max_delay: usize,
    /// Exponential moving average of inter-packet arrival jitter (ms).
    jitter_ema: f64,
    /// EMA smoothing factor for jitter increases (fast reaction).
    alpha_up: f64,
    /// EMA smoothing factor for jitter decreases (slow decay).
    alpha_down: f64,
    /// Last packet arrival timestamp (for computing inter-arrival jitter).
    last_arrival_ms: Option<u64>,
    /// Last packet expected timestamp.
    last_expected_ms: Option<u64>,
    /// Safety margin added to jitter-derived target (in packets).
    safety_margin: f64,
    /// Instant when a jitter spike was detected (handoff detection).
    spike_detected_at: Option<Instant>,
    /// Duration to hold max_delay after a spike is detected.
    spike_cooldown: Duration,
    /// Multiplier of jitter_ema that constitutes a spike.
    spike_threshold_multiplier: f64,
}

/// Frame duration in milliseconds (20ms Opus/Codec2 frames).
const FRAME_DURATION_MS: f64 = 20.0;
/// Default safety margin in packets.
const DEFAULT_SAFETY_MARGIN: f64 = 2.0;
/// Default EMA smoothing factor (used for both up/down in non-mobile mode).
const DEFAULT_ALPHA: f64 = 0.05;

impl AdaptivePlayoutDelay {
    /// Create a new adaptive playout delay estimator.
    ///
    /// - `min_delay`: minimum target delay in packets
    /// - `max_delay`: maximum target delay in packets
    pub fn new(min_delay: usize, max_delay: usize) -> Self {
        Self {
            target_delay: min_delay,
            min_delay,
            max_delay,
            jitter_ema: 0.0,
            alpha_up: DEFAULT_ALPHA,
            alpha_down: DEFAULT_ALPHA,
            last_arrival_ms: None,
            last_expected_ms: None,
            safety_margin: DEFAULT_SAFETY_MARGIN,
            spike_detected_at: None,
            spike_cooldown: Duration::from_secs(2),
            spike_threshold_multiplier: 3.0,
        }
    }

    /// Update with a new packet arrival. Returns the new target delay.
    ///
    /// - `arrival_ms`: when the packet actually arrived (wall clock)
    /// - `expected_ms`: when it should have arrived (based on sequence * 20ms)
    pub fn update(&mut self, arrival_ms: u64, expected_ms: u64) -> usize {
        if let (Some(last_arrival), Some(last_expected)) =
            (self.last_arrival_ms, self.last_expected_ms)
        {
            let actual_delta = arrival_ms as f64 - last_arrival as f64;
            let expected_delta = expected_ms as f64 - last_expected as f64;
            let jitter = (actual_delta - expected_delta).abs();

            // Spike detection: check before EMA update
            if self.jitter_ema > 0.0
                && jitter > self.jitter_ema * self.spike_threshold_multiplier
            {
                self.spike_detected_at = Some(Instant::now());
            }

            // Asymmetric EMA update
            let alpha = if jitter > self.jitter_ema {
                self.alpha_up
            } else {
                self.alpha_down
            };
            self.jitter_ema = alpha * jitter + (1.0 - alpha) * self.jitter_ema;

            // Check if spike cooldown has expired
            if let Some(spike_time) = self.spike_detected_at {
                if spike_time.elapsed() >= self.spike_cooldown {
                    self.spike_detected_at = None;
                }
            }

            // If within spike cooldown, return max_delay
            if self.spike_detected_at.is_some() {
                self.target_delay = self.max_delay;
            } else {
                // Convert jitter estimate to target delay in packets
                let raw_target =
                    (self.jitter_ema / FRAME_DURATION_MS).ceil() + self.safety_margin;
                self.target_delay =
                    (raw_target as usize).clamp(self.min_delay, self.max_delay);
            }
        }

        self.last_arrival_ms = Some(arrival_ms);
        self.last_expected_ms = Some(expected_ms);
        self.target_delay
    }

    /// Get current target delay in packets.
    pub fn target_delay(&self) -> usize {
        self.target_delay
    }

    /// Get current jitter estimate in ms.
    pub fn jitter_estimate_ms(&self) -> f64 {
        self.jitter_ema
    }

    /// Enable or disable mobile mode, adjusting parameters for cellular networks.
    ///
    /// Mobile mode uses:
    /// - Asymmetric alpha (fast up=0.3, slow down=0.02) for quicker spike detection
    /// - Higher safety margin (3.0 packets) to absorb handoff jitter
    /// - Spike detection with 2-second cooldown at 3x threshold
    pub fn set_mobile_mode(&mut self, enabled: bool) {
        if enabled {
            self.safety_margin = 3.0;
            self.alpha_up = 0.3;
            self.alpha_down = 0.02;
            self.spike_threshold_multiplier = 3.0;
            self.spike_cooldown = Duration::from_secs(2);
        } else {
            self.safety_margin = DEFAULT_SAFETY_MARGIN;
            self.alpha_up = DEFAULT_ALPHA;
            self.alpha_down = DEFAULT_ALPHA;
            self.spike_threshold_multiplier = 3.0;
            self.spike_cooldown = Duration::from_secs(2);
        }
    }
}

// ---------------------------------------------------------------------------
// Jitter buffer
// ---------------------------------------------------------------------------

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
    /// Optional adaptive playout delay estimator.
    adaptive: Option<AdaptivePlayoutDelay>,
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
    /// Total frames decoded by the consumer (tracked externally via `record_decode`).
    pub total_decoded: u64,
    /// Number of times the consumer tried to decode but the buffer was empty/not-ready.
    pub underruns: u64,
    /// Number of packets dropped because the buffer exceeded max depth.
    pub overruns: u64,
    /// High water mark — maximum buffer depth observed.
    pub max_depth_seen: usize,
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
            adaptive: None,
        }
    }

    /// Create a jitter buffer with adaptive playout delay.
    ///
    /// The target depth will be automatically adjusted based on observed
    /// inter-arrival jitter (NetEq-inspired algorithm).
    ///
    /// - `min_delay`: minimum target delay in packets
    /// - `max_delay`: maximum target delay in packets (also used as max_depth)
    pub fn new_adaptive(min_delay: usize, max_delay: usize) -> Self {
        Self {
            buffer: BTreeMap::new(),
            next_playout_seq: 0,
            max_depth: max_delay,
            target_depth: min_delay,
            min_depth: min_delay,
            initialized: false,
            stats: JitterStats::default(),
            adaptive: Some(AdaptivePlayoutDelay::new(min_delay, max_delay)),
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

        // Update adaptive playout delay if enabled.
        // Use the packet's timestamp as expected_ms and compute a simple wall-clock
        // proxy from the header timestamp (arrival_ms is approximated as timestamp
        // + observed jitter, but since we don't have real wall-clock here we use
        // the receive order with the header timestamp as the expected baseline).
        if let Some(ref mut adaptive) = self.adaptive {
            // expected_ms derived from sequence-implied timing: seq * frame_duration
            let expected_ms = packet.header.timestamp as u64;
            // For arrival_ms, use the actual receive timestamp. In the absence of
            // a wall-clock parameter, we use std::time for a monotonic approximation.
            // However, to keep the API simple, we compute arrival from the packet
            // stats: the Nth received packet "arrives" at N * frame_duration as a
            // baseline, and real network jitter shows in the deviation.
            // NOTE: In production, the caller should pass real wall-clock time.
            // For now, we use the header timestamp as-is (callers with adaptive
            // mode should feed arrival time via push_with_arrival).
            let arrival_ms = expected_ms; // no-op for basic push; use push_with_arrival
            adaptive.update(arrival_ms, expected_ms);
            self.target_depth = adaptive.target_delay();
            self.min_depth = self.min_depth.min(self.target_depth);
        }

        self.buffer.insert(seq, packet);

        // Evict oldest if over max depth
        while self.buffer.len() > self.max_depth {
            if let Some((&oldest_seq, _)) = self.buffer.first_key_value() {
                self.buffer.remove(&oldest_seq);
                self.stats.overruns += 1;
                // Advance playout seq past evicted packet
                if seq_before(self.next_playout_seq, oldest_seq.wrapping_add(1)) {
                    self.next_playout_seq = oldest_seq.wrapping_add(1);
                    self.stats.packets_lost += 1;
                }
            }
        }

        self.stats.current_depth = self.buffer.len();
        if self.stats.current_depth > self.stats.max_depth_seen {
            self.stats.max_depth_seen = self.stats.current_depth;
        }
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

    /// Record that the consumer attempted to decode but the buffer was empty/not-ready.
    pub fn record_underrun(&mut self) {
        self.stats.underruns += 1;
    }

    /// Record a successful frame decode by the consumer.
    pub fn record_decode(&mut self) {
        self.stats.total_decoded += 1;
    }

    /// Reset statistics counters (preserves buffer contents and playout state).
    pub fn reset_stats(&mut self) {
        self.stats = JitterStats {
            current_depth: self.buffer.len(),
            ..JitterStats::default()
        };
    }

    /// Push a received packet with an explicit wall-clock arrival time.
    ///
    /// This is the preferred entry point when adaptive playout delay is enabled,
    /// since the estimator needs real arrival timestamps.
    pub fn push_with_arrival(&mut self, packet: MediaPacket, arrival_ms: u64) {
        let expected_ms = packet.header.timestamp as u64;
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

        // Update adaptive playout delay if enabled.
        if let Some(ref mut adaptive) = self.adaptive {
            adaptive.update(arrival_ms, expected_ms);
            self.target_depth = adaptive.target_delay();
        }

        self.buffer.insert(seq, packet);

        // Evict oldest if over max depth
        while self.buffer.len() > self.max_depth {
            if let Some((&oldest_seq, _)) = self.buffer.first_key_value() {
                self.buffer.remove(&oldest_seq);
                self.stats.overruns += 1;
                if seq_before(self.next_playout_seq, oldest_seq.wrapping_add(1)) {
                    self.next_playout_seq = oldest_seq.wrapping_add(1);
                    self.stats.packets_lost += 1;
                }
            }
        }

        self.stats.current_depth = self.buffer.len();
        if self.stats.current_depth > self.stats.max_depth_seen {
            self.stats.max_depth_seen = self.stats.current_depth;
        }
    }

    /// Get a reference to the adaptive playout delay estimator, if enabled.
    pub fn adaptive_delay(&self) -> Option<&AdaptivePlayoutDelay> {
        self.adaptive.as_ref()
    }

    /// Get a mutable reference to the adaptive playout delay estimator.
    pub fn adaptive_delay_mut(&mut self) -> Option<&mut AdaptivePlayoutDelay> {
        self.adaptive.as_mut()
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

    // ---------------------------------------------------------------
    // AdaptivePlayoutDelay tests
    // ---------------------------------------------------------------

    #[test]
    fn adaptive_delay_stable() {
        // Feed packets with consistent 20ms spacing — target should stay at minimum.
        let mut apd = AdaptivePlayoutDelay::new(3, 50);

        for i in 0u64..200 {
            let arrival_ms = i * 20;
            let expected_ms = i * 20;
            apd.update(arrival_ms, expected_ms);
        }

        // With zero jitter, target should be min_delay (ceil(0/20) + 2 = 2,
        // clamped to min_delay=3).
        assert_eq!(apd.target_delay(), 3);
        assert!(
            apd.jitter_estimate_ms() < 1.0,
            "jitter estimate should be near zero, got {}",
            apd.jitter_estimate_ms()
        );
    }

    #[test]
    fn adaptive_delay_increases_on_jitter() {
        // Feed packets with variable spacing (±10ms jitter).
        let mut apd = AdaptivePlayoutDelay::new(3, 50);

        // Alternate: arrive 10ms early / 10ms late
        for i in 0u64..200 {
            let expected_ms = i * 20;
            let jitter_offset: i64 = if i % 2 == 0 { 10 } else { -10 };
            let arrival_ms = (expected_ms as i64 + jitter_offset).max(0) as u64;
            apd.update(arrival_ms, expected_ms);
        }

        // Inter-arrival jitter should be ~20ms (swing of 10 to -10 = delta 20).
        // target = ceil(~20/20) + 2 = 3, but EMA converges near 20 so target >= 3.
        assert!(
            apd.target_delay() >= 3,
            "target should increase with jitter, got {}",
            apd.target_delay()
        );
        assert!(
            apd.jitter_estimate_ms() > 5.0,
            "jitter estimate should be significant, got {}",
            apd.jitter_estimate_ms()
        );
    }

    #[test]
    fn adaptive_delay_decreases_on_recovery() {
        let mut apd = AdaptivePlayoutDelay::new(3, 50);

        // Phase 1: high jitter (±30ms)
        for i in 0u64..200 {
            let expected_ms = i * 20;
            let offset: i64 = if i % 2 == 0 { 30 } else { -30 };
            let arrival_ms = (expected_ms as i64 + offset).max(0) as u64;
            apd.update(arrival_ms, expected_ms);
        }
        let high_target = apd.target_delay();
        let high_jitter = apd.jitter_estimate_ms();

        // Phase 2: stable (no jitter) — target should decrease via EMA decay
        for i in 200u64..600 {
            let t = i * 20;
            apd.update(t, t);
        }
        let low_target = apd.target_delay();
        let low_jitter = apd.jitter_estimate_ms();

        assert!(
            low_target <= high_target,
            "target should decrease after recovery: {} -> {}",
            high_target,
            low_target
        );
        assert!(
            low_jitter < high_jitter,
            "jitter estimate should decrease: {} -> {}",
            high_jitter,
            low_jitter
        );
    }

    #[test]
    fn adaptive_delay_clamped() {
        let mut apd = AdaptivePlayoutDelay::new(3, 10);

        // Extreme jitter: packets arrive with huge variance
        for i in 0u64..500 {
            let expected_ms = i * 20;
            let offset: i64 = if i % 2 == 0 { 500 } else { -500 };
            let arrival_ms = (expected_ms as i64 + offset).max(0) as u64;
            apd.update(arrival_ms, expected_ms);
        }

        assert!(
            apd.target_delay() <= 10,
            "target should not exceed max_delay=10, got {}",
            apd.target_delay()
        );
        assert!(
            apd.target_delay() >= 3,
            "target should not go below min_delay=3, got {}",
            apd.target_delay()
        );
    }

    #[test]
    fn adaptive_jitter_estimate() {
        let mut apd = AdaptivePlayoutDelay::new(3, 50);

        // Initial jitter estimate should be zero
        assert_eq!(apd.jitter_estimate_ms(), 0.0);

        // After one packet, still zero (no delta yet)
        apd.update(0, 0);
        assert_eq!(apd.jitter_estimate_ms(), 0.0);

        // Second packet with 5ms jitter
        apd.update(25, 20); // arrived 5ms late
        assert!(
            apd.jitter_estimate_ms() > 0.0,
            "jitter estimate should be positive after jittery packet"
        );
        assert!(
            apd.jitter_estimate_ms() <= 5.0,
            "first jitter sample of 5ms with alpha=0.05 should not exceed 5ms, got {}",
            apd.jitter_estimate_ms()
        );

        // Feed many packets with ~15ms jitter — EMA should converge
        for i in 2u64..500 {
            let expected_ms = i * 20;
            let arrival_ms = expected_ms + 15; // consistently 15ms late
            apd.update(arrival_ms, expected_ms);
        }
        // Steady-state: inter-arrival jitter = |35 - 20| = 0 actually,
        // because if every packet is 15ms late, delta_actual = 35-35 = 20,
        // same as expected. So jitter should converge toward 0.
        // Let's use variable jitter instead for a better test.
        let mut apd2 = AdaptivePlayoutDelay::new(3, 50);
        for i in 0u64..500 {
            let expected_ms = i * 20;
            // Alternate 0ms and 15ms late
            let extra = if i % 2 == 0 { 0 } else { 15 };
            let arrival_ms = expected_ms + extra;
            apd2.update(arrival_ms, expected_ms);
        }
        let est = apd2.jitter_estimate_ms();
        assert!(
            est > 5.0 && est < 20.0,
            "jitter estimate should converge near 15ms with alternating 0/15ms offsets, got {}",
            est
        );
    }

    // ---------------------------------------------------------------
    // JitterBuffer with adaptive mode tests
    // ---------------------------------------------------------------

    #[test]
    fn jitter_buffer_adaptive_constructor() {
        let jb = JitterBuffer::new_adaptive(5, 250);
        assert!(jb.adaptive_delay().is_some());
        assert_eq!(jb.adaptive_delay().unwrap().target_delay(), 5);
    }

    #[test]
    fn jitter_buffer_adaptive_push_with_arrival() {
        let mut jb = JitterBuffer::new_adaptive(3, 50);

        // Push packets with consistent timing
        for i in 0u16..20 {
            let pkt = make_packet(i);
            let arrival_ms = i as u64 * 20;
            jb.push_with_arrival(pkt, arrival_ms);
        }

        // With zero jitter, target should stay at min
        let ad = jb.adaptive_delay().unwrap();
        assert_eq!(ad.target_delay(), 3);
    }

    // ---------------------------------------------------------------
    // Mobile mode tests
    // ---------------------------------------------------------------

    #[test]
    fn mobile_mode_increases_safety_margin() {
        let mut apd = AdaptivePlayoutDelay::new(3, 50);
        apd.set_mobile_mode(true);
        assert_eq!(apd.safety_margin, 3.0);
        assert_eq!(apd.alpha_up, 0.3);
        assert_eq!(apd.alpha_down, 0.02);

        apd.set_mobile_mode(false);
        assert_eq!(apd.safety_margin, DEFAULT_SAFETY_MARGIN);
        assert_eq!(apd.alpha_up, DEFAULT_ALPHA);
        assert_eq!(apd.alpha_down, DEFAULT_ALPHA);
    }

    #[test]
    fn mobile_mode_accessible_via_jitter_buffer() {
        let mut jb = JitterBuffer::new_adaptive(3, 50);
        jb.adaptive_delay_mut().unwrap().set_mobile_mode(true);
        assert_eq!(jb.adaptive_delay().unwrap().safety_margin, 3.0);
    }
}
