//! Route resolution — given a target fingerprint, find the relay chain
//! needed to reach that user.
//!
//! Uses the [`PresenceRegistry`] as its data source. Currently supports
//! single-hop resolution (local or direct peer). The `resolve_multi_hop`
//! method has the signature for future multi-hop expansion but falls back
//! to single-hop for now.

use std::net::SocketAddr;

use serde::Serialize;

use crate::presence::{PresenceLocation, PresenceRegistry};

// ---------------------------------------------------------------------------
// Route type
// ---------------------------------------------------------------------------

/// The resolved route to a target fingerprint.
#[derive(Clone, Debug, PartialEq, Eq, Serialize)]
pub enum Route {
    /// Target is connected to this relay directly.
    Local,
    /// Target is on a directly connected peer relay.
    DirectPeer(SocketAddr),
    /// Target is reachable via a chain of relays (multi-hop).
    Chain(Vec<SocketAddr>),
    /// Target not found in any known relay.
    NotFound,
}

impl std::fmt::Display for Route {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Route::Local => write!(f, "local"),
            Route::DirectPeer(addr) => write!(f, "direct_peer({})", addr),
            Route::Chain(chain) => {
                let addrs: Vec<String> = chain.iter().map(|a| a.to_string()).collect();
                write!(f, "chain({})", addrs.join(" -> "))
            }
            Route::NotFound => write!(f, "not_found"),
        }
    }
}

// ---------------------------------------------------------------------------
// RouteResolver
// ---------------------------------------------------------------------------

/// Resolves fingerprints to relay routes using the presence registry.
pub struct RouteResolver {
    /// Our own relay address (how peers know us).
    local_addr: SocketAddr,
}

impl RouteResolver {
    /// Create a new route resolver for the relay at `local_addr`.
    pub fn new(local_addr: SocketAddr) -> Self {
        Self { local_addr }
    }

    /// Our local relay address.
    pub fn local_addr(&self) -> SocketAddr {
        self.local_addr
    }

    /// Look up a fingerprint in the registry and return the route.
    ///
    /// - If `registry.lookup()` returns `Local` -> `Route::Local`
    /// - If returns `Remote(addr)` -> `Route::DirectPeer(addr)`
    /// - If not found -> `Route::NotFound`
    pub fn resolve(&self, registry: &PresenceRegistry, target_fingerprint: &str) -> Route {
        match registry.lookup(target_fingerprint) {
            Some(PresenceLocation::Local) => Route::Local,
            Some(PresenceLocation::Remote(addr)) => Route::DirectPeer(addr),
            None => Route::NotFound,
        }
    }

    /// Multi-hop route resolution (future expansion).
    ///
    /// For now this is equivalent to `resolve()` — single-hop only.
    /// When multi-hop is implemented, this will query peers transitively
    /// up to `max_hops` relays deep, using `RouteQuery` / `RouteResponse`
    /// signals over probe connections.
    pub fn resolve_multi_hop(
        &self,
        registry: &PresenceRegistry,
        target: &str,
        _max_hops: usize,
    ) -> Route {
        // Phase 1: single-hop only (same as resolve).
        // Future: if resolve returns NotFound and max_hops > 0,
        // send RouteQuery to each known peer with ttl = max_hops - 1,
        // collect RouteResponse, and build a Chain.
        self.resolve(registry, target)
    }

    /// Build a JSON-serializable route response for the HTTP API.
    pub fn route_json(
        &self,
        fingerprint: &str,
        route: &Route,
    ) -> serde_json::Value {
        let (route_type, relay_chain) = match route {
            Route::Local => ("local", vec![self.local_addr.to_string()]),
            Route::DirectPeer(addr) => ("direct_peer", vec![self.local_addr.to_string(), addr.to_string()]),
            Route::Chain(chain) => {
                let mut addrs = vec![self.local_addr.to_string()];
                addrs.extend(chain.iter().map(|a| a.to_string()));
                ("chain", addrs)
            }
            Route::NotFound => ("not_found", vec![]),
        };

        serde_json::json!({
            "fingerprint": fingerprint,
            "route": route_type,
            "relay_chain": relay_chain,
        })
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::HashSet;
    use std::net::SocketAddr;

    fn addr(s: &str) -> SocketAddr {
        s.parse().unwrap()
    }

    fn make_resolver() -> RouteResolver {
        RouteResolver::new(addr("10.0.0.1:4433"))
    }

    #[test]
    fn resolve_local() {
        let resolver = make_resolver();
        let mut reg = PresenceRegistry::new();
        reg.register_local("aabbccdd", Some("alice".into()), Some("room1".into()));

        let route = resolver.resolve(&reg, "aabbccdd");
        assert_eq!(route, Route::Local);
    }

    #[test]
    fn resolve_direct_peer() {
        let resolver = make_resolver();
        let mut reg = PresenceRegistry::new();
        let peer = addr("10.0.0.2:4433");
        let mut fps = HashSet::new();
        fps.insert("deadbeef".to_string());
        reg.update_peer(peer, fps);

        let route = resolver.resolve(&reg, "deadbeef");
        assert_eq!(route, Route::DirectPeer(peer));
    }

    #[test]
    fn resolve_not_found() {
        let resolver = make_resolver();
        let reg = PresenceRegistry::new();

        let route = resolver.resolve(&reg, "unknown_fp");
        assert_eq!(route, Route::NotFound);
    }

    #[test]
    fn resolve_multi_hop_fallback() {
        // multi-hop currently falls back to single-hop behavior
        let resolver = make_resolver();
        let mut reg = PresenceRegistry::new();
        reg.register_local("local_fp", None, None);

        let peer = addr("10.0.0.3:4433");
        let mut fps = HashSet::new();
        fps.insert("remote_fp".to_string());
        reg.update_peer(peer, fps);

        // Local lookup works via multi-hop
        assert_eq!(resolver.resolve_multi_hop(&reg, "local_fp", 3), Route::Local);
        // Remote lookup works via multi-hop
        assert_eq!(
            resolver.resolve_multi_hop(&reg, "remote_fp", 3),
            Route::DirectPeer(peer)
        );
        // Not-found works via multi-hop
        assert_eq!(
            resolver.resolve_multi_hop(&reg, "nobody", 3),
            Route::NotFound
        );
    }

    #[test]
    fn route_query_signal_roundtrip() {
        use wzp_proto::SignalMessage;

        let query = SignalMessage::RouteQuery {
            fingerprint: "aabbccdd".to_string(),
            ttl: 3,
        };
        let json = serde_json::to_string(&query).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            SignalMessage::RouteQuery { ref fingerprint, ttl }
            if fingerprint == "aabbccdd" && ttl == 3
        ));

        let response = SignalMessage::RouteResponse {
            fingerprint: "aabbccdd".to_string(),
            found: true,
            relay_chain: vec!["10.0.0.1:4433".to_string(), "10.0.0.2:4433".to_string()],
        };
        let json = serde_json::to_string(&response).unwrap();
        let decoded: SignalMessage = serde_json::from_str(&json).unwrap();
        assert!(matches!(
            decoded,
            SignalMessage::RouteResponse { ref fingerprint, found, ref relay_chain }
            if fingerprint == "aabbccdd" && found && relay_chain.len() == 2
        ));
    }

    #[test]
    fn route_display() {
        assert_eq!(Route::Local.to_string(), "local");
        assert_eq!(
            Route::DirectPeer(addr("10.0.0.2:4433")).to_string(),
            "direct_peer(10.0.0.2:4433)"
        );
        assert_eq!(
            Route::Chain(vec![addr("10.0.0.2:4433"), addr("10.0.0.3:4433")]).to_string(),
            "chain(10.0.0.2:4433 -> 10.0.0.3:4433)"
        );
        assert_eq!(Route::NotFound.to_string(), "not_found");

        // Debug is also useful
        let debug = format!("{:?}", Route::Local);
        assert!(debug.contains("Local"));
    }

    #[test]
    fn route_json_output() {
        let resolver = make_resolver();

        let json = resolver.route_json("fp1", &Route::Local);
        assert_eq!(json["route"], "local");
        assert_eq!(json["fingerprint"], "fp1");
        assert_eq!(json["relay_chain"].as_array().unwrap().len(), 1);

        let json = resolver.route_json("fp2", &Route::DirectPeer(addr("10.0.0.2:4433")));
        assert_eq!(json["route"], "direct_peer");
        assert_eq!(json["relay_chain"].as_array().unwrap().len(), 2);

        let json = resolver.route_json("fp3", &Route::NotFound);
        assert_eq!(json["route"], "not_found");
        assert_eq!(json["relay_chain"].as_array().unwrap().len(), 0);
    }
}
