# Architecture

## System Overview

The Android client is a four-layer stack: Kotlin UI, JNI bridge, Rust engine, and C++ audio I/O. Each layer communicates through well-defined interfaces with minimal coupling.

```mermaid
graph TB
    subgraph "Kotlin (Main Thread)"
        CA[CallActivity]
        VM[CallViewModel]
        UI[InCallScreen<br/>Compose UI]
        CA --> VM
        VM --> UI
    end

    subgraph "JNI Bridge"
        JB[jni_bridge.rs<br/>panic-safe FFI]
    end

    subgraph "Rust Engine"
        ENG[WzpEngine<br/>Orchestrator]
        CT[Codec Thread<br/>20ms real-time loop]
        NET[Tokio Runtime<br/>2 async workers]
        PIPE[Pipeline<br/>Encode/Decode/FEC/Jitter]
    end

    subgraph "C++ Audio"
        OBOE[Oboe Bridge<br/>Capture + Playout callbacks]
        RB[Ring Buffers<br/>Lock-free SPSC]
    end

    subgraph "Network"
        QUIC[QUIC Connection<br/>quinn]
        RELAY[WZP Relay<br/>SFU Room]
    end

    VM <-->|"JNI calls<br/>+ JSON stats"| JB
    JB <--> ENG
    ENG --> CT
    ENG --> NET
    CT <--> PIPE
    CT <-->|"Atomic R/W"| RB
    OBOE <-->|"Atomic R/W"| RB
    CT <-->|"mpsc channels"| NET
    NET <-->|"QUIC datagrams<br/>+ streams"| QUIC
    QUIC <--> RELAY
```

## Thread Model

The engine uses four distinct thread contexts, each with specific responsibilities and real-time constraints.

```mermaid
graph LR
    subgraph "Android Main Thread"
        UI_T["UI + JNI calls<br/>startCall / stopCall / getStats"]
    end

    subgraph "Oboe Audio Thread (system)"
        AUD["Capture callback: mic → ring buf<br/>Playout callback: ring buf → speaker<br/>⚡ Highest priority, no allocations"]
    end

    subgraph "Codec Thread (wzp-codec)"
        COD["20ms loop:<br/>1. Read capture ring buf<br/>2. AEC → AGC → Encode<br/>3. Send to network channel<br/>4. Recv from network channel<br/>5. FEC → Jitter → Decode<br/>6. Write playout ring buf<br/>⚡ Pinned to big core, RT priority"]
    end

    subgraph "Tokio Runtime (2 workers)"
        NET_S["Send task:<br/>Channel → MediaPacket → QUIC datagram"]
        NET_R["Recv task:<br/>QUIC datagram → MediaPacket → Channel"]
        HS["Handshake:<br/>CallOffer → CallAnswer"]
    end

    UI_T -->|"mpsc command channel"| COD
    COD -->|"tokio::mpsc send_tx"| NET_S
    NET_R -->|"tokio::mpsc recv_tx"| COD
    AUD <-->|"Atomic ring buffers"| COD
```

### Thread Priorities and Constraints

| Thread | Priority | Allocations | Blocking | Lock-free |
|--------|----------|-------------|----------|-----------|
| Oboe audio | SCHED_FIFO (system) | None | Never | Yes |
| Codec | RT priority, big core | Pre-allocated buffers | sleep(remainder of 20ms) | Ring buf: yes, Stats: Mutex |
| Tokio workers | Normal | Allowed | Async only | N/A |
| Main/JNI | Normal | Allowed | Allowed | N/A |

## Call Lifecycle

```mermaid
sequenceDiagram
    participant User
    participant UI as InCallScreen
    participant VM as CallViewModel
    participant ENG as WzpEngine (JNI)
    participant NET as Tokio Network
    participant RELAY as WZP Relay

    User->>UI: Tap CALL
    UI->>VM: startCall()
    VM->>ENG: init() + startCall(relay, room)
    ENG->>ENG: Create tokio runtime
    ENG->>NET: Spawn network task

    NET->>RELAY: QUIC connect (SNI = room name)
    RELAY-->>NET: Connection established

    Note over NET,RELAY: Crypto Handshake
    NET->>RELAY: CallOffer {identity_pub, ephemeral_pub, signature, profiles}
    RELAY-->>NET: CallAnswer {ephemeral_pub, chosen_profile, signature}
    NET->>NET: Derive ChaCha20-Poly1305 session

    ENG->>ENG: Spawn codec thread
    Note over ENG: State → Active

    loop Every 20ms
        ENG->>ENG: Read mic → AEC → AGC → Encode
        ENG->>NET: Encoded frame via channel
        NET->>RELAY: MediaPacket via QUIC DATAGRAM
        RELAY->>NET: MediaPacket from other peer
        NET->>ENG: MediaPacket via channel
        ENG->>ENG: FEC → Jitter → Decode → Speaker
    end

    User->>UI: Tap END
    UI->>VM: stopCall()
    VM->>ENG: stopCall()
    ENG->>ENG: Set running=false, send Stop command
    ENG->>ENG: Join codec thread
    ENG->>NET: Drop tokio runtime
    NET->>RELAY: Connection close
```

## Audio Pipeline Detail

```mermaid
graph LR
    subgraph "Capture Path"
        MIC[Microphone] -->|"48kHz i16"| OBOE_C[Oboe Capture<br/>Callback]
        OBOE_C -->|"ring_write()"| RB_C[Capture<br/>Ring Buffer]
        RB_C -->|"read_capture()"| AEC[Echo<br/>Canceller]
        AEC --> AGC[Auto Gain<br/>Control]
        AGC --> ENC[AdaptiveEncoder<br/>Opus 24k]
        ENC -->|"Vec u8"| FEC_E[RaptorQ<br/>FEC Encoder]
        FEC_E -->|"send_tx"| CHAN_S[Send Channel]
    end

    subgraph "Network"
        CHAN_S --> PKT_S[MediaPacket<br/>Header + Payload]
        PKT_S -->|"QUIC DATAGRAM"| RELAY[Relay SFU]
        RELAY -->|"QUIC DATAGRAM"| PKT_R[MediaPacket<br/>Deserialize]
        PKT_R -->|"recv_tx"| CHAN_R[Recv Channel]
    end

    subgraph "Playout Path"
        CHAN_R --> FEC_D[RaptorQ<br/>FEC Decoder]
        FEC_D --> JB[Jitter Buffer<br/>10-250 pkts]
        JB --> DEC[AdaptiveDecoder<br/>Opus 24k]
        DEC -->|"48kHz i16"| AEC_REF[AEC Far-End<br/>Reference]
        DEC -->|"write_playout()"| RB_P[Playout<br/>Ring Buffer]
        RB_P -->|"ring_read()"| OBOE_P[Oboe Playout<br/>Callback]
        OBOE_P --> SPK[Speaker]
    end
```

### Audio Parameters

| Parameter | Value | Notes |
|-----------|-------|-------|
| Sample rate | 48,000 Hz | Opus native rate |
| Channels | 1 (mono) | VoIP only |
| Frame size | 960 samples | 20ms at 48kHz |
| Ring buffer | 7,680 samples | 160ms (8 frames) |
| Bit depth | 16-bit signed int | PCM format |
| AEC tail | 100ms | Echo canceller filter length |

## Crypto Handshake

```mermaid
sequenceDiagram
    participant Client as Android Client
    participant Relay as WZP Relay

    Note over Client: Identity seed (32 bytes, random per launch)
    Note over Client: HKDF → Ed25519 signing key + X25519 static key

    Client->>Client: Generate ephemeral X25519 keypair
    Client->>Client: Sign(ephemeral_pub || "call-offer") with Ed25519

    Client->>Relay: SignalMessage::CallOffer<br/>{identity_pub, ephemeral_pub, signature, [GOOD, DEGRADED, CATASTROPHIC]}

    Relay->>Relay: Verify Ed25519 signature
    Relay->>Relay: Generate own ephemeral X25519
    Relay->>Relay: Sign(ephemeral_pub || "call-answer")
    Relay->>Relay: DH(relay_ephemeral, client_ephemeral) → shared secret
    Relay->>Relay: HKDF(shared_secret) → ChaCha20-Poly1305 key

    Relay->>Client: SignalMessage::CallAnswer<br/>{identity_pub, ephemeral_pub, signature, chosen_profile=GOOD}

    Client->>Client: Verify relay signature
    Client->>Client: DH(client_ephemeral, relay_ephemeral) → same shared secret
    Client->>Client: HKDF(shared_secret) → same ChaCha20-Poly1305 key

    Note over Client,Relay: Both sides now have identical session key
    Note over Client,Relay: Media packets can be encrypted (not yet applied)
```

### Key Derivation Chain

```
Identity Seed (32 bytes, random)
    │
    ├── HKDF(seed, info="warzone-ed25519") → Ed25519 signing key
    │       └── Public key = identity_pub (32 bytes)
    │       └── SHA-256(identity_pub)[:16] = fingerprint (16 bytes)
    │
    └── HKDF(seed, info="warzone-x25519") → X25519 static key (unused currently)

Per-Call Ephemeral:
    Random X25519 keypair → ephemeral_pub (sent in CallOffer)

Session Key:
    DH(our_ephemeral_secret, peer_ephemeral_pub) → shared_secret
    HKDF(shared_secret, info="warzone-session-key") → ChaCha20-Poly1305 key (32 bytes)
```

## QUIC Transport

```mermaid
graph TB
    subgraph "QUIC Connection"
        EP[Client Endpoint<br/>0.0.0.0:0 UDP]
        CONN[Connection to Relay<br/>SNI = room name]

        subgraph "Unreliable Channel"
            DG_S[Send DATAGRAM<br/>MediaPacket serialized]
            DG_R[Recv DATAGRAM<br/>MediaPacket deserialized]
        end

        subgraph "Reliable Channel"
            ST_S[Open bidi stream<br/>JSON length-prefixed<br/>SignalMessage]
            ST_R[Accept bidi stream<br/>JSON length-prefixed<br/>SignalMessage]
        end

        EP --> CONN
        CONN --> DG_S
        CONN --> DG_R
        CONN --> ST_S
        CONN --> ST_R
    end
```

### QUIC Configuration (VoIP-tuned)

| Setting | Value | Rationale |
|---------|-------|-----------|
| ALPN | `wzp` | Protocol identification |
| Idle timeout | 30s | Keep connection alive during silence |
| Keep-alive | 5s | Prevent NAT timeout |
| Datagram receive buffer | 65 KB | Buffer for burst arrivals |
| Flow control (recv) | 256 KB | Conservative for VoIP |
| Flow control (send) | 128 KB | Prevent bufferbloat |
| TLS | Self-signed certs | Development mode |
| Certificate verification | Disabled | Client accepts any cert |

## MediaPacket Wire Format

```
12-byte header:
┌─────────────────────────────────────────────────┐
│ Byte 0: V(1) T(1) CodecID(4) Q(1) FecHi(1)    │
│ Byte 1: FecLo(6) unused(2)                      │
│ Byte 2-3: Sequence number (u16 BE)               │
│ Byte 4-7: Timestamp ms (u32 BE)                  │
│ Byte 8: FEC block ID                             │
│ Byte 9: FEC symbol index                         │
│ Byte 10: Reserved                                │
│ Byte 11: CSRC count                              │
├─────────────────────────────────────────────────┤
│ Payload: Opus-encoded audio frame                │
├─────────────────────────────────────────────────┤
│ Optional: QualityReport (4 bytes, if Q=1)        │
│   loss_pct(u8) rtt_4ms(u8) jitter_ms(u8)        │
│   bitrate_cap_kbps(u8)                           │
└─────────────────────────────────────────────────┘
```

## Relay Room Mode (SFU)

```mermaid
graph LR
    subgraph "Room: android"
        P1[Phone A<br/>QUIC conn] -->|MediaPacket| RELAY[Relay SFU]
        RELAY -->|MediaPacket| P2[Phone B<br/>QUIC conn]
        P2 -->|MediaPacket| RELAY
        RELAY -->|MediaPacket| P1
    end

    Note1["Room name from QUIC TLS SNI<br/>No auth required<br/>Packets forwarded to all others"]
```

The relay operates as a Selective Forwarding Unit:
1. Client connects via QUIC, room name extracted from TLS SNI
2. Crypto handshake completes (relay has its own ephemeral identity)
3. Client joins named room
4. All received media packets are forwarded to every other participant in the room
5. Signaling messages are not forwarded (point-to-point with relay)

## Adaptive Quality System

```mermaid
graph TD
    QR[QualityReport<br/>loss%, RTT, jitter] --> AQC[AdaptiveQualityController]

    AQC -->|"loss<10%, RTT<400ms"| GOOD[GOOD<br/>Opus 24kbps<br/>FEC 20%<br/>20ms frames]
    AQC -->|"loss 10-40%<br/>RTT 400-600ms"| DEG[DEGRADED<br/>Opus 6kbps<br/>FEC 50%<br/>40ms frames]
    AQC -->|"loss>40%<br/>RTT>600ms"| CAT[CATASTROPHIC<br/>Codec2 1.2kbps<br/>FEC 100%<br/>40ms frames]

    GOOD -->|"Hysteresis:<br/>sustained degradation"| DEG
    DEG -->|"Sustained improvement"| GOOD
    DEG -->|"Further degradation"| CAT
    CAT -->|"Improvement"| DEG
```

| Profile | Codec | Bitrate | FEC Ratio | Frame Size | FEC Block |
|---------|-------|---------|-----------|------------|-----------|
| GOOD | Opus 24k | 24 kbps | 20% | 20ms | 5 frames |
| DEGRADED | Opus 6k | 6 kbps | 50% | 40ms | 10 frames |
| CATASTROPHIC | Codec2 1.2k | 1.2 kbps | 100% | 40ms | 8 frames |

## Module Dependency Graph

```mermaid
graph BT
    PROTO[wzp-proto<br/>Types, traits, jitter,<br/>quality, session]
    CODEC[wzp-codec<br/>Opus, Codec2, AEC,<br/>AGC, resampling]
    FEC[wzp-fec<br/>RaptorQ fountain codes]
    CRYPTO[wzp-crypto<br/>Ed25519, X25519,<br/>ChaCha20-Poly1305]
    TRANSPORT[wzp-transport<br/>QUIC, datagrams,<br/>signaling streams]
    ANDROID[wzp-android<br/>Engine, JNI bridge,<br/>Oboe audio, pipeline]
    RELAY[wzp-relay<br/>SFU, rooms, auth,<br/>metrics, probes]

    CODEC --> PROTO
    FEC --> PROTO
    CRYPTO --> PROTO
    TRANSPORT --> PROTO
    ANDROID --> PROTO
    ANDROID --> CODEC
    ANDROID --> FEC
    ANDROID --> CRYPTO
    ANDROID --> TRANSPORT
    RELAY --> PROTO
    RELAY --> CRYPTO
    RELAY --> TRANSPORT
```

## File Map

### Kotlin (`android/app/src/main/java/com/wzp/`)

| File | Purpose |
|------|---------|
| `WzpApplication.kt` | App entry, notification channel creation |
| `engine/WzpEngine.kt` | JNI wrapper for native engine |
| `engine/WzpCallback.kt` | Callback interface for engine events |
| `engine/CallStats.kt` | Stats data class with JSON deserialization |
| `ui/call/CallActivity.kt` | Activity host, permissions, theme |
| `ui/call/CallViewModel.kt` | MVVM state holder, stats polling |
| `ui/call/InCallScreen.kt` | Compose UI (idle + in-call states) |
| `service/CallService.kt` | Foreground service, wake/wifi locks |
| `audio/AudioRouteManager.kt` | Speaker/earpiece/Bluetooth routing |

### Rust (`crates/wzp-android/src/`)

| File | Purpose |
|------|---------|
| `lib.rs` | Module declarations |
| `jni_bridge.rs` | JNI FFI (panic-safe, proper jni crate) |
| `engine.rs` | Call orchestrator (threads, channels, lifecycle) |
| `pipeline.rs` | Codec pipeline (AEC, AGC, encode, FEC, jitter, decode) |
| `audio_android.rs` | Oboe backend, SPSC ring buffers, RT scheduling |
| `commands.rs` | Engine command enum |
| `stats.rs` | CallState/CallStats types (serde) |

### C++ (`crates/wzp-android/cpp/`)

| File | Purpose |
|------|---------|
| `oboe_bridge.h` | FFI header for Rust-C++ audio interface |
| `oboe_bridge.cpp` | Oboe capture/playout callbacks, ring buffer I/O |
| `oboe_stub.cpp` | No-op stub for non-Android builds |

### Build

| File | Purpose |
|------|---------|
| `android/app/build.gradle.kts` | Android build config, cargo-ndk task |
| `crates/wzp-android/Cargo.toml` | Rust dependencies (cdylib output) |
| `crates/wzp-android/build.rs` | C++ compilation, Oboe fetch |
