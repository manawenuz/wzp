//! WarzonePhone Client Library
//!
//! Client-side pipeline:
//! mic → encode → FEC → encrypt → send / recv → decrypt → FEC decode → decode → speaker
//!
//! Targets:
//! - Android (via JNI/uniffi)
//! - Windows desktop
//! - macOS/Linux (testing)
//!
//! Built after the 5 agent crates (proto, codec, fec, crypto, transport) are complete.
