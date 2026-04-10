//! WarzonePhone Client Library
//!
//! End-to-end voice call pipeline:
//! - **Send**: mic → encode (Opus/Codec2) → FEC → encrypt → QUIC DATAGRAM
//! - **Recv**: QUIC DATAGRAM → decrypt → FEC decode → jitter buffer → decode → speaker
//!
//! Targets: Android (JNI), Windows desktop, macOS/Linux (testing)

#[cfg(feature = "audio")]
pub mod audio_io;
#[cfg(feature = "audio")]
pub mod audio_ring;
// VoiceProcessingIO is an Apple Core Audio API — only compile the module
// when the `vpio` feature is on AND we're targeting macOS. Enabling the
// feature on Windows/Linux was previously silently broken.
#[cfg(all(feature = "vpio", target_os = "macos"))]
pub mod audio_vpio;
pub mod bench;
pub mod call;
pub mod drift_test;
pub mod echo_test;
pub mod featherchat;
pub mod handshake;
pub mod metrics;
pub mod sweep;

#[cfg(feature = "audio")]
pub use audio_io::{AudioCapture, AudioPlayback};
pub use call::{CallConfig, CallDecoder, CallEncoder};
pub use handshake::perform_handshake;
