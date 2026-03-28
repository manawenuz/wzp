//! Prometheus metrics for the WZP relay daemon.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};
use std::sync::Arc;

/// All relay-level Prometheus metrics.
#[derive(Clone)]
pub struct RelayMetrics {
    pub active_sessions: IntGauge,
    pub active_rooms: IntGauge,
    pub packets_forwarded: IntCounter,
    pub bytes_forwarded: IntCounter,
    pub auth_attempts: IntCounterVec,
    pub handshake_duration: Histogram,
    registry: Registry,
}

impl RelayMetrics {
    /// Create and register all relay metrics with a new registry.
    pub fn new() -> Self {
        let registry = Registry::new();

        let active_sessions = IntGauge::with_opts(
            Opts::new("wzp_relay_active_sessions", "Current active sessions"),
        )
        .expect("metric");
        let active_rooms = IntGauge::with_opts(
            Opts::new("wzp_relay_active_rooms", "Current active rooms"),
        )
        .expect("metric");
        let packets_forwarded = IntCounter::with_opts(
            Opts::new("wzp_relay_packets_forwarded_total", "Total packets forwarded"),
        )
        .expect("metric");
        let bytes_forwarded = IntCounter::with_opts(
            Opts::new("wzp_relay_bytes_forwarded_total", "Total bytes forwarded"),
        )
        .expect("metric");
        let auth_attempts = IntCounterVec::new(
            Opts::new("wzp_relay_auth_attempts_total", "Auth validation attempts"),
            &["result"],
        )
        .expect("metric");
        let handshake_duration = Histogram::with_opts(
            HistogramOpts::new(
                "wzp_relay_handshake_duration_seconds",
                "Crypto handshake time",
            )
            .buckets(vec![0.001, 0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5]),
        )
        .expect("metric");

        registry.register(Box::new(active_sessions.clone())).expect("register");
        registry.register(Box::new(active_rooms.clone())).expect("register");
        registry.register(Box::new(packets_forwarded.clone())).expect("register");
        registry.register(Box::new(bytes_forwarded.clone())).expect("register");
        registry.register(Box::new(auth_attempts.clone())).expect("register");
        registry.register(Box::new(handshake_duration.clone())).expect("register");

        Self {
            active_sessions,
            active_rooms,
            packets_forwarded,
            bytes_forwarded,
            auth_attempts,
            handshake_duration,
            registry,
        }
    }

    /// Gather all metrics and encode them as Prometheus text format.
    pub fn metrics_handler(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buffer = Vec::new();
        encoder.encode(&metric_families, &mut buffer).expect("encode");
        String::from_utf8(buffer).expect("utf8")
    }
}

/// Start an HTTP server serving GET /metrics on the given port.
pub async fn serve_metrics(port: u16, metrics: Arc<RelayMetrics>) {
    use axum::{routing::get, Router};

    let app = Router::new().route(
        "/metrics",
        get(move || {
            let m = metrics.clone();
            async move { m.metrics_handler() }
        }),
    );

    let addr = std::net::SocketAddr::from(([0, 0, 0, 0], port));
    let listener = tokio::net::TcpListener::bind(addr)
        .await
        .expect("failed to bind metrics port");
    tracing::info!(%addr, "metrics endpoint serving");
    axum::serve(listener, app)
        .await
        .expect("metrics server error");
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn metrics_register() {
        let m = RelayMetrics::new();
        // Touch the CounterVec labels so they appear in output
        m.auth_attempts.with_label_values(&["ok"]);
        m.auth_attempts.with_label_values(&["fail"]);
        let output = m.metrics_handler();
        // Should contain all registered metric names (as HELP or TYPE lines)
        assert!(output.contains("wzp_relay_active_sessions"));
        assert!(output.contains("wzp_relay_active_rooms"));
        assert!(output.contains("wzp_relay_packets_forwarded_total"));
        assert!(output.contains("wzp_relay_bytes_forwarded_total"));
        assert!(output.contains("wzp_relay_auth_attempts_total"));
        assert!(output.contains("wzp_relay_handshake_duration_seconds"));
    }

    #[test]
    fn metrics_increment() {
        let m = RelayMetrics::new();

        m.active_sessions.set(5);
        m.active_rooms.set(2);
        m.packets_forwarded.inc_by(100);
        m.bytes_forwarded.inc_by(48000);
        m.auth_attempts.with_label_values(&["ok"]).inc();
        m.auth_attempts.with_label_values(&["fail"]).inc_by(3);
        m.handshake_duration.observe(0.042);

        let output = m.metrics_handler();
        assert!(output.contains("wzp_relay_active_sessions 5"));
        assert!(output.contains("wzp_relay_active_rooms 2"));
        assert!(output.contains("wzp_relay_packets_forwarded_total 100"));
        assert!(output.contains("wzp_relay_bytes_forwarded_total 48000"));
        assert!(output.contains("wzp_relay_auth_attempts_total{result=\"ok\"} 1"));
        assert!(output.contains("wzp_relay_auth_attempts_total{result=\"fail\"} 3"));
        assert!(output.contains("wzp_relay_handshake_duration_seconds_count 1"));
    }
}
