# WarzonePhone Detailed Design Decisions

## Why Opus + Codec2 (Not Just One)

The dual-codec architecture is driven by the extreme range of network conditions WarzonePhone targets:

**Opus** (24/16/6 kbps) is the clear choice for normal to degraded conditions. It offers excellent quality at moderate bitrates, has built-in inband FEC and DTX (discontinuous transmission), and the `audiopus` crate provides mature Rust bindings to libopus. Opus operates at 48 kHz natively.

**Codec2** (3200/1200 bps) is a narrowband vocoder designed specifically for HF radio links with extreme bandwidth constraints. At 1200 bps (1.2 kbps), it produces intelligible speech in only 6 bytes per 40ms frame -- roughly 20x lower bitrate than Opus at its minimum. The pure-Rust `codec2` crate means no C dependencies for this codec. Codec2 operates at 8 kHz, so the adaptive layer handles 48 kHz <-> 8 kHz resampling transparently.

The `AdaptiveEncoder`/`AdaptiveDecoder` in `crates/wzp-codec/src/adaptive.rs` hold both codec instances and switch between them based on the active `QualityProfile`. This avoids codec re-initialization latency during tier transitions.

**Bandwidth comparison with FEC overhead:**

| Tier | Codec Bitrate | FEC Ratio | Total Bandwidth |
|------|--------------|-----------|----------------|
| GOOD | 24 kbps | 20% | ~28.8 kbps |
| DEGRADED | 6 kbps | 50% | ~9.0 kbps |
| CATASTROPHIC | 1.2 kbps | 100% | ~2.4 kbps |

At the catastrophic tier, the entire call (audio + FEC + headers) fits within approximately 3 kbps, which is viable even over severely degraded links.

## Why RaptorQ Over Reed-Solomon

**Reed-Solomon** is a classical block erasure code. It works well but has fixed-rate overhead: you must decide in advance how many repair symbols to generate, and decoding requires receiving exactly K of any K+R symbols.

**RaptorQ** (RFC 6330) is a fountain code with several advantages for VoIP:

1. **Rateless**: You can generate an arbitrary number of repair symbols on the fly. If conditions worsen mid-block, you can generate additional repair without re-encoding.

2. **Efficient decoding**: RaptorQ can decode from any K symbols with high probability (typically K + 1 or K + 2 suffice), compared to Reed-Solomon which requires exactly K.

3. **Lower computational complexity**: O(K) encoding and decoding time, compared to O(K^2) for Reed-Solomon. This matters for real-time audio at 50 frames/second.

4. **Variable block sizes**: The encoder handles 1-56403 source symbols per block (the WZP implementation uses 5-10, but the flexibility is there).

The `raptorq` crate (v2) provides a well-tested pure-Rust implementation. The WZP FEC layer adds length-prefixed padding (2-byte LE prefix + zero-pad to 256 bytes) so that variable-length audio frames can be recovered exactly.

**FEC bandwidth math at different loss rates:**

With 5 source frames per block:
- 20% repair (GOOD): 1 repair symbol. Survives loss of 1 out of 6 packets (16.7% loss).
- 50% repair (DEGRADED): 3 repair symbols. Survives loss of 3 out of 8 packets (37.5% loss).
- 100% repair (CATASTROPHIC): 5 repair symbols. Survives loss of 5 out of 10 packets (50% loss).

The benchmark (`wzp-bench --fec --loss 30`) dynamically scales the FEC ratio to survive the requested loss percentage.

## Why QUIC Over Raw UDP

Raw UDP would be simpler and lower-latency, but QUIC (via the `quinn` crate) provides:

1. **DATAGRAM frames**: Unreliable delivery without head-of-line blocking (RFC 9221). Media packets use this path, so they behave like UDP datagrams but benefit from QUIC's connection management.

2. **Reliable streams**: Signaling messages (CallOffer, CallAnswer, Rekey, Hangup) require reliable delivery. QUIC provides multiplexed streams without needing a separate TCP connection.

3. **Built-in congestion control**: QUIC's congestion control prevents overwhelming degraded links, which is important when chaining relays.

4. **Connection migration**: QUIC connections survive IP address changes (e.g., WiFi to cellular handoff), which is valuable for mobile clients.

5. **TLS 1.3 built-in**: The QUIC handshake provides encryption at the transport level. While WZP has its own end-to-end ChaCha20 layer, the QUIC TLS protects the header and signaling from eavesdroppers.

6. **NAT keepalive**: QUIC's built-in keep-alive (configured at 5-second intervals) maintains NAT bindings without application-level pings.

7. **Firewall traversal**: QUIC runs on UDP port 443 by default, which is commonly allowed through firewalls. The `wzp` ALPN protocol identifier distinguishes WZP traffic.

The tradeoff is approximately 20-40 bytes of additional per-packet overhead compared to raw UDP (QUIC short header + DATAGRAM frame overhead).

## Why ChaCha20-Poly1305 Over AES-GCM

1. **Software performance**: ChaCha20-Poly1305 is faster than AES-GCM on hardware without AES-NI instructions. This matters for ARM devices (Android phones, Raspberry Pi relays, embedded systems) where AES hardware acceleration may be absent.

2. **Constant-time by design**: ChaCha20 uses only add-rotate-XOR operations, making it inherently resistant to timing side-channel attacks. AES-GCM implementations without hardware support often require careful constant-time implementation.

3. **Warzone messenger compatibility**: The existing Warzone messenger uses ChaCha20-Poly1305 for message encryption. Reusing the same primitive simplifies the security audit and allows key material to be shared across messaging and calling.

4. **16-byte overhead**: Both ChaCha20-Poly1305 and AES-128-GCM produce a 16-byte authentication tag. There is no size advantage to AES-GCM.

5. **AEAD with AAD**: The MediaHeader is used as Associated Authenticated Data (AAD), ensuring the header is authenticated but not encrypted. This allows relays to read routing information (block ID, sequence number) without decrypting the payload.

## Why Star Dependency Graph (Parallel Development)

The workspace follows a strict star dependency pattern:

```
         wzp-proto (hub)
        /    |    \    \
  wzp-codec  wzp-fec  wzp-crypto  wzp-transport
        \    |    /    /
         wzp-relay
         wzp-client
         wzp-web
```

- `wzp-proto` defines all trait interfaces and wire format types
- Each "leaf" crate (codec, fec, crypto, transport) depends only on `wzp-proto`
- No leaf crate depends on another leaf crate
- Integration crates (relay, client, web) depend on all leaves

This enables:
1. **Parallel development**: 5 agents/developers can work on 5 crates simultaneously with zero merge conflicts
2. **Independent testing**: Each crate has comprehensive tests that run without requiring other implementations
3. **Pluggability**: Any implementation can be swapped (e.g., replace RaptorQ with Reed-Solomon) by implementing the same trait
4. **Fast compilation**: Changes to one leaf only recompile that leaf and the integration crates, not other leaves

## Jitter Buffer Trade-offs

The jitter buffer must balance two competing goals:

**Lower latency** (smaller buffer):
- Better conversational interactivity
- Less memory usage
- But more vulnerable to jitter and reordering

**Higher quality** (larger buffer):
- More time to receive out-of-order packets
- More time for FEC recovery (repair packets may arrive after source packets)
- But adds perceptible delay to the conversation

The default configuration:
- Target: 10 packets (200ms) for the client, 50 packets (1s) for the relay
- Minimum: 3 packets (60ms) before playout begins (client), 25 packets (500ms) for relay
- Maximum: 250 packets (5s) absolute cap

The relay uses a deeper buffer because it needs to absorb jitter from the lossy inter-relay link. The client uses a shallower buffer for lower latency since it is on the last hop.

**Known issue**: The current jitter buffer does not adapt its depth based on observed jitter. It uses sequence-number ordering only, without timestamp-based playout scheduling. This can lead to drift during long calls, as observed in echo tests.

## Browser Audio: AudioWorklet vs ScriptProcessorNode

The web bridge (`crates/wzp-web/static/`) uses AudioWorklet as the primary audio I/O mechanism, with ScriptProcessorNode as a fallback.

**AudioWorklet** (preferred):
- Runs on a dedicated audio rendering thread
- Lower latency (no main-thread round-trip)
- Consistent 128-sample callback timing
- Supported in Chrome 66+, Firefox 76+, Safari 14.1+

**ScriptProcessorNode** (fallback):
- Runs on the main thread via `onaudioprocess` callback
- Higher latency, potential glitches from main-thread GC pauses
- Deprecated by the Web Audio specification
- Used when AudioWorklet is not available

Both paths accumulate Float32 samples into 960-sample (20ms) Int16 frames before sending via WebSocket, matching the WZP codec frame size.

**Playback** uses an AudioWorklet with a ring buffer capped at 200ms (9600 samples at 48 kHz). When the buffer exceeds this limit, old samples are dropped to prevent unbounded drift. The fallback path uses scheduled `AudioBufferSourceNode` instances.

## Room Mode: SFU vs MCU Trade-offs

WarzonePhone implements an **SFU** (Selective Forwarding Unit) architecture:

**SFU** (implemented):
- Relay forwards each participant's packets to all other participants unchanged
- No transcoding -- the relay never decodes or re-encodes audio
- O(N) bandwidth at the relay for N participants (each packet is sent N-1 times)
- Each client receives separate streams from each other participant
- Client must mix/decode multiple streams locally
- Lower relay CPU usage (no transcoding)
- End-to-end encryption is preserved (relay never sees plaintext)

**MCU** (not implemented, for comparison):
- Relay would decode all streams, mix them, and re-encode a single combined stream
- O(1) bandwidth to each client (receives one mixed stream)
- Requires the relay to have codec keys (breaks E2E encryption)
- Higher relay CPU (decoding N streams + mixing + re-encoding)
- Audio quality loss from re-encoding

The SFU choice is driven by the E2E encryption requirement: since relays never have access to the audio codec keys, they cannot decode, mix, or re-encode. The current room implementation in `crates/wzp-relay/src/room.rs` forwards received datagrams to all other participants in the room with best-effort delivery -- if one send fails, the relay continues to the next participant.
