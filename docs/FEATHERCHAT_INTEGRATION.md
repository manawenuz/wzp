# WarzonePhone (WZP) Integration with featherChat

**Version:** 0.2.0
**Date:** 2026-03-28
**Status:** Confirmed Design Document (based on real code access to both codebases)

All items in this document are marked **[CONFIRMED]** and reference actual
source code in the `warzone/` (featherChat) and `warzone-phone/` (WZP)
repositories. The previous speculative draft has been fully replaced.

---

## 1. Executive Summary

### featherChat (Warzone Messenger)

A seed-based, end-to-end encrypted messaging system in Rust (v0.0.20).

**Crate structure** (`warzone/Cargo.toml`):

| Crate | Purpose |
|-------|---------|
| `warzone-protocol` | Core crypto & wire types (X3DH, Double Ratchet, Sender Keys, identity) |
| `warzone-server` | axum HTTP + WebSocket server with sled embedded DB |
| `warzone-client` | CLI/TUI client (clap + ratatui) |
| `warzone-wasm` | WASM bridge for web client |
| `warzone-mule` | Mule binary (placeholder) |

**Key primitives:** Ed25519 signing, X25519 DH, ChaCha20-Poly1305 AEAD,
HKDF-SHA256, Argon2id. Identity derived from a single BIP39 seed.

### WarzonePhone (WZP)

An encrypted voice calling system in Rust (v0.1.0, edition 2024, rust 1.85+).

**Crate structure** (`warzone-phone/Cargo.toml`):

| Crate | Purpose |
|-------|---------|
| `wzp-proto` | Shared types, traits, session state machine, jitter buffer, quality controller |
| `wzp-codec` | Adaptive audio encoding: Opus (24k/16k/6k) + Codec2 (3200/1200 bps) |
| `wzp-fec` | RaptorQ fountain codes with temporal interleaving |
| `wzp-crypto` | Per-call ChaCha20-Poly1305 sessions, X25519 key exchange, rekeying |
| `wzp-transport` | QUIC (quinn) with DATAGRAM frames for media, reliable streams for signaling |
| `wzp-relay` | Relay daemon: recv - FEC decode - jitter buffer - FEC encode - send |
| `wzp-client` | End-to-end voice call pipeline + cpal audio I/O |

**Key primitives:** X25519 ephemeral DH, ChaCha20-Poly1305 AEAD, Ed25519
signing, HKDF-SHA256, RaptorQ FEC, Opus + Codec2 codecs, QUIC transport.

### Why Integrate

[CONFIRMED] Both systems derive identity from a 32-byte seed via HKDF and
share the same cryptographic primitive stack (Ed25519, X25519, ChaCha20-Poly1305,
HKDF-SHA256). WZP's `KeyExchange` trait (`wzp-proto/src/traits.rs:141-176`)
explicitly documents compatibility with the "Warzone identity model" and its
`from_identity_seed()` method uses the same HKDF derivation pattern.

Integration benefits:

1. **Single identity** -- one BIP39 mnemonic controls messaging, calling, and
   Ethereum wallet.
2. **Reuse crypto infrastructure** -- featherChat's X3DH sessions provide
   authenticated peer relationships; WZP's per-call ephemeral exchange builds
   on the same identity keys.
3. **Encrypted signaling** -- call setup can travel through featherChat's E2E
   encrypted Double Ratchet channels.
4. **Shared contact/group model** -- featherChat groups map to WZP call rooms.
5. **Warzone resilience** -- voice messages as file attachments, missed call
   notifications via mule delivery.

---

## 2. Shared Identity Model

### featherChat Key Derivation

[CONFIRMED] `warzone-protocol/src/identity.rs:29-47` (`Seed::derive_identity()`):

```
BIP39 Seed (32 bytes)
    |
    +-- HKDF(ikm=seed, salt="", info="warzone-ed25519")   --> Ed25519 signing keypair
    |                                                           |
    |                                                           +-> SHA-256[:16] = Fingerprint
    |
    +-- HKDF(ikm=seed, salt="", info="warzone-x25519")    --> X25519 encryption keypair
    |
    +-- HKDF(ikm=seed, salt="", info="warzone-secp256k1") --> secp256k1 keypair (Ethereum)
    |
    +-- HKDF(ikm=seed, salt="", info="warzone-history")   --> History encryption key
```

Fingerprint: `SHA-256(Ed25519_pubkey)[:16]`, displayed as
`xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx:xxxx`.

### WZP Key Derivation

[CONFIRMED] `wzp-crypto/src/handshake.rs:32-53` (`WarzoneKeyExchange::from_identity_seed()`):

```
32-byte Seed
    |
    +-- HKDF(ikm=seed, salt=None, info="warzone-ed25519-identity")  --> Ed25519 signing keypair
    |
    +-- HKDF(ikm=seed, salt=None, info="warzone-x25519-identity")   --> X25519 static keypair
```

Fingerprint: `SHA-256(Ed25519_pubkey)[:16]` -- identical algorithm to featherChat.
See `wzp-crypto/src/handshake.rs:66-71`.

### Identity Compatibility Gap

[CONFIRMED] The HKDF info strings differ:

| Key | featherChat info | WZP info |
|-----|-----------------|----------|
| Ed25519 | `"warzone-ed25519"` | `"warzone-ed25519-identity"` |
| X25519 | `"warzone-x25519"` | `"warzone-x25519-identity"` |

**This means the same seed produces DIFFERENT keypairs in each system.**

**Resolution required:** One of the two must be updated to match. The
recommended approach is to update WZP to use featherChat's info strings
(`"warzone-ed25519"` and `"warzone-x25519"`), since featherChat is the
established system with deployed users and stored identities. This is a
two-line change in `wzp-crypto/src/handshake.rs:36,43`.

### Per-Call Ephemeral Keys (WZP-specific)

[CONFIRMED] WZP generates per-call ephemeral X25519 keypairs
(`wzp-crypto/src/handshake.rs:55-59`). The call session key is derived from:

```
shared_secret = X25519_DH(our_ephemeral_secret, peer_ephemeral_pub)
session_key   = HKDF(ikm=shared_secret, salt=None, info="warzone-session-key")
```

This is independent of featherChat's X3DH/Double Ratchet -- each call creates
fresh ephemeral keys for perfect forward secrecy per call.

---

## 3. Authentication Flow

### featherChat Challenge-Response Auth

[CONFIRMED] `warzone-server/src/routes/auth.rs:1-11`:

```
Step 1: Client -> Server   POST /v1/auth/challenge { fingerprint }
Step 2: Server -> Client   { challenge: random_hex(32), expires_at }
                           Challenge valid 60 seconds (CHALLENGE_TTL_SECS = 60)
Step 3: Client -> Server   POST /v1/auth/verify {
                             fingerprint,
                             challenge,
                             signature   // Ed25519 sign(challenge_bytes)
                           }
Step 4: Server verifies Ed25519 signature against stored PreKeyBundle
        (auth.rs:117-154)
Step 5: Server -> Client   { token: random_hex(32), expires_at }
                           Token valid 7 days (TOKEN_TTL_SECS = 604800)
Step 6: Client includes Authorization: Bearer <token> on requests
```

Challenges stored in-memory (`LazyLock<Mutex<HashMap>>`, auth.rs:54-55).
Tokens stored in `tokens` sled tree (key: token bytes, value: JSON
`{fingerprint, expires_at}`). The `validate_token()` function (auth.rs:177-186)
checks existence and expiry.

### WZP Authentication Model

[CONFIRMED] WZP does NOT have its own authentication server or HTTP endpoints.
Authentication is entirely peer-to-peer during the QUIC handshake:

1. Caller sends `SignalMessage::CallOffer` containing their Ed25519 identity
   public key, ephemeral X25519 public key, and an Ed25519 signature over
   `(ephemeral_pub || "call-offer")`.
   See `wzp-client/src/handshake.rs:22-45`.

2. Callee verifies the signature against the caller's identity public key,
   then sends `SignalMessage::CallAnswer` with their own identity key,
   ephemeral key, and signature over `(ephemeral_pub || "call-answer")`.
   See `wzp-relay/src/handshake.rs:19-80`.

3. Both sides derive the shared session key from the ephemeral DH.

### Integrated Auth Flow

For WZP to use featherChat infrastructure, the flow is:

```
featherChat Client                featherChat Server           WZP Relay/Peer
      |                                 |                           |
  Unlock seed (passphrase + Argon2id)   |                           |
      |                                 |                           |
  POST /v1/auth/challenge               |                           |
  POST /v1/auth/verify (Ed25519 sig)    |                           |
      |<--- bearer token (7d TTL) ------|                           |
      |                                 |                           |
  Send CallSignal via featherChat WS    |                           |
  (Double Ratchet encrypted)            |--- WS push ------------->|
      |                                 |                           |
      |  Connect QUIC to WZP relay/peer |                           |
      |  SignalMessage::CallOffer --------------------------------->|
      |  (identity_pub, ephemeral_pub, signature)                   |
      |                                 |                           |
      |<------------------------------------- SignalMessage::CallAnswer
      |  (identity_pub, ephemeral_pub, signature)                   |
      |                                 |                           |
      |  Both derive ChaCha20-Poly1305 session                     |
      |  ================ encrypted media flows ===================|
```

WZP validates peer identity via Ed25519 signature verification
(wzp-crypto/src/handshake.rs:79-88) rather than tokens. The featherChat
token is used only for accessing featherChat server resources (key bundles,
message relay, group membership).

### Proposed Server-Side Addition

A `POST /v1/auth/validate` endpoint should be added to featherChat server
to allow WZP relays to verify bearer tokens:

```
POST /v1/auth/validate
Body: { "token": "hex..." }
Response: { "valid": true, "fingerprint": "a3f8c912...", "expires_at": ... }
```

This reuses the existing `validate_token()` function from `auth.rs:177-186`.

---

## 4. Signaling Integration

### WZP Signal Messages

[CONFIRMED] `wzp-proto/src/packet.rs:249-310` defines `SignalMessage`:

```rust
pub enum SignalMessage {
    CallOffer {
        identity_pub: [u8; 32],
        ephemeral_pub: [u8; 32],
        signature: Vec<u8>,
        supported_profiles: Vec<QualityProfile>,
    },
    CallAnswer {
        identity_pub: [u8; 32],
        ephemeral_pub: [u8; 32],
        signature: Vec<u8>,
        chosen_profile: QualityProfile,
    },
    IceCandidate { candidate: String },
    Rekey {
        new_ephemeral_pub: [u8; 32],
        signature: Vec<u8>,
    },
    QualityUpdate {
        report: QualityReport,
        recommended_profile: QualityProfile,
    },
    Ping { timestamp_ms: u64 },
    Pong { timestamp_ms: u64 },
    Hangup { reason: HangupReason },
}
```

These are serialized as JSON over reliable QUIC streams
(`wzp-transport/src/reliable.rs:12-58`, length-prefixed framing: 4-byte
BE length + serde_json payload).

### Bridging Signaling via featherChat

To initiate a WZP call through featherChat, a new `WireMessage` variant
should be added to `warzone-protocol/src/message.rs`:

```rust
/// VoIP call signaling via WarzonePhone.
/// Encrypted by the existing Double Ratchet session.
CallSignal {
    id: String,
    sender_fingerprint: String,
    signal: Vec<u8>,  // Serialized wzp_proto::SignalMessage (JSON)
},
```

The `signal` field carries the serialized `SignalMessage` opaquely. The
featherChat server treats it identically to any other `WireMessage` -- an
encrypted blob routed via WebSocket.

### Signaling Flow (1:1 Call)

```
Alice (featherChat+WZP)       featherChat Server       Bob (featherChat+WZP)
       |                              |                         |
       |  WireMessage::CallSignal     |                         |
       |  { signal: CallOffer{...} }  |                         |
       |  (Double Ratchet encrypted)  |                         |
       |----------------------------->|--- WS push ------------>|
       |                              |                         |
       |                              |  WireMessage::CallSignal|
       |                              |  { signal: CallAnswer } |
       |<-----------------------------|<------------------------|
       |                              |                         |
       |  WireMessage::CallSignal     |                         |
       |  { signal: IceCandidate }    |                         |
       |----------------------------->|------------------------>|
       |                              |                         |
       |  ============ QUIC connection established ============ |
       |  ============ ephemeral X25519 DH complete =========== |
       |  ============ ChaCha20-Poly1305 media flows ========== |
       |                              |                         |
       |  WireMessage::CallSignal     |                         |
       |  { signal: Hangup{Normal} }  |                         |
       |----------------------------->|------------------------>|
```

### Server-Side Changes Required

1. **`extract_message_id()` in `routes/ws.rs:25-41`** -- add match arm:
   ```rust
   WireMessage::CallSignal { id, .. } => Some(id),
   ```

2. **No new routes needed** -- `CallSignal` messages flow through existing
   WebSocket relay (`routes/ws.rs:43-190`) and HTTP send/poll endpoints.
   The server treats them as opaque encrypted blobs.

3. **DedupTracker** -- existing bounded FIFO (10,000 IDs) handles call
   signaling dedup automatically.

---

## 5. Media Security

### Per-Call Encryption

[CONFIRMED] WZP uses per-call ChaCha20-Poly1305 sessions, NOT DTLS-SRTP.

**Key Exchange:** Ephemeral X25519 DH between caller and callee, expanded via
HKDF (`wzp-crypto/src/handshake.rs:90-114`):

```
shared_secret = X25519_DH(our_ephemeral, peer_ephemeral)
session_key   = HKDF(ikm=shared_secret, salt=None, info="warzone-session-key")
cipher        = ChaCha20-Poly1305(session_key)
```

**Nonce Construction:** Deterministic, not transmitted. 12-byte nonce layout
(`wzp-crypto/src/nonce.rs:17-24`):

```
Bytes 0-3:  session_id (SHA-256(session_key)[:4])
Bytes 4-7:  sequence_number (u32 big-endian)
Byte  8:    direction (0=Send, 1=Recv)
Bytes 9-11: zero padding
```

This saves 12 bytes per packet since nonces are never on the wire.

**AEAD:** Media packet header bytes serve as AAD (authenticated associated
data), so the header is authenticated but not encrypted
(`wzp-crypto/src/session.rs:62-87`). Encryption overhead is 16 bytes
(Poly1305 tag) per packet.

### Rekeying (Forward Secrecy)

[CONFIRMED] `wzp-crypto/src/rekey.rs:1-68`:

- Rekey interval: every 2^16 (65,536) packets (`REKEY_INTERVAL`).
- Rekeying uses fresh ephemeral X25519 DH mixed with the old key via HKDF:
  ```
  new_dh  = X25519(our_new_ephemeral, peer_new_ephemeral)
  new_key = HKDF(ikm=new_dh, salt=old_key, info="warzone-rekey")
  ```
- Old key material is zeroized after derivation (rekey.rs:54-55).
- Session sequence counters reset to zero after rekey (session.rs:134-135).
- Rekeying is signaled via `SignalMessage::Rekey` over the reliable QUIC stream
  (packet.rs:281-286), with Ed25519 signature over
  `(new_ephemeral_pub || session_id)`.

### Anti-Replay Protection

[CONFIRMED] `wzp-crypto/src/anti_replay.rs:1-136`:

- Sliding window bitmap: 1024-packet window (`WINDOW_SIZE = 1024`).
- Bitmap stored as `Vec<u64>` (16 words for 1024 bits).
- Handles u16 sequence number wrapping correctly (RFC 1982 serial arithmetic).
- Rejects duplicates and packets older than the window.

### Comparison with Previous DTLS-SRTP Proposal

The previous speculative document proposed DTLS-SRTP. The actual WZP
implementation uses a custom, lighter-weight approach:

| Aspect | Previous Proposal (DTLS-SRTP) | Actual WZP Implementation |
|--------|-------------------------------|---------------------------|
| Key exchange | DTLS handshake | Ephemeral X25519 DH via QUIC reliable stream |
| Encryption | SRTP (AES-128-CM or AES-256-GCM) | ChaCha20-Poly1305 (same as featherChat) |
| Nonce | SRTP packet index | Deterministic: session_id + seq + direction |
| Rekeying | DTLS renegotiation | Ephemeral DH + HKDF mixing every 65536 packets |
| Anti-replay | SRTP replay window | 1024-packet bitmap window |
| Certificate | X.509 (DTLS) | Ed25519 identity key (Warzone identity model) |
| Transport | UDP (DTLS + SRTP) | QUIC DATAGRAM frames |

The actual approach is more aligned with WireGuard's design philosophy than
WebRTC's.

---

## 6. Architecture Diagram

### Confirmed System Architecture

```
+==========================================================+
|                  featherChat Clients                      |
|                                                           |
|  +----------------+  +----------------+  +--------------+ |
|  | CLI/TUI Client |  | Web Client     |  | WZP Client   | |
|  | (warzone-      |  | (warzone-wasm) |  | (wzp-client) | |
|  |  client)       |  |                |  | cpal audio   | |
|  +-------+--------+  +-------+--------+  +------+-------+ |
|          |                    |                   |         |
|  +-------+--------------------+-------------------+------+ |
|  |                warzone-protocol                       | |
|  |  Identity . X3DH . Double Ratchet . Sender Keys      | |
|  |  + CallSignal WireMessage variant (new)               | |
|  +------------------------------+------------------------+ |
+==========================================================+
                                  |
                    HTTP / WebSocket / bincode
                                  |
+==========================================================+
|                 featherChat Server                        |
|                                                           |
|  +----------+  +----------+  +---------+  +-------------+ |
|  | HTTP API |  | WebSocket|  | Auth    |  | Message     | |
|  |  (axum)  |  |  Relay   |  | (Ed25519|  | Router +    | |
|  | :7700    |  |          |  | challng)|  | Dedup       | |
|  +----+-----+  +----+-----+  +----+----+  +------+------+ |
|       |             |             |               |        |
|  +----+-------------+-------------+---------------+------+ |
|  |                   sled Database                       | |
|  |  keys . messages . groups . aliases . tokens          | |
|  +-------------------------------------------------------+ |
+==========================================================+
                                  |
                     (future: federation)
                                  |
+==========================================================+
|                   WZP Infrastructure                      |
|                                                           |
|  +---------------------------------------------------+   |
|  |                  WZP Relay Daemon                  |   |
|  |                  (wzp-relay crate)                 |   |
|  |                                                    |   |
|  |  +----------------+  +---------------------------+ |   |
|  |  | QUIC Endpoint  |  | Per-Session Pipeline      | |   |
|  |  | (quinn :4433)  |  | recv->FEC->jitter->FEC->  | |   |
|  |  | ALPN: "wzp"    |  | send (no audio decode)    | |   |
|  |  +----------------+  +---------------------------+ |   |
|  |                                                    |   |
|  |  +----------------+  +---------------------------+ |   |
|  |  | Session Mgr    |  | Path Quality Monitor      | |   |
|  |  | (max 100 conc) |  | (EWMA loss/RTT/jitter)   | |   |
|  |  +----------------+  +---------------------------+ |   |
|  +---------------------------------------------------+   |
+==========================================================+
```

### Signaling vs Media Paths

```
                  SIGNALING PATH (E2E encrypted via featherChat)
                  ==================================================
Alice             featherChat Server           Bob
  |  CallSignal     (WS relay)                  |
  |  (Double Ratchet encrypted)                 |
  |  ---------> route as opaque blob ---------> |   CallOffer
  |  <--------- route as opaque blob <--------- |   CallAnswer
  |  ---------> route as opaque blob ---------> |   IceCandidate
  |                                             |

                  MEDIA PATH (ChaCha20-Poly1305 encrypted, via QUIC)
                  ==================================================
  |                                             |
  |  --- QUIC connect (ALPN "wzp") -----------> |   (P2P or via relay)
  |  --- SignalMessage::CallOffer (reliable) --> |   identity + ephemeral keys
  |  <-- SignalMessage::CallAnswer (reliable) -- |   identity + ephemeral keys
  |                                             |
  |  === QUIC DATAGRAM: encrypted MediaPacket =>|   audio (Opus/Codec2)
  |  <== QUIC DATAGRAM: encrypted MediaPacket ==|   + FEC repair symbols
  |                                             |

                  WHAT EACH COMPONENT SEES
                  ==================================================

  featherChat Server:
    - Opaque bincode blobs (CallSignal variant)
    - Sender + recipient fingerprints (metadata)
    - Cannot read signaling content

  WZP Relay:
    - Encrypted MediaPackets (cannot decrypt audio)
    - FEC block structure (can forward repair symbols)
    - Packet timing + sizes (traffic analysis possible)
    - IP addresses of both peers

  Neither server:
    - Plaintext audio
    - Session keys
    - Call content
```

---

## 7. Codec Details

### Codec Stack

[CONFIRMED] `wzp-proto/src/codec_id.rs:1-68` and `wzp-codec/src/lib.rs`:

| Codec | Bitrate | Frame Duration | Sample Rate | Wire Format | Use Case |
|-------|---------|----------------|-------------|-------------|----------|
| Opus 24k | 24 kbps | 20 ms | 48 kHz | Variable (~60 bytes/frame) | Good conditions |
| Opus 16k | 16 kbps | 20 ms | 48 kHz | Variable (~40 bytes/frame) | Moderate conditions |
| Opus 6k | 6 kbps | 40 ms | 48 kHz | Variable (~30 bytes/frame) | Degraded conditions |
| Codec2 3200 | 3.2 kbps | 20 ms | 8 kHz | 8 bytes/frame | Poor conditions |
| Codec2 1200 | 1.2 kbps | 40 ms | 8 kHz | 6 bytes/frame | Catastrophic conditions |

**Opus:** Via `audiopus` crate (libopus bindings). Supports inband FEC and DTX
(`wzp-proto/src/traits.rs:27-31`).

**Codec2:** Via the pure-Rust `codec2` crate. Provides military-grade voice
coding at extremely low bitrates.

### Adaptive Codec Switching

[CONFIRMED] `wzp-codec/src/adaptive.rs`:

- `AdaptiveEncoder` wraps both `OpusEncoder` and `Codec2Encoder`.
- Callers always provide 48 kHz mono PCM; resampling is handled internally.
- When Codec2 is active: 48 kHz -> 8 kHz downsampling (6:1 decimation with
  box filter) before encoding (`wzp-codec/src/resample.rs:10-21`).
- When decoding Codec2: 8 kHz -> 48 kHz upsampling (linear interpolation)
  after decoding (`resample.rs:27-51`).
- Profile switching via `set_profile()` is instantaneous -- both inner codecs
  are always instantiated.

### Quality Profiles

[CONFIRMED] `wzp-proto/src/codec_id.rs:82-113`:

| Profile | Codec | FEC Ratio | Frame Duration | Frames/Block | Total Bitrate |
|---------|-------|-----------|----------------|--------------|---------------|
| GOOD | Opus 24k | 0.2 (20%) | 20 ms | 5 | ~28.8 kbps |
| DEGRADED | Opus 6k | 0.5 (50%) | 40 ms | 10 | ~9.0 kbps |
| CATASTROPHIC | Codec2 1200 | 1.0 (100%) | 40 ms | 8 | ~2.4 kbps |

---

## 8. Transport Layer

### QUIC via quinn

[CONFIRMED] `wzp-transport/src/lib.rs` and sub-modules:

WZP uses QUIC (RFC 9000) via the `quinn` crate (v0.11) as its transport layer.
This is fundamentally different from WebRTC's UDP+DTLS+SRTP stack.

**ALPN protocol:** `"wzp"` (`wzp-transport/src/config.rs:27,47`).

**Two transport modes on one QUIC connection:**

| Mode | QUIC Feature | Used For | Reliability |
|------|-------------|----------|-------------|
| Media | DATAGRAM frames | `MediaPacket` (audio + FEC) | Unreliable (fire-and-forget) |
| Signaling | Bidirectional streams | `SignalMessage` (JSON) | Reliable, ordered |

### QUIC Configuration

[CONFIRMED] `wzp-transport/src/config.rs:60-83`:

| Parameter | Value | Rationale |
|-----------|-------|-----------|
| Idle timeout | 30 seconds | Tolerant of lossy links |
| Keep-alive interval | 5 seconds | Prevents NAT timeout |
| DATAGRAM receive buffer | 64 KB | Sufficient for media burst |
| Receive window | 256 KB | Conservative for bandwidth-constrained links |
| Send window | 128 KB | Prevents buffer bloat |
| Stream receive window | 64 KB per stream | Signaling messages are small |
| Initial RTT estimate | 300 ms | Aggressive for high-latency links |

### Media Packet Transport

[CONFIRMED] `wzp-transport/src/datagram.rs` and `wzp-transport/src/quic.rs:42-67`:

Media packets are serialized via `MediaPacket::to_bytes()` and sent as QUIC
DATAGRAM frames. MTU checking is performed before send
(`quic.rs:47-54`). The `PathMonitor` records send/receive observations for
quality estimation (`quic.rs:57-60`).

### Signaling Transport

[CONFIRMED] `wzp-transport/src/reliable.rs:9-58`:

Signaling messages use length-prefixed JSON framing over QUIC bidirectional
streams. Format: `[4-byte BE length][JSON payload]`. Maximum message size: 1 MB.
Each signal message opens a new bidi stream and finishes the send side after
writing.

### Path Quality Monitoring

[CONFIRMED] `wzp-transport/src/path_monitor.rs`:

- EWMA smoothing factor: 0.1 (`ALPHA`).
- Tracks: loss percentage, RTT, jitter (RTT variance), bandwidth estimate.
- Loss estimated from sent/received packet count gaps.
- Bandwidth estimated from bytes received over time.

---

## 9. Forward Error Correction (FEC)

### RaptorQ Fountain Codes

[CONFIRMED] `wzp-fec/src/lib.rs` and sub-modules. Uses the `raptorq` crate (v2).

**Architecture:**

- Source symbols = encoded audio frames (one per codec frame).
- Frames are grouped into blocks (configurable frames_per_block).
- After a block is full, repair symbols are generated at the configured ratio.
- Decoder can reconstruct the full block from any sufficient subset of
  source + repair symbols.

**Adaptive FEC** (`wzp-fec/src/adaptive.rs:18-49`):

| Profile | Frames/Block | Repair Ratio | Symbol Size | Overhead |
|---------|-------------|-------------|-------------|----------|
| GOOD | 5 | 0.2 (20%) | 256 bytes | 1.2x |
| DEGRADED | 10 | 0.5 (50%) | 256 bytes | 1.5x |
| CATASTROPHIC | 8 | 1.0 (100%) | 256 bytes | 2.0x |

**FEC traits** (`wzp-proto/src/traits.rs:52-93`):

```rust
trait FecEncoder: Send + Sync {
    fn add_source_symbol(&mut self, data: &[u8]) -> Result<(), FecError>;
    fn generate_repair(&mut self, ratio: f32) -> Result<Vec<(u8, Vec<u8>)>, FecError>;
    fn finalize_block(&mut self) -> Result<u8, FecError>;
    fn current_block_id(&self) -> u8;
    fn current_block_size(&self) -> usize;
}

trait FecDecoder: Send + Sync {
    fn add_symbol(&mut self, block_id: u8, symbol_index: u8, is_repair: bool, data: &[u8]) -> ...;
    fn try_decode(&mut self, block_id: u8) -> Result<Option<Vec<Vec<u8>>>, FecError>;
    fn expire_before(&mut self, block_id: u8);
}
```

### FEC in Media Packet Header

[CONFIRMED] `wzp-proto/src/packet.rs:1-43`:

The 12-byte `MediaHeader` carries FEC metadata:

- Bit 6 (`is_repair`): distinguishes source from repair symbols.
- Bits for `fec_ratio_encoded`: 7-bit value (0-127) encoding the FEC ratio.
- Byte 8 (`fec_block`): block ID (wrapping u8).
- Byte 9 (`fec_symbol`): symbol index within the block.

---

## 10. Relay Architecture

### WZP Relay Daemon

[CONFIRMED] `wzp-relay/src/lib.rs` and sub-modules.

The relay is a forwarding node that bridges two QUIC endpoints without
decoding audio. Pipeline: `recv -> FEC decode -> jitter buffer -> FEC encode -> send`.

**Key design:** The relay operates on encrypted, FEC-protected packets. It
can reassemble FEC blocks and re-encode them for the next hop, but it never
accesses plaintext audio.

### Relay Configuration

[CONFIRMED] `wzp-relay/src/config.rs:8-35`:

| Parameter | Default | Purpose |
|-----------|---------|---------|
| Listen address | `0.0.0.0:4433` | Client-facing QUIC endpoint |
| Remote relay | `None` | Inter-relay link (if chained) |
| Max sessions | 100 | Concurrent call limit |
| Jitter target depth | 50 packets (1s) | Target buffer before playout |
| Jitter max depth | 250 packets (5s) | Maximum buffer before eviction |

### Relay Pipeline

[CONFIRMED] `wzp-relay/src/pipeline.rs:42-230`:

Each `RelayPipeline` instance manages one direction of a call:

1. **Ingest:** Incoming `MediaPacket` fed to FEC decoder + quality controller.
2. **FEC Decode:** If a complete block's worth of symbols received, recover
   source frames.
3. **Jitter Buffer:** Reorder recovered frames by sequence number.
4. **Playout:** Pop frames in order for forwarding (with PLC gap marking).
5. **Outbound FEC:** Re-encode with FEC for the next hop.

Quality tier changes are detected from `QualityReport` trailers in packets.
On tier change, FEC encoder/decoder are reconfigured (`pipeline.rs:93-107`).

### Session Management

[CONFIRMED] `wzp-relay/src/session_mgr.rs`:

- Each call gets a `RelaySession` with two pipelines (upstream + downstream).
- `SessionManager` tracks all active sessions in a `HashMap<SessionId, RelaySession>`.
- Capacity limited to `max_sessions` (default 100).
- Idle sessions expire after timeout (`expire_idle()` method).
- Session state machine from `wzp-proto::Session` governs lifecycle.

### Relay Handshake

[CONFIRMED] `wzp-relay/src/handshake.rs:19-80`:

The relay performs the callee side of the WZP key exchange:

1. Receive `CallOffer`, verify caller's Ed25519 signature.
2. Generate own ephemeral X25519 keypair.
3. Sign `(ephemeral_pub || "call-answer")`.
4. Derive `ChaChaSession` from X25519 DH.
5. Choose the best quality profile from caller's supported list (prefer
   highest bitrate).
6. Send `CallAnswer`.

---

## 11. Jitter Buffer

[CONFIRMED] `wzp-proto/src/jitter.rs`:

- **Data structure:** `BTreeMap<u16, MediaPacket>` ordered by sequence number.
- **Wrapping-aware:** Uses RFC 1982 serial number arithmetic for u16 sequence
  comparison (`seq_before()`, jitter.rs:174-177).
- **Default configuration** (`default_5s()`): target 50 packets (1s), max 250
  packets (5s), min 25 packets (0.5s) before playout begins.
- **Playout results:** `Packet` (data available), `Missing` (gap -- trigger PLC),
  `NotReady` (insufficient buffer depth).
- **Statistics tracked:** packets received, played, lost, late, duplicate, current depth.
- **Eviction:** When buffer exceeds `max_depth`, oldest packets are evicted.

---

## 12. Adaptive Quality Control

[CONFIRMED] `wzp-proto/src/quality.rs`:

### Tier Classification

| Tier | Loss Threshold | RTT Threshold | Profile |
|------|---------------|---------------|---------|
| Good | < 10% | < 400 ms | Opus 24k, 20% FEC |
| Degraded | 10-40% | 400-600 ms | Opus 6k, 50% FEC |
| Catastrophic | > 40% | > 600 ms | Codec2 1200, 100% FEC |

### Hysteresis

- **Downgrade threshold:** 3 consecutive reports in a worse tier (fast reaction).
- **Upgrade threshold:** 10 consecutive reports in a better tier (slow, cautious).
- **Step-at-a-time upgrades:** Catastrophic -> Degraded -> Good (never skip).
- **History:** Sliding window of 20 recent `QualityReport` observations.
- **Force override:** `force_profile()` disables adaptive logic.

### Quality Reports

[CONFIRMED] `wzp-proto/src/packet.rs:143-184`:

4-byte `QualityReport` appended to media packets when the Q flag is set:

| Field | Size | Encoding |
|-------|------|----------|
| loss_pct | 1 byte | 0-255 maps to 0-100% |
| rtt_4ms | 1 byte | RTT in 4ms units (0-1020 ms range) |
| jitter_ms | 1 byte | Jitter in milliseconds |
| bitrate_cap_kbps | 1 byte | Max receive bitrate in kbps |

---

## 13. Session State Machine

[CONFIRMED] `wzp-proto/src/session.rs:1-144`:

```
Idle --> Connecting --> Handshaking --> Active <--> Rekeying --> Active
                                          |
                                        Closed
```

| Transition | From | To | Trigger |
|-----------|------|-----|---------|
| Initiate | Idle | Connecting | User starts call |
| Connected | Connecting | Handshaking | QUIC connection established |
| HandshakeComplete | Handshaking | Active | Crypto handshake done |
| RekeyStart | Active | Rekeying | Periodic or requested rekey |
| RekeyComplete | Rekeying | Active | New keys installed |
| Terminate/ConnectionLost | Any active | Closed | Hangup or error |

**Media continues flowing during Rekeying** (`is_media_active()` returns
`true` for both `Active` and `Rekeying` states, session.rs:137-138).

Session tracks: unique 16-byte session ID, last transition timestamp,
rekey count.

---

## 14. Group Calls

### featherChat Groups as Call Rooms

[CONFIRMED] featherChat group infrastructure (from ARCHITECTURE.md):

- `POST /v1/groups/create` -- create group
- `POST /v1/groups/:name/join` -- join group
- `GET /v1/groups/:name/members` -- list members with aliases
- `POST /v1/groups/:name/send` -- fan-out message to all members

A featherChat group maps 1:1 to a WZP conference call room.

### Group Call Architecture

WZP currently implements 1:1 calls. Group calls require:

1. **Signaling:** Use featherChat's group message fan-out to distribute
   `CallSignal` to all members via their 1:1 encrypted channels.

2. **Media topology:** Two options:
   - **Mesh:** Each participant connects directly to every other (O(N^2)
     connections). Suitable for 2-4 participants.
   - **SFU:** Each participant sends one stream to a relay; relay forwards
     to all others. The WZP relay crate already supports per-session
     pipeline management.

3. **Media encryption for groups:** featherChat already implements Sender Keys
   for group messaging (`warzone-protocol/src/sender_keys.rs`). The same
   concept applies to media:
   - Each participant generates a media Sender Key.
   - Distribute via 1:1 encrypted featherChat channels.
   - Encrypt QUIC DATAGRAM payloads with Sender Key instead of per-pair session key.
   - SFU/relay forwards encrypted packets without decryption.

4. **WZP relay as SFU:** The relay's `SessionManager` (max 100 sessions)
   and pipeline architecture could be extended to fan-out mode. The relay
   already operates on encrypted packets without decoding audio, making it
   suitable as a zero-knowledge SFU.

### Key Rotation on Membership Change

When a member joins or leaves:

1. All remaining participants generate new media Sender Keys.
2. Distribute via 1:1 featherChat channels.
3. Relay is notified of membership change.
4. Old keys are zeroized.

This matches featherChat's existing group key rotation behavior for chat.

---

## 15. Offline / Warzone Scenarios

### Voice Messages as File Attachments

[CONFIRMED] featherChat supports file transfer up to 10 MB via
`WireMessage::FileHeader` + `WireMessage::FileChunk` (64 KB chunks).

Opus at 6 kbps: ~80 minutes per 10 MB. Codec2 at 1.2 kbps: ~400 minutes per 10 MB.

```
Record voice message:
  1. Capture mic via wzp-client AudioCapture (48 kHz mono)
  2. Encode with wzp-codec (Opus or Codec2)
  3. Package as .opus / .c2 file
  4. Send via featherChat: WireMessage::FileHeader + FileChunk
  5. Recipient decodes and plays via wzp-codec decoder

No WZP relay infrastructure needed. Pure featherChat + wzp-codec.
```

### Call Signaling via Mule Protocol

featherChat's mule protocol provides physical message relay for disconnected
networks. The mule can deliver:

- **Missed call notifications** (CallSignal that expired)
- **Voice messages** (encoded audio file attachments)
- **Call history** (who tried to call, when)

The mule **cannot** enable real-time calls -- this is acknowledged.

### LoRa: Text-Only, No Voice

LoRa (~250 byte payload) is incompatible with real-time voice. It can carry
compact missed call notifications:

```
[1]  version = 0x01
[1]  type = 0x04 (missed_call)
[8]  sender fingerprint (truncated)
[8]  recipient fingerprint (truncated)
[4]  timestamp (unix 32-bit)
[16] call_id
[1]  media_type (0x01=audio, 0x02=video)
---
39 bytes total
```

---

## 16. Implementation Roadmap

### Phase A: Identity Alignment (1-2 days)

**Goal:** Same seed produces same identity in both systems.

- [ ] Change WZP HKDF info strings in `wzp-crypto/src/handshake.rs:36,43`:
  - `"warzone-ed25519-identity"` -> `"warzone-ed25519"`
  - `"warzone-x25519-identity"` -> `"warzone-x25519"`
- [ ] Verify fingerprints match between featherChat and WZP for the same seed.
- [ ] Add cross-crate test: `warzone-protocol::Seed` + `wzp-crypto::WarzoneKeyExchange`
  produce identical Ed25519 public keys and fingerprints.

**Risk:** Low. Two-line change + test.

### Phase B: CallSignal WireMessage (1 week)

**Goal:** Call signaling flows through featherChat's encrypted channels.

- [ ] Add `CallSignal` variant to `WireMessage` in `warzone-protocol/src/message.rs`.
- [ ] Update `extract_message_id()` in `routes/ws.rs` and `routes/messages.rs`.
- [ ] Handle `CallSignal` in TUI poll loop (`tui/app.rs`).
- [ ] Handle in `decrypt_wire_message()` in `warzone-wasm/src/lib.rs`.
- [ ] WZP client sends/receives `CallSignal` via featherChat WebSocket.

**Dependencies:** Phase A complete.
**Risk:** Low. Follows existing WireMessage variant pattern (documented in
ARCHITECTURE.md: "Adding New WireMessage Variants").

### Phase C: Token Validation Endpoint (1-2 days)

**Goal:** WZP relays can verify featherChat bearer tokens.

- [ ] Add `POST /v1/auth/validate` to `routes/auth.rs`, reusing `validate_token()`.
- [ ] WZP relay calls this endpoint before accepting sessions.

**Dependencies:** None (independent of Phase A/B).
**Risk:** Low. Wraps existing function.

### Phase D: Integrated 1:1 Calls (2-4 weeks)

**Goal:** End-to-end voice call: featherChat signaling + WZP media.

- [ ] WZP client reads featherChat seed from `~/.warzone/identity.seed`.
- [ ] Call flow: featherChat WS for signaling -> QUIC for media.
- [ ] QUIC connection establishment via `wzp-transport::connect()` / `accept()`.
- [ ] Ephemeral X25519 handshake via `wzp-client::perform_handshake()`.
- [ ] Media pipeline: `AudioCapture` -> `CallEncoder` -> `QuinnTransport` ->
  `QuinnTransport` -> `CallDecoder` -> `AudioPlayback`.
- [ ] Adaptive quality control via `AdaptiveQualityController`.

**Dependencies:** Phases A, B, C.
**Risk:** Medium. Full pipeline integration, real audio I/O.

### Phase E: Relay Deployment (2 weeks)

**Goal:** WZP relay bridges peers behind NAT.

- [ ] Deploy `wzp-relay` daemon alongside featherChat server.
- [ ] ICE-like candidate exchange via featherChat `CallSignal::IceCandidate`.
- [ ] Fallback: peers connect through relay when direct QUIC fails.

**Dependencies:** Phase D complete.
**Risk:** Medium. NAT traversal is complex.

### Phase F: Group Calls (4-6 weeks)

**Goal:** featherChat groups map to WZP conference calls.

- [ ] Extend `wzp-relay` SessionManager for multi-party fan-out.
- [ ] Sender Key distribution for media encryption via featherChat.
- [ ] Participant management (join/leave/kick mapped from featherChat groups).
- [ ] Scalability target: 10-20 participants.

**Dependencies:** Phase E complete.
**Risk:** High. Multi-party media + Sender Keys is novel.

---

## 17. API Contracts

### featherChat: New WireMessage Variant

```rust
// warzone-protocol/src/message.rs
WireMessage::CallSignal {
    id: String,                  // UUID for dedup
    sender_fingerprint: String,  // caller's fingerprint
    signal: Vec<u8>,             // Serialized wzp_proto::SignalMessage (JSON)
}
```

### featherChat: Token Validation Endpoint

```
POST /v1/auth/validate
Content-Type: application/json

Request:  { "token": "hex..." }
Response: { "valid": true, "fingerprint": "a3f8c912...", "expires_at": 1711843600 }
     or:  { "valid": false, "error": "token expired" }
```

### WZP: SignalMessage (existing, via QUIC reliable stream)

```rust
// wzp-proto/src/packet.rs
SignalMessage::CallOffer { identity_pub, ephemeral_pub, signature, supported_profiles }
SignalMessage::CallAnswer { identity_pub, ephemeral_pub, signature, chosen_profile }
SignalMessage::IceCandidate { candidate }
SignalMessage::Rekey { new_ephemeral_pub, signature }
SignalMessage::QualityUpdate { report, recommended_profile }
SignalMessage::Ping { timestamp_ms }
SignalMessage::Pong { timestamp_ms }
SignalMessage::Hangup { reason }
```

### WZP: MediaPacket (existing, via QUIC DATAGRAM)

```
12-byte header:
  Byte 0:  [V:1][T:1][CodecID:4][Q:1][FecRatioHi:1]
  Byte 1:  [FecRatioLo:6][unused:2]
  Byte 2-3: Sequence number (BE u16)
  Byte 4-7: Timestamp ms (BE u32)
  Byte 8:   FEC block ID
  Byte 9:   FEC symbol index
  Byte 10:  Reserved
  Byte 11:  CSRC count

Payload: Encrypted audio frame (ChaCha20-Poly1305, 16-byte tag appended)

Optional 4-byte QualityReport trailer (when Q flag set):
  Byte 0: loss_pct (0-255)
  Byte 1: rtt_4ms (0-255 = 0-1020ms)
  Byte 2: jitter_ms
  Byte 3: bitrate_cap_kbps
```

### WZP: Client Pipeline APIs

```rust
// wzp-client: encode side
let mut encoder = CallEncoder::new(&CallConfig::default());
let packets: Vec<MediaPacket> = encoder.encode_frame(&pcm_960_samples)?;
// Each packet goes through: transport.send_media(&packet).await

// wzp-client: decode side
let mut decoder = CallDecoder::new(&CallConfig::default());
decoder.ingest(received_packet);
let samples: Option<usize> = decoder.decode_next(&mut pcm_buffer);

// wzp-client: audio I/O
let capture = AudioCapture::start()?;  // 48 kHz mono, 960 samples/frame
let playback = AudioPlayback::start()?;
let frame: Option<Vec<i16>> = capture.read_frame();  // blocking
playback.write_frame(&decoded_pcm);

// wzp-crypto: key exchange
let mut kx = WarzoneKeyExchange::from_identity_seed(&seed);
let eph_pub = kx.generate_ephemeral();
let session: Box<dyn CryptoSession> = kx.derive_session(&peer_eph_pub)?;

// wzp-crypto: encrypt/decrypt media
session.encrypt(header_aad, plaintext, &mut ciphertext)?;
session.decrypt(header_aad, ciphertext, &mut plaintext)?;

// wzp-transport: QUIC connection
let endpoint = create_endpoint(bind_addr, Some(server_config))?;
let conn = connect(&endpoint, peer_addr, "localhost", client_config).await?;
let transport = QuinnTransport::new(conn);
transport.send_media(&packet).await?;
transport.send_signal(&signal_msg).await?;
```

---

## 18. Security Analysis

### Combined Threat Model

| Threat | featherChat Mitigation | WZP Mitigation | Residual Risk |
|--------|----------------------|----------------|---------------|
| Server reads call signaling | Double Ratchet E2E encryption | N/A (tunneled through featherChat) | None -- server sees opaque blobs |
| Server performs MITM on call | Pre-key bundle signed by Ed25519 identity | CallOffer/Answer signed by Ed25519 identity | Fingerprint verification required (TOFU) |
| Relay reads audio | N/A | ChaCha20-Poly1305 per-call encryption | None -- relay sees encrypted datagrams |
| Replay of media packets | N/A | Anti-replay window (1024 packets) | Old packets beyond window are rejected |
| Long call key compromise | N/A | Rekey every 65536 packets (~22 min at 50 pps) | Window of 65536 packets between rekeys |
| Call metadata | Server sees WireMessage routing (sender/recipient fp) | Relay sees IP addresses and packet timing | Both see who is calling whom |
| Codec fingerprinting | N/A | CodecId is in the encrypted payload (after ChaCha20) but header codec field is authenticated-only | Header reveals codec in use (4-bit field) |
| Nonce reuse | N/A | Deterministic nonce: session_id + seq + direction; reset on rekey | Safe as long as seq counter doesn't wrap within a rekey epoch (2^16 limit enforced) |
| Token theft | 7-day TTL, local storage | Tokens not used for media auth (Ed25519 signatures instead) | Device compromise = token + seed compromise |
| Seed compromise | Both systems compromised | All derived keys compromised | Catastrophic -- protect seed above all else |

### Comparison with Signal Calling

| Aspect | featherChat + WZP (Confirmed) | Signal Calling |
|--------|-------------------------------|----------------|
| Signaling encryption | Double Ratchet (E2E) | Signal Protocol (E2E) |
| Media encryption | ChaCha20-Poly1305 (per-call ephemeral) | SRTP via DTLS-SRTP |
| Key exchange | Ephemeral X25519 DH | DTLS handshake |
| Nonce scheme | Deterministic (not transmitted) | SRTP packet index |
| Forward secrecy | Rekey every 2^16 packets | DTLS renegotiation |
| Anti-replay | 1024-packet bitmap window | SRTP replay window |
| FEC | RaptorQ fountain codes (adaptive) | Opus inband FEC only |
| Codec range | Opus 24k-6k + Codec2 3200-1200 | Opus only |
| Transport | QUIC (DATAGRAM + streams) | ICE/DTLS/SRTP over UDP |
| NAT traversal | QUIC relay (wzp-relay) | TURN relay |
| Group calls | Planned: Sender Keys + SFU relay | SFU + Sender Keys |
| Identity | Seed-based (BIP39 mnemonic) | Phone number |
| Obfuscation | Trait defined (Phase 2 planned) | None standard |

### Key Advantages of WZP Approach

1. **Unified crypto stack:** ChaCha20-Poly1305 + X25519 + Ed25519 everywhere
   (same as featherChat messaging). No DTLS/SRTP complexity.
2. **Extreme low-bitrate resilience:** Codec2 at 1.2 kbps with 100% FEC
   enables voice calls at ~2.4 kbps total bandwidth.
3. **RaptorQ FEC:** Fountain codes provide better loss recovery than Opus
   inband FEC, especially at high loss rates (>20%).
4. **QUIC transport:** Built-in congestion control, multiplexing, and NAT
   traversal. DATAGRAM frames provide unreliable delivery without head-of-line
   blocking.
5. **Obfuscation ready:** `ObfuscationLayer` trait (`wzp-proto/src/traits.rs:218-232`)
   defined for DPI evasion on client-relay links.

### Known Limitations

1. **No sealed sender** -- featherChat server sees sender/recipient fingerprints
   for CallSignal messages. Same limitation as chat.
2. **Header codec field is not encrypted** -- the MediaHeader is used as AAD
   (authenticated but cleartext). An observer can see which codec tier is active.
3. **Relay sees packet timing** -- traffic analysis reveals voice activity
   patterns. Mitigation: constant-bitrate encoding + DTX disabled.
4. **HKDF info string mismatch** (see Section 2) -- must be resolved before
   deployment.
5. **No post-quantum protection** -- all key exchanges use classical X25519.
   Hybrid X25519 + ML-KEM is feasible but not implemented.
6. **Self-signed QUIC certificates** -- current config uses
   `SkipServerVerification` (`wzp-transport/src/config.rs:88-134`). Production
   deployment needs proper certificate validation or identity-based verification.

---

## Appendix A: featherChat Code References

| Component | File | Key Types/Functions |
|-----------|------|---------------------|
| Seed & Identity | `warzone-protocol/src/identity.rs` | `Seed`, `IdentityKeyPair`, `PublicIdentity` |
| Wire Protocol | `warzone-protocol/src/message.rs` | `WireMessage` enum (7 variants) |
| Server Auth | `warzone-server/src/routes/auth.rs` | `create_challenge()`, `verify_challenge()`, `validate_token()` |
| WebSocket Relay | `warzone-server/src/routes/ws.rs` | `handle_socket()`, `extract_message_id()` |

## Appendix B: WZP Code References

| Component | File | Key Types/Functions |
|-----------|------|---------------------|
| Protocol types | `wzp-proto/src/packet.rs` | `MediaHeader`, `MediaPacket`, `QualityReport`, `SignalMessage`, `HangupReason` |
| Codec IDs | `wzp-proto/src/codec_id.rs` | `CodecId` (5 variants), `QualityProfile` (GOOD/DEGRADED/CATASTROPHIC) |
| Traits | `wzp-proto/src/traits.rs` | `AudioEncoder`, `AudioDecoder`, `FecEncoder`, `FecDecoder`, `CryptoSession`, `KeyExchange`, `MediaTransport`, `ObfuscationLayer`, `QualityController` |
| Session FSM | `wzp-proto/src/session.rs` | `Session`, `SessionState`, `SessionEvent` |
| Jitter buffer | `wzp-proto/src/jitter.rs` | `JitterBuffer`, `PlayoutResult` |
| Quality control | `wzp-proto/src/quality.rs` | `AdaptiveQualityController`, `Tier` |
| Adaptive codec | `wzp-codec/src/adaptive.rs` | `AdaptiveEncoder`, `AdaptiveDecoder` |
| Resampling | `wzp-codec/src/resample.rs` | `resample_48k_to_8k()`, `resample_8k_to_48k()` |
| Key exchange | `wzp-crypto/src/handshake.rs` | `WarzoneKeyExchange` |
| Crypto session | `wzp-crypto/src/session.rs` | `ChaChaSession` |
| Nonce | `wzp-crypto/src/nonce.rs` | `build_nonce()`, `Direction` |
| Rekeying | `wzp-crypto/src/rekey.rs` | `RekeyManager` (interval: 2^16 packets) |
| Anti-replay | `wzp-crypto/src/anti_replay.rs` | `AntiReplayWindow` (1024-packet bitmap) |
| FEC adaptive | `wzp-fec/src/adaptive.rs` | `AdaptiveFec` |
| QUIC config | `wzp-transport/src/config.rs` | `server_config()`, `client_config()`, ALPN `"wzp"` |
| QUIC transport | `wzp-transport/src/quic.rs` | `QuinnTransport` |
| Path monitor | `wzp-transport/src/path_monitor.rs` | `PathMonitor` (EWMA alpha=0.1) |
| Connection | `wzp-transport/src/connection.rs` | `create_endpoint()`, `connect()`, `accept()` |
| Relay pipeline | `wzp-relay/src/pipeline.rs` | `RelayPipeline`, `PipelineStats` |
| Relay sessions | `wzp-relay/src/session_mgr.rs` | `SessionManager`, `RelaySession` |
| Relay config | `wzp-relay/src/config.rs` | `RelayConfig` (listen :4433, max 100 sessions) |
| Relay handshake | `wzp-relay/src/handshake.rs` | `accept_handshake()` |
| Client pipeline | `wzp-client/src/call.rs` | `CallEncoder`, `CallDecoder`, `CallConfig` |
| Client handshake | `wzp-client/src/handshake.rs` | `perform_handshake()` |
| Audio I/O | `wzp-client/src/audio_io.rs` | `AudioCapture`, `AudioPlayback` (cpal, 48 kHz mono) |
| Benchmarks | `wzp-client/src/bench.rs` | `bench_codec_roundtrip()`, `bench_fec_recovery()`, `bench_encrypt_decrypt()`, `bench_full_pipeline()` |
