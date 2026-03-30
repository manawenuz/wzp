# WZP Web Client Variants

Three browser-based client implementations with different trade-offs between simplicity, features, and performance.

## Variant Comparison

```mermaid
graph LR
    subgraph "Variant 1: Pure JS"
        P_MIC[Mic] --> P_WRK[AudioWorklet<br/>48kHz PCM]
        P_WRK --> P_WS[WebSocket<br/>TCP]
        P_WS --> P_BRIDGE[wzp-web Bridge<br/>Opus + FEC + Crypto]
        P_BRIDGE --> P_QUIC[QUIC Datagram<br/>to Relay]
    end

    style P_BRIDGE fill:#ff9f43
    style P_WS fill:#74b9ff
```

```mermaid
graph LR
    subgraph "Variant 2: Hybrid"
        H_MIC[Mic] --> H_WRK[AudioWorklet<br/>48kHz PCM]
        H_WRK --> H_FEC[WASM RaptorQ<br/>FEC Encode]
        H_FEC --> H_WS[WebSocket<br/>TCP]
        H_WS --> H_BRIDGE[wzp-web Bridge<br/>Opus + Crypto]
        H_BRIDGE --> H_QUIC[QUIC Datagram<br/>to Relay]
    end

    style H_FEC fill:#a29bfe
    style H_BRIDGE fill:#ff9f43
    style H_WS fill:#74b9ff
```

```mermaid
graph LR
    subgraph "Variant 3: Full WASM"
        F_MIC[Mic] --> F_WRK[AudioWorklet<br/>48kHz PCM]
        F_WRK --> F_FEC[WASM RaptorQ<br/>FEC Encode]
        F_FEC --> F_ENC[WASM ChaCha20<br/>Encrypt]
        F_ENC --> F_WT[WebTransport<br/>UDP Datagrams]
        F_WT --> F_RELAY[Direct to Relay<br/>No Bridge]
    end

    style F_FEC fill:#a29bfe
    style F_ENC fill:#ee5a24
    style F_WT fill:#00b894
```

## Summary Table

| | Pure JS | Hybrid | Full WASM |
|--|---------|--------|-----------|
| **Bundle** | ~20KB JS | ~120KB (JS + 337KB WASM) | ~20KB JS + 337KB WASM |
| **Transport** | WebSocket (TCP) | WebSocket (TCP) | WebTransport (UDP) |
| **Encryption** | Bridge-side (ChaCha20 on QUIC) | Bridge-side | Browser-side ChaCha20-Poly1305 WASM |
| **FEC** | None | RaptorQ WASM (ready, not active over TCP) | RaptorQ WASM (active over UDP) |
| **Codec** | Bridge Opus (server-side) | Bridge Opus | Browser Opus (future) / Bridge Opus |
| **E2E Encrypted** | No (bridge sees plaintext PCM) | No (bridge sees plaintext PCM) | Yes (bridge eliminated) |
| **Latency** | ~50-80ms (TCP overhead) | ~50-80ms (TCP) | ~20-40ms (UDP datagrams) |
| **Loss Recovery** | TCP retransmit (adds latency) | TCP retransmit | RaptorQ FEC (no retransmit) |
| **Browser Support** | All browsers | All browsers | Chrome 97+, Edge 97+, Firefox 114+, Safari 17.4+ |
| **Relay Changes** | None | None | Needs HTTP/3 (h3-quinn) |
| **Status** | Ready | Ready (FEC testable in console) | Architecture complete, needs relay HTTP/3 |

## Variant 1: Pure JS

The lightest implementation. No WASM, no FEC, no browser-side encryption. The `wzp-web` Rust bridge handles everything on the server side.

### Architecture

```mermaid
sequenceDiagram
    participant B as Browser
    participant W as wzp-web Bridge
    participant R as wzp-relay

    B->>B: getUserMedia() mic access
    B->>B: AudioWorklet captures 960 samples / 20ms

    B->>W: WebSocket connect /ws/room-name
    W->>R: QUIC connect (SNI = hashed room)
    W->>R: Crypto handshake (X25519 + ChaCha20)

    loop Every 20ms
        B->>W: WS Binary: Int16[960] raw PCM
        W->>W: Opus encode + FEC + Encrypt
        W->>R: QUIC Datagram
    end

    loop Incoming
        R->>W: QUIC Datagram
        W->>W: Decrypt + FEC decode + Opus decode
        W->>B: WS Binary: Int16[960] raw PCM
    end

    B->>B: AudioWorklet plays received PCM
```

### Data Flow

```
Browser (Pure JS)
├── Capture: getUserMedia → AudioWorklet (WZPCaptureProcessor)
│   └── 128-sample blocks accumulated → 960-sample frame
│       └── Float32 → Int16 conversion
│           └── postMessage(ArrayBuffer) to main thread
├── Send: onmessage → ws.send(pcmBuffer)
│   └── Binary WebSocket frame (1920 bytes = 960 × 2)
├── Receive: ws.onmessage → ArrayBuffer
│   └── Int16Array(960) → playback port
└── Playback: AudioWorklet (WZPPlaybackProcessor)
    └── Ring buffer (max 120ms)
        └── Int16 → Float32 → output blocks
```

### Files
- `js/wzp-pure.js` — `WZPPureClient` class (~100 lines)
- `js/wzp-core.js` — Shared UI + audio (used by all variants)
- `audio-processor.js` — AudioWorklet (unchanged)

### Limitations
- No packet loss recovery (TCP retransmit adds latency spikes)
- Bridge sees plaintext audio (not E2E encrypted)
- Full audio processing pipeline runs on server (Opus, FEC, crypto)
- Each browser connection = one QUIC session on the bridge

---

## Variant 2: Hybrid (JS + WASM FEC)

Adds RaptorQ forward error correction via a small WASM module. Same WebSocket transport as Pure — the FEC module is loaded and functional but doesn't add value over TCP (no packet loss). It's ready to activate when WebTransport replaces WebSocket.

### Architecture

```mermaid
sequenceDiagram
    participant B as Browser
    participant WASM as WASM Module
    participant W as wzp-web Bridge
    participant R as wzp-relay

    B->>WASM: Load wzp_wasm.js (337KB)
    WASM-->>B: WzpFecEncoder + WzpFecDecoder ready

    B->>W: WebSocket connect /ws/room-name
    W->>R: QUIC connect + handshake

    loop Every 20ms
        B->>B: AudioWorklet captures PCM
        B->>WASM: fecEncoder.add_symbol(pcm_bytes)
        WASM-->>B: FEC packets (source + repair) when block complete
        B->>W: WS Binary: raw PCM (FEC not on wire over TCP)
    end

    Note over B,WASM: FEC encode/decode proven via testFec()
```

### WASM Module (wzp-wasm)

```mermaid
graph TD
    subgraph "wzp-wasm (337KB)"
        FE[WzpFecEncoder<br/>RaptorQ source block accumulator]
        FD[WzpFecDecoder<br/>RaptorQ reconstruction]
        KX[WzpKeyExchange<br/>X25519 ephemeral DH]
        CS[WzpCryptoSession<br/>ChaCha20-Poly1305]
    end

    subgraph "Hybrid uses"
        FE
        FD
    end

    subgraph "Full uses"
        FE
        FD
        KX
        CS
    end

    style FE fill:#a29bfe
    style FD fill:#a29bfe
    style KX fill:#ee5a24
    style CS fill:#ee5a24
```

### FEC Wire Format

```
Per symbol (encoded by WASM, 259 bytes):
┌──────────┬───────────┬──────────┬──────────────────┐
│ block_id │ symbol_idx│ is_repair│ symbol_data      │
│ (1 byte) │ (1 byte)  │ (1 byte) │ (256 bytes)      │
└──────────┴───────────┴──────────┴──────────────────┘

Symbol data internals (256 bytes):
┌────────────┬──────────────────┬─────────┐
│ length     │ audio frame data │ padding │
│ (2B LE)    │ (variable)       │ (zeros) │
└────────────┴──────────────────┴─────────┘

Block = 5 source symbols + ceil(5 × 0.5) = 3 repair symbols = 8 total
Any 5 of 8 received → full block recoverable (RaptorQ fountain code)
```

### Testing FEC in Browser Console

```javascript
// On any hybrid variant page, open console:
client.testFec({ lossRate: 0.3, blockSize: 5, symbolSize: 256 })
// Output: "FEC test passed — recovered from 30% loss"

client.testFec({ lossRate: 0.5 })
// Output: "FEC test passed — recovered from 50% loss"
```

### Files
- `js/wzp-hybrid.js` — `WZPHybridClient` class (~150 lines)
- `js/wzp-core.js` — Shared UI + audio
- `wasm/wzp_wasm.js` + `wasm/wzp_wasm_bg.wasm` — WASM module (337KB)

### Limitations
- FEC doesn't help over TCP WebSocket (no packet loss to recover from)
- Bridge still sees plaintext audio
- WebTransport activation is the unlock for FEC value

---

## Variant 3: Full WASM + WebTransport

The complete WZP client in the browser. No bridge server needed — the browser connects directly to the relay via WebTransport unreliable datagrams. All encryption and FEC happens in WASM.

### Architecture

```mermaid
sequenceDiagram
    participant B as Browser
    participant WASM as WASM Module
    participant R as wzp-relay

    B->>WASM: Load wzp_wasm.js
    WASM-->>B: FEC + Crypto + KeyExchange ready

    B->>R: WebTransport connect (HTTPS/HTTP3)
    B->>R: Bidirectional stream open

    Note over B,R: Key Exchange
    B->>WASM: kx = new WzpKeyExchange()
    B->>R: Stream: our X25519 public key (32 bytes)
    R->>B: Stream: relay X25519 public key (32 bytes)
    B->>WASM: secret = kx.derive_shared_secret(peer_pub)
    B->>WASM: session = new WzpCryptoSession(secret)

    Note over B,R: Media Flow (Unreliable Datagrams)
    loop Every 20ms
        B->>B: AudioWorklet captures PCM
        B->>WASM: fecEncoder.add_symbol(pcm_bytes)
        WASM-->>B: FEC symbols when block complete
        B->>WASM: encrypted = session.encrypt(header, symbol)
        B->>R: WebTransport datagram (encrypted)
    end

    loop Incoming
        R->>B: WebTransport datagram (encrypted)
        B->>WASM: plaintext = session.decrypt(header, ciphertext)
        B->>WASM: frames = fecDecoder.add_symbol(...)
        WASM-->>B: Decoded audio frames
        B->>B: AudioWorklet plays PCM
    end
```

### Encryption Flow

```mermaid
graph TD
    subgraph "Key Exchange (once per session)"
        KX_A[Browser: WzpKeyExchange.new<br/>Generate X25519 keypair] --> PUB_A[Send public key<br/>32 bytes over stream]
        PUB_B[Receive relay public key<br/>32 bytes] --> DH[derive_shared_secret<br/>X25519 DH + HKDF-SHA256]
        DH --> SESSION[WzpCryptoSession<br/>ChaCha20-Poly1305 256-bit key]
    end

    subgraph "Per-Packet Encryption"
        HDR[Build MediaHeader<br/>12 bytes AAD] --> ENC[session.encrypt<br/>header=AAD plaintext=audio]
        ENC --> NONCE[Nonce 12 bytes<br/>session_id 4 + seq 4 + dir 1 + pad 3]
        ENC --> CT[Ciphertext + 16-byte Poly1305 tag]
        CT --> DG[WebTransport datagram send]
    end

    style SESSION fill:#ee5a24
    style NONCE fill:#fdcb6e
```

### Nonce Construction (matches native wzp-crypto)

```
Bytes 0-3:   session_id (SHA-256(session_key)[:4])
Bytes 4-7:   sequence_number (u32 BE, incrementing)
Byte 8:      direction (0x00 = send, 0x01 = recv)
Bytes 9-11:  0x000000 (padding)

Total: 12 bytes — deterministic, never reused (seq increments)
```

### Send Pipeline Detail

```mermaid
graph TD
    MIC[Mic PCM Int16 x 960] --> PAD[Pad to 256 bytes<br/>2-byte LE length + data + zeros]
    PAD --> FEC[WzpFecEncoder.add_symbol<br/>Accumulate 5 frames per block]
    FEC -->|Block complete| SYMBOLS[5 source + 3 repair symbols]
    SYMBOLS --> HDR[Build 12-byte MediaHeader<br/>seq, timestamp, codec, fec_block, symbol_idx]
    HDR --> ENCRYPT[WzpCryptoSession.encrypt<br/>AAD=header, payload=symbol]
    ENCRYPT --> DG[WebTransport datagram<br/>header 12B + ciphertext + tag 16B]

    style FEC fill:#a29bfe
    style ENCRYPT fill:#ee5a24
    style DG fill:#00b894
```

### Receive Pipeline Detail

```mermaid
graph TD
    DG[WebTransport datagram] --> PARSE[Parse 12-byte MediaHeader]
    PARSE --> DECRYPT[WzpCryptoSession.decrypt<br/>AAD=header, ciphertext=rest]
    DECRYPT --> FEC_HDR[Parse 3-byte FEC header<br/>block_id + symbol_idx + is_repair]
    FEC_HDR --> FEC_D[WzpFecDecoder.add_symbol]
    FEC_D -->|Block decoded| FRAMES[Original audio frames]
    FRAMES --> UNPAD[Strip 2-byte length prefix + padding]
    UNPAD --> PLAY[AudioWorklet playback<br/>Int16 PCM x 960]

    style DECRYPT fill:#ee5a24
    style FEC_D fill:#a29bfe
    style PLAY fill:#4a9eff
```

### Testing Crypto + FEC in Browser Console

```javascript
// On any full variant page, open console:
client.testCryptoFec()
// Tests: key exchange → encrypt → FEC encode → simulate 30% loss → FEC decode → decrypt
// Output: "Crypto+FEC test passed — key exchange, encrypt, FEC(30% loss), decrypt all OK"
```

### Files
- `js/wzp-full.js` — `WZPFullClient` class (~250 lines)
- `js/wzp-core.js` — Shared UI + audio
- `wasm/wzp_wasm.js` + `wasm/wzp_wasm_bg.wasm` — WASM module (337KB, shared with hybrid)

### Requirements (not yet met)
- Relay must support HTTP/3 WebTransport (h3-quinn integration)
- Real TLS certificate (WebTransport requires valid HTTPS)
- Browser with WebTransport support (Chrome 97+, Edge 97+, Firefox 114+, Safari 17.4+)

### Limitations
- No Opus encoding in browser yet (sends raw PCM, relay/peer decodes)
- Key exchange is simplified (no Ed25519 signature verification in WASM yet)
- No adaptive quality switching in browser (server-side only)

---

## Shared Infrastructure

### wzp-core.js

Common code used by all three variants:

```mermaid
graph TD
    CORE[wzp-core.js] --> DETECT[detectVariant<br/>URL ?variant= param]
    CORE --> ROOM[getRoom<br/>URL path / input field]
    CORE --> AUDIO[startAudioContext<br/>48kHz AudioContext]
    CORE --> CAP[connectCapture<br/>Mic to AudioWorklet]
    CORE --> PLAY[connectPlayback<br/>AudioWorklet to speaker]
    CORE --> UI[initUI<br/>Buttons, PTT, level meter]
    CORE --> STATUS[updateStatus / updateStats<br/>DOM updates]

    CAP --> WORKLET[AudioWorklet<br/>or ScriptProcessor fallback]
    PLAY --> WORKLET

    style CORE fill:#6c5ce7
    style WORKLET fill:#00b894
```

### AudioWorklet Processors (audio-processor.js)

```
WZPCaptureProcessor:
  AudioWorklet process() → 128 samples per call
  Buffer internally until 960 samples (20ms frame)
  Convert Float32 → Int16
  postMessage(ArrayBuffer) to main thread

WZPPlaybackProcessor:
  Receive Int16 PCM via port.onmessage
  Convert Int16 → Float32
  Write to ring buffer (max ~120ms / 6 frames)
  process() reads from ring buffer → output
```

### index.html Boot Sequence

```mermaid
sequenceDiagram
    participant PAGE as index.html
    participant CORE as wzp-core.js
    participant VAR as Variant JS

    PAGE->>CORE: Load (static script tag)
    CORE->>CORE: detectVariant() from URL
    PAGE->>VAR: Dynamic script load (wzp-pure/hybrid/full.js)
    VAR-->>PAGE: wzpBoot() called on load

    PAGE->>CORE: initUI(callbacks)
    Note over PAGE: User clicks Connect

    PAGE->>CORE: startAudioContext()
    PAGE->>VAR: new WZP*Client(options)
    PAGE->>VAR: client.connect()
    PAGE->>CORE: connectCapture(audioCtx, onFrame)
    PAGE->>CORE: connectPlayback(audioCtx)

    loop Audio flowing
        CORE->>VAR: client.sendAudio(pcmBuffer)
        VAR->>CORE: onAudio(Int16Array) callback
    end
```

## Deployment

### Behind Caddy (recommended)

```
# Caddyfile
wzp.example.com {
    reverse_proxy 127.0.0.1:8080
}
```

```bash
# Relay
./wzp-relay --listen 0.0.0.0:4433

# Web bridge (no --tls, Caddy handles SSL)
./wzp-web --port 8080 --relay 127.0.0.1:4433
```

### Direct TLS

```bash
./wzp-web --port 443 --relay 127.0.0.1:4433 --tls \
  --cert /etc/letsencrypt/live/domain/fullchain.pem \
  --key /etc/letsencrypt/live/domain/privkey.pem
```

### URL Patterns

```
https://domain/room-name                    → Pure (default)
https://domain/room-name?variant=pure       → Pure JS
https://domain/room-name?variant=hybrid     → Hybrid (JS + WASM FEC)
https://domain/room-name?variant=full       → Full WASM (needs HTTP/3 relay)
```

## Future Work

1. **Relay HTTP/3 support** (h3-quinn) — unlocks Full variant for production
2. **Browser Opus encoding** — AudioEncoder API or Opus WASM, removes bridge dependency for Hybrid
3. **Ed25519 signatures in WASM** — full identity verification in Full variant
4. **Adaptive quality in browser** — monitor RTT/loss, switch profiles
5. **WebTransport fallback to WebSocket** — Full variant auto-degrades if WebTransport unavailable
