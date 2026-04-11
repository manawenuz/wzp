//! Multi-relay NAT reflection ("STUN for QUIC" — Phase 2).
//!
//! Phase 1 (`SignalMessage::Reflect` / `ReflectResponse`) lets a
//! client ask a single relay "what source address do you see for
//! me?". Phase 2 queries N relays in parallel and classifies the
//! results into a NAT type so the future P2P hole-punching path
//! can decide whether a direct QUIC handshake is viable:
//!
//! - All relays return the same `(ip, port)` → **Cone NAT**.
//!   Endpoint-independent mapping, P2P hole-punching viable,
//!   `consensus_addr` is the one address to advertise.
//! - Same ip, different ports → **Symmetric port-dependent NAT**.
//!   The mapping changes per destination, so the advertised addr
//!   wouldn't match what a peer actually sees; fall back to
//!   relay-mediated path.
//! - Different ips → multi-homed / anycast / broken DNS, treat as
//!   `Multiple` and do not attempt P2P.
//! - 0 or 1 successful probes → `Unknown`, not enough data.
//!
//! A probe is a throwaway QUIC signal connection: open endpoint,
//! connect, RegisterPresence (with a zero identity — the relay
//! accepts this exactly like the main signaling path does), send
//! Reflect, read ReflectResponse, close. Each probe gets its own
//! ephemeral quinn::Endpoint so the OS assigns a fresh source port
//! per relay — if we shared one endpoint across probes, a
//! symmetric NAT in front of the client would map every probe to
//! the same port and we couldn't detect it.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;
use wzp_proto::{MediaTransport, SignalMessage};
use wzp_transport::{client_config, create_endpoint, QuinnTransport};

/// Result of one probe against one relay. Always returned so the
/// UI can render per-relay status even when some fail.
#[derive(Debug, Clone, Serialize)]
pub struct NatProbeResult {
    pub relay_name: String,
    pub relay_addr: String,
    /// `Some` on successful probe, `None` on failure.
    pub observed_addr: Option<String>,
    /// End-to-end wall-clock from connect start to ReflectResponse
    /// received, in milliseconds. `Some` only on success.
    pub latency_ms: Option<u32>,
    /// Human-readable error on failure.
    pub error: Option<String>,
}

/// Aggregated classification over N `NatProbeResult`s.
#[derive(Debug, Clone, Serialize)]
pub struct NatDetection {
    pub probes: Vec<NatProbeResult>,
    pub nat_type: NatType,
    /// When `nat_type == Cone`, the one address all probes agreed
    /// on. `None` for every other case.
    pub consensus_addr: Option<String>,
}

/// NAT classification. See module doc for semantics.
#[derive(Debug, Clone, Copy, Serialize, PartialEq, Eq)]
pub enum NatType {
    Cone,
    SymmetricPort,
    Multiple,
    Unknown,
}

/// Probe a single relay with a throwaway QUIC connection.
///
/// Each call creates a fresh `quinn::Endpoint` so the OS hands out a
/// fresh ephemeral source port — essential for NAT-type detection
/// because a shared socket would produce the same mapping against
/// every relay and mask symmetric NAT.
pub async fn probe_reflect_addr(
    relay: SocketAddr,
    timeout_ms: u64,
) -> Result<(SocketAddr, u32), String> {
    // Install rustls provider idempotently — a second install on the
    // same thread is a no-op.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let endpoint = create_endpoint(bind, None).map_err(|e| format!("endpoint: {e}"))?;

    let start = Instant::now();
    let probe = async {
        // Open the signal connection.
        let conn =
            wzp_transport::connect(&endpoint, relay, "_signal", client_config())
                .await
                .map_err(|e| format!("connect: {e}"))?;
        let transport = QuinnTransport::new(conn);

        // The relay signal handler waits for a RegisterPresence
        // before entering its main dispatch loop (see
        // wzp-relay/src/main.rs). So a transient probe has to
        // register with a zero identity first — the relay accepts
        // the empty-signature form exactly as the main signaling
        // path does in desktop/src-tauri/src/lib.rs register_signal.
        transport
            .send_signal(&SignalMessage::RegisterPresence {
                identity_pub: [0u8; 32],
                signature: vec![],
                alias: None,
            })
            .await
            .map_err(|e| format!("send RegisterPresence: {e}"))?;
        // Drain the RegisterPresenceAck so the response to our
        // Reflect doesn't land on an unexpected stream order.
        match transport.recv_signal().await {
            Ok(Some(SignalMessage::RegisterPresenceAck { success: true, .. })) => {}
            Ok(Some(other)) => {
                return Err(format!(
                    "unexpected pre-reflect signal: {:?}",
                    std::mem::discriminant(&other)
                ));
            }
            Ok(None) => return Err("connection closed before RegisterPresenceAck".into()),
            Err(e) => return Err(format!("recv RegisterPresenceAck: {e}")),
        }

        // Send Reflect and await response.
        transport
            .send_signal(&SignalMessage::Reflect)
            .await
            .map_err(|e| format!("send Reflect: {e}"))?;

        match transport.recv_signal().await {
            Ok(Some(SignalMessage::ReflectResponse { observed_addr })) => {
                let parsed: SocketAddr = observed_addr
                    .parse()
                    .map_err(|e| format!("parse observed_addr {observed_addr:?}: {e}"))?;
                let latency_ms = start.elapsed().as_millis() as u32;

                // Clean close so the relay's per-connection cleanup
                // runs promptly and we don't leak file descriptors.
                let _ = transport.close().await;

                Ok((parsed, latency_ms))
            }
            Ok(Some(other)) => Err(format!(
                "expected ReflectResponse, got {:?}",
                std::mem::discriminant(&other)
            )),
            Ok(None) => Err("connection closed before ReflectResponse".into()),
            Err(e) => Err(format!("recv ReflectResponse: {e}")),
        }
    };

    let out = tokio::time::timeout(Duration::from_millis(timeout_ms), probe)
        .await
        .map_err(|_| format!("probe timeout ({timeout_ms}ms)"))??;

    // Drop the endpoint explicitly AFTER the probe finishes so the
    // UDP socket is released before we return.
    drop(endpoint);
    Ok(out)
}

/// Detect the client's NAT type by probing N relays in parallel and
/// classifying the returned addresses. Never errors — failing
/// probes surface via `NatProbeResult.error`; aggregate is always
/// returned.
pub async fn detect_nat_type(
    relays: Vec<(String, SocketAddr)>,
    timeout_ms: u64,
) -> NatDetection {
    // Parallel probes via tokio::task::JoinSet so the wall-clock is
    // bounded by the slowest probe, not the sum. JoinSet keeps the
    // dep surface at just tokio — we already depend on it.
    let mut set = tokio::task::JoinSet::new();
    for (name, addr) in relays {
        set.spawn(async move {
            let result = probe_reflect_addr(addr, timeout_ms).await;
            (name, addr, result)
        });
    }

    let mut probes = Vec::new();
    while let Some(join_result) = set.join_next().await {
        let (name, addr, result) = match join_result {
            Ok(tuple) => tuple,
            // Task panicked — surface as a synthetic failed probe so
            // the aggregate still returns a reasonable shape. This
            // shouldn't happen but we don't want one bad probe to
            // poison the whole detection.
            Err(join_err) => {
                probes.push(NatProbeResult {
                    relay_name: "<panicked>".into(),
                    relay_addr: "unknown".into(),
                    observed_addr: None,
                    latency_ms: None,
                    error: Some(format!("probe task panicked: {join_err}")),
                });
                continue;
            }
        };
        probes.push(match result {
            Ok((observed, latency_ms)) => NatProbeResult {
                relay_name: name,
                relay_addr: addr.to_string(),
                observed_addr: Some(observed.to_string()),
                latency_ms: Some(latency_ms),
                error: None,
            },
            Err(e) => NatProbeResult {
                relay_name: name,
                relay_addr: addr.to_string(),
                observed_addr: None,
                latency_ms: None,
                error: Some(e),
            },
        });
    }

    let (nat_type, consensus_addr) = classify_nat(&probes);
    NatDetection {
        probes,
        nat_type,
        consensus_addr,
    }
}

/// Pure-function NAT classifier — split out for unit testing
/// without touching the network.
pub fn classify_nat(probes: &[NatProbeResult]) -> (NatType, Option<String>) {
    let successes: Vec<SocketAddr> = probes
        .iter()
        .filter_map(|p| p.observed_addr.as_deref().and_then(|s| s.parse().ok()))
        .collect();

    if successes.len() < 2 {
        return (NatType::Unknown, None);
    }

    let first = successes[0];
    let same_ip = successes.iter().all(|a| a.ip() == first.ip());
    if !same_ip {
        return (NatType::Multiple, None);
    }

    let same_port = successes.iter().all(|a| a.port() == first.port());
    if same_port {
        (NatType::Cone, Some(first.to_string()))
    } else {
        (NatType::SymmetricPort, None)
    }
}

// ── Unit tests for the pure classifier ───────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    fn mk(addr: Option<&str>) -> NatProbeResult {
        NatProbeResult {
            relay_name: "test".into(),
            relay_addr: "0.0.0.0:0".into(),
            observed_addr: addr.map(|s| s.to_string()),
            latency_ms: addr.map(|_| 10),
            error: None,
        }
    }

    #[test]
    fn classify_empty_is_unknown() {
        let (nt, addr) = classify_nat(&[]);
        assert_eq!(nt, NatType::Unknown);
        assert!(addr.is_none());
    }

    #[test]
    fn classify_single_success_is_unknown() {
        let probes = vec![mk(Some("192.0.2.1:4433"))];
        let (nt, addr) = classify_nat(&probes);
        assert_eq!(nt, NatType::Unknown);
        assert!(addr.is_none());
    }

    #[test]
    fn classify_two_identical_is_cone() {
        let probes = vec![
            mk(Some("192.0.2.1:4433")),
            mk(Some("192.0.2.1:4433")),
        ];
        let (nt, addr) = classify_nat(&probes);
        assert_eq!(nt, NatType::Cone);
        assert_eq!(addr.as_deref(), Some("192.0.2.1:4433"));
    }

    #[test]
    fn classify_same_ip_different_ports_is_symmetric() {
        let probes = vec![
            mk(Some("192.0.2.1:4433")),
            mk(Some("192.0.2.1:51234")),
        ];
        let (nt, addr) = classify_nat(&probes);
        assert_eq!(nt, NatType::SymmetricPort);
        assert!(addr.is_none());
    }

    #[test]
    fn classify_different_ips_is_multiple() {
        let probes = vec![
            mk(Some("192.0.2.1:4433")),
            mk(Some("198.51.100.9:4433")),
        ];
        let (nt, addr) = classify_nat(&probes);
        assert_eq!(nt, NatType::Multiple);
        assert!(addr.is_none());
    }

    #[test]
    fn classify_mix_of_success_and_failure() {
        let probes = vec![
            mk(Some("192.0.2.1:4433")),
            mk(None), // failed probe
            mk(Some("192.0.2.1:4433")),
        ];
        let (nt, addr) = classify_nat(&probes);
        // Two successes both agree → Cone, ignore the failure row.
        assert_eq!(nt, NatType::Cone);
        assert_eq!(addr.as_deref(), Some("192.0.2.1:4433"));
    }

    #[test]
    fn classify_one_success_one_failure_is_unknown() {
        let probes = vec![mk(Some("192.0.2.1:4433")), mk(None)];
        let (nt, addr) = classify_nat(&probes);
        assert_eq!(nt, NatType::Unknown);
        assert!(addr.is_none());
    }
}
