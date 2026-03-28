# WarzonePhone Extension Points & Future Features

## Trait-Based Architecture

The protocol is designed around trait interfaces defined in `crates/wzp-proto/src/traits.rs`. Any implementation that satisfies the trait contract can be plugged in without modifying other crates.

### Adding a New Audio Codec

Implement `AudioEncoder` and `AudioDecoder` from `wzp_proto::traits`:

```rust
pub trait AudioEncoder: Send + Sync {
    fn encode(&mut self, pcm: &[i16], out: &mut [u8]) -> Result<usize, CodecError>;
    fn codec_id(&self) -> CodecId;
    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError>;
    fn max_frame_bytes(&self) -> usize;
    fn set_inband_fec(&mut self, _enabled: bool) {}
    fn set_dtx(&mut self, _enabled: bool) {}
}

pub trait AudioDecoder: Send + Sync {
    fn decode(&mut self, encoded: &[u8], pcm: &mut [i16]) -> Result<usize, CodecError>;
    fn decode_lost(&mut self, pcm: &mut [i16]) -> Result<usize, CodecError>;
    fn codec_id(&self) -> CodecId;
    fn set_profile(&mut self, profile: QualityProfile) -> Result<(), CodecError>;
}
```

Steps:
1. Add a new variant to `CodecId` in `crates/wzp-proto/src/codec_id.rs` (uses 4-bit wire encoding, currently 5 of 16 values used)
2. Implement `AudioEncoder` and `AudioDecoder` for your codec
3. Register the codec in `AdaptiveEncoder`/`AdaptiveDecoder` in `crates/wzp-codec/src/adaptive.rs`
4. Add a `QualityProfile` constant for the new codec

### Adding a New FEC Scheme

Implement `FecEncoder` and `FecDecoder` from `wzp_proto::traits`:

```rust
pub trait FecEncoder: Send + Sync {
    fn add_source_symbol(&mut self, data: &[u8]) -> Result<(), FecError>;
    fn generate_repair(&mut self, ratio: f32) -> Result<Vec<(u8, Vec<u8>)>, FecError>;
    fn finalize_block(&mut self) -> Result<u8, FecError>;
    fn current_block_id(&self) -> u8;
    fn current_block_size(&self) -> usize;
}

pub trait FecDecoder: Send + Sync {
    fn add_symbol(&mut self, block_id: u8, symbol_index: u8, is_repair: bool, data: &[u8]) -> Result<(), FecError>;
    fn try_decode(&mut self, block_id: u8) -> Result<Option<Vec<Vec<u8>>>, FecError>;
    fn expire_before(&mut self, block_id: u8);
}
```

For example, a Reed-Solomon implementation would maintain the same block/symbol structure but use a different coding algorithm internally. The FEC block ID and symbol index fields in `MediaHeader` support any scheme that fits the block/symbol model.

### Adding a New Transport

Implement `MediaTransport` from `wzp_proto::traits`:

```rust
#[async_trait]
pub trait MediaTransport: Send + Sync {
    async fn send_media(&self, packet: &MediaPacket) -> Result<(), TransportError>;
    async fn recv_media(&self) -> Result<Option<MediaPacket>, TransportError>;
    async fn send_signal(&self, msg: &SignalMessage) -> Result<(), TransportError>;
    async fn recv_signal(&self) -> Result<Option<SignalMessage>, TransportError>;
    fn path_quality(&self) -> PathQuality;
    async fn close(&self) -> Result<(), TransportError>;
}
```

A raw UDP transport, a WebRTC data channel transport, or a TCP tunnel transport could all implement this trait.

## Obfuscation Layer (Phase 2)

The `ObfuscationLayer` trait is defined in `crates/wzp-proto/src/traits.rs` but not yet implemented:

```rust
pub trait ObfuscationLayer: Send + Sync {
    fn obfuscate(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), ObfuscationError>;
    fn deobfuscate(&mut self, data: &[u8], out: &mut Vec<u8>) -> Result<(), ObfuscationError>;
}
```

Planned implementations:
- **TLS-in-TLS**: Wrap QUIC traffic inside a TLS connection to port 443, making it look like ordinary HTTPS
- **HTTP/2 mimicry**: Frame QUIC packets as HTTP/2 data frames
- **Random padding**: Add random-length padding to defeat traffic analysis
- **Domain fronting**: Use CDN infrastructure to hide the true destination

The obfuscation layer sits between the crypto layer and the transport layer in the protocol stack, wrapping encrypted packets before transmission.

## FeatherChat / Warzone Messenger Integration

As described in `docs/featherchat.md`, WarzonePhone is designed to integrate with the existing Warzone messenger.

### Shared Identity Model

Both WarzonePhone and Warzone use the same identity derivation:
- 32-byte seed (BIP39 mnemonic backup)
- HKDF with context strings: `"warzone-ed25519-identity"` and `"warzone-x25519-identity"`
- Ed25519 for signing, X25519 for encryption
- Fingerprint: `SHA-256(Ed25519_pub)[:16]`

This is implemented in `crates/wzp-crypto/src/handshake.rs` as `WarzoneKeyExchange::from_identity_seed()`.

### Signaling via Existing WebSocket

Call initiation flows through the Warzone messenger's existing WebSocket connection:
1. Caller looks up callee via `@alias`, federated address, or raw fingerprint
2. Caller sends `WireMessage::CallOffer` through the existing message channel
3. Callee receives the offer and responds with `WireMessage::CallAnswer`
4. Both sides establish a direct QUIC connection to the relay using ephemeral keys from the signaling exchange

The `SignalMessage::CallOffer` and `SignalMessage::CallAnswer` variants in `crates/wzp-proto/src/packet.rs` carry the same fields needed for this flow.

### Key Derivation from Existing Shared Secret

When two Warzone users already have an X3DH shared secret from their messaging session, call keys can be derived from it:
- `HKDF(x3dh_shared_secret, "warzone-call-session")` -> 32-byte session key
- Or: fresh ephemeral exchange per call (current implementation) for independent forward secrecy

### Unified Addressing

The Warzone addressing system resolves user identities across multiple namespaces:

| Method | Format | Resolution |
|--------|--------|------------|
| Local alias | `@manwe` | Server resolves to fingerprint |
| Federated | `@manwe.b1.example.com` | DNS TXT record -> fingerprint + endpoint |
| ENS | `@manwe.eth` | Ethereum address -> fingerprint (planned) |
| Raw fingerprint | `xxxx:xxxx:...` | Direct lookup |

A user calls `@manwe` the same way they message `@manwe`.

## Authentication: Caller Verification Before Bridging

Currently, relays forward packets without verifying caller identity. To add authentication:

1. **Relay-side handshake**: The relay receives the `CallOffer`, verifies the Ed25519 signature, and checks the caller's identity against an allowlist before accepting the connection.

2. **Implementation point**: `crates/wzp-relay/src/handshake.rs` already implements `accept_handshake()` which performs signature verification. To gate admission, add an authorization check after signature verification.

3. **Token-based auth**: Add a `token: Vec<u8>` field to `CallOffer` containing a relay-issued authentication token (e.g., signed by the relay operator's key).

## Multi-Relay Mesh

The current two-relay chain (`--remote` flag) can be extended to a multi-hop mesh:

```
Client -> Relay A -> Relay B -> Relay C -> Destination
```

Each hop uses the relay pipeline (FEC decode -> jitter buffer -> FEC re-encode) to absorb loss on each link independently. This requires:

1. Relay discovery and route selection (not yet implemented)
2. Per-hop FEC parameters (each link may have different loss characteristics)
3. Cumulative latency management (each hop adds jitter buffer delay)

## Video Support

The trait architecture supports video by adding:

1. **Video codec trait**: Similar to `AudioEncoder`/`AudioDecoder` but for video frames
2. **Codec choices**: AV1 (best compression, higher CPU), VP9 SVC (scalable, moderate CPU)
3. **Separate FEC strategy**: Video frames are larger and more critical (I-frames vs P-frames need different protection levels)
4. **SVC (Scalable Video Coding)**: With VP9 SVC, the relay can drop enhancement layers without transcoding, adapting video quality to each receiver's bandwidth

Video would add new `CodecId` variants and a separate `QualityProfile` for video parameters.

## Android Native Client

The workspace is designed with Android in mind (`wzp-client` description mentions "for Android (JNI) and Windows desktop"):

1. **JNI bindings**: Use `jni` crate or `uniffi` to expose `CallEncoder`, `CallDecoder`, and `MediaTransport` to Kotlin/Java
2. **Audio I/O**: Android uses AAudio or OpenSL ES instead of cpal
3. **Build**: Cross-compile with `cargo ndk` targeting `aarch64-linux-android` and `armv7-linux-androideabi`
4. **Permissions**: `RECORD_AUDIO`, `INTERNET`, `WAKE_LOCK`

## STUN/TURN NAT Traversal Integration

The `SignalMessage::IceCandidate` variant is already defined for NAT traversal:

```rust
IceCandidate { candidate: String }
```

Integration would involve:
1. STUN server queries to discover the client's public IP/port
2. ICE candidate exchange via the signaling channel
3. TURN relay fallback when direct UDP is blocked
4. Integration with the existing QUIC transport (QUIC can traverse NATs via its connection migration)

## Bandwidth Estimation and Adaptive Bitrate

The `PathMonitor` in `crates/wzp-transport/src/path_monitor.rs` already estimates bandwidth from observed packet rates. To close the loop:

1. Feed `PathMonitor::quality()` into `AdaptiveQualityController::observe()` as `QualityReport` values
2. The controller will trigger tier transitions when conditions change
3. Propagate the new `QualityProfile` to both encoder (codec switch) and FEC (ratio change)
4. Signal the peer via `SignalMessage::QualityUpdate` so both sides switch simultaneously

The framework is in place; the missing piece is the integration wiring in the client's main loop to periodically generate quality reports from path metrics.
