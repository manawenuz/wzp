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

- **Adaptive loop integration (resolved)**: AdaptiveQualityController wired into both desktop and Android send/recv tasks. Relay-coordinated codec switching broadcasts QualityDirective — now handled by both engines (fixed 2026-04-13). 5-tier classification (Studio64k through Catastrophic) with asymmetric hysteresis.

- **Relay FEC pass-through**: In room mode, the relay forwards packets opaquely without FEC decode/re-encode. This means FEC protection is end-to-end only, not per-hop. In forward mode, the relay pipeline does perform FEC decode/re-encode.

- **No certificate verification**: The QUIC client config uses `SkipServerVerification` (accepts any certificate). This is intentional for testing but must be addressed for production deployments.

## Test Coverage

372+ tests across 7 crates (wzp-web has no Rust tests):

| Crate | Test Count |
|-------|------------|
| wzp-proto | ~84 |
| wzp-codec | ~69 |
| wzp-fec | ~21 |
| wzp-crypto | ~21 |
| wzp-transport | ~11 |
| wzp-relay | ~120 |
| wzp-client | ~57 |
| **Total** | **372+** |

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

## Recent Changes (2026-04-13)

### P2P Adaptive Quality (#23, 2026-04-13)
- QualityReport::from_path_stats() — construct reports from local quinn stats
- CallEncoder.pending_quality_report — one-shot attachment to source packets
- Send tasks generate quality reports every 50 frames (~1s) from path stats
- Recv tasks self-observe from own QUIC stats for P2P adaptation
- Both relay and P2P calls now have full adaptive quality

### Protocol Analyzer (#13-17, 2026-04-13)
- New binary: wzp-analyzer (crates/wzp-client/src/analyzer.rs, ~900 lines)
- Passive observer: joins room, receives all media, never sends
- TUI mode (ratatui): per-participant table with loss%, jitter, codec, color-coded
- No-TUI mode: stats printed to stderr every 2s
- Binary capture format (.wzp) with microsecond timestamps
- Replay mode: offline analysis from capture files
- HTML report: self-contained with Chart.js loss/jitter timelines
- Encrypted decode: stub (needs session key + nonce context for SFU E2E)

### Codebase Refactoring (2026-04-13)
- DashMap relay concurrency: global Mutex → 64-shard DashMap
- Federation clone-before-send: eliminated last lock-during-I/O
- Engine deduplication: 3 shared helpers, eliminated 250 lines duplication
- 29 federation tests (was 0)
- Clap CLI parser for relay (replaced 154-line manual parser)
- Magic number constants, error handling helpers, safety docs

### 5-Tier Adaptive Quality Classification (#9)
- `Tier` enum extended from 3 to 6 levels: Studio64k > Studio48k > Studio32k > Good > Degraded > Catastrophic
- WiFi thresholds: loss < 1%/RTT < 30ms (Studio64k) through loss >= 15%/RTT >= 200ms (Catastrophic)
- Cellular stays at Good ceiling (no studio tiers on mobile data)
- Asymmetric hysteresis: downgrade 3 reports, upgrade 5, studio upgrade 10
- `Tier` derives `Ord` — ordering matches quality level (Catastrophic=0, Studio64k=5)
- `weakest_tier()` simplified to `.min()` via Ord

### Client QualityDirective Handling (#27)
- Both desktop signal tasks (P2P and relay engines) now match `QualityDirective` signals
- Android signal task matches `QualityDirective` and stores profile index via `pending_profile_recv`
- Relay-coordinated codec switching now works end-to-end: relay broadcasts → clients react
- Closes the gap documented in PRD-coordinated-codec.md

### Debug Tap Enhancements (#11, #12)
- `log_signal()`: logs `RoomUpdate` (count + participant names), `QualityDirective` (codec + reason)
- `log_event()`: logs participant join/leave lifecycle events
- `log_stats()`: periodic 5-second summary — packets in/out, fan-out avg, seq gaps, codecs seen
- `TapStats` struct tracks per-participant metrics across the forwarding loop
- All output via `target: "debug_tap"` for RUST_LOG filtering

### Bug Fix: dual_path.rs Phase 7 regression
- Added missing `ipv6_endpoint: None` parameter to 3 `race()` call sites in integration tests
- Phase 7 IPv6 dual-socket changed the function signature but tests were not updated

### Build: Keystore sync (f17420a)
- `build.sh` syncs keystores from persistent cache before build

## Previous Changes (2026-04-12)

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

### Opus6k Frame Starvation Fix (2026-04-13)
- Root cause: partial reads from capture ring consumed samples that were discarded on retry
- `audio_read_capture(&mut buf[..1920])` with only 960 available → read 960, loop retried from buf[0], overwriting
- Added `wzp_native_audio_capture_available()` — check before reading (matches desktop pattern)
- `frame_samples` made mutable and updated on adaptive profile switch
- `buf` sized to max frame (1920) with `[..frame_samples]` slices throughout
- Result: Opus6k frame rate restored from ~11/s to expected 25/s

### Build Script Fixes (2026-04-13)
- Stale APK cleanup: delete all APKs before build, prefer `*release*.apk` on upload
- APK signing: added zipalign + apksigner pipeline to `build.sh` (was in `build-tauri-android.sh` only)
- Keystore persistence: `$BASE_DIR/data/keystore/` cache synced into source tree before build
- Fixes: 384MB debug APK uploaded instead of 25MB release; unsigned APK on alt server

### Phase 8: Tailscale-Inspired STUN/ICE Enhancements (2026-04-14)

5 new modules in `wzp-client`, 83 new unit tests (588 total across workspace).

#### Public STUN Client (`stun.rs`)
- Minimal RFC 5389 STUN Binding Request/Response over raw UDP
- XOR-MAPPED-ADDRESS (preferred) + MAPPED-ADDRESS (fallback) parsing
- Default servers: `stun.l.google.com:19302`, `stun1.l.google.com:19302`, `stun.cloudflare.com:3478`
- `discover_reflexive()` — first-success parallel probe across N servers
- `probe_stun_servers()` — full results for NAT classification
- Integrated into `detect_nat_type_with_stun()` combining relay + STUN probes
- Desktop STUN fallback in `try_reflect_own_addr()` when relay reflection fails

#### PCP/PMP/UPnP Port Mapping (`portmap.rs`)
- **NAT-PMP** (RFC 6886): UDP to gateway:5351, external address + port mapping
- **PCP** (RFC 6887): PCP MAP opcode, IPv4-mapped IPv6 client address
- **UPnP IGD**: SSDP M-SEARCH discovery + SOAP `AddPortMapping`/`GetExternalIPAddress`
- Gateway discovery: macOS (`route -n get default`), Linux (`/proc/net/route`)
- `acquire_port_mapping()` tries NAT-PMP → PCP → UPnP, first success wins
- `release_port_mapping()` + `spawn_refresh()` for lifecycle management
- Signal protocol: `caller_mapped_addr`/`callee_mapped_addr` on offer/answer, `peer_mapped_addr` on CallSetup
- `PeerCandidates.mapped` — new candidate type in dial order (host → mapped → reflexive)

#### Mid-Call ICE Re-Gathering (`ice_agent.rs`)
- `IceAgent`: owns candidate lifecycle with `gather()`, `re_gather()`, `apply_peer_update()`
- Monotonic generation counter prevents stale candidate updates from reordering
- `SignalMessage::CandidateUpdate` — new signal for mid-call candidate exchange
- Relay forwards `CandidateUpdate` to call peer (same pattern as `MediaPathReport`)
- Desktop handles `CandidateUpdate` in signal recv loop, emits to JS frontend
- Transport hot-swap architecture designed (TODO: wire into live call engine)

#### Netcheck Diagnostic (`netcheck.rs`)
- `NetcheckReport`: NAT type, reflexive addr, IPv4/v6, port mapping, relay latencies, gateway
- `run_netcheck()` — parallel probes for STUN + relay + portmap + IPv6
- `format_report()` — human-readable diagnostic output
- CLI: `wzp-client --netcheck <relay>` runs diagnostic

#### Region-Based Relay Selection (`relay_map.rs`)
- `RelayMap` sorted by RTT, `preferred()` returns lowest-latency reachable relay
- `populate_from_ack()` — parses `RegisterPresenceAck.available_relays`
- Stale detection (`needs_reprobe()`, `stale_entries()`)
- `RegisterPresenceAck` extended with `relay_region` and `available_relays`

#### Hard NAT Port Allocation Detection (`stun.rs` Phase A)
- `PortAllocation` enum: `PortPreserving` / `Sequential { delta }` / `Random` / `Unknown`
- `detect_port_allocation()` — sequential STUN probes from single socket, analyzes external port sequence
- `classify_port_allocation()` — pure classifier with wraparound handling, jitter tolerance (±1), 60% threshold for noisy sequences
- `predict_ports(last_port, delta, offset, spread)` — generates target port range for sequential NATs
- `HardNatProbe` signal message for peer coordination (carries port_sequence, allocation, external_ip)
- Relay forwards `HardNatProbe` to call peer
- `NetcheckReport.port_allocation` field populated automatically
- 17 new tests for classification, prediction, serde, Display

#### Relay End-to-End Wiring (2026-04-14)
- `CallRegistry` stores + cross-wires `caller_mapped_addr`/`callee_mapped_addr` into `CallSetup.peer_mapped_addr`
- `RelayConfig` extended with `region` + `advertised_addr` fields
- `RegisterPresenceAck` populates `relay_region` from config, `available_relays` from federation peers
- Desktop `place_call`/`answer_call` call `acquire_port_mapping()` and fill mapped addr fields
- Legacy `build-android-docker.sh` renamed to `build-android-docker-LEGACY.sh` to prevent accidental use

## Wave 5: Video Infrastructure (2026-05-12)

**Tasks completed:** T5.1, T5.1.1, T5.2, T5.3, T5.4, T5.5, T5.6, T5.7, T5.7.1, T5.8

### Relay: Audio + Video Scoring

New files in `crates/wzp-relay/src/`:

- `audio_scorer.rs` — per-stream audio quality scorer tracking packet loss, codec consistency, bitrate stability
- `response_policy.rs` — relay response policy engine mapping scores to action thresholds
- `verdict.rs` — `Verdict` enum: `Allow`, `RateLimit`, `Drop`, `Malicious`
- `video_scorer.rs` — `VideoScorer` with legitimacy scoring: keyframe regularity, I/P ratio, bandwidth responsiveness. **Note: wired but `observe()` not yet called from room forwarding path — T6.2 follow-up open.**

### Video: H.265 + Quality Controller

New files in `crates/wzp-video/src/`:

- `controller.rs` — `VideoQualityController`: maps (bwe_bps, loss_pct, rtt_ms, priority_mode) to (target_bitrate, target_fps, target_resolution, simulcast_layer)
- `simulcast.rs` — simulcast layer management (base + enhancement layers)
- `encoder_mode.rs` — encoder mode selection (CBR/VBR, keyframe intervals, quality presets)

H.265 encode/decode path added to:
- `videotoolbox.rs` — VideoToolbox H.265 encoder + decoder (macOS/iOS)
- `mediacodec.rs` — MediaCodec H.265 encoder + decoder (Android; NDK 0.9 compile errors pending in T4.3.1.1)

**Test delta:** wzp-relay 99→127, wzp-video 43→71

---

## Wave 6: AV1 + Federation Gossip Design (2026-05-12)

**Tasks completed:** T6.1, T6.1.2, T6.2

### Video: AV1 Codec Support

New files in `crates/wzp-video/src/`:

- `av1_obu.rs` — AV1 OBU (Open Bitstream Unit) framing and depacketizer
- `dav1d.rs` — dav1d AV1 software decoder (non-Android; gated via cfg)
- `svt_av1.rs` — SVT-AV1 software encoder (non-Android; gated via cfg)

Updated files:
- `videotoolbox.rs` — VideoToolbox AV1 decoder + encoder (macOS M3+, iOS A17+)
- `mediacodec.rs` — MediaCodec AV1 (Android; compile errors pending)
- `factory.rs` — `create_video_encoder(codec, platform)` dispatcher added; H.264, H.265, AV1 wired

**T6.1.2 follow-up open:** `create_video_encoder(Av1Main, ...)` has no caller in the call engine yet — wiring step is unstarted.

### Relay: Federation Reputation Gossip (Design Phase)

- T6.3 design exploration committed at `1e729e4`
- `docs/PRD/PRD-relay-federation-gossip.md` — Ban-List Distribution approach selected (Approach 3)
- Implementation not started; task spec pending conversion

### Test Counts

**Test delta Wave 6:** wzp-video 76→88, wzp-relay 127→137

**Total workspace tests: 702** (excluding `wzp-android`)

| Crate | Tests |
|---|---|
| wzp-proto | 112 |
| wzp-codec | 69 |
| wzp-fec | 21 |
| wzp-crypto | 64 |
| wzp-transport | 11 |
| wzp-relay | 137 |
| wzp-client | 200 |
| wzp-video | 88 |
| wzp-web | 2 |
| wzp-native | 0 |

---

## Current Status (2026-05-25)

### What Works (Audio)

All audio path items from previous status section remain working. Additionally:

- MediaHeader v2 (16 bytes) deployed across all paths
- MiniHeader v2 (5 bytes with seq_delta) deployed
- Anti-replay windows per stream with media-type-aware sizing (audio 64, video 1024)
- Relay DashMap + RwLock concurrency model (T3.1 resolved the Mutex bottleneck)

### What Works (Video — partial)

- H.264 framer/depacketizer with FU-A fragmentation handling
- H.264, H.265, AV1 VideoToolbox encode/decode (macOS)
- AV1 dav1d + SVT-AV1 software path (non-Android)
- Video quality controller, simulcast, encoder mode selection (controller only; no active call wiring yet)
- Video scorer (scoring logic complete; not yet wired into relay forwarding)
- NACK framework (`nack.rs`; not yet wired into room forwarding)

### Open Blockers

- **Android video:** `mediacodec.rs` has 31 NDK 0.9 compile errors (T4.3.1.1 in progress)
- **AV1 call wiring:** `create_video_encoder(Av1Main, ...)` has no caller (T6.1.2 follow-up)
- **VideoScorer wiring:** `VideoScorer::observe()` commented out at `room.rs:1263` (T6.2 follow-up)
- **NACK wiring:** NACK path not wired into room forwarding (Phase V2/V4)
- **BWE:** `AdaptiveQualityController` does not consume `cwnd`/`bytes_in_flight` (Phase V2)
- **Crypto nonce bug:** `decrypt()` uses `recv_seq` instead of `MediaHeader.seq` (see AUDIT-2026-05-25.md C1)
