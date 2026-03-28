//! Prometheus metrics for the WZP web bridge.

use prometheus::{
    Encoder, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, Opts, Registry,
    TextEncoder,
};

/// Holds all Prometheus metrics for the web bridge.
#[derive(Clone)]
pub struct WebMetrics {
    pub active_connections: IntGauge,
    pub frames_bridged: IntCounterVec,
    pub auth_failures: IntCounter,
    pub handshake_latency: Histogram,
    registry: Registry,
}

impl WebMetrics {
    /// Create and register all web bridge metrics.
    pub fn new() -> Self {
        let registry = Registry::new();

        let active_connections = IntGauge::with_opts(
            Opts::new("wzp_web_active_connections", "Current WebSocket connections"),
        )
        .expect("metric");
        registry
            .register(Box::new(active_connections.clone()))
            .expect("register");

        let frames_bridged = IntCounterVec::new(
            Opts::new("wzp_web_frames_bridged_total", "Audio frames bridged"),
            &["direction"],
        )
        .expect("metric");
        registry
            .register(Box::new(frames_bridged.clone()))
            .expect("register");

        let auth_failures = IntCounter::with_opts(
            Opts::new("wzp_web_auth_failures_total", "Browser auth failures"),
        )
        .expect("metric");
        registry
            .register(Box::new(auth_failures.clone()))
            .expect("register");

        let handshake_latency = Histogram::with_opts(
            HistogramOpts::new(
                "wzp_web_handshake_latency_seconds",
                "Relay handshake time",
            )
            .buckets(vec![0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0]),
        )
        .expect("metric");
        registry
            .register(Box::new(handshake_latency.clone()))
            .expect("register");

        Self {
            active_connections,
            frames_bridged,
            auth_failures,
            handshake_latency,
            registry,
        }
    }

    /// Encode all metrics as Prometheus text exposition format.
    pub fn gather(&self) -> String {
        let encoder = TextEncoder::new();
        let metric_families = self.registry.gather();
        let mut buf = Vec::new();
        encoder.encode(&metric_families, &mut buf).unwrap();
        String::from_utf8(buf).unwrap()
    }
}

/// Axum handler that returns Prometheus text metrics.
pub async fn metrics_handler(
    axum::extract::State(state): axum::extract::State<super::AppState>,
) -> String {
    state.metrics.gather()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn web_metrics_register() {
        let m = WebMetrics::new();
        // Touch CounterVec labels so they appear in output
        m.frames_bridged.with_label_values(&["up"]);
        m.frames_bridged.with_label_values(&["down"]);
        let output = m.gather();
        assert!(
            output.contains("wzp_web_active_connections"),
            "missing active_connections"
        );
        assert!(
            output.contains("wzp_web_frames_bridged_total"),
            "missing frames_bridged"
        );
        assert!(
            output.contains("wzp_web_auth_failures_total"),
            "missing auth_failures"
        );
        assert!(
            output.contains("wzp_web_handshake_latency_seconds"),
            "missing handshake_latency"
        );
    }

    #[test]
    fn web_metrics_track_connections() {
        let m = WebMetrics::new();
        assert_eq!(m.active_connections.get(), 0);

        m.active_connections.inc();
        m.active_connections.inc();
        assert_eq!(m.active_connections.get(), 2);

        m.active_connections.dec();
        assert_eq!(m.active_connections.get(), 1);

        let output = m.gather();
        assert!(output.contains("wzp_web_active_connections 1"));
    }
}
