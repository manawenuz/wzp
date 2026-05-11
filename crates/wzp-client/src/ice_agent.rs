//! Phase 8 (Tailscale-inspired): ICE agent for candidate lifecycle
//! management and mid-call re-gathering.
//!
//! The `IceAgent` owns the state of all candidate discovery
//! mechanisms (STUN, port mapping, host candidates) and provides:
//!
//! - `gather()`: initial candidate gathering during call setup
//! - `re_gather()`: triggered on network change, produces a
//!   `CandidateUpdate` to send to the peer
//! - `apply_peer_update()`: processes peer's candidate updates
//!
//! This is NOT a full ICE agent (RFC 8445). It's the Tailscale-style
//! "gather all candidates, race them all in parallel, pick the
//! winner" approach, adapted for QUIC transport.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU32, Ordering};
use std::time::Duration;

use wzp_proto::SignalMessage;

use crate::dual_path::PeerCandidates;
use crate::portmap;
use crate::reflect;
use crate::stun;

/// All candidates gathered for the local side.
#[derive(Debug, Clone)]
pub struct CandidateSet {
    /// STUN-discovered server-reflexive address.
    pub reflexive: Option<SocketAddr>,
    /// LAN host candidates from local interfaces.
    pub local: Vec<SocketAddr>,
    /// Port-mapped address from NAT-PMP/PCP/UPnP.
    pub mapped: Option<SocketAddr>,
    /// Generation counter (monotonically increasing per call).
    pub generation: u32,
}

/// Configuration for the ICE agent.
#[derive(Debug, Clone)]
pub struct IceAgentConfig {
    /// STUN servers to use for reflexive discovery.
    pub stun_config: stun::StunConfig,
    /// Whether to attempt port mapping.
    pub enable_portmap: bool,
    /// Timeout for each discovery mechanism.
    pub gather_timeout: Duration,
    /// The QUIC endpoint's local port (for host candidate pairing).
    pub local_v4_port: u16,
    /// Optional IPv6 port.
    pub local_v6_port: Option<u16>,
}

impl Default for IceAgentConfig {
    fn default() -> Self {
        Self {
            stun_config: stun::StunConfig::default(),
            enable_portmap: true,
            gather_timeout: Duration::from_secs(3),
            local_v4_port: 0,
            local_v6_port: None,
        }
    }
}

/// ICE agent managing candidate lifecycle.
pub struct IceAgent {
    config: IceAgentConfig,
    generation: AtomicU32,
    call_id: String,
    /// Last-seen peer generation (to filter stale updates).
    peer_generation: AtomicU32,
}

impl IceAgent {
    pub fn new(call_id: String, config: IceAgentConfig) -> Self {
        Self {
            config,
            generation: AtomicU32::new(0),
            call_id,
            peer_generation: AtomicU32::new(0),
        }
    }

    /// Initial candidate gathering. Runs all discovery mechanisms
    /// in parallel and returns the full candidate set.
    pub async fn gather(&self) -> CandidateSet {
        let generation = self.generation.fetch_add(1, Ordering::Relaxed);

        // Run STUN + port mapping + host candidates in parallel.
        let stun_fut = stun::discover_reflexive(&self.config.stun_config);
        let portmap_fut = async {
            if self.config.enable_portmap && self.config.local_v4_port > 0 {
                portmap::acquire_port_mapping(self.config.local_v4_port, None)
                    .await
                    .ok()
            } else {
                None
            }
        };

        let (stun_result, portmap_result) = tokio::join!(
            tokio::time::timeout(self.config.gather_timeout, stun_fut),
            tokio::time::timeout(self.config.gather_timeout, portmap_fut),
        );

        let reflexive = stun_result.ok().and_then(|r| r.ok());
        let mapped = portmap_result.ok().flatten().map(|m| m.external_addr);
        let local =
            reflect::local_host_candidates(self.config.local_v4_port, self.config.local_v6_port);

        tracing::info!(
            generation,
            reflexive = ?reflexive,
            mapped = ?mapped,
            local_count = local.len(),
            "ice_agent: gathered candidates"
        );

        CandidateSet {
            reflexive,
            local,
            mapped,
            generation,
        }
    }

    /// Re-gather candidates after a network change. Increments the
    /// generation counter and returns a `CandidateUpdate` signal
    /// message to send to the peer.
    pub async fn re_gather(&self) -> (CandidateSet, SignalMessage) {
        let candidates = self.gather().await;

        let update = SignalMessage::CandidateUpdate {
            call_id: self.call_id.clone(),
            reflexive_addr: candidates.reflexive.map(|a| a.to_string()),
            local_addrs: candidates.local.iter().map(|a| a.to_string()).collect(),
            mapped_addr: candidates.mapped.map(|a| a.to_string()),
            generation: candidates.generation,
        };

        (candidates, update)
    }

    /// Process a peer's candidate update. Returns `Some(PeerCandidates)`
    /// if the update is newer than the last-seen generation, `None`
    /// if it's stale.
    pub fn apply_peer_update(&self, update: &SignalMessage) -> Option<PeerCandidates> {
        let (reflexive_addr, local_addrs, mapped_addr, generation) = match update {
            SignalMessage::CandidateUpdate {
                reflexive_addr,
                local_addrs,
                mapped_addr,
                generation,
                ..
            } => (reflexive_addr, local_addrs, mapped_addr, *generation),
            _ => return None,
        };

        // Only accept if newer than last-seen generation.
        let prev = self.peer_generation.fetch_max(generation, Ordering::AcqRel);
        if generation <= prev {
            tracing::debug!(
                generation,
                prev,
                "ice_agent: ignoring stale CandidateUpdate"
            );
            return None;
        }

        let reflexive = reflexive_addr.as_deref().and_then(|s| s.parse().ok());
        let local: Vec<SocketAddr> = local_addrs.iter().filter_map(|s| s.parse().ok()).collect();
        let mapped = mapped_addr.as_deref().and_then(|s| s.parse().ok());

        tracing::info!(
            generation,
            reflexive = ?reflexive,
            mapped = ?mapped,
            local_count = local.len(),
            "ice_agent: applied peer candidate update"
        );

        Some(PeerCandidates {
            reflexive,
            local,
            mapped,
        })
    }

    /// Get the current generation counter.
    pub fn generation(&self) -> u32 {
        self.generation.load(Ordering::Relaxed)
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn apply_peer_update_rejects_stale() {
        let agent = IceAgent::new("test-call".into(), IceAgentConfig::default());

        // First update (gen=1) should succeed.
        let update1 = SignalMessage::CandidateUpdate {
            call_id: "test-call".into(),
            reflexive_addr: Some("203.0.113.5:4433".into()),
            local_addrs: vec!["192.168.1.10:4433".into()],
            mapped_addr: None,
            generation: 1,
        };
        let result = agent.apply_peer_update(&update1);
        assert!(result.is_some());
        let candidates = result.unwrap();
        assert_eq!(
            candidates.reflexive,
            Some("203.0.113.5:4433".parse().unwrap())
        );
        assert_eq!(candidates.local.len(), 1);

        // Same generation (gen=1) should be rejected.
        let update1b = SignalMessage::CandidateUpdate {
            call_id: "test-call".into(),
            reflexive_addr: Some("198.51.100.9:4433".into()),
            local_addrs: vec![],
            mapped_addr: None,
            generation: 1,
        };
        assert!(agent.apply_peer_update(&update1b).is_none());

        // Older generation (gen=0) should be rejected.
        let update0 = SignalMessage::CandidateUpdate {
            call_id: "test-call".into(),
            reflexive_addr: Some("10.0.0.1:4433".into()),
            local_addrs: vec![],
            mapped_addr: None,
            generation: 0,
        };
        assert!(agent.apply_peer_update(&update0).is_none());

        // Newer generation (gen=2) should succeed.
        let update2 = SignalMessage::CandidateUpdate {
            call_id: "test-call".into(),
            reflexive_addr: Some("198.51.100.9:5555".into()),
            local_addrs: vec![],
            mapped_addr: Some("203.0.113.5:12345".into()),
            generation: 2,
        };
        let result = agent.apply_peer_update(&update2);
        assert!(result.is_some());
        let candidates = result.unwrap();
        assert_eq!(
            candidates.reflexive,
            Some("198.51.100.9:5555".parse().unwrap())
        );
        assert_eq!(
            candidates.mapped,
            Some("203.0.113.5:12345".parse().unwrap())
        );
    }

    #[test]
    fn apply_wrong_signal_returns_none() {
        let agent = IceAgent::new("test-call".into(), IceAgentConfig::default());
        let wrong = SignalMessage::Reflect;
        assert!(agent.apply_peer_update(&wrong).is_none());
    }

    #[test]
    fn generation_increments() {
        let agent = IceAgent::new("test".into(), IceAgentConfig::default());
        assert_eq!(agent.generation(), 0);
        // Simulate what gather() does internally
        let g1 = agent.generation.fetch_add(1, Ordering::Relaxed);
        assert_eq!(g1, 0);
        assert_eq!(agent.generation(), 1);
        let g2 = agent.generation.fetch_add(1, Ordering::Relaxed);
        assert_eq!(g2, 1);
        assert_eq!(agent.generation(), 2);
    }

    #[test]
    fn apply_peer_update_parses_all_fields() {
        let agent = IceAgent::new("test-call".into(), IceAgentConfig::default());

        let update = SignalMessage::CandidateUpdate {
            call_id: "test-call".into(),
            reflexive_addr: Some("203.0.113.5:4433".into()),
            local_addrs: vec!["192.168.1.10:4433".into(), "10.0.0.5:4433".into()],
            mapped_addr: Some("198.51.100.42:12345".into()),
            generation: 1,
        };

        let candidates = agent.apply_peer_update(&update).unwrap();
        assert_eq!(
            candidates.reflexive,
            Some("203.0.113.5:4433".parse().unwrap())
        );
        assert_eq!(candidates.local.len(), 2);
        assert_eq!(
            candidates.local[0],
            "192.168.1.10:4433".parse::<SocketAddr>().unwrap()
        );
        assert_eq!(
            candidates.mapped,
            Some("198.51.100.42:12345".parse().unwrap())
        );
    }

    #[test]
    fn apply_peer_update_handles_empty_fields() {
        let agent = IceAgent::new("test".into(), IceAgentConfig::default());

        let update = SignalMessage::CandidateUpdate {
            call_id: "test".into(),
            reflexive_addr: None,
            local_addrs: vec![],
            mapped_addr: None,
            generation: 1,
        };

        let candidates = agent.apply_peer_update(&update).unwrap();
        assert!(candidates.reflexive.is_none());
        assert!(candidates.local.is_empty());
        assert!(candidates.mapped.is_none());
    }

    #[test]
    fn apply_peer_update_skips_unparseable_addrs() {
        let agent = IceAgent::new("test".into(), IceAgentConfig::default());

        let update = SignalMessage::CandidateUpdate {
            call_id: "test".into(),
            reflexive_addr: Some("not-an-addr".into()),
            local_addrs: vec![
                "192.168.1.10:4433".into(),
                "garbage".into(),
                "10.0.0.5:4433".into(),
            ],
            mapped_addr: Some("also-bad".into()),
            generation: 1,
        };

        let candidates = agent.apply_peer_update(&update).unwrap();
        assert!(candidates.reflexive.is_none()); // unparseable
        assert_eq!(candidates.local.len(), 2); // garbage filtered
        assert!(candidates.mapped.is_none()); // unparseable
    }

    #[test]
    fn default_config_values() {
        let cfg = IceAgentConfig::default();
        assert!(cfg.enable_portmap);
        assert!(cfg.gather_timeout.as_secs() > 0);
        assert!(!cfg.stun_config.servers.is_empty());
        assert_eq!(cfg.local_v4_port, 0);
        assert!(cfg.local_v6_port.is_none());
    }

    #[tokio::test]
    async fn gather_returns_candidates_even_with_no_stun() {
        // With default config (port 0 = no portmap, STUN will timeout
        // quickly on loopback), gather should still return host candidates.
        let agent = IceAgent::new(
            "test".into(),
            IceAgentConfig {
                stun_config: stun::StunConfig {
                    servers: vec![], // no servers = quick failure
                    timeout: Duration::from_millis(100),
                },
                enable_portmap: false,
                gather_timeout: Duration::from_millis(200),
                local_v4_port: 12345,
                local_v6_port: None,
            },
        );

        let candidates = agent.gather().await;
        assert_eq!(candidates.generation, 0);
        // Reflexive should be None (no STUN servers)
        assert!(candidates.reflexive.is_none());
        // Mapped should be None (portmap disabled)
        assert!(candidates.mapped.is_none());
        // Local candidates depend on the machine's interfaces
        // but gather() should not panic.
    }

    #[tokio::test]
    async fn re_gather_produces_signal_message() {
        let agent = IceAgent::new(
            "call-42".into(),
            IceAgentConfig {
                stun_config: stun::StunConfig {
                    servers: vec![],
                    timeout: Duration::from_millis(50),
                },
                enable_portmap: false,
                gather_timeout: Duration::from_millis(100),
                local_v4_port: 4433,
                local_v6_port: None,
            },
        );

        let (candidates, signal) = agent.re_gather().await;
        assert_eq!(candidates.generation, 0);

        match signal {
            SignalMessage::CandidateUpdate {
                call_id,
                generation,
                ..
            } => {
                assert_eq!(call_id, "call-42");
                assert_eq!(generation, 0);
            }
            _ => panic!("expected CandidateUpdate"),
        }

        // Second re_gather increments generation
        let (candidates2, signal2) = agent.re_gather().await;
        assert_eq!(candidates2.generation, 1);
        match signal2 {
            SignalMessage::CandidateUpdate { generation, .. } => {
                assert_eq!(generation, 1);
            }
            _ => panic!("expected CandidateUpdate"),
        }
    }
}
