//! Phase 8 (Tailscale-inspired): Comprehensive network diagnostic.
//!
//! Probes STUN servers, relay infrastructure, port mapping
//! capabilities, IPv6 reachability, and NAT hairpinning in parallel
//! to produce a `NetcheckReport` that captures the client's network
//! environment at a point in time.
//!
//! Used for:
//! - Troubleshooting connectivity issues
//! - Automatic relay selection (Phase 5)
//! - Pre-call NAT assessment
//! - Quality prediction

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;

use crate::portmap::{self, PortMapProtocol};
use crate::reflect::{self, NatType};
use crate::stun::{self, StunConfig};

/// Complete network diagnostic report.
#[derive(Debug, Clone, Serialize)]
pub struct NetcheckReport {
    /// NAT type classification (from combined STUN + relay probes).
    pub nat_type: NatType,
    /// Server-reflexive address (consensus from probes).
    pub reflexive_addr: Option<String>,
    /// Whether IPv4 connectivity is available.
    pub ipv4_reachable: bool,
    /// Whether IPv6 connectivity is available.
    pub ipv6_reachable: bool,
    /// Whether the NAT supports hairpinning (loopback to own
    /// reflexive address).
    pub hairpin_works: Option<bool>,
    /// Which port mapping protocol is available (if any).
    pub port_mapping: Option<PortMapProtocol>,
    /// Per-relay latency measurements.
    pub relay_latencies: Vec<RelayLatency>,
    /// Preferred relay (lowest latency).
    pub preferred_relay: Option<String>,
    /// STUN latency to first responding server (ms).
    pub stun_latency_ms: Option<u32>,
    /// Whether UPnP is available on the gateway.
    pub upnp_available: bool,
    /// Whether PCP is available on the gateway.
    pub pcp_available: bool,
    /// Whether NAT-PMP is available on the gateway.
    pub nat_pmp_available: bool,
    /// Default gateway address.
    pub gateway: Option<String>,
    /// Total time taken for the diagnostic (ms).
    pub duration_ms: u32,
    /// Individual STUN probe results.
    pub stun_probes: Vec<reflect::NatProbeResult>,
}

/// Latency to a specific relay.
#[derive(Debug, Clone, Serialize)]
pub struct RelayLatency {
    pub name: String,
    pub addr: String,
    pub rtt_ms: Option<u32>,
    pub error: Option<String>,
}

/// Configuration for the netcheck run.
#[derive(Debug, Clone)]
pub struct NetcheckConfig {
    /// STUN servers to probe.
    pub stun_config: StunConfig,
    /// Relay servers to probe (name, address pairs).
    pub relays: Vec<(String, SocketAddr)>,
    /// Per-probe timeout.
    pub timeout: Duration,
    /// Whether to test port mapping.
    pub test_portmap: bool,
    /// Whether to test IPv6.
    pub test_ipv6: bool,
    /// Local port for port mapping test (0 = skip).
    pub local_port: u16,
}

impl Default for NetcheckConfig {
    fn default() -> Self {
        Self {
            stun_config: StunConfig::default(),
            relays: Vec::new(),
            timeout: Duration::from_secs(5),
            test_portmap: true,
            test_ipv6: true,
            local_port: 0,
        }
    }
}

/// Run a comprehensive network diagnostic.
///
/// Probes run in parallel for speed — the total time is bounded
/// by the slowest individual probe, not the sum.
pub async fn run_netcheck(config: &NetcheckConfig) -> NetcheckReport {
    let start = Instant::now();

    // Run all probes in parallel.
    let stun_fut = stun::probe_stun_servers(&config.stun_config);
    let relay_fut = probe_relays(&config.relays, config.timeout);
    let portmap_fut = probe_portmap(config.test_portmap, config.local_port);
    let gateway_fut = portmap::default_gateway();
    let ipv6_fut = test_ipv6(config.test_ipv6, config.timeout);

    let (stun_probes, relay_latencies, portmap_result, gateway_result, ipv6_reachable) =
        tokio::join!(stun_fut, relay_fut, portmap_fut, gateway_result_fut(gateway_fut), ipv6_fut);

    // Classify NAT from STUN probes.
    let (nat_type, consensus_addr) = reflect::classify_nat(&stun_probes);

    // Determine STUN latency (first successful probe).
    let stun_latency_ms = stun_probes
        .iter()
        .filter_map(|p| p.latency_ms)
        .min();

    // IPv4 reachable if any STUN probe succeeded.
    let ipv4_reachable = stun_probes
        .iter()
        .any(|p| p.observed_addr.is_some());

    // Preferred relay = lowest RTT.
    let preferred_relay = relay_latencies
        .iter()
        .filter_map(|r| r.rtt_ms.map(|rtt| (r.name.clone(), rtt)))
        .min_by_key(|(_, rtt)| *rtt)
        .map(|(name, _)| name);

    // Port mapping availability.
    let (port_mapping, nat_pmp_available, pcp_available, upnp_available) = match portmap_result {
        Some(mapping) => {
            let proto = mapping.protocol;
            (
                Some(proto),
                proto == PortMapProtocol::NatPmp,
                proto == PortMapProtocol::Pcp,
                proto == PortMapProtocol::UPnP,
            )
        }
        None => (None, false, false, false),
    };

    let gateway = match gateway_result {
        Ok(gw) => Some(gw.to_string()),
        Err(_) => None,
    };

    NetcheckReport {
        nat_type,
        reflexive_addr: consensus_addr,
        ipv4_reachable,
        ipv6_reachable,
        hairpin_works: None, // TODO: implement hairpin test
        port_mapping,
        relay_latencies,
        preferred_relay,
        stun_latency_ms,
        upnp_available,
        pcp_available,
        nat_pmp_available,
        gateway,
        duration_ms: start.elapsed().as_millis() as u32,
        stun_probes,
    }
}

/// Probe relay latencies via reflect.
async fn probe_relays(
    relays: &[(String, SocketAddr)],
    timeout: Duration,
) -> Vec<RelayLatency> {
    if relays.is_empty() {
        return Vec::new();
    }

    let timeout_ms = timeout.as_millis() as u64;
    let mut set = tokio::task::JoinSet::new();

    for (name, addr) in relays {
        let name = name.clone();
        let addr = *addr;
        set.spawn(async move {
            let start = Instant::now();
            match reflect::probe_reflect_addr(addr, timeout_ms, None).await {
                Ok((_observed, _latency)) => RelayLatency {
                    name,
                    addr: addr.to_string(),
                    rtt_ms: Some(start.elapsed().as_millis() as u32),
                    error: None,
                },
                Err(e) => RelayLatency {
                    name,
                    addr: addr.to_string(),
                    rtt_ms: None,
                    error: Some(e),
                },
            }
        });
    }

    let mut results = Vec::with_capacity(relays.len());
    while let Some(join_result) = set.join_next().await {
        match join_result {
            Ok(r) => results.push(r),
            Err(_) => {}
        }
    }

    // Sort by RTT (lowest first).
    results.sort_by_key(|r| r.rtt_ms.unwrap_or(u32::MAX));
    results
}

/// Attempt port mapping and return the mapping if successful.
async fn probe_portmap(
    enabled: bool,
    local_port: u16,
) -> Option<portmap::PortMapping> {
    if !enabled || local_port == 0 {
        return None;
    }
    portmap::acquire_port_mapping(local_port, None).await.ok()
}

/// Wrap the gateway future to handle the Result.
async fn gateway_result_fut(
    fut: impl std::future::Future<Output = Result<std::net::Ipv4Addr, portmap::PortMapError>>,
) -> Result<std::net::Ipv4Addr, portmap::PortMapError> {
    fut.await
}

/// Test IPv6 connectivity by attempting to bind and send on an IPv6 socket.
async fn test_ipv6(enabled: bool, timeout: Duration) -> bool {
    if !enabled {
        return false;
    }

    // Try to resolve and connect to an IPv6 STUN server.
    let result = tokio::time::timeout(timeout, async {
        let sock = tokio::net::UdpSocket::bind("[::]:0").await.ok()?;
        // Try Google's IPv6 STUN — if DNS resolves to an AAAA record
        // and we can send a packet, IPv6 is working.
        let addr = stun::resolve_stun_server("stun.l.google.com:19302").await.ok()?;
        if addr.is_ipv6() {
            sock.send_to(&[0u8; 1], addr).await.ok()?;
            Some(true)
        } else {
            // Server resolved to IPv4 — try binding to [::] at least
            Some(false)
        }
    })
    .await;

    match result {
        Ok(Some(true)) => true,
        _ => {
            // Fallback: can we at least bind an IPv6 socket?
            tokio::net::UdpSocket::bind("[::]:0").await.is_ok()
        }
    }
}

/// Format a netcheck report as a human-readable string.
pub fn format_report(report: &NetcheckReport) -> String {
    let mut out = String::new();

    out.push_str(&format!("=== WarzonePhone Netcheck ===\n\n"));
    out.push_str(&format!(
        "NAT Type:       {:?}\n",
        report.nat_type
    ));
    out.push_str(&format!(
        "Reflexive Addr: {}\n",
        report.reflexive_addr.as_deref().unwrap_or("(unknown)")
    ));
    out.push_str(&format!(
        "IPv4:           {}\n",
        if report.ipv4_reachable { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "IPv6:           {}\n",
        if report.ipv6_reachable { "yes" } else { "no" }
    ));
    out.push_str(&format!(
        "Gateway:        {}\n",
        report.gateway.as_deref().unwrap_or("(unknown)")
    ));

    out.push_str(&format!("\n--- Port Mapping ---\n"));
    out.push_str(&format!(
        "NAT-PMP: {}  PCP: {}  UPnP: {}\n",
        if report.nat_pmp_available { "yes" } else { "no" },
        if report.pcp_available { "yes" } else { "no" },
        if report.upnp_available { "yes" } else { "no" },
    ));
    if let Some(proto) = &report.port_mapping {
        out.push_str(&format!("Active mapping: {:?}\n", proto));
    }

    if !report.stun_probes.is_empty() {
        out.push_str(&format!("\n--- STUN Probes ---\n"));
        for p in &report.stun_probes {
            out.push_str(&format!(
                "  {} → {} ({}ms){}\n",
                p.relay_name,
                p.observed_addr.as_deref().unwrap_or("failed"),
                p.latency_ms.map(|ms| ms.to_string()).unwrap_or_else(|| "-".into()),
                p.error.as_ref().map(|e| format!(" [{e}]")).unwrap_or_default(),
            ));
        }
    }

    if !report.relay_latencies.is_empty() {
        out.push_str(&format!("\n--- Relay Latencies ---\n"));
        for r in &report.relay_latencies {
            out.push_str(&format!(
                "  {} ({}) → {}ms{}\n",
                r.name,
                r.addr,
                r.rtt_ms.map(|ms| ms.to_string()).unwrap_or_else(|| "-".into()),
                r.error.as_ref().map(|e| format!(" [{e}]")).unwrap_or_default(),
            ));
        }
        if let Some(ref pref) = report.preferred_relay {
            out.push_str(&format!("  Preferred: {pref}\n"));
        }
    }

    out.push_str(&format!("\nCompleted in {}ms\n", report.duration_ms));
    out
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn default_config_has_stun_servers() {
        let config = NetcheckConfig::default();
        assert!(!config.stun_config.servers.is_empty());
    }

    #[test]
    fn format_report_produces_output() {
        let report = NetcheckReport {
            nat_type: NatType::Cone,
            reflexive_addr: Some("203.0.113.5:4433".into()),
            ipv4_reachable: true,
            ipv6_reachable: false,
            hairpin_works: None,
            port_mapping: None,
            relay_latencies: vec![RelayLatency {
                name: "relay-1".into(),
                addr: "10.0.0.1:4433".into(),
                rtt_ms: Some(25),
                error: None,
            }],
            preferred_relay: Some("relay-1".into()),
            stun_latency_ms: Some(15),
            upnp_available: false,
            pcp_available: false,
            nat_pmp_available: false,
            gateway: Some("192.168.1.1".into()),
            duration_ms: 1500,
            stun_probes: vec![],
        };

        let text = format_report(&report);
        assert!(text.contains("Cone"));
        assert!(text.contains("203.0.113.5:4433"));
        assert!(text.contains("relay-1"));
        assert!(text.contains("1500ms"));
    }

    #[test]
    fn report_serializes_to_json() {
        let report = NetcheckReport {
            nat_type: NatType::Cone,
            reflexive_addr: Some("203.0.113.5:4433".into()),
            ipv4_reachable: true,
            ipv6_reachable: false,
            hairpin_works: None,
            port_mapping: Some(PortMapProtocol::NatPmp),
            relay_latencies: vec![],
            preferred_relay: None,
            stun_latency_ms: Some(25),
            upnp_available: false,
            pcp_available: false,
            nat_pmp_available: true,
            gateway: Some("192.168.1.1".into()),
            duration_ms: 500,
            stun_probes: vec![],
        };
        let json = serde_json::to_string(&report).unwrap();
        assert!(json.contains("Cone"));
        assert!(json.contains("203.0.113.5:4433"));
        assert!(json.contains("NatPmp"));

        // Roundtrip
        let decoded: serde_json::Value = serde_json::from_str(&json).unwrap();
        assert_eq!(decoded["ipv4_reachable"], true);
        assert_eq!(decoded["ipv6_reachable"], false);
        assert_eq!(decoded["stun_latency_ms"], 25);
    }

    #[test]
    fn relay_latency_serializes() {
        let lat = RelayLatency {
            name: "eu-west".into(),
            addr: "10.0.0.1:4433".into(),
            rtt_ms: Some(42),
            error: None,
        };
        let json = serde_json::to_string(&lat).unwrap();
        assert!(json.contains("eu-west"));
        assert!(json.contains("42"));
    }

    #[test]
    fn format_report_empty_relays() {
        let report = NetcheckReport {
            nat_type: NatType::Unknown,
            reflexive_addr: None,
            ipv4_reachable: false,
            ipv6_reachable: false,
            hairpin_works: None,
            port_mapping: None,
            relay_latencies: vec![],
            preferred_relay: None,
            stun_latency_ms: None,
            upnp_available: false,
            pcp_available: false,
            nat_pmp_available: false,
            gateway: None,
            duration_ms: 100,
            stun_probes: vec![],
        };
        let text = format_report(&report);
        assert!(text.contains("Unknown"));
        assert!(text.contains("(unknown)")); // reflexive addr
        assert!(text.contains("100ms"));
    }

    #[test]
    fn format_report_with_stun_probes() {
        let report = NetcheckReport {
            nat_type: NatType::SymmetricPort,
            reflexive_addr: None,
            ipv4_reachable: true,
            ipv6_reachable: true,
            hairpin_works: Some(false),
            port_mapping: Some(PortMapProtocol::UPnP),
            relay_latencies: vec![
                RelayLatency {
                    name: "us-east".into(),
                    addr: "10.0.0.1:4433".into(),
                    rtt_ms: Some(15),
                    error: None,
                },
                RelayLatency {
                    name: "eu-west".into(),
                    addr: "10.0.0.2:4433".into(),
                    rtt_ms: None,
                    error: Some("timeout".into()),
                },
            ],
            preferred_relay: Some("us-east".into()),
            stun_latency_ms: Some(20),
            upnp_available: true,
            pcp_available: false,
            nat_pmp_available: false,
            gateway: Some("192.168.0.1".into()),
            duration_ms: 3000,
            stun_probes: vec![reflect::NatProbeResult {
                relay_name: "stun:google".into(),
                relay_addr: "74.125.250.129:19302".into(),
                observed_addr: Some("203.0.113.5:12345".into()),
                latency_ms: Some(20),
                error: None,
            }],
        };
        let text = format_report(&report);
        assert!(text.contains("SymmetricPort"));
        assert!(text.contains("us-east"));
        assert!(text.contains("eu-west"));
        assert!(text.contains("Preferred: us-east"));
        assert!(text.contains("UPnP: yes"));
        assert!(text.contains("stun:google"));
        assert!(text.contains("3000ms"));
    }

    /// Integration test: run actual netcheck (requires network).
    #[tokio::test]
    #[ignore]
    async fn integration_netcheck() {
        let config = NetcheckConfig::default();
        let report = run_netcheck(&config).await;
        println!("{}", format_report(&report));
        assert!(report.duration_ms > 0);
    }
}
