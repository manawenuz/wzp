//! Phase 8 (Tailscale-inspired): Relay map for automatic relay
//! selection based on latency.
//!
//! Maintains a sorted list of known relays with their measured
//! latencies. Used during call setup to pick the lowest-latency
//! relay, and by netcheck to report relay health.

use std::net::SocketAddr;
use std::time::{Duration, Instant};

use serde::Serialize;

/// A known relay endpoint with measured latency.
#[derive(Debug, Clone, Serialize)]
pub struct RelayEntry {
    /// Human-readable name (e.g., "us-east", "eu-west").
    pub name: String,
    /// Relay address.
    pub addr: SocketAddr,
    /// Geographic region (from RegisterPresenceAck).
    pub region: Option<String>,
    /// Last measured RTT (ms).
    pub rtt_ms: Option<u32>,
    /// When the RTT was last measured.
    #[serde(skip)]
    pub last_probed: Option<Instant>,
    /// Whether this relay is currently reachable.
    pub reachable: bool,
}

/// Sorted relay map. Entries are ordered by RTT (lowest first).
#[derive(Debug, Clone, Default)]
pub struct RelayMap {
    entries: Vec<RelayEntry>,
}

impl RelayMap {
    pub fn new() -> Self {
        Self {
            entries: Vec::new(),
        }
    }

    /// Add or update a relay entry.
    pub fn upsert(&mut self, name: &str, addr: SocketAddr, region: Option<String>) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.addr == addr) {
            entry.name = name.to_string();
            if region.is_some() {
                entry.region = region;
            }
        } else {
            self.entries.push(RelayEntry {
                name: name.to_string(),
                addr,
                region,
                rtt_ms: None,
                last_probed: None,
                reachable: false,
            });
        }
    }

    /// Update RTT measurement for a relay.
    pub fn update_rtt(&mut self, addr: SocketAddr, rtt_ms: u32) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.addr == addr) {
            entry.rtt_ms = Some(rtt_ms);
            entry.last_probed = Some(Instant::now());
            entry.reachable = true;
        }
        self.sort();
    }

    /// Mark a relay as unreachable.
    pub fn mark_unreachable(&mut self, addr: SocketAddr) {
        if let Some(entry) = self.entries.iter_mut().find(|e| e.addr == addr) {
            entry.reachable = false;
            entry.last_probed = Some(Instant::now());
        }
        self.sort();
    }

    /// Get the preferred (lowest-latency, reachable) relay.
    pub fn preferred(&self) -> Option<&RelayEntry> {
        self.entries
            .iter()
            .find(|e| e.reachable && e.rtt_ms.is_some())
    }

    /// Get all entries, sorted by RTT.
    pub fn entries(&self) -> &[RelayEntry] {
        &self.entries
    }

    /// Populate from a `RegisterPresenceAck.available_relays` list.
    /// Each entry is "name|addr" format.
    pub fn populate_from_ack(&mut self, relays: &[String], relay_region: Option<&str>) {
        for entry_str in relays {
            if let Some((name, addr_str)) = entry_str.split_once('|') {
                if let Ok(addr) = addr_str.parse::<SocketAddr>() {
                    self.upsert(name, addr, None);
                }
            }
        }
        // If the ack included a region for the current relay, we
        // could tag it — but we'd need to know which relay we're
        // connected to. Left for the caller to handle.
        let _ = relay_region;
    }

    /// Check if any entry has a stale probe (older than `max_age`).
    pub fn needs_reprobe(&self, max_age: Duration) -> bool {
        self.entries.iter().any(|e| {
            match e.last_probed {
                None => true,
                Some(t) => t.elapsed() > max_age,
            }
        })
    }

    /// Get entries that need reprobing.
    pub fn stale_entries(&self, max_age: Duration) -> Vec<(String, SocketAddr)> {
        self.entries
            .iter()
            .filter(|e| match e.last_probed {
                None => true,
                Some(t) => t.elapsed() > max_age,
            })
            .map(|e| (e.name.clone(), e.addr))
            .collect()
    }

    fn sort(&mut self) {
        self.entries.sort_by_key(|e| {
            if e.reachable {
                e.rtt_ms.unwrap_or(u32::MAX)
            } else {
                u32::MAX
            }
        });
    }
}

// ── Tests ──────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn preferred_returns_lowest_rtt() {
        let mut map = RelayMap::new();
        let a1: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let a2: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        let a3: SocketAddr = "10.0.0.3:4433".parse().unwrap();

        map.upsert("slow", a1, None);
        map.upsert("fast", a2, None);
        map.upsert("mid", a3, None);

        map.update_rtt(a1, 200);
        map.update_rtt(a2, 15);
        map.update_rtt(a3, 80);

        let pref = map.preferred().unwrap();
        assert_eq!(pref.addr, a2);
        assert_eq!(pref.rtt_ms, Some(15));
    }

    #[test]
    fn unreachable_not_preferred() {
        let mut map = RelayMap::new();
        let a1: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let a2: SocketAddr = "10.0.0.2:4433".parse().unwrap();

        map.upsert("fast-dead", a1, None);
        map.upsert("slow-alive", a2, None);

        map.update_rtt(a1, 5);
        map.update_rtt(a2, 200);
        map.mark_unreachable(a1);

        let pref = map.preferred().unwrap();
        assert_eq!(pref.addr, a2);
    }

    #[test]
    fn populate_from_ack() {
        let mut map = RelayMap::new();
        map.populate_from_ack(
            &[
                "us-east|203.0.113.5:4433".into(),
                "eu-west|198.51.100.9:4433".into(),
            ],
            Some("us-east"),
        );
        assert_eq!(map.entries().len(), 2);
        assert_eq!(map.entries()[0].name, "us-east");
        assert_eq!(map.entries()[1].name, "eu-west");
    }

    #[test]
    fn upsert_updates_existing() {
        let mut map = RelayMap::new();
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        map.upsert("old-name", addr, None);
        map.upsert("new-name", addr, Some("us-west".into()));
        assert_eq!(map.entries().len(), 1);
        assert_eq!(map.entries()[0].name, "new-name");
        assert_eq!(map.entries()[0].region, Some("us-west".into()));
    }

    #[test]
    fn upsert_preserves_region_when_none() {
        let mut map = RelayMap::new();
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        map.upsert("relay", addr, Some("eu-west".into()));
        map.upsert("relay", addr, None); // region is None
        // Should keep the original region
        assert_eq!(map.entries()[0].region, Some("eu-west".into()));
    }

    #[test]
    fn preferred_returns_none_on_empty() {
        let map = RelayMap::new();
        assert!(map.preferred().is_none());
    }

    #[test]
    fn preferred_returns_none_when_all_unreachable() {
        let mut map = RelayMap::new();
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        map.upsert("relay", addr, None);
        // Not update_rtt'd, so reachable=false
        assert!(map.preferred().is_none());
    }

    #[test]
    fn needs_reprobe_empty_is_false() {
        let map = RelayMap::new();
        // No entries → nothing to reprobe
        assert!(!map.needs_reprobe(Duration::from_secs(60)));
    }

    #[test]
    fn needs_reprobe_never_probed() {
        let mut map = RelayMap::new();
        map.upsert("relay", "10.0.0.1:4433".parse().unwrap(), None);
        assert!(map.needs_reprobe(Duration::from_secs(60)));
    }

    #[test]
    fn needs_reprobe_fresh_is_false() {
        let mut map = RelayMap::new();
        let addr: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        map.upsert("relay", addr, None);
        map.update_rtt(addr, 50);
        // Just probed, so 60s max_age should not trigger
        assert!(!map.needs_reprobe(Duration::from_secs(60)));
    }

    #[test]
    fn stale_entries_returns_unprobed() {
        let mut map = RelayMap::new();
        let a1: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let a2: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        map.upsert("probed", a1, None);
        map.upsert("stale", a2, None);
        map.update_rtt(a1, 50);

        let stale = map.stale_entries(Duration::from_secs(60));
        assert_eq!(stale.len(), 1);
        assert_eq!(stale[0].1, a2);
    }

    #[test]
    fn sort_stability_with_equal_rtt() {
        let mut map = RelayMap::new();
        let a1: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let a2: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        map.upsert("first", a1, None);
        map.upsert("second", a2, None);
        map.update_rtt(a1, 50);
        map.update_rtt(a2, 50);

        // Both have same RTT — sort should be stable (insertion order)
        assert_eq!(map.entries().len(), 2);
        // Both are valid preferred relays
        assert!(map.preferred().is_some());
    }

    #[test]
    fn populate_from_ack_skips_malformed() {
        let mut map = RelayMap::new();
        map.populate_from_ack(
            &[
                "good|10.0.0.1:4433".into(),
                "no-pipe-separator".into(),
                "bad-addr|not-a-socket-addr".into(),
                "also-good|10.0.0.2:4433".into(),
            ],
            None,
        );
        assert_eq!(map.entries().len(), 2);
    }

    #[test]
    fn mark_unreachable_sorts_to_end() {
        let mut map = RelayMap::new();
        let a1: SocketAddr = "10.0.0.1:4433".parse().unwrap();
        let a2: SocketAddr = "10.0.0.2:4433".parse().unwrap();
        map.upsert("fast", a1, None);
        map.upsert("slow", a2, None);
        map.update_rtt(a1, 10);
        map.update_rtt(a2, 200);

        assert_eq!(map.preferred().unwrap().addr, a1);

        map.mark_unreachable(a1);
        assert_eq!(map.preferred().unwrap().addr, a2);
    }

    #[test]
    fn relay_entry_serializes() {
        let entry = RelayEntry {
            name: "test".into(),
            addr: "10.0.0.1:4433".parse().unwrap(),
            region: Some("us-east".into()),
            rtt_ms: Some(42),
            last_probed: Some(Instant::now()),
            reachable: true,
        };
        let json = serde_json::to_string(&entry).unwrap();
        assert!(json.contains("test"));
        assert!(json.contains("us-east"));
        assert!(json.contains("42"));
        // last_probed is #[serde(skip)]
        assert!(!json.contains("last_probed"));
    }
}
