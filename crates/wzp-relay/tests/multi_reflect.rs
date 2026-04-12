//! Phase 2 integration tests for multi-relay NAT reflection
//! (PRD: .taskmaster/docs/prd_multi_relay_reflect.txt).
//!
//! These spin up one or two mock relays that implement the full
//! pre-reflect dance — RegisterPresence → RegisterPresenceAck →
//! Reflect → ReflectResponse — which is what the transient
//! probe helper in `wzp_client::reflect::probe_reflect_addr` does
//! against a real relay.
//!
//! Test matrix:
//!   1. `probe_reflect_addr_happy_path`
//!      — single mock relay, assert the probe helper returns the
//!        observed addr as 127.0.0.1:<client ephemeral port>
//!   2. `detect_nat_type_two_loopback_relays_is_cone`
//!      — two mock relays, one client; loopback single-host means
//!        every probe sees the same (127.0.0.1, same_port) so the
//!        classifier returns `Cone` + a consensus addr
//!   3. `detect_nat_type_dead_relay_is_unknown`
//!      — one alive relay + one dead address; aggregator returns
//!        `Unknown` with a non-empty `error` field on the failed
//!        probe

use std::net::{Ipv4Addr, SocketAddr};
use std::sync::Arc;
use std::time::Duration;

use wzp_client::reflect::{detect_nat_type, probe_reflect_addr, NatType};
use wzp_proto::{MediaTransport, SignalMessage};
use wzp_transport::{create_endpoint, server_config, QuinnTransport};

/// Minimal mock relay that loops accepting connections, handles
/// RegisterPresence + Reflect, and responds correctly. Mirrors the
/// two match arms from `wzp-relay/src/main.rs` that matter here.
///
/// Each accepted connection gets its own inner task so multiple
/// simultaneous probes work.
async fn spawn_mock_relay() -> (SocketAddr, tokio::task::JoinHandle<()>) {
    let _ = rustls::crypto::ring::default_provider().install_default();
    let (sc, _cert_der) = server_config();
    let bind: SocketAddr = (Ipv4Addr::LOCALHOST, 0).into();
    let endpoint = create_endpoint(bind, Some(sc)).expect("server endpoint");
    let listen_addr = endpoint.local_addr().expect("local_addr");

    let handle = tokio::spawn(async move {
        loop {
            // Accept the next incoming connection. `wzp_transport::accept`
            // returns the established `quinn::Connection`.
            let conn = match wzp_transport::accept(&endpoint).await {
                Ok(c) => c,
                Err(_) => break, // endpoint closed
            };
            let observed_addr = conn.remote_address();
            let transport = Arc::new(QuinnTransport::new(conn));

            // Per-connection handler. Keep servicing messages until
            // the peer closes so one probe connection can do
            // RegisterPresence → Ack → Reflect → Response without
            // racing other incoming connections.
            let t = transport;
            tokio::spawn(async move {
                loop {
                    match t.recv_signal().await {
                        Ok(Some(SignalMessage::RegisterPresence { .. })) => {
                            let _ = t
                                .send_signal(&SignalMessage::RegisterPresenceAck {
                                    success: true,
                                    error: None,
                                    relay_build: None,
                                })
                                .await;
                        }
                        Ok(Some(SignalMessage::Reflect)) => {
                            let _ = t
                                .send_signal(&SignalMessage::ReflectResponse {
                                    observed_addr: observed_addr.to_string(),
                                })
                                .await;
                        }
                        Ok(Some(_other)) => { /* ignore */ }
                        Ok(None) => break,
                        Err(_) => break,
                    }
                }
            });
        }
    });

    (listen_addr, handle)
}

// -----------------------------------------------------------------------
// Test 1: probe_reflect_addr against a single mock relay
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn probe_reflect_addr_happy_path() {
    let (relay_addr, _relay_handle) = spawn_mock_relay().await;

    let (observed, latency_ms) = tokio::time::timeout(
        Duration::from_secs(3),
        probe_reflect_addr(relay_addr, 2000, None),
    )
    .await
    .expect("probe must complete within 3s")
    .expect("probe must succeed");

    assert_eq!(
        observed.ip().to_string(),
        "127.0.0.1",
        "loopback test should see 127.0.0.1"
    );
    assert_ne!(observed.port(), 0, "observed port must be non-zero");
    // Latency on same host is dominated by the handshake — generously
    // allow up to 2s (the timeout) rather than picking a tight number
    // that would be flaky on busy CI runners.
    assert!(latency_ms < 2000, "latency {latency_ms}ms too high");
}

// -----------------------------------------------------------------------
// Test 2: two loopback relays → probes succeed, classification is Unknown
// -----------------------------------------------------------------------
//
// With the private-IP filter added in the NAT classifier, loopback
// reflex addrs (127.0.0.1) are dropped before classification —
// they can't possibly indicate public-internet NAT state. So the
// test now asserts:
//   - both probes succeed end-to-end (wire plumbing works)
//   - both return 127.0.0.1 (same-host is visible)
//   - the aggregated verdict is Unknown (no public probes)

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detect_nat_type_two_loopback_relays_probes_work_but_classify_unknown() {
    let (addr_a, _h_a) = spawn_mock_relay().await;
    let (addr_b, _h_b) = spawn_mock_relay().await;

    let detection = detect_nat_type(
        vec![
            ("RelayA".into(), addr_a),
            ("RelayB".into(), addr_b),
        ],
        2000,
        None,
    )
    .await;

    assert_eq!(detection.probes.len(), 2);
    for p in &detection.probes {
        assert!(
            p.observed_addr.is_some(),
            "probe {:?} failed: {:?}",
            p.relay_name,
            p.error
        );
    }
    let observed_ips: Vec<String> = detection
        .probes
        .iter()
        .map(|p| {
            p.observed_addr
                .as_ref()
                .and_then(|s| s.parse::<SocketAddr>().ok())
                .map(|a| a.ip().to_string())
                .unwrap_or_default()
        })
        .collect();
    assert_eq!(observed_ips[0], "127.0.0.1");
    assert_eq!(observed_ips[1], "127.0.0.1");

    // Classification: loopback probes are filtered out of the
    // public-NAT classifier, so with 0 public probes the result
    // is Unknown.
    assert_eq!(
        detection.nat_type,
        NatType::Unknown,
        "loopback-only probes must not contribute to public NAT classification"
    );
    assert!(detection.consensus_addr.is_none());
}

// -----------------------------------------------------------------------
// Test 3: one alive relay + one dead address → Unknown
// -----------------------------------------------------------------------

#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn detect_nat_type_dead_relay_is_unknown() {
    let (alive_addr, _alive_handle) = spawn_mock_relay().await;

    // Dead relay: a port that nothing is listening on. OS will drop
    // the packets, the probe should time out within the 600ms budget
    // we give it. Pick a port unlikely to be in use — port 1 on
    // loopback works on every OS I care about and fails fast.
    let dead_addr: SocketAddr = "127.0.0.1:1".parse().unwrap();

    let detection = detect_nat_type(
        vec![
            ("Alive".into(), alive_addr),
            ("Dead".into(), dead_addr),
        ],
        600, // tight timeout so the dead probe fails fast
        None,
    )
    .await;

    assert_eq!(detection.probes.len(), 2);

    // Find the alive and dead probes by name (order of JoinSet
    // completions is not guaranteed).
    let alive = detection.probes.iter().find(|p| p.relay_name == "Alive").unwrap();
    let dead = detection.probes.iter().find(|p| p.relay_name == "Dead").unwrap();

    assert!(
        alive.observed_addr.is_some(),
        "alive probe must succeed: {:?}",
        alive.error
    );
    assert!(
        dead.observed_addr.is_none(),
        "dead probe must fail, got addr {:?}",
        dead.observed_addr
    );
    assert!(
        dead.error.is_some(),
        "dead probe must surface an error string"
    );

    // With only 1 successful probe, the classifier returns Unknown.
    assert_eq!(detection.nat_type, NatType::Unknown);
    assert!(detection.consensus_addr.is_none());
}
