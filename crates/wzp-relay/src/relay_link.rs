//! Per-session relay forwarding — connect to a peer relay and forward only
//! specific sessions' media packets there.
//!
//! This is the building block for relay chaining (multi-hop calls). Instead
//! of forwarding ALL traffic to a single hardcoded relay (forward mode) or
//! to everyone in a room (SFU mode), a `RelayLink` represents a QUIC
//! connection to one peer relay used for forwarding a specific set of
//! sessions.
//!
//! `RelayLinkManager` tracks all active relay links and their session
//! assignments, providing get-or-connect semantics and idle cleanup.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::sync::Arc;

use tracing::{debug, info, warn};

use wzp_proto::MediaPacket;
use wzp_proto::MediaTransport;

/// A connection to a peer relay for forwarding specific sessions.
///
/// Each `RelayLink` holds a QUIC transport to one peer relay and tracks
/// which session IDs are being forwarded through it. When all sessions
/// are removed the link is considered idle and can be cleaned up.
pub struct RelayLink {
    target_addr: SocketAddr,
    /// The underlying QUIC transport. `None` only in unit-test stubs where
    /// no real connection is established.
    transport: Option<Arc<wzp_transport::QuinnTransport>>,
    active_sessions: HashSet<String>,
}

impl RelayLink {
    /// Connect to a peer relay at `target`.
    ///
    /// Uses the `"_relay"` SNI to signal that this is a relay-to-relay
    /// connection (similar to `"_probe"` for health checks). The peer
    /// should skip normal client auth/handshake for relay-SNI connections.
    pub async fn connect(target: SocketAddr) -> Result<Self, anyhow::Error> {
        // Create a client-only endpoint on an OS-assigned port.
        let endpoint = wzp_transport::create_endpoint(
            "0.0.0.0:0".parse().unwrap(),
            None,
        )?;

        let client_cfg = wzp_transport::client_config();
        let conn = wzp_transport::connect(&endpoint, target, "_relay", client_cfg).await?;
        let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

        info!(%target, "relay link established");

        Ok(Self {
            target_addr: target,
            transport: Some(transport),
            active_sessions: HashSet::new(),
        })
    }

    /// Create a `RelayLink` from an existing transport (useful when the
    /// connection was established through other means).
    pub fn from_transport(
        target_addr: SocketAddr,
        transport: Arc<wzp_transport::QuinnTransport>,
    ) -> Self {
        Self {
            target_addr,
            transport: Some(transport),
            active_sessions: HashSet::new(),
        }
    }

    /// Create a stub `RelayLink` with no transport — for unit tests that
    /// only exercise session-tracking / management logic.
    #[cfg(test)]
    fn stub(target_addr: SocketAddr) -> Self {
        Self {
            target_addr,
            transport: None,
            active_sessions: HashSet::new(),
        }
    }

    /// Forward a media packet to this peer relay.
    pub async fn forward(&self, pkt: &MediaPacket) -> Result<(), anyhow::Error> {
        match &self.transport {
            Some(t) => t
                .send_media(pkt)
                .await
                .map_err(|e| anyhow::anyhow!("relay link forward to {}: {e}", self.target_addr)),
            None => Err(anyhow::anyhow!(
                "relay link to {} has no transport (stub)",
                self.target_addr
            )),
        }
    }

    /// The address of the peer relay this link connects to.
    pub fn target_addr(&self) -> SocketAddr {
        self.target_addr
    }

    /// A reference to the underlying QUIC transport (if connected).
    pub fn transport(&self) -> Option<&Arc<wzp_transport::QuinnTransport>> {
        self.transport.as_ref()
    }

    /// Add a session to be forwarded through this link.
    pub fn add_session(&mut self, session_id: &str) {
        if self.active_sessions.insert(session_id.to_string()) {
            debug!(
                target_relay = %self.target_addr,
                session = session_id,
                count = self.active_sessions.len(),
                "session added to relay link"
            );
        }
    }

    /// Remove a session from this link.
    pub fn remove_session(&mut self, session_id: &str) {
        if self.active_sessions.remove(session_id) {
            debug!(
                target_relay = %self.target_addr,
                session = session_id,
                count = self.active_sessions.len(),
                "session removed from relay link"
            );
        }
    }

    /// Check if this link is forwarding any sessions.
    pub fn is_idle(&self) -> bool {
        self.active_sessions.is_empty()
    }

    /// Number of sessions being forwarded through this link.
    pub fn session_count(&self) -> usize {
        self.active_sessions.len()
    }

    /// Check if a specific session is being forwarded through this link.
    pub fn has_session(&self, session_id: &str) -> bool {
        self.active_sessions.contains(session_id)
    }

    /// Close the underlying QUIC connection (no-op if no transport).
    pub async fn close(&self) {
        info!(target_relay = %self.target_addr, "closing relay link");
        if let Some(ref t) = self.transport {
            let _ = t.close().await;
        }
    }
}

// ---------------------------------------------------------------------------
// RelayLinkManager
// ---------------------------------------------------------------------------

/// Manages connections to multiple peer relays for per-session forwarding.
///
/// Each peer relay gets at most one `RelayLink`. Sessions are registered
/// on specific links, and idle links (no sessions) can be cleaned up.
pub struct RelayLinkManager {
    links: HashMap<SocketAddr, RelayLink>,
}

impl RelayLinkManager {
    /// Create an empty link manager.
    pub fn new() -> Self {
        Self {
            links: HashMap::new(),
        }
    }

    /// Get or create a link to a peer relay.
    ///
    /// If a link already exists it is returned. Otherwise a new QUIC
    /// connection is established using `RelayLink::connect`.
    pub async fn get_or_connect(
        &mut self,
        target: SocketAddr,
    ) -> Result<&RelayLink, anyhow::Error> {
        if !self.links.contains_key(&target) {
            let link = RelayLink::connect(target).await?;
            self.links.insert(target, link);
        }
        Ok(self.links.get(&target).unwrap())
    }

    /// Get a mutable reference to an existing link (if any).
    pub fn get_mut(&mut self, target: &SocketAddr) -> Option<&mut RelayLink> {
        self.links.get_mut(target)
    }

    /// Get a reference to an existing link (if any).
    pub fn get(&self, target: &SocketAddr) -> Option<&RelayLink> {
        self.links.get(target)
    }

    /// Forward a packet for a specific session to the appropriate relay.
    ///
    /// The link must already exist (created via `get_or_connect`).
    pub async fn forward_to(
        &self,
        target: SocketAddr,
        pkt: &MediaPacket,
    ) -> Result<(), anyhow::Error> {
        match self.links.get(&target) {
            Some(link) => link.forward(pkt).await,
            None => Err(anyhow::anyhow!(
                "no relay link to {target} — call get_or_connect first"
            )),
        }
    }

    /// Register a session on a specific link.
    ///
    /// The link must already exist. If it does not, a warning is logged
    /// and the registration is silently skipped.
    pub fn register_session(&mut self, target: SocketAddr, session_id: &str) {
        match self.links.get_mut(&target) {
            Some(link) => link.add_session(session_id),
            None => {
                warn!(
                    %target,
                    session = session_id,
                    "cannot register session — no link to target"
                );
            }
        }
    }

    /// Unregister a session. If the link becomes idle, close and remove it.
    pub async fn unregister_session(&mut self, target: SocketAddr, session_id: &str) {
        let should_remove = if let Some(link) = self.links.get_mut(&target) {
            link.remove_session(session_id);
            if link.is_idle() {
                link.close().await;
                true
            } else {
                false
            }
        } else {
            false
        };

        if should_remove {
            self.links.remove(&target);
            info!(%target, "idle relay link removed");
        }
    }

    /// Close all links and clear the manager.
    pub async fn close_all(&mut self) {
        for (addr, link) in self.links.drain() {
            info!(%addr, "closing relay link (shutdown)");
            link.close().await;
        }
    }

    /// Number of active links.
    pub fn link_count(&self) -> usize {
        self.links.len()
    }

    /// Total number of sessions being forwarded across all links.
    pub fn session_count(&self) -> usize {
        self.links.values().map(|l| l.session_count()).sum()
    }

    /// Insert a pre-built relay link (for testing or manual setup).
    pub fn insert(&mut self, link: RelayLink) {
        self.links.insert(link.target_addr(), link);
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    // ---------- RelayLink session tracking ----------

    #[test]
    fn link_manager_tracks_sessions() {
        let mut mgr = RelayLinkManager::new();
        let target1 = addr("10.0.0.2:4433");

        let mut link = RelayLink::stub(target1);
        link.add_session("session-aaa");
        link.add_session("session-bbb");
        mgr.insert(link);

        assert_eq!(mgr.link_count(), 1);
        assert_eq!(mgr.session_count(), 2);

        // Register another session on the same link
        mgr.register_session(target1, "session-ccc");
        assert_eq!(mgr.session_count(), 3);

        // Verify individual link
        let link_ref = mgr.get(&target1).unwrap();
        assert!(link_ref.has_session("session-aaa"));
        assert!(link_ref.has_session("session-bbb"));
        assert!(link_ref.has_session("session-ccc"));
        assert!(!link_ref.has_session("unknown"));
    }

    #[test]
    fn link_manager_idle_detection() {
        let mut link = RelayLink::stub(addr("10.0.0.3:4433"));

        // Empty link is idle
        assert!(link.is_idle());
        assert_eq!(link.session_count(), 0);

        // Add a session — no longer idle
        link.add_session("sess-1");
        assert!(!link.is_idle());
        assert_eq!(link.session_count(), 1);

        // Remove it — idle again
        link.remove_session("sess-1");
        assert!(link.is_idle());
        assert_eq!(link.session_count(), 0);
    }

    #[test]
    fn session_forward_signal_roundtrip() {
        use wzp_proto::SignalMessage;

        // SessionForward roundtrip
        let msg = SignalMessage::SessionForward {
            session_id: "abcd1234".to_string(),
            target_fingerprint: "deadbeef".to_string(),
            source_relay: "10.0.0.1:4433".to_string(),
        };
        let json = serde_json::to_string(&msg).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::SessionForward {
                session_id,
                target_fingerprint,
                source_relay,
            } => {
                assert_eq!(session_id, "abcd1234");
                assert_eq!(target_fingerprint, "deadbeef");
                assert_eq!(source_relay, "10.0.0.1:4433");
            }
            _ => panic!("expected SessionForward variant"),
        }

        // SessionForwardAck roundtrip
        let ack = SignalMessage::SessionForwardAck {
            session_id: "abcd1234".to_string(),
            room_name: "relay-room-42".to_string(),
        };
        let json = serde_json::to_string(&ack).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        match decoded {
            SignalMessage::SessionForwardAck {
                session_id,
                room_name,
            } => {
                assert_eq!(session_id, "abcd1234");
                assert_eq!(room_name, "relay-room-42");
            }
            _ => panic!("expected SessionForwardAck variant"),
        }
    }

    #[test]
    fn link_manager_multi_target() {
        let mut mgr = RelayLinkManager::new();
        let target_a = addr("10.0.0.2:4433");
        let target_b = addr("10.0.0.3:4433");
        let target_c = addr("10.0.0.4:4433");

        for (target, sessions) in [
            (target_a, vec!["s1", "s2"]),
            (target_b, vec!["s3"]),
            (target_c, vec!["s4", "s5", "s6"]),
        ] {
            let mut link = RelayLink::stub(target);
            for s in sessions {
                link.add_session(s);
            }
            mgr.insert(link);
        }

        assert_eq!(mgr.link_count(), 3);
        assert_eq!(mgr.session_count(), 6); // 2 + 1 + 3

        assert_eq!(mgr.get(&target_a).unwrap().session_count(), 2);
        assert_eq!(mgr.get(&target_b).unwrap().session_count(), 1);
        assert_eq!(mgr.get(&target_c).unwrap().session_count(), 3);
    }

    #[test]
    fn link_manager_cleanup() {
        let mut mgr = RelayLinkManager::new();
        let target = addr("10.0.0.5:4433");

        let mut link = RelayLink::stub(target);
        link.add_session("s1");
        link.add_session("s2");
        link.add_session("s3");
        mgr.insert(link);

        assert_eq!(mgr.link_count(), 1);
        assert_eq!(mgr.session_count(), 3);

        // Remove sessions one by one via the manager's mutable access.
        // We cannot call the async unregister_session with stub links here,
        // so we exercise the synchronous management path directly.
        {
            let link = mgr.get_mut(&target).unwrap();
            link.remove_session("s1");
            assert!(!link.is_idle());
            link.remove_session("s2");
            assert!(!link.is_idle());
            link.remove_session("s3");
            assert!(link.is_idle());
        }

        // All sessions removed — link is idle
        assert_eq!(mgr.session_count(), 0);
        assert!(mgr.get(&target).unwrap().is_idle());

        // Simulate what unregister_session does: remove the idle link
        mgr.links.remove(&target);
        assert_eq!(mgr.link_count(), 0);
    }

    #[test]
    fn register_session_on_nonexistent_link_is_noop() {
        let mut mgr = RelayLinkManager::new();
        // Should not panic, just warn
        mgr.register_session(addr("10.0.0.99:4433"), "orphan-session");
        assert_eq!(mgr.link_count(), 0);
        assert_eq!(mgr.session_count(), 0);
    }

    #[test]
    fn forward_to_nonexistent_link_errors() {
        let mgr = RelayLinkManager::new();
        let target = addr("10.0.0.99:4433");

        let pkt = MediaPacket {
            header: wzp_proto::packet::MediaHeader {
                version: 0,
                is_repair: false,
                codec_id: wzp_proto::CodecId::Opus16k,
                has_quality_report: false,
                fec_ratio_encoded: 0,
                seq: 1,
                timestamp: 100,
                fec_block: 0,
                fec_symbol: 0,
                reserved: 0,
                csrc_count: 0,
            },
            payload: bytes::Bytes::from_static(b"test"),
            quality_report: None,
        };

        let rt = tokio::runtime::Builder::new_current_thread()
            .build()
            .unwrap();
        let result = rt.block_on(mgr.forward_to(target, &pkt));
        assert!(result.is_err());
        assert!(result.unwrap_err().to_string().contains("no relay link"));
    }
}
