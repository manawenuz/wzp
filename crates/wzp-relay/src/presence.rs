//! Presence registry — tracks which fingerprints are connected to this relay
//! and to peer relays (via gossip over probe connections).
//!
//! This enables route resolution: given a fingerprint, determine whether the
//! user is local, on a known peer relay, or unknown.

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;

// ---------------------------------------------------------------------------
// Data structures
// ---------------------------------------------------------------------------

/// Where a fingerprint is connected.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum PresenceLocation {
    /// Connected directly to this relay.
    Local,
    /// Connected to a peer relay at the given address.
    Remote(SocketAddr),
}

/// Presence entry for a fingerprint connected directly to this relay.
#[derive(Clone, Debug)]
pub struct LocalPresence {
    pub fingerprint: String,
    pub alias: Option<String>,
    pub connected_at: Instant,
    pub room: Option<String>,
}

/// Presence entry for a fingerprint reported by a peer relay.
#[derive(Clone, Debug)]
pub struct RemotePresence {
    pub fingerprint: String,
    pub relay_addr: SocketAddr,
    pub last_seen: Instant,
}

/// Known peer relay and its reported fingerprints.
#[derive(Clone, Debug)]
pub struct PeerRelay {
    pub addr: SocketAddr,
    pub fingerprints: HashSet<String>,
    pub last_update: Instant,
    pub rtt_ms: Option<f64>,
}

// ---------------------------------------------------------------------------
// Registry
// ---------------------------------------------------------------------------

/// Central presence registry tracking local and remote fingerprints.
pub struct PresenceRegistry {
    /// Fingerprints connected directly to THIS relay.
    local: HashMap<String, LocalPresence>,
    /// Fingerprints reported by peer relays (via gossip).
    remote: HashMap<String, RemotePresence>,
    /// Known peer relays and their status.
    peers: HashMap<SocketAddr, PeerRelay>,
}

impl PresenceRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self {
            local: HashMap::new(),
            remote: HashMap::new(),
            peers: HashMap::new(),
        }
    }

    /// Register a fingerprint as locally connected (called after auth + handshake).
    pub fn register_local(&mut self, fingerprint: &str, alias: Option<String>, room: Option<String>) {
        self.local.insert(fingerprint.to_string(), LocalPresence {
            fingerprint: fingerprint.to_string(),
            alias,
            connected_at: Instant::now(),
            room,
        });
    }

    /// Unregister a locally connected fingerprint (called on disconnect).
    pub fn unregister_local(&mut self, fingerprint: &str) {
        self.local.remove(fingerprint);
    }

    /// Update the fingerprints reported by a peer relay.
    /// Replaces the previous set for that peer.
    pub fn update_peer(&mut self, addr: SocketAddr, fingerprints: HashSet<String>) {
        let now = Instant::now();

        // Remove old remote entries that belonged to this peer
        self.remote.retain(|_, rp| rp.relay_addr != addr);

        // Insert new remote entries
        for fp in &fingerprints {
            self.remote.insert(fp.clone(), RemotePresence {
                fingerprint: fp.clone(),
                relay_addr: addr,
                last_seen: now,
            });
        }

        // Update the peer record
        let peer = self.peers.entry(addr).or_insert_with(|| PeerRelay {
            addr,
            fingerprints: HashSet::new(),
            last_update: now,
            rtt_ms: None,
        });
        peer.fingerprints = fingerprints;
        peer.last_update = now;
    }

    /// Look up where a fingerprint is connected.
    /// Local presence takes priority over remote.
    pub fn lookup(&self, fingerprint: &str) -> Option<PresenceLocation> {
        if self.local.contains_key(fingerprint) {
            return Some(PresenceLocation::Local);
        }
        if let Some(rp) = self.remote.get(fingerprint) {
            return Some(PresenceLocation::Remote(rp.relay_addr));
        }
        None
    }

    /// Return all fingerprints connected directly to this relay.
    pub fn local_fingerprints(&self) -> HashSet<String> {
        self.local.keys().cloned().collect()
    }

    /// Return a full dump of every known fingerprint and its location.
    pub fn all_known(&self) -> Vec<(String, PresenceLocation)> {
        let mut out = Vec::new();
        for fp in self.local.keys() {
            out.push((fp.clone(), PresenceLocation::Local));
        }
        for (fp, rp) in &self.remote {
            // Skip if also local (local wins)
            if !self.local.contains_key(fp) {
                out.push((fp.clone(), PresenceLocation::Remote(rp.relay_addr)));
            }
        }
        out
    }

    /// Remove remote entries older than `timeout`.
    pub fn expire_stale(&mut self, timeout: Duration) {
        let cutoff = Instant::now() - timeout;

        // Expire remote presence entries
        self.remote.retain(|_, rp| rp.last_seen > cutoff);

        // Expire peer relay records and their fingerprint sets
        let stale_peers: Vec<SocketAddr> = self.peers
            .iter()
            .filter(|(_, p)| p.last_update <= cutoff)
            .map(|(addr, _)| *addr)
            .collect();

        for addr in stale_peers {
            self.peers.remove(&addr);
        }
    }

    /// Return a reference to the peer relay map (for HTTP API).
    pub fn peers(&self) -> &HashMap<SocketAddr, PeerRelay> {
        &self.peers
    }

    /// Return a reference to the local presence map (for HTTP API).
    pub fn local_entries(&self) -> &HashMap<String, LocalPresence> {
        &self.local
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    #[test]
    fn register_and_lookup_local() {
        let mut reg = PresenceRegistry::new();
        reg.register_local("aabbccdd", Some("alice".into()), Some("room1".into()));

        assert_eq!(reg.lookup("aabbccdd"), Some(PresenceLocation::Local));
        // Unknown fingerprint returns None
        assert_eq!(reg.lookup("00000000"), None);
    }

    #[test]
    fn unregister_removes() {
        let mut reg = PresenceRegistry::new();
        reg.register_local("aabbccdd", None, None);
        assert_eq!(reg.lookup("aabbccdd"), Some(PresenceLocation::Local));

        reg.unregister_local("aabbccdd");
        assert_eq!(reg.lookup("aabbccdd"), None);
    }

    #[test]
    fn update_peer_and_lookup() {
        let mut reg = PresenceRegistry::new();
        let peer = addr("10.0.0.2:4433");
        let mut fps = HashSet::new();
        fps.insert("deadbeef".to_string());
        fps.insert("cafebabe".to_string());

        reg.update_peer(peer, fps);

        assert_eq!(reg.lookup("deadbeef"), Some(PresenceLocation::Remote(peer)));
        assert_eq!(reg.lookup("cafebabe"), Some(PresenceLocation::Remote(peer)));
        assert_eq!(reg.lookup("unknown"), None);
    }

    #[test]
    fn expire_stale_removes_old() {
        let mut reg = PresenceRegistry::new();
        let peer = addr("10.0.0.3:4433");

        let mut fps = HashSet::new();
        fps.insert("olduser".to_string());
        reg.update_peer(peer, fps);

        // Verify it's there
        assert_eq!(reg.lookup("olduser"), Some(PresenceLocation::Remote(peer)));

        // Manually backdate the last_seen and last_update
        if let Some(rp) = reg.remote.get_mut("olduser") {
            rp.last_seen = Instant::now() - Duration::from_secs(120);
        }
        if let Some(p) = reg.peers.get_mut(&peer) {
            p.last_update = Instant::now() - Duration::from_secs(120);
        }

        // Expire with 60s timeout — should remove the 120s-old entries
        reg.expire_stale(Duration::from_secs(60));

        assert_eq!(reg.lookup("olduser"), None);
        assert!(reg.peers.get(&peer).is_none());
    }

    #[test]
    fn local_fingerprints_list() {
        let mut reg = PresenceRegistry::new();
        reg.register_local("fp1", None, None);
        reg.register_local("fp2", Some("bob".into()), Some("room-a".into()));
        reg.register_local("fp3", None, None);

        let fps = reg.local_fingerprints();
        assert_eq!(fps.len(), 3);
        assert!(fps.contains("fp1"));
        assert!(fps.contains("fp2"));
        assert!(fps.contains("fp3"));
    }

    #[test]
    fn all_known_includes_local_and_remote() {
        let mut reg = PresenceRegistry::new();
        reg.register_local("local1", None, None);

        let peer = addr("10.0.0.5:4433");
        let mut fps = HashSet::new();
        fps.insert("remote1".to_string());
        reg.update_peer(peer, fps);

        let all = reg.all_known();
        assert_eq!(all.len(), 2);

        let local_entries: Vec<_> = all.iter()
            .filter(|(_, loc)| *loc == PresenceLocation::Local)
            .collect();
        assert_eq!(local_entries.len(), 1);
        assert_eq!(local_entries[0].0, "local1");

        let remote_entries: Vec<_> = all.iter()
            .filter(|(_, loc)| matches!(loc, PresenceLocation::Remote(_)))
            .collect();
        assert_eq!(remote_entries.len(), 1);
        assert_eq!(remote_entries[0].0, "remote1");
    }

    #[test]
    fn local_overrides_remote_in_lookup() {
        let mut reg = PresenceRegistry::new();
        let peer = addr("10.0.0.6:4433");

        // Register as remote first
        let mut fps = HashSet::new();
        fps.insert("dupfp".to_string());
        reg.update_peer(peer, fps);
        assert_eq!(reg.lookup("dupfp"), Some(PresenceLocation::Remote(peer)));

        // Now register locally — local should win
        reg.register_local("dupfp", None, None);
        assert_eq!(reg.lookup("dupfp"), Some(PresenceLocation::Local));
    }

    #[test]
    fn update_peer_replaces_old_fingerprints() {
        let mut reg = PresenceRegistry::new();
        let peer = addr("10.0.0.7:4433");

        let mut fps1 = HashSet::new();
        fps1.insert("user_a".to_string());
        fps1.insert("user_b".to_string());
        reg.update_peer(peer, fps1);

        assert_eq!(reg.lookup("user_a"), Some(PresenceLocation::Remote(peer)));
        assert_eq!(reg.lookup("user_b"), Some(PresenceLocation::Remote(peer)));

        // Update with only user_b — user_a should be gone
        let mut fps2 = HashSet::new();
        fps2.insert("user_b".to_string());
        reg.update_peer(peer, fps2);

        assert_eq!(reg.lookup("user_a"), None);
        assert_eq!(reg.lookup("user_b"), Some(PresenceLocation::Remote(peer)));
    }
}
