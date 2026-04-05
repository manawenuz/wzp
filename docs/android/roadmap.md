# Roadmap & Known Gaps

## Current State Summary

The Android client can connect to a WZP relay, complete the crypto handshake, and exchange audio in real-time. Two phones on the same network can talk to each other through the relay.

## What Works (April 2025)

- QUIC transport to relay with room-based SFU
- Full crypto handshake (X25519 ephemeral + Ed25519 signatures)
- Opus 24kbps encoding/decoding at 48kHz
- Lock-free audio I/O via Oboe (capture + playout)
- AEC (acoustic echo cancellation) with 100ms tail
- AGC (automatic gain control)
- RaptorQ FEC encoder/decoder (wired to pipeline)
- Adaptive jitter buffer (10-250 packets)
- UI with connect/disconnect, mute, speaker, live stats
- Random identity seed per app launch

## Known Gaps

### P0 — Must fix for usable calls

| Gap | Impact | Where to fix |
|-----|--------|--------------|
| **Media encryption not applied** | Audio sent in cleartext over QUIC | `engine.rs` — pass `_session` to send/recv, encrypt/decrypt payloads |
| **FEC repair symbols not sent** | No loss recovery — audio gaps on packet loss | `engine.rs` send task — call `fec_encoder.generate_repair()` and send repair packets |
| **Quality reports not sent** | Relay can't monitor quality, no adaptive switching | `engine.rs` — periodically attach `QualityReport` to MediaPacket header |
| **CallService not started** | Call dies when app is backgrounded | `CallViewModel.startCall()` — call `CallService.start(context)` |

### P1 — Important for production

| Gap | Impact | Where to fix |
|-----|--------|--------------|
| **Hardcoded relay address** | Can't change server without rebuild | Add settings screen with `SharedPreferences` |
| **No reconnection logic** | Connection drop = call over | `engine.rs` network task — detect disconnect, retry with backoff |
| **No adaptive quality switching** | Stays on GOOD profile even in bad conditions | Wire `AdaptiveQualityController` to network path quality from `QuinnTransport` |
| **Identity seed not persisted** | New identity every launch | Save seed to Android Keystore or SharedPreferences |
| **No Bluetooth audio routing** | `AudioRouteManager` exists but not wired to UI | Add Bluetooth button to InCallScreen, call `AudioRouteManager` methods |
| **No ringtone/notification for incoming** | Only outgoing calls supported | Need signaling for call setup (currently both sides initiate independently) |

### P2 — Nice to have

| Gap | Impact | Where to fix |
|-----|--------|--------------|
| **No android_logger** | Rust tracing output lost on Android | Add `android_logger` crate, init in `nativeInit()` |
| **Stats don't include network metrics** | Loss/RTT/jitter always 0 | Feed `QuinnTransport.path_quality()` back to stats |
| **No ProGuard/R8 minification** | Release APK larger than necessary | Enable `isMinifyEnabled = true` in build.gradle.kts |
| **Single ABI (arm64-v8a)** | No support for older 32-bit devices or emulators | Add `armeabi-v7a` and `x86_64` to cargo-ndk build |
| **No call history** | Can't see past calls | Add Room database for call log |
| **No contact integration** | Manual relay/room entry | Add contacts with fingerprint-based identity |

## Architecture Evolution Plan

### Phase 1: Make Calls Reliable (current → next)

```
[x] QUIC connection to relay
[x] Crypto handshake
[x] Audio encode/decode pipeline
[ ] Media encryption (ChaCha20-Poly1305)
[ ] FEC repair packet transmission
[ ] Foreground service for background calls
[ ] Reconnection on network change
```

### Phase 2: Quality & Polish

```
[ ] Adaptive quality (GOOD → DEGRADED → CATASTROPHIC switching)
[ ] Quality reports in MediaPacket headers
[ ] Network path quality display (real RTT, loss, jitter)
[ ] Settings screen (relay, room, seed persistence)
[ ] Bluetooth/wired headset audio routing
[ ] Rust android_logger for debugging
```

### Phase 3: Production Features

```
[ ] featherChat authentication
[ ] Persistent identity (Android Keystore)
[ ] Push notifications for incoming calls
[ ] Multi-party rooms (already supported by relay)
[ ] Call transfer
[ ] End-to-end encryption (bypass relay decryption)
```

## Dependency Upgrade Path

### quinn 0.11 → 0.12 (when released)

Quinn 0.12 will likely require rustls 0.24. Update both together:
1. `Cargo.toml`: bump quinn and rustls versions
2. Check `client_config()` and `server_config()` in wzp-transport for API changes
3. DATAGRAM API may change — check `send_datagram()` / `read_datagram()`

### Compose BOM 2024.01 → 2025.x

The `LinearProgressIndicator` `progress` parameter changed from `Float` to `() -> Float` in Material3 1.2+. If upgrading the BOM:

```kotlin
// Old (current):
LinearProgressIndicator(progress = level, ...)

// New (Material3 1.2+):
LinearProgressIndicator(progress = { level }, ...)
```

### Kotlin 1.9 → 2.x

Kotlin 2.0 changed the Compose compiler plugin. Update `kotlinCompilerExtensionVersion` in `composeOptions` and the Kotlin Gradle plugin version together.
