//! Relay conformance metering — Tier A/B/C enforcement.
//!
//! Each participant gets a [`ConformanceMeter`] that tracks per-second
//! traffic against the declared codec's nominal bitrate ceiling.
//! Violations are logged and counted but do **not** drop packets
//! (observe-only mode).

use std::collections::VecDeque;
use std::time::{Duration, Instant};
use wzp_proto::{CodecId, MediaHeader};

/// Rolling window size for timestamp-drift detection (Tier C).
const DRIFT_WINDOW_SIZE: usize = 200;

/// Kinds of conformance violation detected by the relay.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Violation {
    /// Cumulative bitrate in the current 1 s window exceeds the Tier A ceiling.
    BitrateExceeded,
    /// Packet rate exceeds the per-codec safety limit (Tier B).
    PacketRateExceeded,
    /// Timestamp jumped backwards or forwards suspiciously (Tier C).
    TimestampDrift,
}

/// Per-participant traffic conformance meter.
pub struct ConformanceMeter {
    window_start: Instant,
    bytes_in_window: u64,
    packets_in_window: u64,
    /// Rolling (seq, timestamp) pairs for drift detection.
    drift_window: VecDeque<(u32, u32)>,
}

impl ConformanceMeter {
    pub fn new() -> Self {
        Self {
            window_start: Instant::now(),
            bytes_in_window: 0,
            packets_in_window: 0,
            drift_window: VecDeque::with_capacity(DRIFT_WINDOW_SIZE),
        }
    }

    /// Inspect an incoming media packet and accumulate it against the
    /// current 1-second window. Returns [`Err(Violation)`] when a limit
    /// is crossed.
    pub fn observe(
        &mut self,
        header: &MediaHeader,
        payload_len: usize,
        now: Instant,
    ) -> Result<(), Violation> {
        // Roll the window forward if a second has elapsed.
        if now.duration_since(self.window_start) >= Duration::from_secs(1) {
            self.window_start = now;
            self.bytes_in_window = 0;
            self.packets_in_window = 0;
        }

        let packet_size = (MediaHeader::WIRE_SIZE + payload_len) as u64;
        self.bytes_in_window += packet_size;
        self.packets_in_window += 1;

        // Tier A — bitrate ceiling.
        let ceiling = ceiling_bps(header.codec_id);
        let max_bytes_per_sec = ceiling / 8;
        if self.bytes_in_window > max_bytes_per_sec {
            return Err(Violation::BitrateExceeded);
        }

        // Tier B — packet-rate ceiling.
        let max_pps = max_pps(header.codec_id);
        let pps_threshold = (max_pps as f32 * 1.5) as u64;
        if self.packets_in_window > pps_threshold {
            return Err(Violation::PacketRateExceeded);
        }

        // Tier C — timestamp drift.
        self.drift_window.push_back((header.seq, header.timestamp));
        if self.drift_window.len() > DRIFT_WINDOW_SIZE {
            self.drift_window.pop_front();
        }
        if self.drift_window.len() >= 2 {
            let (first_seq, first_ts) = self.drift_window.front().copied().unwrap();
            let (last_seq, last_ts) = self.drift_window.back().copied().unwrap();

            let ds = last_seq.wrapping_sub(first_seq) as f64;
            let dt = last_ts.wrapping_sub(first_ts) as f64;

            if ds > 0.0 {
                let avg_ms_per_packet = dt / ds;
                let frame_ms = header.codec_id.frame_duration_ms() as f64;
                let min_ratio = frame_ms * 0.5;
                let max_ratio = frame_ms * 2.0;
                if avg_ms_per_packet < min_ratio || avg_ms_per_packet > max_ratio {
                    return Err(Violation::TimestampDrift);
                }
            }
        }

        Ok(())
    }
}

impl Default for ConformanceMeter {
    fn default() -> Self {
        Self::new()
    }
}

/// Compute the Tier A bitrate ceiling for a given codec.
///
/// Formula:
///   nominal_bitrate * 3 (FEC 2.0 overhead) * 115 / 100 (15% safety margin)
/// with a floor of 2 kbps.
pub fn ceiling_bps(codec: CodecId) -> u64 {
    let nominal = codec.bitrate_bps() as u64;
    (nominal * 3 * 115 / 100).max(2_000)
}

/// Compute the Tier B packet-rate ceiling for a given codec.
///
/// Formula:
///   1000 / frame_duration_ms * 3 (FEC overhead factor)
pub fn max_pps(codec: CodecId) -> u32 {
    let fd = codec.frame_duration_ms() as u32;
    if fd == 0 {
        return 0;
    }
    (1000 / fd) * 3
}

#[cfg(test)]
mod tests {
    use super::*;
    use wzp_proto::MediaType;

    fn make_header(codec_id: CodecId) -> MediaHeader {
        MediaHeader {
            version: 2,
            flags: 0,
            media_type: MediaType::Audio,
            codec_id,
            seq: 0,
            timestamp: 0,
            fec_block: 0,
            stream_id: 0,
            fec_ratio: 0,
        }
    }

    fn make_header_with_seq_ts(codec_id: CodecId, seq: u32, timestamp: u32) -> MediaHeader {
        MediaHeader {
            version: 2,
            flags: 0,
            media_type: MediaType::Audio,
            codec_id,
            seq,
            timestamp,
            fec_block: 0,
            stream_id: 0,
            fec_ratio: 0,
        }
    }

    #[test]
    fn bitrate_exceeded_for_opus24k() {
        let mut meter = ConformanceMeter::new();
        let header = make_header(CodecId::Opus24k);

        // Ceiling for Opus24k = 24_000 * 3 * 115 / 100 = 82_800 bps
        // = 10_350 bytes/sec.  1 MB/s = 125_000 bytes/packet will blow past
        // that in a single packet.
        let now = Instant::now();
        let result = meter.observe(&header, 1_000_000, now);
        assert_eq!(result, Err(Violation::BitrateExceeded));
    }

    #[test]
    fn small_packets_stay_within_ceiling() {
        let mut meter = ConformanceMeter::new();
        let header = make_header(CodecId::Opus24k);

        // Ceiling = 82_800 bps = 10_350 bytes/sec.
        // Each packet = 16-byte header + 80 bytes = 96 bytes.
        // 100 packets = 9_600 bytes < 10_350.
        let now = Instant::now();
        for _ in 0..100 {
            assert!(meter.observe(&header, 80, now).is_ok());
        }
    }

    #[test]
    fn window_resets_after_one_second() {
        let mut meter = ConformanceMeter::new();
        let header = make_header(CodecId::Opus24k);

        // Fill the window to just under the limit.
        let t0 = Instant::now();
        for _ in 0..10 {
            assert!(meter.observe(&header, 1000, t0).is_ok());
        }
        // 10 * (header wire size + 1000) ≈ 10 * 1034 = 10_340 bytes < 10_350

        // Same packets 1.1 seconds later should be fine because the window
        // rolls over.
        let t1 = t0 + Duration::from_millis(1_100);
        for _ in 0..10 {
            assert!(meter.observe(&header, 1000, t1).is_ok());
        }
    }

    #[test]
    fn ceiling_bps_floor() {
        // ComfortNoise has 0 nominal bitrate, so the floor kicks in.
        assert_eq!(ceiling_bps(CodecId::ComfortNoise), 2_000);
    }

    // ------------------------------------------------------------------
    // Tier B — packet rate
    // ------------------------------------------------------------------

    #[test]
    fn packet_rate_exceeded() {
        let mut meter = ConformanceMeter::new();
        // Opus24k: max_pps = 1000/20 * 3 = 150. Threshold = 150 * 1.5 = 225.
        let header = make_header(CodecId::Opus24k);
        let now = Instant::now();
        for _ in 0..225 {
            assert!(meter.observe(&header, 10, now).is_ok());
        }
        // 226th packet should trip the limit.
        assert_eq!(
            meter.observe(&header, 10, now),
            Err(Violation::PacketRateExceeded)
        );
    }

    #[test]
    fn packet_rate_within_limit() {
        let mut meter = ConformanceMeter::new();
        // Opus6k: max_pps = 1000/40 * 3 = 75. Threshold = 75 * 1.5 = 112.
        // Use 0-byte payload so bitrate ceiling (2_587 bytes/sec) is not the
        // limiting factor. 112 packets × 16 bytes = 1_792 bytes < 2_587.
        let header = make_header(CodecId::Opus6k);
        let now = Instant::now();
        for _ in 0..112 {
            assert!(meter.observe(&header, 0, now).is_ok());
        }
    }

    // ------------------------------------------------------------------
    // Tier C — timestamp drift
    // ------------------------------------------------------------------

    #[test]
    fn timestamp_drift_detected_when_too_fast() {
        let mut meter = ConformanceMeter::new();
        // Opus24k frame_duration = 20 ms.
        // Acceptable range: [10, 40] ms per packet.
        // Send packets with timestamp advancing by 5 ms each (too fast).
        let now = Instant::now();
        let mut drift_seen = false;
        for i in 0..200 {
            let header = make_header_with_seq_ts(CodecId::Opus24k, i, i * 5);
            match meter.observe(&header, 10, now) {
                Ok(()) => {}
                Err(Violation::TimestampDrift) => drift_seen = true,
                Err(other) => panic!("unexpected violation: {other:?}"),
            }
        }
        assert!(drift_seen, "expected TimestampDrift to be detected");
    }

    #[test]
    fn timestamp_drift_detected_when_too_slow() {
        let mut meter = ConformanceMeter::new();
        // Opus24k frame_duration = 20 ms.
        // Acceptable range: [10, 40] ms per packet.
        // Send packets with timestamp advancing by 50 ms each (too slow).
        let now = Instant::now();
        let mut drift_seen = false;
        for i in 0..200 {
            let header = make_header_with_seq_ts(CodecId::Opus24k, i, i * 50);
            match meter.observe(&header, 10, now) {
                Ok(()) => {}
                Err(Violation::TimestampDrift) => drift_seen = true,
                Err(other) => panic!("unexpected violation: {other:?}"),
            }
        }
        assert!(drift_seen, "expected TimestampDrift to be detected");
    }

    #[test]
    fn timestamp_normal_no_drift() {
        let mut meter = ConformanceMeter::new();
        // Opus24k frame_duration = 20 ms.
        // Send 200 packets with timestamp advancing by exactly 20 ms each.
        let now = Instant::now();
        for i in 0..200 {
            let header = make_header_with_seq_ts(CodecId::Opus24k, i, i * 20);
            assert!(meter.observe(&header, 10, now).is_ok());
        }
    }

    #[test]
    fn timestamp_drift_not_checked_before_two_packets() {
        let mut meter = ConformanceMeter::new();
        let now = Instant::now();
        // Single packet with wild timestamp — should not trigger drift.
        let header = make_header_with_seq_ts(CodecId::Opus24k, 0, 999_999);
        assert!(meter.observe(&header, 10, now).is_ok());
    }
}
