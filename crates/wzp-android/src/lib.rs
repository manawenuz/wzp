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
//!
//! ## Status
//!
//! **Dead code as of the Tauri mobile rewrite.** The legacy Kotlin+JNI
//! Android app that consumed this crate was replaced by a Tauri 2.x
//! Mobile app (see `desktop/src-tauri/src/engine.rs` for the live
//! Android audio recv path and `crates/wzp-native/` for the Oboe
//! bridge). We keep this crate in the workspace for reference and to
//! preserve the commit history, but it is not built by any shipping
//! target. Allow the accumulated leftover warnings so CI/workspace
//! checks stay clean — any real cleanup should happen as part of
//! removing the crate entirely, not piecemeal.
#![allow(dead_code, unused_imports, unused_variables, unused_mut)]

pub mod audio_android;
pub mod audio_ring;
pub mod commands;
pub mod engine;
pub mod pipeline;
pub mod stats;
pub mod jni_bridge;
