//! Relay conformance metering — Tier A/B/C/D/E enforcement.
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
    /// Sustained payload size exceeds 2× the typical bound for the declared codec (Tier D).
    PayloadSizeExceeded,
    /// Per-session token-bucket rate cap exceeded (Tier E).
    RateCapExceeded,
}

/// Simple token bucket for per-session rate capping (Tier E).
///
/// Tokens represent bytes.  The bucket refills at `refill_per_sec` bytes per
/// second, up to `capacity`.  A packet is allowed only if the bucket holds
/// enough tokens for its size.
pub struct TokenBucket {
    capacity: u64,
    tokens: f64,
    refill_per_sec: u64,
    last_refill: Instant,
}

impl TokenBucket {
    /// Create a new bucket with the given byte capacity and refill rate.
    pub fn new(capacity: u64, refill_per_sec: u64) -> Self {
        Self {
            capacity,
            tokens: capacity as f64,
            refill_per_sec,
            last_refill: Instant::now(),
        }
    }

    /// Per-session audio cap: 256 kbps with 30 s @ 2× burst.
    /// Capacity = 30 s × 64 KB/s = 1_920_000 bytes.
    pub fn for_audio_session() -> Self {
        let refill_per_sec = 256_000 / 8; // 32_000 bytes/sec
        let capacity = refill_per_sec * 30 * 2; // 1_920_000 bytes
        Self::new(capacity, refill_per_sec)
    }

    /// Attempt to consume `bytes` from the bucket.
    ///
    /// Refills based on elapsed time since the last call, then deducts the
    /// cost.  Returns `Ok(())` if enough tokens were available, `Err(())`
    /// otherwise.
    pub fn try_consume(&mut self, bytes: u64, now: Instant) -> Result<(), ()> {
        let elapsed = now.duration_since(self.last_refill);
        self.last_refill = now;
        self.tokens += elapsed.as_secs_f64() * self.refill_per_sec as f64;
        if self.tokens > self.capacity as f64 {
            self.tokens = self.capacity as f64;
        }
        if self.tokens >= bytes as f64 {
            self.tokens -= bytes as f64;
            Ok(())
        } else {
            Err(())
        }
    }
}

/// Per-participant traffic conformance meter.
pub struct ConformanceMeter {
    window_start: Instant,
    bytes_in_window: u64,
    packets_in_window: u64,
    /// Rolling (seq, timestamp) pairs for drift detection.
    drift_window: VecDeque<(u32, u32)>,
    /// EWMA of payload size for Tier D sanity checks.
    ewma_payload_size: f64,
    /// Optional token bucket for Tier E per-session rate cap.
    token_bucket: Option<TokenBucket>,
}

impl ConformanceMeter {
    pub fn new() -> Self {
        Self {
            window_start: Instant::now(),
            bytes_in_window: 0,
            packets_in_window: 0,
            drift_window: VecDeque::with_capacity(DRIFT_WINDOW_SIZE),
            ewma_payload_size: 0.0,
            token_bucket: None,
        }
    }

    /// Create a meter with a Tier E token bucket for per-session rate capping.
    pub fn with_token_bucket(bucket: TokenBucket) -> Self {
        let mut meter = Self::new();
        meter.token_bucket = Some(bucket);
        meter
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

        // Tier D — payload-size sanity (EWMA).
        let alpha = 0.05; // ~20-packet smoothing
        self.ewma_payload_size =
            alpha * payload_len as f64 + (1.0 - alpha) * self.ewma_payload_size;
        let bound = payload_size_bound(header.codec_id);
        if self.ewma_payload_size > (bound * 2) as f64 {
            return Err(Violation::PayloadSizeExceeded);
        }

        // Tier E — per-session token-bucket rate cap.
        if let Some(ref mut bucket) = self.token_bucket {
            let packet_size = (MediaHeader::WIRE_SIZE + payload_len) as u64;
            if bucket.try_consume(packet_size, now).is_err() {
                return Err(Violation::RateCapExceeded);
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

/// Typical per-codec payload size bound in bytes (Tier D).
///
/// These are empirical upper bounds for a single audio frame at the codec's
/// nominal configuration.  The EWMA must not exceed 2× this value.
pub fn payload_size_bound(codec: CodecId) -> usize {
    match codec {
        CodecId::Opus64k => 320,
        CodecId::Opus48k => 240,
        CodecId::Opus32k => 200,
        CodecId::Opus24k => 160,
        CodecId::Opus16k => 100,
        CodecId::Opus6k => 90,
        CodecId::Codec2_3200 => 30,
        CodecId::Codec2_1200 => 30,
        CodecId::ComfortNoise => 16,
    }
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
        // Use 300-byte payloads (under Tier D 2× bound of 320 for Opus24k).
        let t0 = Instant::now();
        for _ in 0..32 {
            assert!(meter.observe(&header, 300, t0).is_ok());
        }
        // 32 * (header wire size + 300) ≈ 32 * 316 = 10_112 bytes < 10_350

        // Same packets 1.1 seconds later should be fine because the window
        // rolls over.
        let t1 = t0 + Duration::from_millis(1_100);
        for _ in 0..32 {
            assert!(meter.observe(&header, 300, t1).is_ok());
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

    // ------------------------------------------------------------------
    // Tier D — payload-size sanity
    // ------------------------------------------------------------------

    #[test]
    fn conformance_tier_d() {
        let mut meter = ConformanceMeter::new();
        let header = make_header(CodecId::Codec2_1200);
        let now = Instant::now();

        // Codec2_1200 bound = 30 bytes.  2× bound = 60 bytes.
        // Feed 1400-byte payloads — EWMA should cross 60 within a few packets.
        let mut flagged = false;
        for _ in 0..200 {
            if meter.observe(&header, 1400, now).is_err() {
                flagged = true;
                break;
            }
        }
        assert!(
            flagged,
            "expected PayloadSizeExceeded for 1400-byte Codec2_1200 payloads"
        );
    }

    #[test]
    fn payload_size_normal_stays_within_bound() {
        let mut meter = ConformanceMeter::new();
        let header = make_header(CodecId::Opus24k);
        let now = Instant::now();

        // Opus24k bound = 160 bytes.  2× bound = 320 bytes.
        // Feed 150-byte payloads — well within the 2× limit.
        // Limit to 10 packets so the 1-second bitrate window (10_350 bytes)
        // is not exhausted: 10 * (16 + 150) = 1_660 < 10_350.
        for _ in 0..10 {
            assert!(
                meter.observe(&header, 150, now).is_ok(),
                "150-byte Opus24k payloads should stay within Tier D limit"
            );
        }
    }

    // ------------------------------------------------------------------
    // Tier E — token-bucket rate cap
    // ------------------------------------------------------------------

    #[test]
    fn token_bucket_small_burst_ok() {
        let mut bucket = TokenBucket::new(100_000, 32_000);
        let now = Instant::now();
        // 50 KB burst fits inside 100 KB capacity.
        assert!(bucket.try_consume(50_000, now).is_ok());
    }

    #[test]
    fn token_bucket_large_burst_fails() {
        let mut bucket = TokenBucket::new(100_000, 32_000);
        let now = Instant::now();
        // 1 MB exceeds 100 KB capacity.
        assert!(bucket.try_consume(1_000_000, now).is_err());
    }

    #[test]
    fn token_bucket_refills_over_time() {
        let mut bucket = TokenBucket::new(100_000, 32_000);
        let t0 = Instant::now();
        // Drain the bucket.
        assert!(bucket.try_consume(100_000, t0).is_ok());
        // Immediately try again — should fail.
        assert!(bucket.try_consume(10_000, t0).is_err());
        // Wait 1 second — bucket refills 32_000 bytes.
        let t1 = t0 + Duration::from_secs(1);
        assert!(bucket.try_consume(30_000, t1).is_ok());
        // 40_000 is more than the 32_000 refilled.
        assert!(bucket.try_consume(40_000, t1).is_err());
    }

    #[test]
    fn token_bucket_sustained_rate_balanced() {
        let mut bucket = TokenBucket::new(1_000_000, 32_000);
        let t0 = Instant::now();
        // Send 32 KB every second for 5 seconds — exactly at refill rate.
        // The bucket should never empty because each second it refills
        // exactly what was consumed.
        for i in 0..5 {
            let t = t0 + Duration::from_secs(i);
            assert!(
                bucket.try_consume(32_000, t).is_ok(),
                "32 KB/s sustained should stay within bucket limit"
            );
        }
    }

    #[test]
    fn conformance_tier_e_integration() {
        // Use Opus64k (high bitrate ceiling + high payload bound) so Tiers
        // A/B/D never fire on the small bursts used here.  Only Tier E.
        let mut meter = ConformanceMeter::with_token_bucket(TokenBucket::new(1_000, 500));
        let header = make_header(CodecId::Opus64k);
        let now = Instant::now();

        // Two 500-byte (wire) packets = 1_000 bytes — exactly the bucket cap.
        assert!(
            meter
                .observe(&header, 500 - MediaHeader::WIRE_SIZE, now)
                .is_ok()
        );
        assert!(
            meter
                .observe(&header, 500 - MediaHeader::WIRE_SIZE, now)
                .is_ok()
        );

        // Third packet exceeds the 1_000-byte cap.
        let result = meter.observe(&header, 10, now);
        assert_eq!(result, Err(Violation::RateCapExceeded));
    }
}
