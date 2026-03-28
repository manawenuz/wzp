# WarzonePhone Protocol Design & Architecture

## Network Topology

```
                 Lossy / censored link
                 ◄──────────────────────►
  ┌────────┐      ┌─────────┐      ┌─────────┐      ┌─────────────┐
  │ Client │─QUIC─│ Relay A │─QUIC─│ Relay B │─QUIC─│ Destination │
  └────────┘      └─────────┘      └─────────┘      └─────────────┘
      │                │                │                   │
   Encode           Forward          Forward             Decode
   FEC              FEC              FEC                 FEC
   Encrypt          (opaque)         (opaque)            Decrypt
```

In the simplest deployment a single relay serves as the meeting point (room mode, SFU). Clients connect directly to one relay, which forwards media to all other participants in the same room. For censorship-resistant links, two relays can be chained: a client-facing relay forwards all traffic to a remote relay via QUIC.

Room names are carried in the QUIC SNI field during the TLS handshake, so a single relay can host many independent rooms without additional signaling.

## Protocol Stack

```
┌──────────────────────────────────────────────┐
│  Application (Opus / Codec2 audio)           │  wzp-codec
├──────────────────────────────────────────────┤
│  Redundancy (RaptorQ FEC + interleaving)     │  wzp-fec
├──────────────────────────────────────────────┤
│  Crypto (ChaCha20-Poly1305 + AEAD)          │  wzp-crypto
├──────────────────────────────────────────────┤
│  Transport (QUIC DATAGRAM + reliable stream) │  wzp-transport
├──────────────────────────────────────────────┤
│  Obfuscation (Phase 2 — trait defined)       │  wzp-proto::ObfuscationLayer
└──────────────────────────────────────────────┘
```

Audio and FEC are end-to-end between caller and callee. The relay operates on opaque, encrypted, FEC-protected packets. Crypto keys are never shared with relays.

## Wire Format

### MediaHeader (12 bytes)

```
Byte 0:  [V:1][T:1][CodecID:4][Q:1][FecRatioHi:1]
Byte 1:  [FecRatioLo:6][unused:2]
Byte 2-3: Sequence number (big-endian u16)
Byte 4-7: Timestamp in ms since session start (big-endian u32)
Byte 8:   FEC block ID (wrapping u8)
Byte 9:   FEC symbol index within block
Byte 10:  Reserved / flags
Byte 11:  CSRC count (for future mixing)
```

Field details:

| Field | Bits | Description |
|-------|------|-------------|
| V | 1 | Protocol version (0 = v1) |
| T | 1 | 1 = FEC repair packet, 0 = source media |
| CodecID | 4 | Codec identifier (0=Opus24k, 1=Opus16k, 2=Opus6k, 3=Codec2_3200, 4=Codec2_1200) |
| Q | 1 | QualityReport trailer appended |
| FecRatio | 7 | FEC ratio encoded as 7-bit value (0-127 maps to 0.0-2.0) |
| Seq | 16 | Wrapping packet sequence number |
| Timestamp | 32 | Milliseconds since session start |
| FEC block | 8 | Source block ID (wrapping) |
| FEC symbol | 8 | Symbol index within the FEC block |
| Reserved | 8 | Reserved flags |
| CSRC count | 8 | Contributing source count (future) |

Defined in `crates/wzp-proto/src/packet.rs` as `MediaHeader`.

### QualityReport (4 bytes)

Appended to a media packet when the Q flag is set.

```
Byte 0: loss_pct     — 0-255 maps to 0-100% loss
Byte 1: rtt_4ms      — RTT in 4ms units (0-255 = 0-1020ms)
Byte 2: jitter_ms    — Jitter in milliseconds
Byte 3: bitrate_cap  — Max receive bitrate in kbps
```

Defined in `crates/wzp-proto/src/packet.rs` as `QualityReport`.

### MediaPacket

A complete media packet on the wire:

```
[MediaHeader: 12 bytes][Payload: variable][QualityReport: 4 bytes if Q=1]
```

Defined in `crates/wzp-proto/src/packet.rs` as `MediaPacket`.

### SignalMessage (reliable stream)

Signaling uses length-prefixed JSON over reliable QUIC bidirectional streams. Each message opens a new bidi stream, writes a 4-byte big-endian length prefix followed by the JSON payload, then finishes the send side.

Variants defined in `crates/wzp-proto/src/packet.rs`:

- `CallOffer` — identity_pub, ephemeral_pub, signature, supported_profiles
- `CallAnswer` — identity_pub, ephemeral_pub, signature, chosen_profile
- `IceCandidate` — NAT traversal candidate string
- `Rekey` — new_ephemeral_pub, signature
- `QualityUpdate` — report, recommended_profile
- `Ping` / `Pong` — timestamp_ms for RTT measurement
- `Hangup` — reason (Normal, Busy, Declined, Timeout, Error)

## FEC Strategy

WarzonePhone uses **RaptorQ fountain codes** (via the `raptorq` crate) for forward error correction. This is implemented in `crates/wzp-fec/`.

### Block Structure

Audio frames are grouped into FEC blocks. Each block contains a fixed number of source symbols (configured per quality profile). Each source symbol is a single encoded audio frame, zero-padded to a uniform 256-byte symbol size with a 2-byte little-endian length prefix.

### Encoding Process

1. Audio frames are added to the encoder as source symbols
2. When a block is full (`frames_per_block` symbols), repair symbols are generated
3. The repair ratio determines how many repair symbols: `ceil(num_source * ratio)`
4. Both source and repair packets are transmitted with the block ID and symbol index in the header

### Decoding Process

1. Received symbols (source or repair) are fed to the decoder keyed by block ID
2. The decoder attempts reconstruction when sufficient symbols arrive
3. RaptorQ can recover the full block from any `K` symbols out of `K + R` total (where K = source count, R = repair count)
4. Old blocks are expired via wrapping u8 distance

### Interleaving

The `Interleaver` spreads symbols from multiple FEC blocks across transmission slots in round-robin fashion. With depth=3, a burst loss of 6 consecutive packets damages at most 2 symbols per block instead of 6 symbols in one block.

### FEC Configuration by Quality Tier

| Tier | Frames/Block | Repair Ratio | Total Bandwidth Overhead |
|------|-------------|-------------|-------------------------|
| GOOD | 5 | 0.2 (20%) | 1.2x |
| DEGRADED | 10 | 0.5 (50%) | 1.5x |
| CATASTROPHIC | 8 | 1.0 (100%) | 2.0x |

## Adaptive Quality

Three quality tiers drive codec and FEC selection. The controller is implemented in `crates/wzp-proto/src/quality.rs` as `AdaptiveQualityController`.

### Tier Thresholds

| Tier | Loss | RTT | Codec | FEC Ratio |
|------|------|-----|-------|-----------|
| GOOD | < 10% | < 400ms | Opus 24kbps, 20ms frames | 0.2 |
| DEGRADED | 10-40% or 400-600ms | | Opus 6kbps, 40ms frames | 0.5 |
| CATASTROPHIC | > 40% or > 600ms | | Codec2 1200bps, 40ms frames | 1.0 |

### Hysteresis

- **Downgrade**: Triggers after 3 consecutive reports in a worse tier (fast reaction)
- **Upgrade**: Triggers after 10 consecutive reports in a better tier (slow, cautious)
- **Step limit**: Upgrades move only one tier at a time (Catastrophic -> Degraded -> Good)
- **History**: A sliding window of 20 recent reports is maintained for smoothing
- **Force mode**: Manual `force_profile()` disables adaptive logic entirely

### QualityProfile Constants

```rust
GOOD:         Opus24k,     fec=0.2, 20ms, 5 frames/block  → 28.8 kbps total
DEGRADED:     Opus6k,      fec=0.5, 40ms, 10 frames/block → 9.0 kbps total
CATASTROPHIC: Codec2_1200, fec=1.0, 40ms, 8 frames/block  → 2.4 kbps total
```

## Encryption

Implemented in `crates/wzp-crypto/`.

### Identity Model (Warzone-Compatible)

- **Seed**: 32-byte random value (BIP39 mnemonic for backup)
- **Ed25519**: Derived via `HKDF(seed, "warzone-ed25519-identity")` -- signing/identity
- **X25519**: Derived via `HKDF(seed, "warzone-x25519-identity")` -- encryption
- **Fingerprint**: `SHA-256(Ed25519_pub)[:16]` -- 128-bit identifier

### Per-Call Key Exchange

1. Each side generates an ephemeral X25519 keypair
2. Ephemeral public keys are exchanged via `CallOffer`/`CallAnswer` signaling
3. Signatures are computed: `Ed25519_sign(ephemeral_pub || context_string)`
4. Shared secret: `X25519_DH(our_ephemeral_secret, peer_ephemeral_pub)`
5. Session key: `HKDF(shared_secret, "warzone-session-key")` -> 32 bytes

### Nonce Construction (12 bytes, not transmitted)

```
session_id[0..4] || sequence_number (u32 BE) || direction (1 byte) || padding (3 bytes zero)
```

- `session_id`: First 4 bytes of `SHA-256(session_key)`
- `direction`: 0 = Send, 1 = Recv
- Nonces are derived deterministically, saving 12 bytes per packet

### AEAD Encryption

- Algorithm: ChaCha20-Poly1305
- AAD: The 12-byte MediaHeader (authenticated but not encrypted)
- Tag: 16 bytes appended to ciphertext
- Overhead per packet: 16 bytes

### Rekeying

- Trigger: Every 2^16 packets (65536)
- Process: New ephemeral X25519 exchange, mixed with old key via HKDF
- Key evolution: `HKDF(old_key as salt, new_DH_result, "warzone-rekey")`
- Old key is zeroized after derivation (forward secrecy)
- Sequence counters reset to 0 after rekey

### Anti-Replay

- Sliding window of 1024 packets using a bitmap
- Sequence numbers too old (> 1024 behind highest seen) are rejected
- Handles u16 wrapping correctly (RFC 1982 serial number arithmetic)
- Implemented in `crates/wzp-crypto/src/anti_replay.rs` as `AntiReplayWindow`

## Jitter Buffer

Implemented in `crates/wzp-proto/src/jitter.rs` as `JitterBuffer`.

- **Structure**: BTreeMap keyed by sequence number for ordered playout
- **Target depth**: 50 packets (1 second) default
- **Max depth**: 250 packets (5 seconds at 20ms/frame)
- **Min depth**: 25 packets (0.5 seconds) before playout begins
- **Sequence wrapping**: RFC 1982 serial number arithmetic for u16
- **Duplicate handling**: Silently dropped
- **Late packets**: Packets arriving after their sequence has been played out are dropped
- **Overflow**: When buffer exceeds max depth, oldest packets are evicted

### Playout Results

- `Packet(MediaPacket)` -- normal delivery
- `Missing { seq }` -- gap detected, decoder should generate PLC
- `NotReady` -- buffer not yet filled to minimum depth

### Known Limitations

- No adaptive depth adjustment based on observed jitter (target_depth is configurable but not self-tuning in the current implementation)
- No timestamp-based playout scheduling (uses sequence-number ordering only)
- Jitter buffer drift has been observed during long echo tests

## Session State Machine

Defined in `crates/wzp-proto/src/session.rs`:

```
Idle -> Connecting -> Handshaking -> Active <-> Rekeying -> Active
                                       |
                                     Closed
```

- Media flows during both `Active` and `Rekeying` states
- Any state can transition to `Closed` via `Terminate` or `ConnectionLost`
- Invalid transitions produce a `TransitionError`

## Relay Modes

### Room Mode (Default, SFU)

- Clients join named rooms via QUIC SNI
- When a participant sends a packet, the relay forwards it to all other participants
- No transcoding -- packets are forwarded opaquely
- Rooms are auto-created when the first participant joins and auto-deleted when empty
- Managed by `RoomManager` in `crates/wzp-relay/src/room.rs`

### Forward Mode (`--remote`)

- All incoming traffic is forwarded to a remote relay via QUIC
- Two-pipeline architecture: upstream (client->remote) and downstream (remote->client)
- Each direction has its own `RelayPipeline` with FEC decode/encode and jitter buffering
- Intended for chaining relays across censored/lossy boundaries

### Relay Pipeline (Forward Mode)

Implemented in `crates/wzp-relay/src/pipeline.rs` as `RelayPipeline`:

```
Inbound:  recv -> FEC decode -> jitter buffer -> pop
Outbound: packet -> assign seq -> FEC encode -> repair packets -> send
```

The pipeline does NOT decode/re-encode audio. It operates on FEC-protected packets, managing loss recovery and re-FEC-encoding for the next hop.

## Transport

Implemented in `crates/wzp-transport/` using QUIC via the `quinn` crate.

### QUIC Configuration

- ALPN protocol: `wzp`
- Idle timeout: 30 seconds
- Keep-alive interval: 5 seconds
- DATAGRAM extension enabled (for unreliable media)
- Datagram receive buffer: 64 KB
- Receive window: 256 KB
- Send window: 128 KB
- Stream receive window: 64 KB per stream
- Initial RTT estimate: 300ms (tuned for high-latency links)

### Media Transport

- **Unreliable media**: QUIC DATAGRAM frames (no retransmission, no head-of-line blocking)
- **Reliable signaling**: QUIC bidirectional streams with length-prefixed JSON framing

### Path Quality Monitoring

`PathMonitor` in `crates/wzp-transport/src/path_monitor.rs` tracks:

- **Loss**: EWMA-smoothed percentage from sent/received packet counts
- **RTT**: EWMA-smoothed round-trip time (alpha=0.1)
- **Jitter**: EWMA of RTT variance (|current_rtt - previous_rtt|)
- **Bandwidth**: Estimated from bytes received over elapsed time

### Codec Selection by Tier

| Codec | Sample Rate | Frame Duration | Bitrate | Use Case |
|-------|------------|----------------|---------|----------|
| Opus24k | 48 kHz | 20ms (960 samples) | 24 kbps | Good conditions |
| Opus16k | 48 kHz | 20ms | 16 kbps | Moderate conditions |
| Opus6k | 48 kHz | 40ms (1920 samples) | 6 kbps | Degraded conditions |
| Codec2_3200 | 8 kHz | 20ms (160 samples) | 3.2 kbps | Poor conditions |
| Codec2_1200 | 8 kHz | 40ms (320 samples) | 1.2 kbps | Catastrophic conditions |

Opus operates at 48 kHz natively. When Codec2 is selected, the adaptive codec layer handles 48 kHz <-> 8 kHz resampling transparently using a simple linear resampler (6:1 decimation/interpolation).
