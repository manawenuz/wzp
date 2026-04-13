# PRD: Protocol Analyzer & Debug Tap

## 1. Relay-Side Metadata Tap (`--debug-tap`)

### Problem

When debugging federation, codec issues, or packet flow problems, there's no visibility into what's actually flowing through the relay. You have to guess from client-side logs.

### Solution

A `--debug-tap <room>` flag on the relay that logs every packet's **header metadata** for a specific room (or all rooms with `--debug-tap *`). No decryption needed — the MediaHeader is not encrypted, only the audio payload is.

### Output Format

```
[12:00:00.123] TAP room=test dir=in  src=192.168.1.5:54321 seq=1234 codec=Opus24k ts=24000 fec_block=5 fec_sym=2 repair=false len=87
[12:00:00.123] TAP room=test dir=out dst=192.168.1.6:54322 seq=1234 codec=Opus24k ts=24000 fec_block=5 fec_sym=2 repair=false len=87 fan_out=2
[12:00:00.143] TAP room=test dir=in  src=192.168.1.5:54321 seq=1235 codec=Opus24k ts=24960 fec_block=5 fec_sym=3 repair=false len=91
[12:00:00.500] TAP room=test dir=in  src=192.168.1.6:54322 seq=0042 codec=Codec2_1200 ts=40000 fec_block=1 fec_sym=0 repair=false len=6
[12:00:01.000] TAP room=test SIGNAL type=RoomUpdate count=3 participants=[Alice,Bob,Charlie]
[12:00:05.000] TAP room=test STATS period=5s in_pkts=250 out_pkts=500 fan_out_avg=2.0 loss_detected=0 codecs_seen=[Opus24k,Codec2_1200]
```

### What it shows

- **Per-packet**: direction, source/dest, sequence number, codec ID, timestamp, FEC block/symbol, repair flag, payload size
- **Signals**: RoomUpdate, FederationRoomJoin/Leave, handshake events
- **Periodic stats**: packets in/out, average fan-out, codecs seen, detected sequence gaps (loss)
- **Federation**: room-hash tagged datagrams with source/dest relay

### Implementation

**File:** `crates/wzp-relay/src/room.rs` — in `run_participant_plain()` and `run_participant_trunked()`

After receiving a packet and before forwarding:
```rust
if debug_tap_enabled {
    let h = &pkt.header;
    info!(
        room = %room_name,
        dir = "in",
        src = %addr,
        seq = h.seq,
        codec = ?h.codec_id,
        ts = h.timestamp,
        fec_block = h.fec_block,
        fec_sym = h.fec_symbol,
        repair = h.is_repair,
        len = pkt.payload.len(),
        "TAP"
    );
}
```

**Activation:** `--debug-tap <room_name>` CLI flag, or `debug_tap = "test"` / `debug_tap = "*"` in TOML config.

**Performance:** Only active when enabled. When enabled, adds one `info!()` log per packet per direction. At 50 fps × 5 participants = 500 log lines/sec — acceptable for debugging, not for production.

**Output options:**
- Default: tracing log (stderr)
- `--debug-tap-file <path>`: write to a dedicated file (JSONL format for machine parsing)

### Effort: 0.5 day

### Implementation Status (2026-04-13)

Fully implemented. `--debug-tap <room>` (or `*` for all rooms) logs:

- **Per-packet metadata** (`TAP`): direction, addr, seq, codec, timestamp, FEC fields, payload size, fan_out
- **Signal events** (`TAP SIGNAL`): `RoomUpdate` (count + participant names), `QualityDirective` (codec + reason), other signals by discriminant
- **Lifecycle events** (`TAP EVENT`): participant join (id, addr, alias), participant leave (id, addr, forwarded count, or room closed)

All output uses tracing `target: "debug_tap"` so it can be filtered with `RUST_LOG=debug_tap=info`.

---

## 2. Full Protocol Analyzer (Standalone Tool)

### Problem

The metadata tap shows packet flow but can't inspect audio content, verify encryption, or measure audio quality. For deep debugging (codec issues, resampling bugs, encryption mismatches), you need to see the actual decrypted audio.

### Solution

A standalone `wzp-analyzer` binary that either:
- **A)** Acts as a transparent proxy between client and relay (MITM mode)
- **B)** Reads a pcap/capture file with QUIC session keys (passive mode)
- **C)** Runs as a special "observer" client that joins a room in listen-only mode with all participants' consent

### Architecture

**Option C (recommended — simplest, no MITM):**

```
                    ┌──────────────┐
  Client A ────────►│    Relay     │◄──────── Client B
                    │              │
                    │   (SFU)      │◄──────── wzp-analyzer
                    └──────────────┘          (observer mode)
                                              │
                                              ▼
                                    ┌──────────────────┐
                                    │  Decode + Analyze │
                                    │  - Packet timing  │
                                    │  - Codec decode   │
                                    │  - Audio quality  │
                                    │  - Jitter stats   │
                                    │  - Waveform plot  │
                                    └──────────────────┘
```

The analyzer joins the room as a regular participant (receives all media via SFU forwarding) but doesn't send audio. It decodes everything it receives and produces analysis.

**Limitation:** End-to-end encrypted payloads can't be decoded without session keys. The analyzer would either:
1. Need the session key (shared out-of-band for debugging)
2. Or only analyze unencrypted headers + timing (same as the relay tap, but from client perspective with jitter buffer simulation)

For now, since encryption is not fully enforced in the current codebase (the crypto session is established but the actual ChaCha20 encryption of payloads is TODO in some paths), the analyzer can decode raw Opus/Codec2 payloads directly.

### Features

**Real-time display (TUI):**
```
┌─ wzp-analyzer: room "podcast" on 193.180.213.68:4433 ─────────────┐
│                                                                      │
│  Participants: Alice (Opus24k), Bob (Codec2_3200)                   │
│                                                                      │
│  Alice ────────────────────────────────────────                      │
│  seq: 5234  codec: Opus24k  ts: 125760  loss: 0.2%  jitter: 3ms   │
│  RMS: 4521  peak: 15280  silence: no                                │
│  FEC blocks: 1046/1046 complete (0 recovered)                       │
│  ▁▂▃▅▇█▇▅▃▂▁▁▂▃▅▇█▇▅▃▂▁  (waveform last 1s)                     │
│                                                                      │
│  Bob ──────────────────────────────────────                          │
│  seq: 2617  codec: Codec2_3200  ts: 62800  loss: 1.5%  jitter: 8ms│
│  RMS: 1250  peak: 6800  silence: no                                 │
│  FEC blocks: 523/525 complete (4 recovered)                         │
│  ▁▁▂▃▅▇▅▃▂▁▁▁▂▃▅▇▅▃▂▁▁  (waveform last 1s)                     │
│                                                                      │
│  Total: 7851 pkts recv, 0 pkts sent, 2 participants                │
│  Uptime: 2m 35s                                                      │
└──────────────────────────────────────────────────────────────────────┘
```

**Recorded analysis:**
- Save all received packets to a capture file
- Post-session report: per-participant stats, quality timeline, codec switches, packet loss patterns
- Export decoded audio as WAV per participant (if decryptable)

**Quality metrics per participant:**
- Packet loss % (from sequence gaps)
- Jitter (inter-arrival time variance)
- Codec switches (timestamps + reasons)
- RMS audio level over time
- Silence detection
- FEC recovery rate
- Round-trip estimates (from Ping/Pong if available)

### Implementation

**Binary:** `wzp-analyzer` (new crate or subcommand of `wzp-client`)

```
wzp-analyzer 193.180.213.68:4433 --room podcast
wzp-analyzer 193.180.213.68:4433 --room podcast --record capture.wzp
wzp-analyzer --replay capture.wzp --report report.html
```

**Dependencies:**
- Existing: `wzp-transport`, `wzp-proto`, `wzp-codec`, `wzp-crypto`
- New: `ratatui` for TUI display (optional)

### Phases

| Phase | Scope | Effort |
|-------|-------|--------|
| 1 | Header-only analysis: join room, log packet metadata, show per-participant stats (TUI) | 2 days |
| 2 | Audio decode: decode Opus/Codec2 payloads (unencrypted path), show waveform + RMS | 1-2 days |
| 3 | Capture/replay: save packets to file, replay offline with full analysis | 1 day |
| 4 | HTML report: post-session quality report with charts | 2 days |
| 5 | Encrypted payload support: accept session keys, decrypt ChaCha20 | 1 day |

### Non-Goals (v1)

- Active probing (sending test patterns)
- Modifying packets in transit
- Automated quality scoring (MOS estimation)
- Video support

## Implementation Status (2026-04-13)

All phases implemented:
- Phase 1 (Observer + stats): wzp-analyzer binary, passive room observer, per-participant stats — DONE
- Phase 2 (TUI): ratatui display with color-coded loss severity — DONE
- Phase 3 (Capture/Replay): Binary .wzp format + CaptureReader for offline replay — DONE
- Phase 4 (HTML report): Self-contained with Chart.js loss/jitter timelines — DONE
- Phase 5 (Encrypted decode): Stub — SFU E2E encryption requires session context. Header-only analysis works. — PARTIAL

Binary: `cargo build --bin wzp-analyzer`
Usage: `wzp-analyzer relay:4433 --room test [--capture out.wzp] [--html report.html] [--no-tui]`
