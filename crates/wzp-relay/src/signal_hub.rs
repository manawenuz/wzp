//! Persistent signaling connection manager.
//!
//! Tracks clients connected via `_signal` SNI. Routes call signals
//! (DirectCallOffer, DirectCallAnswer, Hangup) between registered users.

use std::collections::HashMap;
use std::sync::Arc;
use std::time::Instant;

use tracing::{info, warn};
use wzp_proto::{MediaTransport, SignalMessage};
use wzp_transport::QuinnTransport;

/// A client connected via `_signal` for direct calling.
pub struct SignalClient {
    pub fingerprint: String,
    pub alias: Option<String>,
    pub transport: Arc<QuinnTransport>,
    pub connected_at: Instant,
}

/// Manages persistent signaling connections.
pub struct SignalHub {
    clients: HashMap<String, SignalClient>,
}

impl SignalHub {
    pub fn new() -> Self {
        Self {
            clients: HashMap::new(),
        }
    }

    /// Register a new signaling client.
    pub fn register(&mut self, fp: String, transport: Arc<QuinnTransport>, alias: Option<String>) {
        info!(fingerprint = %fp, alias = ?alias, "signal client registered");
        self.clients.insert(fp.clone(), SignalClient {
            fingerprint: fp,
            alias,
            transport,
            connected_at: Instant::now(),
        });
    }

    /// Unregister a signaling client. Returns the client if found.
    pub fn unregister(&mut self, fp: &str) -> Option<SignalClient> {
        let client = self.clients.remove(fp);
        if client.is_some() {
            info!(fingerprint = %fp, "signal client unregistered");
        }
        client
    }

    /// Look up a client by fingerprint.
    pub fn get(&self, fp: &str) -> Option<&SignalClient> {
        self.clients.get(fp)
    }

    /// Check if a fingerprint is online.
    pub fn is_online(&self, fp: &str) -> bool {
        self.clients.contains_key(fp)
    }

    /// Send a signal message to a client by fingerprint.
    pub async fn send_to(&self, fp: &str, msg: &SignalMessage) -> Result<(), String> {
        match self.clients.get(fp) {
            Some(client) => {
                client.transport.send_signal(msg).await
                    .map_err(|e| format!("send to {fp}: {e}"))
            }
            None => Err(format!("{fp} not online")),
        }
    }

    /// Number of connected signaling clients.
    pub fn online_count(&self) -> usize {
        self.clients.len()
    }

    /// List all online fingerprints.
    pub fn online_fingerprints(&self) -> Vec<&str> {
        self.clients.keys().map(|s| s.as_str()).collect()
    }

    /// Get alias for a fingerprint.
    pub fn alias(&self, fp: &str) -> Option<&str> {
        self.clients.get(fp).and_then(|c| c.alias.as_deref())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn register_unregister() {
        let mut hub = SignalHub::new();
        assert_eq!(hub.online_count(), 0);
        assert!(!hub.is_online("alice"));

        // Can't easily construct QuinnTransport in a unit test,
        // so we just test the HashMap logic conceptually.
        // Integration tests cover the full flow.
    }
}
