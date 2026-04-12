# WarzonePhone Android Client

The WZP Android client is a native VoIP application built with Kotlin/Jetpack Compose on top of a Rust audio engine. It connects to WZP relay servers over QUIC, providing encrypted voice calls with adaptive quality, forward error correction, and acoustic echo cancellation.

## Quick Start

1. **Build**: `cd android && ./gradlew assembleRelease` (requires NDK 26.1, cargo-ndk)
2. **Install**: `adb install app/build/outputs/apk/release/app-release.apk`
3. **Run**: Open "WZ Phone", tap **CALL** to connect to the hardcoded relay
4. **Relay**: Must be running at the configured address (default `172.16.81.125:4433`)

## Current State (April 2025)

| Feature | Status |
|---------|--------|
| QUIC transport to relay | Working |
| Crypto handshake (X25519 + Ed25519) | Working |
| Opus 24k encoding/decoding | Working |
| Oboe audio I/O (48kHz mono) | Working |
| AEC / AGC signal processing | Working |
| RaptorQ FEC | Wired (repair symbols not sent yet) |
| Jitter buffer | Working |
| Adaptive quality switching | Codec-ready, not network-driven yet |
| Authentication (featherChat) | Skipped (relay has no --auth-url) |
| Media encryption (ChaCha20-Poly1305) | Session derived but not applied to packets |
| Foreground service / wake locks | Implemented, not started from UI |

## Documentation Index

- [Architecture](architecture.md) - System design, data flow diagrams, thread model
- [Build Guide](build-guide.md) - Build environment setup, dependencies, signing
- [Debugging](debugging.md) - Crash diagnosis, logcat filters, common issues
- [Maintenance](maintenance.md) - Code map, dependency management, upgrade paths
- [Roadmap](roadmap.md) - Planned work and known gaps

## Key Design Decisions

- **Rust native engine**: All audio processing, codecs, FEC, crypto, and networking run in Rust. Kotlin is UI-only.
- **Lock-free audio**: SPSC ring buffers with atomic ordering between Oboe C++ callbacks and the Rust codec thread. No mutexes in the audio path.
- **cargo-ndk**: The native library (`libwzp_android.so`) is cross-compiled for `arm64-v8a` using cargo-ndk, invoked automatically by Gradle's `cargoNdkBuild` task.
- **Single-activity Compose**: One `CallActivity` hosts all UI via Jetpack Compose with `CallViewModel` as the state holder.
