//! Prometheus metrics for the WZP relay daemon.

use prometheus::{
    Encoder, GaugeVec, Histogram, HistogramOpts, IntCounter, IntCounterVec, IntGauge, IntGaugeVec,
    Opts, Registry, TextEncoder,
};
use wzp_proto::packet::QualityReport;
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
    // Federation metrics
    pub federation_peer_status: IntGaugeVec,
    pub federation_peer_rtt_ms: GaugeVec,
    pub federation_packets_forwarded: IntCounterVec,
    pub federation_packets_deduped: IntCounter,
    pub federation_packets_rate_limited: IntCounter,
    pub federation_active_rooms: IntGauge,
    // Per-session metrics
    pub session_buffer_depth: IntGaugeVec,
    pub session_loss_pct: GaugeVec,
    pub session_rtt_ms: GaugeVec,
    pub session_underruns: IntCounterVec,
    pub session_overruns: IntCounterVec,
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

        let federation_peer_status = IntGaugeVec::new(
            Opts::new("wzp_federation_peer_status", "Peer connection status (0=disconnected, 1=connected)"),
            &["peer"],
        ).expect("metric");
        let federation_peer_rtt_ms = GaugeVec::new(
            Opts::new("wzp_federation_peer_rtt_ms", "QUIC RTT to federated peer in milliseconds"),
            &["peer"],
        ).expect("metric");
        let federation_packets_forwarded = IntCounterVec::new(
            Opts::new("wzp_federation_packets_forwarded_total", "Packets forwarded to/from federated peers"),
            &["peer", "direction"],
        ).expect("metric");
        let federation_packets_deduped = IntCounter::with_opts(
            Opts::new("wzp_federation_packets_deduped_total", "Duplicate federation packets dropped"),
        ).expect("metric");
        let federation_packets_rate_limited = IntCounter::with_opts(
            Opts::new("wzp_federation_packets_rate_limited_total", "Federation packets dropped by rate limiter"),
        ).expect("metric");
        let federation_active_rooms = IntGauge::with_opts(
            Opts::new("wzp_federation_active_rooms", "Number of federated rooms currently active"),
        ).expect("metric");

        let session_buffer_depth = IntGaugeVec::new(
            Opts::new(
                "wzp_relay_session_jitter_buffer_depth",
                "Buffer depth per session",
            ),
            &["session_id"],
        )
        .expect("metric");
        let session_loss_pct = GaugeVec::new(
            Opts::new(
                "wzp_relay_session_loss_pct",
                "Packet loss percentage per session",
            ),
            &["session_id"],
        )
        .expect("metric");
        let session_rtt_ms = GaugeVec::new(
            Opts::new(
                "wzp_relay_session_rtt_ms",
                "Round-trip time per session",
            ),
            &["session_id"],
        )
        .expect("metric");
        let session_underruns = IntCounterVec::new(
            Opts::new(
                "wzp_relay_session_underruns_total",
                "Jitter buffer underruns per session",
            ),
            &["session_id"],
        )
        .expect("metric");
        let session_overruns = IntCounterVec::new(
            Opts::new(
                "wzp_relay_session_overruns_total",
                "Jitter buffer overruns per session",
            ),
            &["session_id"],
        )
        .expect("metric");

        registry.register(Box::new(active_sessions.clone())).expect("register");
        registry.register(Box::new(active_rooms.clone())).expect("register");
        registry.register(Box::new(packets_forwarded.clone())).expect("register");
        registry.register(Box::new(bytes_forwarded.clone())).expect("register");
        registry.register(Box::new(auth_attempts.clone())).expect("register");
        registry.register(Box::new(handshake_duration.clone())).expect("register");
        registry.register(Box::new(federation_peer_status.clone())).expect("register");
        registry.register(Box::new(federation_peer_rtt_ms.clone())).expect("register");
        registry.register(Box::new(federation_packets_forwarded.clone())).expect("register");
        registry.register(Box::new(federation_packets_deduped.clone())).expect("register");
        registry.register(Box::new(federation_packets_rate_limited.clone())).expect("register");
        registry.register(Box::new(federation_active_rooms.clone())).expect("register");
        registry.register(Box::new(session_buffer_depth.clone())).expect("register");
        registry.register(Box::new(session_loss_pct.clone())).expect("register");
        registry.register(Box::new(session_rtt_ms.clone())).expect("register");
        registry.register(Box::new(session_underruns.clone())).expect("register");
        registry.register(Box::new(session_overruns.clone())).expect("register");

        Self {
            active_sessions,
            active_rooms,
            packets_forwarded,
            bytes_forwarded,
            auth_attempts,
            handshake_duration,
            federation_peer_status,
            federation_peer_rtt_ms,
            federation_packets_forwarded,
            federation_packets_deduped,
            federation_packets_rate_limited,
            federation_active_rooms,
            session_buffer_depth,
            session_loss_pct,
            session_rtt_ms,
            session_underruns,
            session_overruns,
            registry,
        }
    }

    /// Update per-session quality metrics from a QualityReport.
    pub fn update_session_quality(&self, session_id: &str, report: &QualityReport) {
        self.session_loss_pct
            .with_label_values(&[session_id])
            .set(report.loss_percent() as f64);
        self.session_rtt_ms
            .with_label_values(&[session_id])
            .set(report.rtt_ms() as f64);
    }

    /// Update per-session buffer metrics.
    pub fn update_session_buffer(
        &self,
        session_id: &str,
        depth: usize,
        underruns: u64,
        overruns: u64,
    ) {
        self.session_buffer_depth
            .with_label_values(&[session_id])
            .set(depth as i64);
        // IntCounterVec doesn't have a `set` — we inc by the delta.
        // Since these are cumulative from the jitter buffer, we use inc_by
        // with the current totals. To avoid double-counting, callers should
        // track previous values externally. For simplicity the relay reports
        // the absolute value each tick; counters only go up so we take the
        // max(0, new - current) approach.
        let cur_underruns = self
            .session_underruns
            .with_label_values(&[session_id])
            .get();
        if underruns > cur_underruns as u64 {
            self.session_underruns
                .with_label_values(&[session_id])
                .inc_by(underruns - cur_underruns as u64);
        }
        let cur_overruns = self
            .session_overruns
            .with_label_values(&[session_id])
            .get();
        if overruns > cur_overruns as u64 {
            self.session_overruns
                .with_label_values(&[session_id])
                .inc_by(overruns - cur_overruns as u64);
        }
    }

    /// Remove all per-session label values for a disconnected session.
    pub fn remove_session_metrics(&self, session_id: &str) {
        let _ = self.session_buffer_depth.remove_label_values(&[session_id]);
        let _ = self.session_loss_pct.remove_label_values(&[session_id]);
        let _ = self.session_rtt_ms.remove_label_values(&[session_id]);
        let _ = self.session_underruns.remove_label_values(&[session_id]);
        let _ = self.session_overruns.remove_label_values(&[session_id]);
    }

    /// Get a reference to the underlying Prometheus registry.
    /// Probe metrics are registered on this same registry so they appear in /metrics output.
    pub fn registry(&self) -> &Registry {
        &self.registry
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

/// Start an HTTP server serving GET /metrics, GET /mesh, presence, and route endpoints on the given port.
pub async fn serve_metrics(
    port: u16,
    metrics: Arc<RelayMetrics>,
    presence: Option<Arc<tokio::sync::Mutex<crate::presence::PresenceRegistry>>>,
    route_resolver: Option<Arc<crate::route::RouteResolver>>,
) {
    use axum::{extract::Path, routing::get, Router};

    let metrics_clone = metrics.clone();
    let presence_all = presence.clone();
    let presence_lookup = presence.clone();
    let presence_peers = presence.clone();
    let presence_route = presence;

    let app = Router::new()
        .route(
            "/metrics",
            get(move || {
                let m = metrics.clone();
                async move { m.metrics_handler() }
            }),
        )
        .route(
            "/mesh",
            get(move || {
                let m = metrics_clone.clone();
                async move { crate::probe::mesh_summary(m.registry()) }
            }),
        )
        .route(
            "/presence",
            get(move || {
                let reg = presence_all.clone();
                async move {
                    match reg {
                        Some(r) => {
                            let r = r.lock().await;
                            let entries: Vec<serde_json::Value> = r.all_known().into_iter().map(|(fp, loc)| {
                                serde_json::json!({ "fingerprint": fp, "location": loc })
                            }).collect();
                            serde_json::to_string_pretty(&entries).unwrap_or_else(|_| "[]".to_string())
                        }
                        None => "[]".to_string(),
                    }
                }
            }),
        )
        .route(
            "/presence/:fingerprint",
            get(move |Path(fingerprint): Path<String>| {
                let reg = presence_lookup.clone();
                async move {
                    match reg {
                        Some(r) => {
                            let r = r.lock().await;
                            match r.lookup(&fingerprint) {
                                Some(loc) => serde_json::to_string_pretty(
                                    &serde_json::json!({ "fingerprint": fingerprint, "location": loc })
                                ).unwrap_or_else(|_| "{}".to_string()),
                                None => serde_json::json!({ "fingerprint": fingerprint, "location": null }).to_string(),
                            }
                        }
                        None => serde_json::json!({ "fingerprint": fingerprint, "location": null }).to_string(),
                    }
                }
            }),
        )
        .route(
            "/peers",
            get(move || {
                let reg = presence_peers.clone();
                async move {
                    match reg {
                        Some(r) => {
                            let r = r.lock().await;
                            let peers: Vec<serde_json::Value> = r.peers().iter().map(|(addr, peer)| {
                                serde_json::json!({
                                    "addr": addr.to_string(),
                                    "fingerprints": peer.fingerprints.iter().collect::<Vec<_>>(),
                                    "rtt_ms": peer.rtt_ms,
                                })
                            }).collect();
                            serde_json::to_string_pretty(&peers).unwrap_or_else(|_| "[]".to_string())
                        }
                        None => "[]".to_string(),
                    }
                }
            }),
        )
        .route(
            "/route/:fingerprint",
            get(move |Path(fingerprint): Path<String>| {
                let reg = presence_route.clone();
                let resolver = route_resolver.clone();
                async move {
                    match (reg, resolver) {
                        (Some(r), Some(res)) => {
                            let r = r.lock().await;
                            let route = res.resolve(&r, &fingerprint);
                            let json = res.route_json(&fingerprint, &route);
                            serde_json::to_string_pretty(&json)
                                .unwrap_or_else(|_| "{}".to_string())
                        }
                        _ => {
                            serde_json::json!({
                                "fingerprint": fingerprint,
                                "route": "not_found",
                                "relay_chain": [],
                            })
                            .to_string()
                        }
                    }
                }
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
    fn session_quality_update() {
        let m = RelayMetrics::new();
        let report = QualityReport {
            loss_pct: 128,   // ~50%
            rtt_4ms: 25,     // 100ms
            jitter_ms: 10,
            bitrate_cap_kbps: 200,
        };
        m.update_session_quality("sess-abc", &report);

        let output = m.metrics_handler();
        assert!(output.contains("wzp_relay_session_loss_pct{session_id=\"sess-abc\"}"));
        assert!(output.contains("wzp_relay_session_rtt_ms{session_id=\"sess-abc\"}"));
        // Verify rtt value (25 * 4 = 100)
        assert!(output.contains("wzp_relay_session_rtt_ms{session_id=\"sess-abc\"} 100"));
    }

    #[test]
    fn session_metrics_cleanup() {
        let m = RelayMetrics::new();
        let report = QualityReport {
            loss_pct: 50,
            rtt_4ms: 10,
            jitter_ms: 5,
            bitrate_cap_kbps: 100,
        };
        m.update_session_quality("sess-cleanup", &report);
        m.update_session_buffer("sess-cleanup", 42, 3, 1);

        // Verify they appear
        let output = m.metrics_handler();
        assert!(output.contains("sess-cleanup"));

        // Remove and verify they are gone
        m.remove_session_metrics("sess-cleanup");
        let output = m.metrics_handler();
        assert!(!output.contains("sess-cleanup"));
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
