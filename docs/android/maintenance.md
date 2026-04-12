# Maintenance Guide

## Code Map — Where to Change Things

### Changing the relay address or room

Edit `CallViewModel.kt`:
```kotlin
companion object {
    const val DEFAULT_RELAY = "172.16.81.125:4433"
    const val DEFAULT_ROOM = "android"
}
```

For a proper settings screen, add a new Composable in `ui/` that persists to `SharedPreferences` and passes values to `viewModel.startCall(relay, room)`.

### Adding authentication

1. In `CallViewModel.startCall()`, pass a token parameter
2. In `engine.rs`, after QUIC connect but before CallOffer, send:
   ```rust
   transport.send_signal(&SignalMessage::AuthToken { token: auth_token }).await?;
   ```
3. Wait for the relay to accept before proceeding to handshake
4. Start relay with `--auth-url <featherchat-endpoint>`

### Enabling media encryption

The crypto session is already derived in `engine.rs` but not applied to packets. To enable:

1. Pass `_session` (currently unused) to the send/recv tasks
2. Before `transport.send_media()`, encrypt the payload:
   ```rust
   let mut ciphertext = Vec::new();
   session.encrypt(&header_bytes, &payload, &mut ciphertext)?;
   packet.payload = Bytes::from(ciphertext);
   ```
3. After `transport.recv_media()`, decrypt:
   ```rust
   let mut plaintext = Vec::new();
   session.decrypt(&header_bytes, &pkt.payload, &mut plaintext)?;
   pkt.payload = Bytes::from(plaintext);
   ```

### Adding a new codec / quality profile

1. Define the profile in `wzp-proto/src/codec_id.rs`
2. Implement `AudioEncoder`/`AudioDecoder` traits in `wzp-codec`
3. Register in `AdaptiveEncoder`/`AdaptiveDecoder` switch logic
4. Add to `supported_profiles` in the CallOffer (engine.rs)

### Changing audio parameters

- **Sample rate**: Change `FRAME_SAMPLES` in `audio_android.rs` and `WzpOboeConfig.sample_rate` in `oboe_bridge.cpp`. Must match the codec's expected rate.
- **Frame duration**: Change `FRAME_SAMPLES` (960 = 20ms at 48kHz, 1920 = 40ms)
- **Ring buffer size**: Change `RING_CAPACITY` in `audio_android.rs`
- **AEC tail length**: Change the `100` in `Pipeline::new()` → `EchoCanceller::new(48000, 100)`

### Adding x86_64 support (emulator)

1. `build.gradle.kts`: add `"x86_64"` to `abiFilters`
2. `cargoNdkBuild` task: add `-t x86_64`
3. `build.rs`: handle `x86_64-linux-android` target for Oboe
4. Note: Oboe in the emulator uses a different audio HAL — audio quality will differ

## Dependency Overview

### Rust Crate Dependencies (wzp-android)

| Crate | Version | Purpose | Upgrade risk |
|-------|---------|---------|--------------|
| `jni` | 0.21 | Java FFI | Low — stable API |
| `tokio` | 1.x | Async runtime | Low |
| `quinn` | 0.11 | QUIC transport | Medium — breaking changes between 0.x |
| `rustls` | 0.23 | TLS for QUIC | Medium — tied to quinn version |
| `serde_json` | 1.x | Stats serialization | Low |
| `anyhow` | 1.x | Error handling | Low |
| `tracing` | 0.1 | Logging | Low |
| `rand` | 0.8 | Random seed generation | Low |

### Workspace Crate Dependencies

| Crate | Purpose | Key trait |
|-------|---------|-----------|
| `wzp-proto` | Shared types and traits | `MediaTransport`, `AudioEncoder`, `KeyExchange` |
| `wzp-codec` | Opus + Codec2 + signal processing | `AdaptiveEncoder`, `EchoCanceller` |
| `wzp-fec` | RaptorQ FEC | `RaptorQFecEncoder` |
| `wzp-crypto` | Key exchange + encryption | `WarzoneKeyExchange`, `ChaChaSession` |
| `wzp-transport` | QUIC connection management | `QuinnTransport`, `connect()` |

### Android/Kotlin Dependencies

| Library | Version | Purpose |
|---------|---------|---------|
| `compose-bom` | 2024.01.00 | Compose version alignment |
| `material3` | (from BOM) | UI components |
| `activity-compose` | 1.8.2 | Activity integration |
| `lifecycle-runtime-ktx` | 2.7.0 | ViewModel + coroutines |
| `core-ktx` | 1.12.0 | Kotlin extensions |

## Updating Dependencies

### Rust

```bash
cargo update -p wzp-android
cargo ndk -t arm64-v8a build --release -p wzp-android
```

Watch for `quinn`/`rustls` version coupling. They must be compatible:
- quinn 0.11 requires rustls 0.23

### Android/Kotlin

Update versions in `android/app/build.gradle.kts`. Key compatibility:
- `kotlinCompilerExtensionVersion` must match the Kotlin version
- `compose-bom` version determines all Compose library versions
- `compileSdk` and `targetSdk` should stay in sync

### NDK

If upgrading the NDK:
1. Update `ndkVersion` in `build.gradle.kts`
2. Update `ANDROID_NDK_HOME` environment variable
3. Update `CC_aarch64_linux_android` and friends
4. Verify Oboe still builds with the new toolchain

## Key Invariants to Preserve

1. **JNI function names must match package structure**: If the Kotlin package changes, all `Java_com_wzp_engine_WzpEngine_*` functions in `jni_bridge.rs` must be renamed.

2. **Manifest uses fully-qualified class names**: Never use `.ClassName` shorthand because the Gradle namespace (`com.wzp.phone`) differs from the Kotlin package (`com.wzp`).

3. **Stats JSON field names are snake_case**: Rust serializes with serde defaults (snake_case). Kotlin's `CallStats.fromJson()` expects `duration_secs`, `loss_pct`, etc.

4. **Ring buffer ordering**: Producer uses Release store on write index, consumer uses Acquire load. Breaking this causes torn reads.

5. **Codec thread owns Pipeline**: Pipeline is `!Send` (Opus encoder state). It must never be accessed from another thread.

6. **panic::catch_unwind on all JNI functions**: Rust panics unwinding across the FFI boundary is UB. Every JNI-exposed function must catch panics.

7. **Channel capacity (64)**: Both `send_tx` and `recv_tx` are bounded at 64 packets. If the network is slow, packets are dropped (`try_send` best-effort).

## Testing

### Unit Tests (Rust)

```bash
# Run all workspace tests (host, not Android)
cargo test

# Run only wzp-android tests (uses oboe_stub.cpp on host)
cargo test -p wzp-android
```

Note: Pipeline, codec, FEC, crypto tests run on the host. Audio tests use stubs.

### On-Device Testing

1. Build and install debug APK
2. Open app, tap CALL
3. Verify in logcat:
   - `WzpEngine created via JNI`
   - `connecting to relay...`
   - `QUIC connected to relay`
   - `CallOffer sent`
   - `handshake complete, call active`
   - `codec thread started`
4. Check stats overlay: frame counters should increment
5. Speak into mic — other connected device should hear audio

### Stress Testing

- Run a call for 30+ minutes — check for memory leaks (stats should be stable)
- Kill and restart the relay — client should eventually get a connection error
- Toggle mute rapidly — verify no crashes
- Switch speaker on/off — verify audio route changes

## Performance Monitoring

Key metrics to watch during a call:

| Metric | Healthy Range | Warning | Critical |
|--------|--------------|---------|----------|
| frames_encoded | Increasing ~50/sec | Stalled | 0 |
| frames_decoded | Increasing ~50/sec | Stalled | 0 |
| underruns | < 5/min | > 20/min | > 100/min |
| jitter_buffer_depth | 2-5 | 0 or >10 | N/A |
| loss_pct | < 5% | 5-20% | > 20% |
| rtt_ms | < 100ms | 100-300ms | > 500ms |
