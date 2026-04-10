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
// WASAPI-direct capture with Windows's OS-level AEC (AudioCategory_Communications).
// Only compiled when `windows-aec` feature is on AND target is Windows. The
// `windows` dependency is itself gated to Windows in Cargo.toml, so enabling
// this feature on non-Windows targets is a no-op.
#[cfg(all(feature = "windows-aec", target_os = "windows"))]
pub mod audio_wasapi;
// WebRTC AEC3 (Audio Processing Module) wrapper around CPAL capture + playback
// on Linux. Only compiled when `linux-aec` feature is on AND target is Linux.
// The webrtc-audio-processing dep is itself gated to Linux in Cargo.toml.
#[cfg(all(feature = "linux-aec", target_os = "linux"))]
pub mod audio_linux_aec;
pub mod bench;
pub mod call;
pub mod drift_test;
pub mod echo_test;
pub mod featherchat;
pub mod handshake;
pub mod metrics;
pub mod sweep;

// AudioPlayback: three possible backends depending on feature flags.
//   1. Default CPAL (`audio_io::AudioPlayback`) — baseline on every platform.
//   2. Linux AEC (`audio_linux_aec::LinuxAecPlayback`) — CPAL + WebRTC APM
//      render-side tee, so echo from speakers gets cancelled from the mic.
//
// On macOS and Windows we always use the default CPAL playback because:
//   - macOS: VoiceProcessingIO handles AEC at the capture side (Apple's
//     native hardware AEC uses its own reference signal handling).
//   - Windows: WASAPI AudioCategory_Communications AEC uses the system
//     render mix as reference — no per-process plumbing needed.
//
// Linux is the only platform where the in-app approach is necessary, so
// the AEC playback path is gated to target_os = "linux".

#[cfg(all(
    feature = "audio",
    any(not(feature = "linux-aec"), not(target_os = "linux"))
))]
pub use audio_io::AudioPlayback;

#[cfg(all(feature = "linux-aec", target_os = "linux"))]
pub use audio_linux_aec::LinuxAecPlayback as AudioPlayback;

// AudioCapture: three possible backends depending on feature flags.
//   1. Default CPAL (`audio_io::AudioCapture`) — baseline on every platform.
//   2. Windows AEC (`audio_wasapi::WasapiAudioCapture`) — direct WASAPI
//      with AudioCategory_Communications, OS APO chain does AEC.
//   3. Linux AEC (`audio_linux_aec::LinuxAecCapture`) — CPAL + WebRTC APM
//      capture-side echo cancellation using the playback tee as reference.
// All three expose the same public API (`start`, `ring`, `stop`, `Drop`).

#[cfg(all(
    feature = "audio",
    any(not(feature = "windows-aec"), not(target_os = "windows")),
    any(not(feature = "linux-aec"), not(target_os = "linux"))
))]
pub use audio_io::AudioCapture;

#[cfg(all(feature = "windows-aec", target_os = "windows"))]
pub use audio_wasapi::WasapiAudioCapture as AudioCapture;

#[cfg(all(feature = "linux-aec", target_os = "linux"))]
pub use audio_linux_aec::LinuxAecCapture as AudioCapture;
pub use call::{CallConfig, CallDecoder, CallEncoder};
pub use handshake::perform_handshake;
