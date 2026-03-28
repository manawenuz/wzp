//! Inter-relay health probe.
//!
//! A `ProbeRunner` maintains a persistent QUIC connection to a peer relay,
//! sends 1 Ping/s, and measures RTT, loss, and jitter. Results are exported
//! as Prometheus gauges with a `target` label.

use std::collections::VecDeque;
use std::net::SocketAddr;
use std::sync::Arc;
use std::time::{Duration, SystemTime, UNIX_EPOCH};

use prometheus::{Gauge, IntGauge, Opts, Registry};
use tokio::sync::Mutex;
use tracing::{error, info, warn};

use wzp_proto::{MediaTransport, SignalMessage};

/// Configuration for a single probe target.
#[derive(Clone, Debug)]
pub struct ProbeConfig {
    pub target: SocketAddr,
    pub interval: Duration,
}

impl ProbeConfig {
    pub fn new(target: SocketAddr) -> Self {
        Self {
            target,
            interval: Duration::from_secs(1),
        }
    }
}

/// Prometheus metrics for one probe target.
pub struct ProbeMetrics {
    pub rtt_ms: Gauge,
    pub loss_pct: Gauge,
    pub jitter_ms: Gauge,
    pub up: IntGauge,
}

impl ProbeMetrics {
    /// Register probe metrics with the given `target` label value.
    pub fn register(target: &str, registry: &Registry) -> Self {
        let rtt_ms = Gauge::with_opts(
            Opts::new("wzp_probe_rtt_ms", "RTT to peer relay in ms")
                .const_label("target", target),
        )
        .expect("probe metric");

        let loss_pct = Gauge::with_opts(
            Opts::new("wzp_probe_loss_pct", "Packet loss to peer relay in %")
                .const_label("target", target),
        )
        .expect("probe metric");

        let jitter_ms = Gauge::with_opts(
            Opts::new("wzp_probe_jitter_ms", "Jitter to peer relay in ms")
                .const_label("target", target),
        )
        .expect("probe metric");

        let up = IntGauge::with_opts(
            Opts::new("wzp_probe_up", "1 if peer relay is reachable, 0 if not")
                .const_label("target", target),
        )
        .expect("probe metric");

        registry.register(Box::new(rtt_ms.clone())).expect("register");
        registry.register(Box::new(loss_pct.clone())).expect("register");
        registry.register(Box::new(jitter_ms.clone())).expect("register");
        registry.register(Box::new(up.clone())).expect("register");

        Self {
            rtt_ms,
            loss_pct,
            jitter_ms,
            up,
        }
    }
}

/// Sliding window for tracking probe results over the last N pings.
pub struct SlidingWindow {
    /// Capacity (number of pings to track).
    capacity: usize,
    /// Timestamps of sent pings (ms since epoch) in order.
    sent: VecDeque<u64>,
    /// RTT values for received pongs (ms). None = no pong received yet.
    rtts: VecDeque<Option<f64>>,
}

impl SlidingWindow {
    pub fn new(capacity: usize) -> Self {
        Self {
            capacity,
            sent: VecDeque::with_capacity(capacity),
            rtts: VecDeque::with_capacity(capacity),
        }
    }

    /// Record a sent ping.
    pub fn record_sent(&mut self, timestamp_ms: u64) {
        if self.sent.len() >= self.capacity {
            self.sent.pop_front();
            self.rtts.pop_front();
        }
        self.sent.push_back(timestamp_ms);
        self.rtts.push_back(None);
    }

    /// Record a received pong. Returns the computed RTT in ms, or None if
    /// the timestamp doesn't match any pending ping.
    pub fn record_pong(&mut self, timestamp_ms: u64, now_ms: u64) -> Option<f64> {
        // Find the sent ping with this timestamp
        for (i, &sent_ts) in self.sent.iter().enumerate() {
            if sent_ts == timestamp_ms {
                let rtt = (now_ms as f64) - (sent_ts as f64);
                self.rtts[i] = Some(rtt);
                return Some(rtt);
            }
        }
        None
    }

    /// Compute loss percentage (0.0-100.0) from the current window.
    /// A ping is considered lost if it has no matching pong.
    pub fn loss_pct(&self) -> f64 {
        if self.sent.is_empty() {
            return 0.0;
        }
        let total = self.rtts.len() as f64;
        let lost = self.rtts.iter().filter(|r| r.is_none()).count() as f64;
        (lost / total) * 100.0
    }

    /// Compute jitter as the standard deviation of RTT values (ms).
    /// Only considers pings that received a pong.
    pub fn jitter_ms(&self) -> f64 {
        let rtts: Vec<f64> = self.rtts.iter().filter_map(|r| *r).collect();
        if rtts.len() < 2 {
            return 0.0;
        }
        let mean = rtts.iter().sum::<f64>() / rtts.len() as f64;
        let variance = rtts.iter().map(|r| (r - mean).powi(2)).sum::<f64>() / rtts.len() as f64;
        variance.sqrt()
    }

    /// Return the most recent RTT value, if any.
    pub fn latest_rtt(&self) -> Option<f64> {
        self.rtts.iter().rev().find_map(|r| *r)
    }
}

/// Runs a health probe against a single peer relay.
pub struct ProbeRunner {
    config: ProbeConfig,
    metrics: ProbeMetrics,
}

impl ProbeRunner {
    /// Create a new probe runner, registering metrics with the given registry.
    pub fn new(config: ProbeConfig, registry: &Registry) -> Self {
        let target_str = config.target.to_string();
        let metrics = ProbeMetrics::register(&target_str, registry);
        Self { config, metrics }
    }

    /// Run the probe forever. This function never returns under normal operation.
    /// It connects to the target relay, sends Ping every `interval`, and processes
    /// Pong replies to compute RTT, loss, and jitter.
    pub async fn run(&self) -> ! {
        loop {
            info!(target = %self.config.target, "probe connecting...");
            match self.run_session().await {
                Ok(()) => {
                    // Session ended cleanly (shouldn't happen in practice)
                    warn!(target = %self.config.target, "probe session ended, reconnecting in 5s");
                }
                Err(e) => {
                    error!(target = %self.config.target, "probe session error: {e}, reconnecting in 5s");
                }
            }
            self.metrics.up.set(0);
            self.metrics.rtt_ms.set(0.0);
            tokio::time::sleep(Duration::from_secs(5)).await;
        }
    }

    /// Run one probe session (one QUIC connection). Returns when the connection drops.
    async fn run_session(&self) -> anyhow::Result<()> {
        // Create a client-only endpoint on an ephemeral port
        let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
        let client_cfg = wzp_transport::client_config();
        let conn = wzp_transport::connect(
            &endpoint,
            self.config.target,
            "_probe",
            client_cfg,
        )
        .await?;

        let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));
        self.metrics.up.set(1);
        info!(target = %self.config.target, "probe connected");

        let window = Arc::new(Mutex::new(SlidingWindow::new(60)));

        // Spawn recv task for pong messages
        let recv_transport = transport.clone();
        let recv_window = window.clone();
        let rtt_gauge = self.metrics.rtt_ms.clone();
        let loss_gauge = self.metrics.loss_pct.clone();
        let jitter_gauge = self.metrics.jitter_ms.clone();
        let up_gauge = self.metrics.up.clone();

        let recv_handle = tokio::spawn(async move {
            loop {
                match recv_transport.recv_signal().await {
                    Ok(Some(SignalMessage::Pong { timestamp_ms })) => {
                        let now_ms = SystemTime::now()
                            .duration_since(UNIX_EPOCH)
                            .unwrap()
                            .as_millis() as u64;
                        let mut w = recv_window.lock().await;
                        if let Some(rtt) = w.record_pong(timestamp_ms, now_ms) {
                            rtt_gauge.set(rtt);
                        }
                        loss_gauge.set(w.loss_pct());
                        jitter_gauge.set(w.jitter_ms());
                    }
                    Ok(Some(_)) => {
                        // Ignore non-Pong signals
                    }
                    Ok(None) => {
                        info!("probe recv: connection closed");
                        up_gauge.set(0);
                        break;
                    }
                    Err(e) => {
                        error!("probe recv error: {e}");
                        up_gauge.set(0);
                        break;
                    }
                }
            }
        });

        // Send ping loop
        let mut interval = tokio::time::interval(self.config.interval);
        loop {
            interval.tick().await;

            if recv_handle.is_finished() {
                // Recv task died — connection is lost
                return Ok(());
            }

            let timestamp_ms = SystemTime::now()
                .duration_since(UNIX_EPOCH)
                .unwrap()
                .as_millis() as u64;

            {
                let mut w = window.lock().await;
                w.record_sent(timestamp_ms);
            }

            if let Err(e) = transport
                .send_signal(&SignalMessage::Ping { timestamp_ms })
                .await
            {
                error!(target = %self.config.target, "probe ping send error: {e}");
                recv_handle.abort();
                return Err(e.into());
            }
        }
    }
}

/// Coordinates multiple `ProbeRunner` instances for mesh mode.
///
/// Each relay probes all configured peers concurrently. The `ProbeMesh` owns the
/// runners and spawns them as independent tokio tasks.
pub struct ProbeMesh {
    runners: Vec<ProbeRunner>,
}

impl ProbeMesh {
    /// Create a new mesh coordinator, registering metrics for every target.
    pub fn new(targets: Vec<SocketAddr>, registry: &Registry) -> Self {
        let runners = targets
            .into_iter()
            .map(|addr| {
                let config = ProbeConfig::new(addr);
                ProbeRunner::new(config, registry)
            })
            .collect();
        Self { runners }
    }

    /// Spawn all runners as concurrent tokio tasks. This consumes the mesh.
    pub async fn run_all(self) {
        let mut handles = Vec::with_capacity(self.runners.len());
        for runner in self.runners {
            let target = runner.config.target;
            info!(target = %target, "spawning mesh probe");
            handles.push(tokio::spawn(async move { runner.run().await }));
        }
        // Probes run forever; if we ever need to wait:
        for h in handles {
            let _ = h.await;
        }
    }

    /// Number of probe targets in this mesh.
    pub fn target_count(&self) -> usize {
        self.runners.len()
    }
}

/// Build a human-readable mesh health table from probe metrics in the registry.
///
/// Scans the registry for `wzp_probe_*` gauges and formats them into a table.
pub fn mesh_summary(registry: &Registry) -> String {
    use std::collections::BTreeMap;

    let families = registry.gather();

    // Collect per-target values: target -> (rtt, loss, jitter, up)
    let mut targets: BTreeMap<String, (f64, f64, f64, bool)> = BTreeMap::new();

    for family in &families {
        let name = family.get_name();
        for metric in family.get_metric() {
            // Find the "target" label
            let target_label = metric
                .get_label()
                .iter()
                .find(|l| l.get_name() == "target");
            let target = match target_label {
                Some(l) => l.get_value().to_string(),
                None => continue,
            };

            let entry = targets.entry(target).or_insert((0.0, 0.0, 0.0, false));

            match name {
                "wzp_probe_rtt_ms" => entry.0 = metric.get_gauge().get_value(),
                "wzp_probe_loss_pct" => entry.1 = metric.get_gauge().get_value(),
                "wzp_probe_jitter_ms" => entry.2 = metric.get_gauge().get_value(),
                "wzp_probe_up" => entry.3 = metric.get_gauge().get_value() as i64 == 1,
                _ => {}
            }
        }
    }

    let mut out = String::new();
    out.push_str("Relay Mesh Health\n");
    out.push_str("\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\u{2500}\n");
    out.push_str(&format!(
        "{:<20} {:>6} {:>6} {:>7}  {}\n",
        "Target", "RTT", "Loss", "Jitter", "Status"
    ));

    for (target, (rtt, loss, jitter, up)) in &targets {
        let status = if *up { "UP" } else { "DOWN" };
        out.push_str(&format!(
            "{:<20} {:>5.0}ms {:>5.1}% {:>5.0}ms  {}\n",
            target, rtt, loss, jitter, status
        ));
    }

    if targets.is_empty() {
        out.push_str("  (no probe targets configured)\n");
    }

    out
}

/// Handle an incoming Ping signal by replying with a Pong carrying the same timestamp.
/// Returns true if the message was a Ping and was handled, false otherwise.
pub async fn handle_ping(
    transport: &wzp_transport::QuinnTransport,
    msg: &SignalMessage,
) -> bool {
    if let SignalMessage::Ping { timestamp_ms } = msg {
        if let Err(e) = transport
            .send_signal(&SignalMessage::Pong {
                timestamp_ms: *timestamp_ms,
            })
            .await
        {
            warn!("failed to send Pong reply: {e}");
        }
        true
    } else {
        false
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use prometheus::Encoder;

    #[test]
    fn probe_metrics_register() {
        let registry = Registry::new();
        let _metrics = ProbeMetrics::register("127.0.0.1:4433", &registry);

        let encoder = prometheus::TextEncoder::new();
        let families = registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("wzp_probe_rtt_ms"), "missing wzp_probe_rtt_ms");
        assert!(output.contains("wzp_probe_loss_pct"), "missing wzp_probe_loss_pct");
        assert!(output.contains("wzp_probe_jitter_ms"), "missing wzp_probe_jitter_ms");
        assert!(output.contains("wzp_probe_up"), "missing wzp_probe_up");
        assert!(
            output.contains("target=\"127.0.0.1:4433\""),
            "missing target label"
        );
    }

    #[test]
    fn rtt_calculation() {
        let mut window = SlidingWindow::new(60);

        // Send a ping at t=1000
        window.record_sent(1000);
        // Receive pong at t=1050 => RTT = 50ms
        let rtt = window.record_pong(1000, 1050);
        assert_eq!(rtt, Some(50.0));

        // Send at t=2000, receive at t=2030 => RTT = 30ms
        window.record_sent(2000);
        let rtt = window.record_pong(2000, 2030);
        assert_eq!(rtt, Some(30.0));

        assert_eq!(window.latest_rtt(), Some(30.0));

        // Unknown timestamp returns None
        let rtt = window.record_pong(9999, 10000);
        assert!(rtt.is_none());
    }

    #[test]
    fn loss_calculation() {
        let mut window = SlidingWindow::new(10);

        // Send 10 pings
        for i in 0..10 {
            window.record_sent(i * 1000);
        }

        // Receive pongs for 7 out of 10 (miss indices 2, 5, 8)
        for i in 0..10u64 {
            if i == 2 || i == 5 || i == 8 {
                continue; // lost
            }
            window.record_pong(i * 1000, i * 1000 + 40);
        }

        // 3 out of 10 lost = 30%
        let loss = window.loss_pct();
        assert!((loss - 30.0).abs() < 0.01, "expected ~30%, got {loss}");
    }

    #[test]
    fn jitter_calculation() {
        let mut window = SlidingWindow::new(10);

        // Send 4 pings with known RTTs: 10, 20, 30, 40
        // Mean = 25, variance = ((15^2 + 5^2 + 5^2 + 15^2) / 4) = (225+25+25+225)/4 = 125
        // std dev = sqrt(125) ≈ 11.18
        let rtts = [10.0, 20.0, 30.0, 40.0];
        for (i, rtt) in rtts.iter().enumerate() {
            let sent = (i as u64) * 1000;
            window.record_sent(sent);
            window.record_pong(sent, sent + *rtt as u64);
        }

        let jitter = window.jitter_ms();
        assert!(
            (jitter - 11.18).abs() < 0.1,
            "expected jitter ~11.18ms, got {jitter}"
        );
    }

    #[test]
    fn sliding_window_eviction() {
        let mut window = SlidingWindow::new(5);

        // Fill window
        for i in 0..5 {
            window.record_sent(i * 1000);
        }
        assert_eq!(window.sent.len(), 5);

        // Add one more — oldest should be evicted
        window.record_sent(5000);
        assert_eq!(window.sent.len(), 5);
        assert_eq!(*window.sent.front().unwrap(), 1000);

        // All 5 are unanswered
        assert!((window.loss_pct() - 100.0).abs() < 0.01);
    }

    #[test]
    fn empty_window_edge_cases() {
        let window = SlidingWindow::new(60);
        assert_eq!(window.loss_pct(), 0.0);
        assert_eq!(window.jitter_ms(), 0.0);
        assert!(window.latest_rtt().is_none());
    }

    #[test]
    fn mesh_creates_runners() {
        let registry = Registry::new();
        let targets: Vec<SocketAddr> = vec![
            "127.0.0.1:4433".parse().unwrap(),
            "127.0.0.2:4433".parse().unwrap(),
            "127.0.0.3:4433".parse().unwrap(),
        ];
        let mesh = ProbeMesh::new(targets, &registry);
        assert_eq!(mesh.target_count(), 3);

        // Verify metrics were registered for each target
        let encoder = prometheus::TextEncoder::new();
        let families = registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&families, &mut buf).unwrap();
        let output = String::from_utf8(buf).unwrap();

        assert!(output.contains("target=\"127.0.0.1:4433\""));
        assert!(output.contains("target=\"127.0.0.2:4433\""));
        assert!(output.contains("target=\"127.0.0.3:4433\""));
    }

    #[test]
    fn mesh_summary_empty() {
        let registry = Registry::new();
        let summary = mesh_summary(&registry);

        // Should contain the header
        assert!(summary.contains("Relay Mesh Health"));
        assert!(summary.contains("Target"));
        assert!(summary.contains("RTT"));
        assert!(summary.contains("Loss"));
        assert!(summary.contains("Jitter"));
        assert!(summary.contains("Status"));
        // Should indicate no targets
        assert!(summary.contains("no probe targets configured"));
    }

    #[test]
    fn mesh_summary_with_targets() {
        let registry = Registry::new();
        // Register probe metrics for two targets and set values
        let m1 = ProbeMetrics::register("relay-b:4433", &registry);
        m1.rtt_ms.set(12.0);
        m1.loss_pct.set(0.0);
        m1.jitter_ms.set(2.0);
        m1.up.set(1);

        let m2 = ProbeMetrics::register("relay-c:4433", &registry);
        m2.rtt_ms.set(45.0);
        m2.loss_pct.set(0.1);
        m2.jitter_ms.set(5.0);
        m2.up.set(0);

        let summary = mesh_summary(&registry);

        assert!(summary.contains("relay-b:4433"));
        assert!(summary.contains("relay-c:4433"));
        assert!(summary.contains("UP"));
        assert!(summary.contains("DOWN"));
        // Should NOT contain "no probe targets"
        assert!(!summary.contains("no probe targets configured"));
    }

    #[test]
    fn mesh_zero_targets() {
        let registry = Registry::new();
        let mesh = ProbeMesh::new(vec![], &registry);
        assert_eq!(mesh.target_count(), 0);
    }
}
