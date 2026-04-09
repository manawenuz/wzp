//! WarzonePhone Android native VoIP engine.
//!
//! Provides:
//! - Oboe audio backend with lock-free SPSC ring buffers
//! - Engine orchestrator managing call lifecycle
//! - Codec pipeline thread (encode/decode/FEC/jitter)
//! - Call statistics and command interface
//!
//! On non-Android targets, the Oboe C++ layer compiles as a stub,
//! allowing `cargo check` and unit tests on the host.

pub mod audio_android;
pub mod audio_ring;
pub mod commands;
pub mod engine;
pub mod pipeline;
pub mod signal_mgr;
pub mod stats;
pub mod jni_bridge;
