# WarzonePhone Architecture

> Custom lossy VoIP protocol built in Rust. E2E encrypted, FEC-protected, adaptive quality, designed for hostile network conditions.

## System Overview

```mermaid
graph TB
    subgraph "Client A (Browser/CLI)"
        MIC[Microphone] --> DN[NoiseSupressor<br/>RNNoise ML]
        DN --> SD[SilenceDetector<br/>VAD + Hangover]
        SD --> ENC[CallEncoder<br/>Opus/Codec2]
        ENC --> FEC_E[FEC Encoder<br/>RaptorQ]
        FEC_E --> CRYPT_E[ChaCha20-Poly1305<br/>Encrypt]
        CRYPT_E --> QUIC_S[QUIC Datagram<br/>Send]

        QUIC_R[QUIC Datagram<br/>Recv] --> CRYPT_D[ChaCha20-Poly1305<br/>Decrypt]
        CRYPT_D --> FEC_D[FEC Decoder<br/>RaptorQ]
        FEC_D --> JIT[JitterBuffer<br/>Adaptive Playout]
        JIT --> DEC[CallDecoder<br/>Opus/Codec2]
        DEC --> SPK[Speaker]
    end

    subgraph "Relay (SFU)"
        ACCEPT[Accept QUIC] --> AUTH{Auth?}
        AUTH -->|token| VALIDATE[POST /v1/auth/validate]
        AUTH -->|no auth| HS
        VALIDATE --> HS[Crypto Handshake<br/>X25519 + Ed25519]
        HS --> ROOM[Room Manager<br/>Named Rooms via SNI]
        ROOM --> FWD[Forward to<br/>Other Participants]
    end

    subgraph "Client B"
        B_SPK[Speaker]
        B_MIC[Microphone]
    end

    QUIC_S -->|UDP/QUIC| ACCEPT
    FWD -->|UDP/QUIC| QUIC_R
    B_MIC -.->|same pipeline| ACCEPT
    FWD -.->|same pipeline| B_SPK

    style MIC fill:#4a9eff
    style SPK fill:#4a9eff
    style B_MIC fill:#4a9eff
    style B_SPK fill:#4a9eff
    style ROOM fill:#ff9f43
    style CRYPT_E fill:#ee5a24
    style CRYPT_D fill:#ee5a24
```

## Crate Dependency Graph

```mermaid
graph TD
    PROTO[wzp-proto<br/>Types, Traits, Wire Format]

    CODEC[wzp-codec<br/>Opus + Codec2 + RNNoise]
    FEC[wzp-fec<br/>RaptorQ FEC]
    CRYPTO[wzp-crypto<br/>ChaCha20 + Identity]
    TRANSPORT[wzp-transport<br/>QUIC/Quinn]

    RELAY[wzp-relay<br/>Relay Daemon]
    CLIENT[wzp-client<br/>CLI + Call Engine]
    WEB[wzp-web<br/>Browser Bridge]

    PROTO --> CODEC
    PROTO --> FEC
    PROTO --> CRYPTO
    PROTO --> TRANSPORT

    CODEC --> CLIENT
    FEC --> CLIENT
    CRYPTO --> CLIENT
    TRANSPORT --> CLIENT
    CODEC --> RELAY
    FEC --> RELAY
    CRYPTO --> RELAY
    TRANSPORT --> RELAY

    CLIENT --> WEB
    TRANSPORT --> WEB
    CRYPTO --> WEB

    FC[warzone-protocol<br/>featherChat Identity] -.->|path dep| CRYPTO

    style PROTO fill:#6c5ce7
    style RELAY fill:#ff9f43
    style CLIENT fill:#00b894
    style WEB fill:#0984e3
    style FC fill:#fd79a8
```

## Wire Formats

### MediaHeader (12 bytes)

```
Byte 0:  [V:1][T:1][CodecID:4][Q:1][FecHi:1]
Byte 1:  [FecLo:6][unused:2]
Bytes 2-3:  sequence (u16 BE)
Bytes 4-7:  timestamp_ms (u32 BE)
Byte 8:     fec_block_id (u8)
Byte 9:     fec_symbol_idx (u8)
Byte 10:    reserved
Byte 11:    csrc_count

V = version (0), T = is_repair, CodecID = codec, Q = quality_report appended
```

### MiniHeader (4 bytes, compressed)

```
Bytes 0-1: timestamp_delta_ms (u16 BE)
Bytes 2-3: payload_len (u16 BE)

Preceded by FRAME_TYPE_MINI (0x01). Full header every 50 frames (~1s).
Saves 8 bytes/packet (67% header reduction).
```

### TrunkFrame (batched datagrams)

```
[count:u16]
  [session_id:2][len:u16][payload:len]  x count

Packs multiple session packets into one QUIC datagram.
Max 10 entries or 1200 bytes, flushed every 5ms.
```

### QualityReport (4 bytes, optional)

```
Byte 0: loss_pct (0-255 maps to 0-100%)
Byte 1: rtt_4ms (0-255 maps to 0-1020ms)
Byte 2: jitter_ms
Byte 3: bitrate_cap_kbps
```

### SignalMessage (JSON over reliable QUIC stream)

```
[4-byte length prefix][serde_json payload]

Variants:
  CallOffer    { identity_pub, ephemeral_pub, signature, supported_profiles }
  CallAnswer   { identity_pub, ephemeral_pub, signature, chosen_profile }
  IceCandidate { candidate }
  Hangup       { reason: Normal|Busy|Declined|Timeout|Error }
  AuthToken    { token }
  Hold, Unhold, Mute, Unmute
  Transfer     { target_fingerprint, relay_addr }
  TransferAck
  Rekey        { new_ephemeral_pub, signature }
  QualityUpdate { report, recommended_profile }
  Ping/Pong    { timestamp_ms }
```

## Quality Profiles

```mermaid
graph LR
    subgraph GOOD ["GOOD (28.8 kbps)"]
        G_C[Opus 24kbps]
        G_F[FEC 20%]
        G_FR[20ms frames]
    end

    subgraph DEGRADED ["DEGRADED (9.0 kbps)"]
        D_C[Opus 6kbps]
        D_F[FEC 50%]
        D_FR[40ms frames]
    end

    subgraph CATASTROPHIC ["CATASTROPHIC (2.4 kbps)"]
        C_C[Codec2 1200bps]
        C_F[FEC 100%]
        C_FR[40ms frames]
    end

    GOOD -->|"loss>5% or RTT>100ms<br/>3 consecutive reports"| DEGRADED
    DEGRADED -->|"loss>15% or RTT>200ms<br/>3 consecutive"| CATASTROPHIC
    CATASTROPHIC -->|"loss<5% and RTT<100ms<br/>3 consecutive"| DEGRADED
    DEGRADED -->|"loss<5% and RTT<100ms<br/>3 consecutive"| GOOD

    style GOOD fill:#00b894
    style DEGRADED fill:#fdcb6e
    style CATASTROPHIC fill:#e17055
```

## Cryptographic Handshake

```mermaid
sequenceDiagram
    participant C as Caller
    participant R as Relay/Callee

    Note over C: Derive identity from seed<br/>Ed25519 + X25519 via HKDF

    C->>C: Generate ephemeral X25519
    C->>C: Sign(ephemeral_pub || "call-offer")
    C->>R: CallOffer { identity_pub, ephemeral_pub, signature, profiles }

    R->>R: Verify Ed25519 signature
    R->>R: Generate ephemeral X25519
    R->>R: shared_secret = DH(eph_b, eph_a)
    R->>R: session_key = HKDF(shared_secret, "warzone-session-key")
    R->>R: Sign(ephemeral_pub || "call-answer")
    R->>C: CallAnswer { identity_pub, ephemeral_pub, signature, chosen_profile }

    C->>C: Verify signature
    C->>C: shared_secret = DH(eph_a, eph_b)
    C->>C: session_key = HKDF(shared_secret)

    Note over C,R: Both have identical ChaCha20-Poly1305 session key
    C->>R: Encrypted media (QUIC datagrams)
    R->>C: Encrypted media (QUIC datagrams)

    Note over C,R: Rekey every 65,536 packets<br/>New ephemeral DH + HKDF mix
```

## Identity Model (featherChat Compatible)

```mermaid
graph TD
    SEED[32-byte Seed<br/>BIP39 Mnemonic 24 words] --> HKDF1[HKDF<br/>salt=None<br/>info=warzone-ed25519]
    SEED --> HKDF2[HKDF<br/>salt=None<br/>info=warzone-x25519]

    HKDF1 --> ED[Ed25519 SigningKey<br/>Digital Signatures]
    HKDF2 --> X25519[X25519 StaticSecret<br/>Key Agreement]

    ED --> VKEY[Ed25519 VerifyingKey<br/>Public]
    X25519 --> XPUB[X25519 PublicKey<br/>Public]

    VKEY --> FP[Fingerprint<br/>SHA-256 pubkey truncated 16 bytes<br/>xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx]

    style SEED fill:#6c5ce7
    style FP fill:#fd79a8
    style ED fill:#ee5a24
    style X25519 fill:#00b894
```

## Relay Modes

```mermaid
graph TB
    subgraph "Room Mode (Default SFU)"
        C1[Client 1] -->|QUIC SNI=room-hash| RM[Room Manager]
        C2[Client 2] -->|QUIC SNI=room-hash| RM
        C3[Client 3] -->|QUIC SNI=room-hash| RM
        RM --> R1[Room abc123]
        R1 -->|fan-out| C1
        R1 -->|fan-out| C2
        R1 -->|fan-out| C3
    end

    subgraph "Forward Mode with --remote"
        C4[Client] -->|QUIC| RA[Relay A]
        RA -->|FEC decode then jitter then FEC encode| RB[Relay B]
        RB -->|QUIC| C5[Client]
    end

    subgraph "Probe Mode with --probe"
        PA[Relay A] -->|Ping 1/s ~50 bytes| PB[Relay B]
        PB -->|Pong| PA
        PA --> PM[Prometheus<br/>RTT Loss Jitter Up/Down]
    end

    style RM fill:#ff9f43
    style R1 fill:#fdcb6e
    style PM fill:#0984e3
```

## Web Bridge Architecture

```mermaid
sequenceDiagram
    participant B as Browser
    participant W as wzp-web
    participant R as wzp-relay

    B->>W: HTTPS GET /room-name
    W->>B: index.html (SPA)

    B->>W: WebSocket /ws/room-name
    Note over B,W: Optional auth JSON message

    W->>R: QUIC connect (SNI = hashed room name)
    Note over W,R: AuthToken then Handshake then Join Room

    loop Every 20ms
        B->>W: WS Binary Int16 x 960 PCM
        W->>W: CallEncoder Opus + FEC
        W->>R: QUIC Datagram encrypted
    end

    loop Incoming audio
        R->>W: QUIC Datagram
        W->>W: CallDecoder FEC + Opus
        W->>B: WS Binary Int16 x 960 PCM
    end

    Note over B: AudioWorklet<br/>WZPCaptureProcessor mic to 960 frames<br/>WZPPlaybackProcessor ring buffer to speaker
```

## FEC Protection (RaptorQ)

```mermaid
graph LR
    subgraph "Encoder"
        F1[Frame 1] --> BLK[Source Block<br/>5-10 frames]
        F2[Frame 2] --> BLK
        F3[Frame 3] --> BLK
        F4[Frame 4] --> BLK
        F5[Frame 5] --> BLK
        BLK --> SRC[5 Source Symbols]
        BLK --> REP[1-10 Repair Symbols<br/>ratio dependent]
        SRC --> INT[Interleaver<br/>depth=3]
        REP --> INT
    end

    subgraph "Network"
        INT --> LOSS{Packet Loss}
        LOSS -->|some lost| RCV[Received Symbols]
    end

    subgraph "Decoder"
        RCV --> DEINT[De-interleaver]
        DEINT --> RAPTORQ[RaptorQ Decoder<br/>Reconstruct from<br/>any K of K+R symbols]
        RAPTORQ --> OUT[Original Frames]
    end

    style LOSS fill:#e17055
    style RAPTORQ fill:#00b894
```

## Telemetry Stack

```mermaid
graph TB
    subgraph "Relay"
        RM[RelayMetrics<br/>sessions rooms packets]
        SM[SessionMetrics<br/>per-session jitter loss RTT]
        PM[ProbeMetrics<br/>inter-relay RTT loss]
        RM --> PROM1[GET /metrics :9090]
        SM --> PROM1
        PM --> PROM1
    end

    subgraph "Web Bridge"
        WM[WebMetrics<br/>connections frames latency]
        WM --> PROM2[GET /metrics :8080]
    end

    subgraph "Client"
        CM[JitterStats + QualityAdapter]
        CM --> JSONL[--metrics-file<br/>JSONL 1 line/sec]
    end

    PROM1 --> GRAF[Grafana Dashboard<br/>4 rows 18 panels]
    PROM2 --> GRAF
    JSONL --> ANALYSIS[Offline Analysis]

    style GRAF fill:#ff6b6b
    style PROM1 fill:#0984e3
    style PROM2 fill:#0984e3
```

## Session State Machine

```mermaid
stateDiagram-v2
    [*] --> Idle
    Idle --> Connecting: connect
    Connecting --> Handshaking: QUIC established
    Handshaking --> Active: CallOffer/Answer complete
    Active --> Rekeying: 65536 packets
    Rekeying --> Active: new key derived
    Active --> Closed: Hangup/Error/Timeout
    Rekeying --> Closed: Error
    Connecting --> Closed: Timeout
    Handshaking --> Closed: Signature fail

    note right of Active: Media flows
    note right of Rekeying: Media continues while rekeying
```

## Audio Processing Pipeline Detail

```mermaid
graph TD
    subgraph "Capture 20ms at 48kHz = 960 samples"
        MIC[Microphone / AudioWorklet] --> PCM[PCM i16 x 960]
        PCM --> RNN[RNNoise Denoise<br/>2 x 480 samples]
        RNN --> VAD{Silent?}
        VAD -->|Yes over 100ms| CN[ComfortNoise packet<br/>every 200ms]
        VAD -->|No or Hangover| OPUS[Opus/Codec2 Encode]
    end

    subgraph "FEC + Crypto"
        OPUS --> SYMBOL[Pad to 256-byte symbol]
        CN --> SYMBOL
        SYMBOL --> BLOCK[Accumulate block<br/>5-10 symbols]
        BLOCK --> RAPTOR[RaptorQ encode<br/>+ repair symbols]
        RAPTOR --> INTERLEAVE[Interleave depth=3]
        INTERLEAVE --> HDR[Add MediaHeader<br/>or MiniHeader]
        HDR --> ENCRYPT[ChaCha20-Poly1305<br/>header=AAD payload=encrypted]
        ENCRYPT --> QUIC[QUIC Datagram]
    end

    style RNN fill:#a29bfe
    style ENCRYPT fill:#ee5a24
    style RAPTOR fill:#00b894
```

## Adaptive Jitter Buffer

```mermaid
graph TD
    PKT[Incoming Packet] --> SEQ{Sequence Check}
    SEQ -->|Duplicate| DROP[Drop + AntiReplay]
    SEQ -->|Valid| BUF[BTreeMap Buffer<br/>ordered by seq]

    BUF --> ADAPT[AdaptivePlayoutDelay<br/>EMA jitter tracking]
    ADAPT --> TARGET[target_delay =<br/>ceil jitter_ema/20ms + 2]

    BUF --> READY{depth >= target?}
    READY -->|No| WAIT[Wait / Underrun++]
    READY -->|Yes| POP[Pop lowest seq]
    POP --> DECODE[Decode to PCM]
    DECODE --> PLAY[Playout]

    BUF --> OVERFLOW{depth > max?}
    OVERFLOW -->|Yes| EVICT[Drop oldest<br/>Overrun++]

    style ADAPT fill:#fdcb6e
    style DROP fill:#e17055
    style EVICT fill:#e17055
```

## Deployment Topology

```mermaid
graph TB
    subgraph "Region A"
        RA[wzp-relay A<br/>:4433 UDP]
        WA[wzp-web A<br/>:8080 HTTPS]
        WA --> RA
    end

    subgraph "Region B"
        RB[wzp-relay B<br/>:4433 UDP]
        WB[wzp-web B<br/>:8080 HTTPS]
        WB --> RB
    end

    RA <-->|Probe 1/s| RB

    BA[Browser A] -->|WSS| WA
    BB[Browser B] -->|WSS| WB
    CA[CLI Client] -->|QUIC| RA

    PROM[Prometheus] -->|scrape| RA
    PROM -->|scrape| RB
    PROM -->|scrape| WA
    PROM --> GRAF[Grafana]

    FC[featherChat Server] -->|auth validate| RA
    FC -->|auth validate| RB

    style RA fill:#ff9f43
    style RB fill:#ff9f43
    style GRAF fill:#ff6b6b
    style FC fill:#fd79a8
```

## featherChat Integration Flow

```mermaid
sequenceDiagram
    participant A as User A WZP Client
    participant FC as featherChat Server
    participant R as WZP Relay
    participant B as User B WZP Client

    Note over A,B: Both users share BIP39 seed = same identity

    A->>FC: WS CallSignal Offer payload=JSON SignalMessage
    FC->>B: WS CallSignal Offer payload + relay_addr + room

    B->>R: QUIC connect SNI = hashed room
    B->>R: AuthToken fc_bearer_token
    R->>FC: POST /v1/auth/validate token
    FC->>R: valid true fingerprint ...
    B->>R: CallOffer then CallAnswer handshake

    A->>R: QUIC connect same room
    A->>R: AuthToken + Handshake

    Note over A,B: Both in same room media flows E2E encrypted
    A->>R: Encrypted media
    R->>B: Forward SFU no decryption
    B->>R: Encrypted media
    R->>A: Forward
```

## Bandwidth Usage

| Profile | Audio | FEC Overhead | Total | Use Case |
|---------|-------|-------------|-------|----------|
| **GOOD** | 24 kbps (Opus) | 20% = 4.8 kbps | **28.8 kbps** | WiFi, LTE, good links |
| **DEGRADED** | 6 kbps (Opus) | 50% = 3 kbps | **9.0 kbps** | 3G, congested WiFi |
| **CATASTROPHIC** | 1.2 kbps (Codec2) | 100% = 1.2 kbps | **2.4 kbps** | Satellite, extreme loss |

With silence suppression: ~50% savings in typical conversations.
With mini-frames: 8 bytes/packet saved (67% header reduction).
With trunking: shared QUIC overhead across multiplexed sessions.

## Project Structure

```
warzonePhone/
├── Cargo.toml                    # Workspace root
├── crates/
│   ├── wzp-proto/                # Protocol types, traits, wire format
│   │   └── src/
│   │       ├── codec_id.rs       # CodecId, QualityProfile
│   │       ├── error.rs          # Error types
│   │       ├── jitter.rs         # JitterBuffer, AdaptivePlayoutDelay
│   │       ├── packet.rs         # MediaHeader, MiniHeader, TrunkFrame, SignalMessage
│   │       ├── quality.rs        # Tier, AdaptiveQualityController
│   │       ├── session.rs        # SessionState machine
│   │       └── traits.rs         # AudioEncoder, FecEncoder, CryptoSession, etc.
│   ├── wzp-codec/                # Audio codecs
│   │   └── src/
│   │       ├── adaptive.rs       # AdaptiveEncoder/Decoder (Opus + Codec2)
│   │       ├── denoise.rs        # NoiseSupressor (RNNoise/nnnoiseless)
│   │       └── silence.rs        # SilenceDetector, ComfortNoise
│   ├── wzp-fec/                  # Forward error correction
│   │   └── src/
│   │       ├── encoder.rs        # RaptorQFecEncoder
│   │       ├── decoder.rs        # RaptorQFecDecoder
│   │       └── interleave.rs     # Interleaver (burst protection)
│   ├── wzp-crypto/               # Cryptography + identity
│   │   └── src/
│   │       ├── identity.rs       # Seed, Fingerprint, hash_room_name
│   │       ├── handshake.rs      # WarzoneKeyExchange (X25519 + Ed25519)
│   │       ├── session.rs        # ChaChaSession (ChaCha20-Poly1305)
│   │       ├── nonce.rs          # Deterministic nonce construction
│   │       ├── anti_replay.rs    # Sliding window replay protection
│   │       └── rekey.rs          # Forward secrecy rekeying
│   ├── wzp-transport/            # QUIC transport layer
│   │   └── src/lib.rs            # QuinnTransport, send/recv media/signal/trunk
│   ├── wzp-relay/                # Relay daemon
│   │   └── src/
│   │       ├── main.rs           # CLI, connection loop, auth + handshake
│   │       ├── room.rs           # RoomManager, TrunkedForwarder
│   │       ├── pipeline.rs       # RelayPipeline (forward mode)
│   │       ├── session_mgr.rs    # SessionManager (limits, lifecycle)
│   │       ├── auth.rs           # featherChat token validation
│   │       ├── handshake.rs      # Relay-side accept_handshake
│   │       ├── metrics.rs        # Prometheus RelayMetrics + per-session
│   │       ├── probe.rs          # Inter-relay probes + ProbeMesh
│   │       └── trunk.rs          # TrunkBatcher
│   ├── wzp-client/               # Call engine + CLI
│   │   └── src/
│   │       ├── cli.rs            # CLI arg parsing + main
│   │       ├── call.rs           # CallEncoder, CallDecoder, QualityAdapter
│   │       ├── handshake.rs      # Client-side perform_handshake
│   │       ├── featherchat.rs    # CallSignal bridge
│   │       ├── echo_test.rs      # Automated echo quality test
│   │       ├── drift_test.rs     # Clock drift measurement
│   │       ├── sweep.rs          # Jitter buffer parameter sweep
│   │       ├── metrics.rs        # JSONL telemetry writer
│   │       └── bench.rs          # Component benchmarks
│   └── wzp-web/                  # Browser bridge
│       ├── src/
│       │   ├── main.rs           # Axum server, WS handler, TLS
│       │   └── metrics.rs        # Prometheus WebMetrics
│       └── static/
│           ├── index.html        # SPA UI (room, PTT, level meter)
│           └── audio-processor.js # AudioWorklet (capture + playback)
├── deps/featherchat/             # Git submodule
├── docs/
│   ├── ARCHITECTURE.md           # This file
│   ├── TELEMETRY.md              # Metrics specification
│   ├── INTEGRATION_TASKS.md      # featherChat task tracker
│   ├── WZP-FC-SHARED-CRATES.md   # Shared crate strategy
│   └── grafana-dashboard.json    # Pre-built Grafana dashboard
└── scripts/
    └── build-linux.sh            # Hetzner VM build
```

## Test Coverage

272 tests across all crates, 0 failures.

| Crate | Tests | Key Coverage |
|-------|-------|-------------|
| wzp-proto | 41 | Wire format, jitter buffer, quality tiers, mini-frames, trunking |
| wzp-codec | 31 | Opus/Codec2 roundtrip, silence detection, noise suppression |
| wzp-fec | 22 | RaptorQ encode/decode, loss recovery, interleaving |
| wzp-crypto | 34 + 28 compat | Encrypt/decrypt, handshake, anti-replay, featherChat identity compat |
| wzp-transport | 2 | QUIC connection setup |
| wzp-relay | 40 + 4 integration | Room ACL, session mgmt, metrics, probes, mesh, trunking |
| wzp-client | 30 + 2 integration | Encoder/decoder, quality adapter, silence, drift, sweep |
| wzp-web | 2 | Metrics |
