# WarzonePhone Development Progress Report

## Phase 1: Protocol Core

**Scope**: Define the protocol types, traits, and core logic in `wzp-proto`.

**What was built**:
- Wire format types: `MediaHeader` (12-byte compact binary), `QualityReport` (4 bytes), `MediaPacket`, `SignalMessage` (8 variants)
- Trait definitions: `AudioEncoder`, `AudioDecoder`, `FecEncoder`, `FecDecoder`, `CryptoSession`, `KeyExchange`, `MediaTransport`, `ObfuscationLayer`, `QualityController`
- `CodecId` enum with 5 variants (Opus24k/16k/6k, Codec2_3200/1200) and 4-bit wire encoding
- `QualityProfile` with 3 preset tiers (GOOD, DEGRADED, CATASTROPHIC)
- `AdaptiveQualityController` with hysteresis (3-down/10-up thresholds, sliding window of 20 reports)
- `JitterBuffer` with BTreeMap-based reordering, wrapping sequence arithmetic, min/max/target depth
- `Session` state machine (Idle -> Connecting -> Handshaking -> Active <-> Rekeying -> Closed)
- Full error type hierarchy (`CodecError`, `FecError`, `CryptoError`, `TransportError`, `ObfuscationError`)

**Tests**: 27 tests across packet roundtrip, quality controller, jitter buffer, session state machine, sequence wrapping

## Phase 2: Implementation Crates (Parallel)

**Scope**: Implement the 4 leaf crates against the trait interfaces, in parallel.

### wzp-codec
- Opus encoder/decoder via `audiopus` (48 kHz mono, VoIP application mode, inband FEC, DTX)
- Codec2 encoder/decoder via pure-Rust `codec2` crate (3200 and 1200 bps modes)
- `AdaptiveEncoder`/`AdaptiveDecoder` wrapping both codecs with transparent switching
- Linear resampler for 48 kHz <-> 8 kHz conversion (box filter downsampling, linear interpolation upsampling)
- All callers work with 48 kHz PCM regardless of active codec

### wzp-fec
- `RaptorQFecEncoder`: accumulates source symbols with 2-byte length prefix + zero padding to 256-byte symbol size
- `RaptorQFecDecoder`: multi-block concurrent decoding with HashMap-based block tracking
- `Interleaver`: round-robin temporal interleaving across multiple FEC blocks
- `BlockManager`: encoder-side (Building/Pending/Sent/Acknowledged) and decoder-side (Assembling/Complete/Expired) lifecycle tracking
- `AdaptiveFec`: maps `QualityProfile` to FEC parameters
- Factory function `create_fec_pair()` for convenient encoder/decoder creation

### wzp-crypto
- `WarzoneKeyExchange`: identity seed -> HKDF -> Ed25519 + X25519, ephemeral generation, signature, verification, session derivation
- `ChaChaSession`: ChaCha20-Poly1305 AEAD with deterministic nonce construction (session_id + seq + direction)
- `RekeyManager`: triggers rekey every 2^16 packets, HKDF mixing of old key + new DH, zeroization of old key
- `AntiReplayWindow`: 1024-packet sliding window bitmap with u16 wrapping support
- Nonce module: 12-byte nonce layout (4-byte session_id + 4-byte seq BE + 1-byte direction + 3-byte padding)

### wzp-transport
- `QuinnTransport`: implements `MediaTransport` trait over quinn QUIC connection
- DATAGRAM frames for unreliable media, bidirectional streams for reliable signaling
- Length-prefixed JSON framing (4-byte BE length + serde_json payload) for signaling
- VoIP-tuned QUIC configuration (30s idle timeout, 5s keepalive, conservative flow control, 300ms initial RTT)
- `PathMonitor`: EWMA-smoothed loss, RTT, jitter, bandwidth estimation
- Connection lifecycle: `create_endpoint()`, `connect()`, `accept()`
- Self-signed certificate generation for testing

**Tests**: 55+ tests across all 4 crates (codec roundtrip, FEC recovery at 30/50/70% loss, crypto encrypt/decrypt, handshake, anti-replay, transport serialization, path monitoring)

## Phase 3: Integration (Relay + Client)

**Scope**: Wire all layers together into working relay and client binaries.

### wzp-relay
- Room mode (SFU): `RoomManager` with named rooms, auto-create/auto-delete, per-participant forwarding
- Forward mode: two-pipeline architecture (upstream/downstream) with FEC re-encode and jitter buffering
- `RelayPipeline`: ingest -> FEC decode -> jitter buffer -> pop -> FEC re-encode -> send
- `SessionManager`: tracks active sessions, max session limit, idle expiration
- Relay-side handshake: `accept_handshake()` with signature verification and profile negotiation
- `RelayConfig`: configurable listen address, remote relay, max sessions, jitter parameters
- Periodic stats logging (upstream/downstream packet counts)

### wzp-client
- `CallEncoder`: PCM -> audio encode -> FEC block management -> source + repair MediaPackets
- `CallDecoder`: MediaPacket -> FEC decode -> jitter buffer -> audio decode -> PCM
- Client-side handshake: `perform_handshake()` with ephemeral key exchange and signature
- CLI modes: silence test, tone generation (440 Hz), file send, file record, echo test, live audio
- `AudioCapture`/`AudioPlayback` via cpal (behind `audio` feature flag), supporting both i16 and f32 sample formats
- Automated echo test with windowed analysis (loss, SNR, correlation, degradation detection)
- Benchmark suite: codec roundtrip (1000 frames), FEC recovery (100 blocks), crypto throughput (30000 packets), full pipeline (50 frames)

**Tests**: 25+ tests for pipeline creation, packet generation, FEC repair generation, session management

## Phase 4: Web Bridge, Rooms, PTT, TLS

**Scope**: Browser support and multi-party calling.

### wzp-web
- Axum-based HTTP/WebSocket server
- Browser audio capture via AudioWorklet (primary) with ScriptProcessorNode fallback
- Browser audio playback via AudioWorklet with scheduled BufferSource fallback
- Room-based routing: `/ws/<room-name>` WebSocket endpoint
- Room name passed as QUIC SNI to the relay
- Push-to-talk (PTT) support: button, mouse hold, spacebar
- Audio level meter in the UI
- TLS support via `--tls` flag with self-signed certificate generation
- Auto-reconnection on WebSocket disconnect
- Static file serving for the web UI

## Current Status

### What Works

- Full encode/decode pipeline: PCM -> Opus/Codec2 -> FEC -> MediaPacket -> FEC decode -> audio decode -> PCM
- Adaptive codec switching between Opus and Codec2 (including resampling)
- RaptorQ FEC recovery at various loss rates (tested up to 50% loss)
- ChaCha20-Poly1305 encryption with deterministic nonces
- X25519 key exchange with Ed25519 identity signatures
- QUIC transport with DATAGRAM frames for media and reliable streams for signaling
- Single relay echo mode (connectivity test)
- Multi-party room calls (SFU)
- Two-relay forwarding chain
- Web browser audio via WebSocket bridge
- File-based send/record for testing
- Live microphone/speaker mode (with `audio` feature)
- Push-to-talk in the web UI
- Automated echo quality test with windowed analysis
- Performance benchmarks
- Cross-compilation CI for amd64, arm64, armv7

### Known Issues

- **Jitter buffer drift**: During long echo tests, the jitter buffer depth can drift because there is no adaptive depth adjustment based on observed jitter. The buffer uses sequence-number ordering only, without timestamp-based playout scheduling.

- **Web audio drift**: The browser AudioWorklet playback buffer caps at 200ms, but clock drift between the WebSocket message arrival rate and the AudioContext output rate can cause occasional underruns or accumulation. The cap prevents unbounded growth but may cause glitches.

- **Adaptive loop integration (partial)**: AdaptiveQualityController is wired into both desktop and Android send/recv tasks for **inbound quality report observation**. Relay broadcasts QualityDirective to all participants based on weakest-link policy, but **neither engine processes QualityDirective signals** — they fall through catch-all match arms silently. Local adaptive quality works; relay-coordinated quality does not.

- **dual_path.rs test regression (Phase 7)**: Phase 7 (IPv6 dual-socket) added `ipv6_endpoint: Option<Endpoint>` parameter to `race()` in `crates/wzp-client/src/dual_path.rs`, but the integration tests in `crates/wzp-client/tests/dual_path.rs` were not updated — 3 call sites pass 6 args instead of 7. `cargo test --workspace` fails to compile.

- **Relay FEC pass-through**: In room mode, the relay forwards packets opaquely without FEC decode/re-encode. This means FEC protection is end-to-end only, not per-hop. In forward mode, the relay pipeline does perform FEC decode/re-encode.

- **No certificate verification**: The QUIC client config uses `SkipServerVerification` (accepts any certificate). This is intentional for testing but must be addressed for production deployments.

## Test Coverage

307+ tests across 7 crates (wzp-web has no Rust tests):

| Crate | Test Count |
|-------|------------|
| wzp-proto | ~79 |
| wzp-codec | ~69 |
| wzp-fec | ~21 |
| wzp-crypto | ~21 |
| wzp-transport | ~11 |
| wzp-relay | ~50 |
| wzp-client | ~57 |
| **Total** | **307+** |

Tests cover:
- Wire format roundtrip (header, quality report, full packet)
- Codec encode/decode for all 5 codec IDs
- Adaptive codec switching (Opus <-> Codec2)
- FEC recovery at 0%, 30%, 50% loss
- Concurrent FEC block decoding
- Full key exchange handshake (Alice/Bob derive same session key)
- Encrypt/decrypt roundtrip, wrong-key rejection, wrong-AAD rejection
- Anti-replay window: sequential, out-of-order, duplicate, wrapping
- Rekeying: interval trigger, key derivation, old key zeroization
- QUIC datagram serialization roundtrip
- Path quality EWMA smoothing
- Jitter buffer: ordering, reordering, missing packets, min depth, duplicates
- Session state machine: happy path, invalid transitions, connection loss
- Pipeline packet generation and FEC repair
- Benchmark correctness (codec, FEC, crypto, pipeline)

## Performance Benchmarks

Run with `wzp-bench --all`. Representative results (Apple M-series, single core):

### Codec Roundtrip (Opus 24kbps)
- 1000 frames of 440 Hz sine wave (20ms each, 48 kHz mono)
- Encode: ~20-40 us/frame average
- Decode: ~10-20 us/frame average
- Throughput: >10,000 frames/sec (200x real-time)
- Compression ratio: ~30x (960 i16 samples = 1920 bytes -> ~60 bytes encoded)

### FEC Recovery
- 100 blocks of 5 frames each
- At 20% loss: ~100% recovery rate
- At 30% loss with scaled FEC ratio: >95% recovery rate

### Crypto (ChaCha20-Poly1305)
- 30,000 packets (60/120/256 byte payloads)
- Throughput: >500,000 packets/sec
- Bandwidth: >50 MB/sec
- Average latency: <2 us per encrypt+decrypt cycle

### Full Pipeline (E2E)
- 50 frames through CallEncoder -> CallDecoder
- Average E2E latency: ~100-200 us/frame (codec + FEC, no network)
- Wire overhead ratio: ~0.05-0.10x of raw PCM (high compression from Opus)

## Deployment Status

- **Local testing**: All modes tested on localhost (single relay, room mode, forward mode, web bridge)
- **Hetzner VPS**: Build script (`scripts/build-linux.sh`) tested for provisioning, building, and downloading Linux binaries
- **CI**: Gitea workflow defined for amd64/arm64/armv7 builds
- **Production**: Not yet deployed to production networks

## Recent Changes (2026-04-12)

### Bluetooth Audio Routing
- 3-way route cycling: Earpiece → Speaker → Bluetooth SCO
- `setCommunicationDevice()` API 31+ with `startBluetoothSco()` fallback
- BT-mode Oboe: capture skips 48kHz + VoiceCommunication, Oboe resamples 8/16kHz ↔ 48kHz
- `MODE_IN_COMMUNICATION` deferred to call start (was at app launch — hijacked system audio)

### Network Change Detection
- `NetworkMonitor.kt` wraps `ConnectivityManager.NetworkCallback`
- WiFi/cellular classification via bandwidth heuristics (no READ_PHONE_STATE needed)
- Feeds `AdaptiveQualityController::signal_network_change()` via JNI → AtomicU8 → recv task

### Hangup Signal Fix
- `SignalMessage::Hangup` now carries optional `call_id`
- Relay only ends the named call (not all calls for the user)
- Fixes race: hangup for call 1 no longer kills newly-placed call 2

### Per-Architecture APK Builds
- `build-tauri-android.sh --arch arm64|armv7|all`
- Separate per-arch APKs (~25MB each vs ~50MB universal)
- Release APKs signed with `wzp-release.jks` via `apksigner`

### Continuous DRED Tuning (Phase A: opus-DRED-v2)
- `DredTuner` in `wzp-proto::dred_tuner` maps live network metrics to continuous DRED duration
- Polls quinn path stats every 25 frames (~500ms): loss%, RTT, jitter
- Linear interpolation between baseline and ceiling per codec tier (not discrete tier jumps)
- Jitter-spike detection: >30% EWMA spike pre-emptively boosts DRED to ceiling for ~5s
- RTT phantom loss: high RTT (>200ms) adds phantom contribution to keep DRED generous
- `set_expected_loss()` and `set_dred_duration()` added to `AudioEncoder` trait
- Integrated into both Android and desktop send tasks in engine.rs

### Extended DRED Window
- Opus6k DRED duration increased from 500ms to 1040ms (max libopus 1.5 supports)
- RDO-VAE naturally degrades quality at longer offsets — extra window costs ~1-2 kbps

### PMTUD (Path MTU Discovery)
- Quinn's PLPMTUD explicitly configured: initial 1200, upper bound 1452, 300s interval
- `QuinnPathSnapshot` exposes discovered MTU via `current_mtu` field
- `TrunkedForwarder` refreshes `max_bytes` from PMTUD (was hard-coded 1200)
- Federation trunk frames now fill the discovered path MTU automatically

### New Tests
- 4 DRED tuner integration tests in wzp-client (encoder adjustment, spike boost, Codec2 no-op, profile switch)
- 10 unit tests in wzp-proto for DredTuner mapping logic
- Jitter variance window tests in wzp-transport PathMonitor
- Pre-existing test fixes: added missing `build_version` fields to 7 SignalMessage constructors

### Desktop Adaptive Quality (#7, #31)
- `AdaptiveQualityController` wired into both Android and desktop send/recv tasks
- `pending_profile: Arc<AtomicU8>` bridge between recv (writer) and send (reader)
- Auto mode: ingests QualityReports from relay, switches encoder profile when adapter recommends
- `tx_codec` display string updated on profile switch for UI indicator
- `profile_to_index()` / `index_to_profile()` mapping for 6-tier range

### Relay Coordinated Codec Switching (#25, #26)
- `ParticipantQuality` struct in relay RoomManager tracks per-participant quality
- Quality reports from forwarded packets feed per-participant `AdaptiveQualityController`
- `weakest_tier()` computes room-wide worst tier across all participants
- `QualityDirective` SignalMessage variant: relay broadcasts recommended profile to all participants
- Triggered on tier change — instant, no negotiation (weakest-link policy)

### Oboe Stream State Polling (#35)
- C++ polling loop after `requestStart()`: checks `getState()` every 10ms for up to 2s
- Waits for both capture and playout streams to reach `Started` state
- Logs initial state, poll count, and final state for HAL debugging
- Does NOT fail on timeout — Rust-side stall detector remains as safety net
- Targets Nothing Phone A059 intermittent silent calls on cold start
