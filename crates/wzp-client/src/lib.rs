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
pub mod bench;
pub mod call;
pub mod drift_test;
pub mod echo_test;
pub mod featherchat;
pub mod handshake;
pub mod metrics;
pub mod sweep;

// AudioPlayback always comes from the CPAL path (`audio_io`). We do not
// need OS-level processing on the playback side because Windows's
// communications AEC, once engaged on the capture stream, uses the system
// render mix as the reference signal — it cancels echo from CPAL playback
// (and any other app's audio) without special handling.
#[cfg(feature = "audio")]
pub use audio_io::AudioPlayback;

// AudioCapture: two possible backends. Windows-AEC path when compiled in,
// otherwise the plain CPAL path. The two types share the same public API
// (`start`, `ring`, `stop`, `Drop`) so downstream code is identical.
#[cfg(all(
    feature = "audio",
    any(not(feature = "windows-aec"), not(target_os = "windows"))
))]
pub use audio_io::AudioCapture;

#[cfg(all(feature = "windows-aec", target_os = "windows"))]
pub use audio_wasapi::WasapiAudioCapture as AudioCapture;
pub use call::{CallConfig, CallDecoder, CallEncoder};
pub use handshake::perform_handshake;
