//! WarzonePhone Transport Layer
//!
//! QUIC-based transport using quinn with:
//! - DATAGRAM frames for unreliable media packets
//! - Reliable streams for signaling messages
//! - Path quality monitoring (EWMA loss, RTT, bandwidth estimation)
//! - Connection lifecycle management
//!
//! ## Architecture
//!
//! - `config` — QUIC configuration tuned for lossy VoIP links
//! - `datagram` — DATAGRAM frame serialization and MTU management
//! - `reliable` — Length-prefixed JSON framing over reliable QUIC streams
//! - `path_monitor` — EWMA-based PathQuality estimation
//! - `quic` — `QuinnTransport` implementing the `MediaTransport` trait
//! - `connection` — Connection lifecycle (create endpoint, connect, accept)

pub mod config;
pub mod connection;
pub mod datagram;
pub mod path_monitor;
pub mod quic;
pub mod reliable;

pub use config::{client_config, server_config, server_config_from_seed, tls_fingerprint};
pub use connection::{accept, connect, create_endpoint, create_ipv6_endpoint};
pub use path_monitor::PathMonitor;
pub use quic::{QuinnPathSnapshot, QuinnTransport};
pub use wzp_proto::{MediaTransport, PathQuality, TransportError};

// Re-export the quinn Endpoint type so downstream crates (wzp-desktop) can
// thread a shared endpoint between signaling and media connections without
// needing to depend on quinn directly.
pub use quinn::Endpoint;
