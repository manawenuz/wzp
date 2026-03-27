//! WarzonePhone Relay Daemon
//!
//! Integration crate that wires together all layers into a relay pipeline:
//! recv → decrypt → FEC decode → jitter → FEC encode → encrypt → send
//!
//! Built after the 5 agent crates (proto, codec, fec, crypto, transport) are complete.
