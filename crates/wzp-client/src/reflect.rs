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

/// Probe a single relay with a QUIC connection.
///
/// # Endpoint reuse (Phase 5 — Nebula-style architecture)
///
/// If `existing_endpoint` is `Some`, the probe uses that socket
/// instead of creating a fresh one. This is the desired mode in
/// production: a port-preserving NAT (MikroTik masquerade, most
/// consumer routers) gives a **stable** external port for the
/// one socket, so the reflex addr observed by ANY relay is the
/// SAME addr and matches what a peer would see on a direct dial.
/// Pass the signal endpoint here.
///
/// If `None`, creates a fresh one-shot endpoint. Kept for:
/// - tests that spin up isolated probes
/// - the "I'm not registered yet" case where there's no signal
///   endpoint to reuse
///
/// NOTE on NAT-type detection: the pre-Phase-5 behavior of
/// forcing a fresh endpoint per probe was wrong — it made every
/// port-preserving NAT look symmetric because the classifier saw
/// a different external port for each fresh source port. With
/// one shared socket, the classifier reflects the REAL NAT
/// behavior.
pub async fn probe_reflect_addr(
    relay: SocketAddr,
    timeout_ms: u64,
    existing_endpoint: Option<wzp_transport::Endpoint>,
) -> Result<(SocketAddr, u32), String> {
    // Install rustls provider idempotently — a second install on the
    // same thread is a no-op.
    let _ = rustls::crypto::ring::default_provider().install_default();

    let endpoint = match existing_endpoint {
        Some(ep) => ep,
        None => {
            let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
            create_endpoint(bind, None).map_err(|e| format!("endpoint: {e}"))?
        }
    };

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

    // `endpoint` is a quinn::Endpoint clone — an Arc under the
    // hood. Letting it drop at end-of-scope is correct whether it
    // was fresh (last ref → socket closes) or shared (ref count
    // decrements, socket stays alive for the signal loop).
    Ok(out)
}

/// Detect the client's NAT type by probing N relays in parallel and
/// classifying the returned addresses. Never errors — failing
/// probes surface via `NatProbeResult.error`; aggregate is always
/// returned.
///
/// # Endpoint reuse (Phase 5)
///
/// If `shared_endpoint` is `Some`, every probe reuses it. This is
/// the PRODUCTION behavior: all probes source from the same UDP
/// port, so port-preserving NATs map them to the same external
/// port, and the classifier reflects the real NAT type. Pass the
/// signal endpoint.
///
/// If `None`, each probe creates its own fresh endpoint — useful
/// in tests that don't have a signal endpoint, but produces
/// spurious `SymmetricPort` classifications against NATs that
/// would otherwise look cone-like.
pub async fn detect_nat_type(
    relays: Vec<(String, SocketAddr)>,
    timeout_ms: u64,
    shared_endpoint: Option<wzp_transport::Endpoint>,
) -> NatDetection {
    // Parallel probes via tokio::task::JoinSet so the wall-clock is
    // bounded by the slowest probe, not the sum. JoinSet keeps the
    // dep surface at just tokio — we already depend on it.
    let mut set = tokio::task::JoinSet::new();
    for (name, addr) in relays {
        let ep = shared_endpoint.clone();
        set.spawn(async move {
            let result = probe_reflect_addr(addr, timeout_ms, ep).await;
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

/// Role assignment for the Phase 3.5 dual-path QUIC race.
///
/// Both peers already know two strings at CallSetup time: their
/// own server-reflexive address (queried via Phase 1 Reflect) and
/// the peer's (carried in `CallSetup.peer_direct_addr`). To avoid
/// a negotiation round-trip, both sides compare the two strings
/// lexicographically and agree on a deterministic role:
///
/// - **Acceptor** — lexicographically smaller addr. Listens for
///   an incoming direct connection from the peer. Does NOT dial.
/// - **Dialer**   — lexicographically larger addr. Dials the
///   peer's direct addr. Does NOT listen.
///
/// Both roles ALSO dial the relay in parallel as a fallback.
/// Whichever future (direct or relay) completes first is used as
/// the media transport. Because the role is deterministic and
/// symmetric, both peers end up holding the same underlying QUIC
/// session on the direct path — A's accepted conn and D's dialed
/// conn are literally the same connection.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Role {
    /// This peer listens for the direct incoming connection.
    Acceptor,
    /// This peer dials the peer's direct address.
    Dialer,
}

/// Compute the deterministic role for this peer in the dual-path
/// race. Returns `None` when no direct attempt is possible —
/// either peer didn't advertise a reflex addr, or the two addrs
/// are identical (same host on loopback / mis-advertised).
///
/// The caller should treat `None` as "skip direct, relay-only".
pub fn determine_role(
    own_reflex_addr: Option<&str>,
    peer_reflex_addr: Option<&str>,
) -> Option<Role> {
    let (own, peer) = match (own_reflex_addr, peer_reflex_addr) {
        (Some(o), Some(p)) => (o, p),
        _ => return None,
    };
    match own.cmp(peer) {
        std::cmp::Ordering::Less => Some(Role::Acceptor),
        std::cmp::Ordering::Greater => Some(Role::Dialer),
        // Equal addrs should never happen in production (both
        // peers behind the same NAT mapping + same port would be
        // a degenerate case). Guard against it so we don't infinite-
        // loop waiting for a connection to ourselves.
        std::cmp::Ordering::Equal => None,
    }
}

/// Returns `true` if the address is in an RFC1918 / link-local /
/// loopback range and therefore cannot possibly be a post-NAT
/// reflex address from the public internet's point of view.
///
/// A probe against a relay ON THE SAME LAN as the client will
/// naturally report the client's LAN IP back (because there's no
/// NAT between them) — that observation is real but says nothing
/// about the client's public-internet-facing NAT state. Mixing
/// LAN reflex addrs with public-internet reflex addrs in
/// `classify_nat` would always report `Multiple` (different IPs)
/// and falsely warn about symmetric NAT. Filter them out before
/// classifying.
fn is_private_or_loopback(addr: &SocketAddr) -> bool {
    match addr.ip() {
        std::net::IpAddr::V4(v4) => {
            let o = v4.octets();
            v4.is_loopback()
                || v4.is_private() // 10/8, 172.16/12, 192.168/16
                || v4.is_link_local() // 169.254/16
                || (o[0] == 100 && (o[1] & 0xc0) == 0x40) // 100.64/10 CGNAT shared
        }
        std::net::IpAddr::V6(v6) => {
            v6.is_loopback() || v6.is_unspecified() || (v6.segments()[0] & 0xffc0) == 0xfe80 // fe80::/10 link-local
        }
    }
}

/// Pure-function NAT classifier — split out for unit testing
/// without touching the network.
///
/// Only considers probes whose reflex addr is a **public-internet**
/// address. LAN / private / loopback reflex addrs are dropped
/// because they reflect the same-network path rather than the
/// real NAT state. CGNAT (100.64/10) is also treated as private
/// because the post-CGNAT address would be what we actually want
/// to classify on — but CGNAT is unreachable from outside the
/// carrier, so a relay seeing the CGNAT addr is on the same
/// carrier network and again not useful for classification.
pub fn classify_nat(probes: &[NatProbeResult]) -> (NatType, Option<String>) {
    // First: parse every successful probe's observed addr.
    let parsed: Vec<SocketAddr> = probes
        .iter()
        .filter_map(|p| p.observed_addr.as_deref().and_then(|s| s.parse().ok()))
        .collect();

    // Then: drop LAN / private / loopback reflex addrs. Those are
    // legitimate observations by same-network relays, but they
    // don't contribute to NAT-type classification because the
    // client's real public-facing NAT mapping is not involved on
    // that path. A relay on the same LAN always sees the client's
    // LAN IP, regardless of whether the NAT beyond it is cone or
    // symmetric.
    let successes: Vec<SocketAddr> = parsed
        .into_iter()
        .filter(|a| !is_private_or_loopback(a))
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
    fn classify_drops_private_ip_probes() {
        // One LAN probe + one public probe should behave like a
        // single public probe — i.e. Unknown (not enough data to
        // classify). This is the common real-world case: the user
        // has a LAN relay + an internet relay configured, the LAN
        // relay sees the LAN IP, the internet relay sees the WAN
        // IP, and the old classifier would flag "Multiple" and
        // falsely warn about symmetric NAT.
        let probes = vec![
            mk(Some("192.168.1.100:4433")), // LAN — must be dropped
            mk(Some("203.0.113.5:4433")),   // public (TEST-NET-3)
        ];
        let (nt, _) = classify_nat(&probes);
        assert_eq!(nt, NatType::Unknown);
    }

    #[test]
    fn classify_drops_loopback_probes() {
        let probes = vec![
            mk(Some("127.0.0.1:4433")),     // loopback — must be dropped
            mk(Some("203.0.113.5:4433")),   // public
            mk(Some("203.0.113.5:4433")),   // public, same addr
        ];
        let (nt, addr) = classify_nat(&probes);
        // Two public probes with identical addrs → Cone.
        assert_eq!(nt, NatType::Cone);
        assert_eq!(addr.as_deref(), Some("203.0.113.5:4433"));
    }

    #[test]
    fn classify_drops_cgnat_probes() {
        // 100.64.0.0/10 is the CGNAT shared-transition range.
        // Filter treats it like RFC1918 — a relay that sees the
        // client with a 100.64/10 addr is on the same CGNAT
        // network and can't contribute to public NAT classification.
        let probes = vec![
            mk(Some("100.64.0.42:4433")),   // CGNAT — dropped
            mk(Some("203.0.113.5:4433")),   // public
            mk(Some("203.0.113.5:12345")),  // public, different port
        ];
        let (nt, _) = classify_nat(&probes);
        // Two public probes same IP different port → SymmetricPort.
        assert_eq!(nt, NatType::SymmetricPort);
    }

    #[test]
    fn classify_two_lan_probes_is_unknown_not_cone() {
        // Even if both probes come back from LAN relays, we can't
        // say anything useful about the public NAT state. Unknown,
        // not Cone.
        let probes = vec![
            mk(Some("192.168.1.100:4433")),
            mk(Some("192.168.1.100:4433")),
        ];
        let (nt, addr) = classify_nat(&probes);
        assert_eq!(nt, NatType::Unknown);
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
    fn determine_role_smaller_is_acceptor() {
        // Lexicographic: "192.0.2.1:4433" < "198.51.100.9:4433"
        assert_eq!(
            determine_role(Some("192.0.2.1:4433"), Some("198.51.100.9:4433")),
            Some(Role::Acceptor)
        );
    }

    #[test]
    fn determine_role_larger_is_dialer() {
        assert_eq!(
            determine_role(Some("198.51.100.9:4433"), Some("192.0.2.1:4433")),
            Some(Role::Dialer)
        );
    }

    #[test]
    fn determine_role_port_difference_matters() {
        // Same ip, different ports — string compare still works
        // because "4433" < "54321".
        assert_eq!(
            determine_role(Some("127.0.0.1:4433"), Some("127.0.0.1:54321")),
            Some(Role::Acceptor)
        );
        assert_eq!(
            determine_role(Some("127.0.0.1:54321"), Some("127.0.0.1:4433")),
            Some(Role::Dialer)
        );
    }

    #[test]
    fn determine_role_equal_addrs_is_none() {
        assert_eq!(
            determine_role(Some("192.0.2.1:4433"), Some("192.0.2.1:4433")),
            None
        );
    }

    #[test]
    fn determine_role_missing_side_is_none() {
        assert_eq!(determine_role(None, Some("192.0.2.1:4433")), None);
        assert_eq!(determine_role(Some("192.0.2.1:4433"), None), None);
        assert_eq!(determine_role(None, None), None);
    }

    #[test]
    fn determine_role_is_symmetric_across_peers() {
        // Both peers compute roles independently; they must end
        // up with opposite assignments (one Acceptor, one Dialer)
        // so that each side ends up talking to the other.
        let a = "192.0.2.1:4433";
        let b = "198.51.100.9:4433";
        let alice_role = determine_role(Some(a), Some(b));
        let bob_role = determine_role(Some(b), Some(a));
        assert_eq!(alice_role, Some(Role::Acceptor));
        assert_eq!(bob_role, Some(Role::Dialer));
    }

    #[test]
    fn classify_one_success_one_failure_is_unknown() {
        let probes = vec![mk(Some("192.0.2.1:4433")), mk(None)];
        let (nt, addr) = classify_nat(&probes);
        assert_eq!(nt, NatType::Unknown);
        assert!(addr.is_none());
    }
}
