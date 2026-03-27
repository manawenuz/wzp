//! WarzonePhone Relay Daemon
//!
//! Integration crate that wires together all layers into a relay pipeline:
//! recv → FEC decode → jitter buffer → FEC encode → send
//!
//! The relay forwards media between two QUIC endpoints without decoding audio.
//! It operates on FEC-protected packets, managing loss recovery and adaptive
//! quality transitions.

pub mod config;
pub mod handshake;
pub mod pipeline;
pub mod session_mgr;

pub use config::RelayConfig;
pub use handshake::accept_handshake;
pub use pipeline::{PipelineConfig, PipelineStats, RelayPipeline};
pub use session_mgr::{RelaySession, SessionId, SessionManager};
