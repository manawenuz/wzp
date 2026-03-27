//! Network path quality estimation using EWMA smoothing.
//!
//! Tracks packet loss (via sequence number gaps), RTT, jitter, and bandwidth.

use wzp_proto::PathQuality;

/// EWMA smoothing factor.
const ALPHA: f64 = 0.1;

/// Monitors network path quality metrics.
pub struct PathMonitor {
    /// EWMA-smoothed loss percentage (0.0 - 100.0).
    loss_ewma: f64,
    /// EWMA-smoothed RTT in milliseconds.
    rtt_ewma: f64,
    /// EWMA-smoothed jitter (RTT variance) in milliseconds.
    jitter_ewma: f64,
    /// Total bytes observed for bandwidth estimation.
    bytes_sent: u64,
    bytes_received: u64,
    /// Timestamps for bandwidth calculation.
    first_send_time_ms: Option<u64>,
    last_send_time_ms: Option<u64>,
    first_recv_time_ms: Option<u64>,
    last_recv_time_ms: Option<u64>,
    /// Sequence tracking for loss detection.
    highest_sent_seq: Option<u16>,
    total_sent: u64,
    total_received: u64,
    /// Last observed RTT for jitter calculation.
    last_rtt_ms: Option<f64>,
    /// Whether we have any observations yet.
    initialized: bool,
}

impl PathMonitor {
    /// Create a new path monitor with default (zero) initial values.
    pub fn new() -> Self {
        Self {
            loss_ewma: 0.0,
            rtt_ewma: 0.0,
            jitter_ewma: 0.0,
            bytes_sent: 0,
            bytes_received: 0,
            first_send_time_ms: None,
            last_send_time_ms: None,
            first_recv_time_ms: None,
            last_recv_time_ms: None,
            highest_sent_seq: None,
            total_sent: 0,
            total_received: 0,
            last_rtt_ms: None,
            initialized: false,
        }
    }

    /// Record that we sent a packet with the given sequence number and timestamp.
    pub fn observe_sent(&mut self, seq: u16, timestamp_ms: u64) {
        self.total_sent += 1;
        self.highest_sent_seq = Some(seq);

        if self.first_send_time_ms.is_none() {
            self.first_send_time_ms = Some(timestamp_ms);
        }
        self.last_send_time_ms = Some(timestamp_ms);

        // Estimate ~100 bytes per packet for bandwidth calculation
        self.bytes_sent += 100;
    }

    /// Record that we received a packet with the given sequence number and timestamp.
    pub fn observe_received(&mut self, seq: u16, timestamp_ms: u64) {
        self.total_received += 1;

        if self.first_recv_time_ms.is_none() {
            self.first_recv_time_ms = Some(timestamp_ms);
        }
        self.last_recv_time_ms = Some(timestamp_ms);

        self.bytes_received += 100;

        // Estimate loss from sequence gaps.
        // After we've sent some packets, compute instantaneous loss.
        if self.total_sent > 0 {
            let expected = self.total_sent;
            let received = self.total_received;
            let inst_loss = if expected > received {
                ((expected - received) as f64 / expected as f64) * 100.0
            } else {
                0.0
            };

            if !self.initialized {
                self.loss_ewma = inst_loss;
                self.initialized = true;
            } else {
                self.loss_ewma = ALPHA * inst_loss + (1.0 - ALPHA) * self.loss_ewma;
            }
        }

        let _ = seq; // seq used implicitly via total counts
    }

    /// Record an RTT observation in milliseconds.
    pub fn observe_rtt(&mut self, rtt_ms: u32) {
        let rtt = rtt_ms as f64;

        // Update jitter (difference from last RTT, smoothed)
        if let Some(last_rtt) = self.last_rtt_ms {
            let diff = (rtt - last_rtt).abs();
            if self.jitter_ewma == 0.0 {
                self.jitter_ewma = diff;
            } else {
                self.jitter_ewma = ALPHA * diff + (1.0 - ALPHA) * self.jitter_ewma;
            }
        }
        self.last_rtt_ms = Some(rtt);

        // Update RTT EWMA
        if self.rtt_ewma == 0.0 {
            self.rtt_ewma = rtt;
        } else {
            self.rtt_ewma = ALPHA * rtt + (1.0 - ALPHA) * self.rtt_ewma;
        }
    }

    /// Get the current estimated path quality.
    pub fn quality(&self) -> PathQuality {
        let bandwidth_kbps = self.estimate_bandwidth_kbps();

        PathQuality {
            loss_pct: self.loss_ewma as f32,
            rtt_ms: self.rtt_ewma as u32,
            jitter_ms: self.jitter_ewma as u32,
            bandwidth_kbps,
        }
    }

    /// Estimate bandwidth in kbps from bytes received over time.
    fn estimate_bandwidth_kbps(&self) -> u32 {
        if let (Some(first), Some(last)) = (self.first_recv_time_ms, self.last_recv_time_ms) {
            let duration_ms = last.saturating_sub(first);
            if duration_ms > 0 {
                // bytes_received * 8 bits / duration_ms * 1000 ms/s / 1000 bits/kbit
                let bits = self.bytes_received * 8;
                let kbps = bits as f64 / duration_ms as f64;
                return kbps as u32;
            }
        }
        0
    }
}

impl Default for PathMonitor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn initial_quality_is_zero() {
        let monitor = PathMonitor::new();
        let q = monitor.quality();
        assert_eq!(q.loss_pct, 0.0);
        assert_eq!(q.rtt_ms, 0);
        assert_eq!(q.jitter_ms, 0);
        assert_eq!(q.bandwidth_kbps, 0);
    }

    #[test]
    fn rtt_ewma_smoothing() {
        let mut monitor = PathMonitor::new();

        // First observation sets the initial value
        monitor.observe_rtt(100);
        let q = monitor.quality();
        assert_eq!(q.rtt_ms, 100);

        // Second observation should be smoothed: 0.1 * 200 + 0.9 * 100 = 110
        monitor.observe_rtt(200);
        let q = monitor.quality();
        assert_eq!(q.rtt_ms, 110);

        // Third: 0.1 * 200 + 0.9 * 110 = 119
        monitor.observe_rtt(200);
        let q = monitor.quality();
        assert_eq!(q.rtt_ms, 119);
    }

    #[test]
    fn jitter_from_rtt_variance() {
        let mut monitor = PathMonitor::new();

        monitor.observe_rtt(100);
        // No jitter yet (only one observation)
        assert_eq!(monitor.quality().jitter_ms, 0);

        monitor.observe_rtt(150);
        // Jitter = |150 - 100| = 50 (first jitter observation, sets directly)
        assert_eq!(monitor.quality().jitter_ms, 50);

        monitor.observe_rtt(140);
        // diff = |140 - 150| = 10
        // jitter = 0.1 * 10 + 0.9 * 50 = 46
        assert_eq!(monitor.quality().jitter_ms, 46);
    }

    #[test]
    fn detect_packet_loss_from_gaps() {
        let mut monitor = PathMonitor::new();

        // Send 10 packets
        for i in 0..10 {
            monitor.observe_sent(i, i as u64 * 20);
        }

        // Receive only 7 of them (30% loss)
        for i in [0u16, 1, 2, 3, 5, 7, 9] {
            monitor.observe_received(i, i as u64 * 20 + 50);
        }

        let q = monitor.quality();
        // After 7 observations, the EWMA should converge towards 30%
        // The exact value depends on the EWMA progression
        assert!(q.loss_pct > 0.0, "should detect some loss");
        assert!(q.loss_pct < 100.0, "loss should be reasonable");
    }

    #[test]
    fn bandwidth_estimation() {
        let mut monitor = PathMonitor::new();

        // Receive 100 packets over 1000ms, each ~100 bytes
        for i in 0..100 {
            monitor.observe_received(i, i as u64 * 10);
            monitor.observe_sent(i, i as u64 * 10);
        }

        let q = monitor.quality();
        // 100 packets * 100 bytes * 8 bits / 990ms ~= 80.8 kbps
        assert!(q.bandwidth_kbps > 0, "should estimate non-zero bandwidth");
    }

    #[test]
    fn no_loss_when_all_received() {
        let mut monitor = PathMonitor::new();

        for i in 0..20 {
            monitor.observe_sent(i, i as u64 * 20);
            monitor.observe_received(i, i as u64 * 20 + 30);
        }

        let q = monitor.quality();
        assert!(
            q.loss_pct < 1.0,
            "loss should be near zero when all packets received"
        );
    }
}
