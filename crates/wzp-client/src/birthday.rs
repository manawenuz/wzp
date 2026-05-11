//! Birthday attack for hard NAT traversal.
//!
//! When both peers are behind symmetric NATs with random port
//! allocation, standard hole-punching fails because neither side
//! can predict the other's external port. This module implements
//! the birthday-paradox approach:
//!
//! 1. **Acceptor** opens N sockets, STUN-probes each to learn
//!    their external ports, reports them to the Dialer.
//! 2. **Dialer** sprays QUIC connect attempts to the Acceptor's
//!    reported ports + random ports on the Acceptor's IP.
//! 3. Birthday paradox: with N=64 ports and M=256 probes across
//!    65536 ports, collision probability is high.
//!
//! In practice, the Acceptor's STUN-probed ports are known
//! exactly (not random), so the Dialer targets them first —
//! making this more like "spray-and-pray with a hit list" than
//! a pure birthday attack.

use std::net::{Ipv4Addr, SocketAddr};
use std::time::{Duration, Instant};

use crate::stun;

/// Configuration for the birthday attack.
#[derive(Debug, Clone)]
pub struct BirthdayConfig {
    /// Number of sockets the Acceptor opens (default: 32).
    /// Each socket gets STUN-probed to learn its external port.
    /// More = higher chance of collision, but more resource usage.
    pub acceptor_ports: u16,
    /// Number of QUIC connect attempts the Dialer makes (default: 128).
    /// Spread across the Acceptor's known ports + random ports.
    pub dialer_probes: u16,
    /// Rate limit: ms between consecutive probes (default: 20ms = 50/s).
    pub probe_interval_ms: u16,
    /// Overall timeout for the birthday attack phase.
    pub timeout: Duration,
    /// STUN config for probing external ports.
    pub stun_config: stun::StunConfig,
}

impl Default for BirthdayConfig {
    fn default() -> Self {
        Self {
            acceptor_ports: 32,
            dialer_probes: 128,
            probe_interval_ms: 20,
            timeout: Duration::from_secs(8),
            stun_config: stun::StunConfig {
                servers: vec!["stun.l.google.com:19302".into()],
                timeout: Duration::from_secs(2),
            },
        }
    }
}

/// Result of the Acceptor's port-opening phase.
#[derive(Debug, Clone, serde::Serialize)]
pub struct AcceptorPorts {
    /// External IP (from STUN).
    pub external_ip: Option<Ipv4Addr>,
    /// List of (local_port, external_port) for each opened socket.
    pub ports: Vec<PortMapping>,
    /// How many sockets we attempted to open.
    pub attempted: u16,
    /// How many STUN probes succeeded.
    pub succeeded: u16,
}

/// A single socket's local↔external port mapping.
#[derive(Debug, Clone, serde::Serialize)]
pub struct PortMapping {
    pub local_port: u16,
    pub external_port: u16,
}

/// Open N sockets and STUN-probe each to discover external ports.
///
/// Returns the set of known external ports that the Dialer should
/// target. Each socket stays open (bound) so the NAT mapping
/// remains active until the returned `PortGuard` is dropped.
///
/// The sockets are returned so the caller can keep them alive
/// during the attack. Dropping them closes the NAT pinholes.
pub async fn open_acceptor_ports(
    config: &BirthdayConfig,
) -> (AcceptorPorts, Vec<tokio::net::UdpSocket>) {
    let mut sockets = Vec::new();
    let mut mappings = Vec::new();
    let mut external_ip: Option<Ipv4Addr> = None;
    let mut succeeded: u16 = 0;

    let stun_server = match config.stun_config.servers.first() {
        Some(s) => match stun::resolve_stun_server(s).await {
            Ok(a) => Some(a),
            Err(_) => None,
        },
        None => None,
    };

    for _ in 0..config.acceptor_ports {
        // Bind to random port
        let sock = match tokio::net::UdpSocket::bind("0.0.0.0:0").await {
            Ok(s) => s,
            Err(_) => continue,
        };
        let local_port = match sock.local_addr() {
            Ok(a) => a.port(),
            Err(_) => continue,
        };

        // STUN probe to learn external port
        if let Some(stun_addr) = stun_server {
            match stun::stun_reflect(&sock, stun_addr, config.stun_config.timeout).await {
                Ok(ext_addr) => {
                    if external_ip.is_none() {
                        if let std::net::IpAddr::V4(ip) = ext_addr.ip() {
                            external_ip = Some(ip);
                        }
                    }
                    mappings.push(PortMapping {
                        local_port,
                        external_port: ext_addr.port(),
                    });
                    succeeded += 1;
                }
                Err(e) => {
                    tracing::debug!(local_port, error = %e, "birthday: STUN probe failed for socket");
                }
            }
        }

        sockets.push(sock);
    }

    tracing::info!(
        attempted = config.acceptor_ports,
        succeeded,
        external_ip = ?external_ip,
        "birthday: acceptor ports opened"
    );

    let result = AcceptorPorts {
        external_ip,
        ports: mappings,
        attempted: config.acceptor_ports,
        succeeded,
    };

    (result, sockets)
}

/// Generate the list of target addresses for the Dialer to spray.
///
/// Priority order:
/// 1. Acceptor's known external ports (from STUN probes) — highest hit rate
/// 2. Random ports on the Acceptor's IP — birthday paradox fill
pub fn generate_dialer_targets(
    acceptor_ip: Ipv4Addr,
    known_ports: &[u16],
    total_probes: u16,
) -> Vec<SocketAddr> {
    let mut targets = Vec::with_capacity(total_probes as usize);

    // First: all known ports (guaranteed targets)
    for &port in known_ports {
        targets.push(SocketAddr::new(std::net::IpAddr::V4(acceptor_ip), port));
    }

    // Fill remaining with random ports (birthday attack)
    let remaining = total_probes.saturating_sub(known_ports.len() as u16);
    if remaining > 0 {
        use rand::Rng;
        let mut rng = rand::thread_rng();
        for _ in 0..remaining {
            let port = rng.gen_range(1024..=65535u16);
            let addr = SocketAddr::new(std::net::IpAddr::V4(acceptor_ip), port);
            if !targets.contains(&addr) {
                targets.push(addr);
            }
        }
    }

    targets
}

/// Run the Dialer side of the birthday attack.
///
/// Sprays QUIC connection attempts at the target addresses.
/// Returns the first successful connection, or None on timeout.
pub async fn spray_dialer(
    endpoint: &wzp_transport::Endpoint,
    targets: &[SocketAddr],
    call_sni: &str,
    probe_interval: Duration,
    timeout: Duration,
) -> Option<wzp_transport::QuinnTransport> {
    let start = Instant::now();
    let mut set = tokio::task::JoinSet::new();

    tracing::info!(
        target_count = targets.len(),
        interval_ms = probe_interval.as_millis(),
        timeout_s = timeout.as_secs(),
        "birthday: dialer starting spray"
    );

    // Spray connects with rate limiting
    for (idx, &target) in targets.iter().enumerate() {
        if start.elapsed() >= timeout {
            break;
        }

        let ep = endpoint.clone();
        let sni = call_sni.to_string();
        let client_cfg = wzp_transport::client_config();
        set.spawn(async move {
            let result = wzp_transport::connect(&ep, target, &sni, client_cfg).await;
            (idx, target, result)
        });

        // Rate limit — don't blast the NAT
        if idx < targets.len() - 1 {
            tokio::time::sleep(probe_interval).await;
        }
    }

    tracing::info!(
        spawned = set.len(),
        elapsed_ms = start.elapsed().as_millis(),
        "birthday: all probes spawned, waiting for first success"
    );

    // Wait for first success or all failures
    let deadline = start + timeout;
    while let Some(join_res) = tokio::select! {
        r = set.join_next() => r,
        _ = tokio::time::sleep_until(tokio::time::Instant::from_std(deadline)) => None,
    } {
        match join_res {
            Ok((idx, target, Ok(conn))) => {
                tracing::info!(
                    idx,
                    %target,
                    remote = %conn.remote_address(),
                    elapsed_ms = start.elapsed().as_millis(),
                    "birthday: HIT! QUIC handshake succeeded"
                );
                set.abort_all();
                return Some(wzp_transport::QuinnTransport::new(conn));
            }
            Ok((idx, target, Err(e))) => {
                tracing::debug!(
                    idx,
                    %target,
                    error = %e,
                    "birthday: probe failed"
                );
            }
            Err(_) => {}
        }
    }

    tracing::info!(
        elapsed_ms = start.elapsed().as_millis(),
        "birthday: all probes failed or timed out"
    );
    None
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn generate_targets_known_ports_first() {
        let ip = Ipv4Addr::new(203, 0, 113, 5);
        let known = vec![10000, 10001, 10002];
        let targets = generate_dialer_targets(ip, &known, 10);

        // Known ports should be first
        assert_eq!(targets[0].port(), 10000);
        assert_eq!(targets[1].port(), 10001);
        assert_eq!(targets[2].port(), 10002);
        // Rest are random
        assert!(targets.len() <= 10);
        // All target the right IP
        assert!(targets.iter().all(|a| a.ip() == std::net::IpAddr::V4(ip)));
    }

    #[test]
    fn generate_targets_no_known_all_random() {
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let targets = generate_dialer_targets(ip, &[], 50);
        assert!(!targets.is_empty());
        assert!(targets.len() <= 50);
        // All ports in valid range
        assert!(targets.iter().all(|a| a.port() >= 1024));
    }

    #[test]
    fn generate_targets_more_known_than_total() {
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let known: Vec<u16> = (10000..10100).collect();
        let targets = generate_dialer_targets(ip, &known, 50);
        // All 100 known ports included even though total=50
        assert_eq!(targets.len(), 100);
    }

    #[test]
    fn generate_targets_dedup() {
        let ip = Ipv4Addr::new(10, 0, 0, 1);
        let targets = generate_dialer_targets(ip, &[], 100);
        // No duplicates
        let mut sorted = targets.clone();
        sorted.sort();
        sorted.dedup();
        assert_eq!(sorted.len(), targets.len());
    }

    #[test]
    fn default_config() {
        let cfg = BirthdayConfig::default();
        assert_eq!(cfg.acceptor_ports, 32);
        assert_eq!(cfg.dialer_probes, 128);
        assert!(cfg.timeout.as_secs() > 0);
    }

    #[test]
    fn acceptor_ports_serializes() {
        let result = AcceptorPorts {
            external_ip: Some(Ipv4Addr::new(203, 0, 113, 5)),
            ports: vec![PortMapping {
                local_port: 12345,
                external_port: 54321,
            }],
            attempted: 32,
            succeeded: 1,
        };
        let json = serde_json::to_string(&result).unwrap();
        assert!(json.contains("54321"));
        assert!(json.contains("203.0.113.5"));
    }
}
