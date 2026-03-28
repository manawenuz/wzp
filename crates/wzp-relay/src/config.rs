//! Relay daemon configuration.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// Configuration for the relay daemon.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct RelayConfig {
    /// Address to listen on for incoming connections (client-facing).
    pub listen_addr: SocketAddr,
    /// Address of the remote relay (for the lossy inter-relay link).
    /// If None, this relay is the destination-side relay.
    pub remote_relay: Option<SocketAddr>,
    /// Maximum concurrent sessions.
    pub max_sessions: usize,
    /// Jitter buffer target depth in packets.
    pub jitter_target_depth: usize,
    /// Jitter buffer maximum depth in packets.
    pub jitter_max_depth: usize,
    /// Logging level (trace, debug, info, warn, error).
    pub log_level: String,
    /// featherChat auth validation URL (e.g., "https://chat.example.com/v1/auth/validate").
    /// If set, clients must present a valid token before joining rooms.
    pub auth_url: Option<String>,
    /// Port for the Prometheus metrics HTTP endpoint (e.g., 9090).
    /// If None, the metrics endpoint is disabled.
    pub metrics_port: Option<u16>,
}

impl Default for RelayConfig {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:4433".parse().unwrap(),
            remote_relay: None,
            max_sessions: 100,
            jitter_target_depth: 50,
            jitter_max_depth: 250,
            log_level: "info".to_string(),
            auth_url: None,
            metrics_port: None,
        }
    }
}
