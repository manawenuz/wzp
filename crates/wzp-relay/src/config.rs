//! Relay daemon configuration.

use serde::{Deserialize, Serialize};
use std::net::SocketAddr;

/// A federated peer relay.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct PeerConfig {
    /// Address of the peer relay (e.g., "193.180.213.68:4433").
    pub url: String,
    /// Expected TLS certificate fingerprint (hex, with colons).
    pub fingerprint: String,
    /// Optional human-readable label.
    #[serde(default)]
    pub label: Option<String>,
}

/// A trusted relay — accepts inbound federation without needing the peer's address.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct TrustedConfig {
    /// Expected TLS certificate fingerprint (hex, with colons).
    pub fingerprint: String,
    /// Optional human-readable label.
    #[serde(default)]
    pub label: Option<String>,
}

/// A room declared global — bridged across all federated peers.
#[derive(Clone, Debug, Serialize, Deserialize)]
pub struct GlobalRoomConfig {
    /// Room name to bridge (e.g., "android").
    pub name: String,
}

/// Configuration for the relay daemon.
///
/// All fields have defaults, so a minimal TOML file only needs the
/// fields you want to override (e.g., just `[[peers]]`).
#[derive(Clone, Debug, Serialize, Deserialize)]
#[serde(default)]
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
    /// Peer relay addresses to probe for health monitoring.
    /// Each target gets a persistent QUIC connection sending 1 Ping/s.
    #[serde(default)]
    pub probe_targets: Vec<SocketAddr>,
    /// Enable mesh mode: each relay probes all configured targets concurrently.
    /// Discovery is manual via multiple --probe flags; this flag signals intent.
    #[serde(default)]
    pub probe_mesh: bool,
    /// Enable trunk batching for outgoing media in room mode.
    /// When true, packets destined for the same receiver are accumulated into
    /// [`TrunkFrame`]s and flushed every 5 ms (or when the batcher is full),
    /// reducing per-packet QUIC datagram overhead.
    #[serde(default)]
    pub trunking_enabled: bool,
    /// Port for the WebSocket listener (browser clients connect here).
    /// If None, WebSocket support is disabled.
    pub ws_port: Option<u16>,
    /// Directory to serve static files from (HTML/JS/WASM for web clients).
    pub static_dir: Option<String>,
    /// Federation peer relays.
    #[serde(default)]
    pub peers: Vec<PeerConfig>,
    /// Global rooms bridged across federation.
    #[serde(default)]
    pub global_rooms: Vec<GlobalRoomConfig>,
    /// Trusted relay fingerprints — accept inbound federation from these relays.
    /// Unlike [[peers]], no url is needed — the peer connects to us.
    #[serde(default)]
    pub trusted: Vec<TrustedConfig>,
    /// Debug tap: log packet headers for matching rooms ("*" = all rooms).
    /// Activated via --debug-tap <room> or debug_tap = "room" in TOML.
    pub debug_tap: Option<String>,
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
            probe_targets: Vec::new(),
            probe_mesh: false,
            trunking_enabled: false,
            ws_port: None,
            static_dir: None,
            peers: Vec::new(),
            global_rooms: Vec::new(),
            trusted: Vec::new(),
            debug_tap: None,
        }
    }
}

/// Load relay configuration from a TOML file.
pub fn load_config(path: &str) -> Result<RelayConfig, anyhow::Error> {
    let content = std::fs::read_to_string(path)?;
    let config: RelayConfig = toml::from_str(&content)?;
    Ok(config)
}

/// Load config from path, or create an example config file if it doesn't exist.
pub fn load_or_create_config(path: &str) -> Result<RelayConfig, anyhow::Error> {
    let p = std::path::Path::new(path);
    if p.exists() {
        return load_config(path);
    }
    // Create parent directory if needed
    if let Some(parent) = p.parent() {
        std::fs::create_dir_all(parent)?;
    }
    // Write example config
    let example = EXAMPLE_CONFIG;
    std::fs::write(p, example)?;
    eprintln!("Created example config at {path} — edit it and restart.");
    let config: RelayConfig = toml::from_str(example)?;
    Ok(config)
}

/// Example TOML configuration written when --config points to a non-existent file.
pub const EXAMPLE_CONFIG: &str = r#"# WarzonePhone Relay Configuration
# See docs/ADMINISTRATION.md for full reference.

# Listen address for client connections
listen_addr = "0.0.0.0:4433"

# Maximum concurrent sessions
# max_sessions = 100

# Prometheus metrics endpoint (uncomment to enable)
# metrics_port = 9090

# featherChat auth endpoint (uncomment to enable)
# auth_url = "https://chat.example.com/v1/auth/validate"

# Federation: peer relays we connect to (outbound)
# [[peers]]
# url = "relay-b.example.com:4433"
# fingerprint = "aa:bb:cc:dd:..."
# label = "Relay B"

# Federation: relays we trust inbound connections from
# [[trusted]]
# fingerprint = "ee:ff:00:11:..."
# label = "Relay X"

# Global rooms bridged across all federated peers
# [[global_rooms]]
# name = "general"

# Debug: log packet headers for a room ("*" for all)
# debug_tap = "*"
"#;
