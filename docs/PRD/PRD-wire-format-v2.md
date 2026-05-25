# PRD: Wire Format v2

> **Status:** proposed
> **Resolves:** Audit W1, W4, W9, W10. Keystone prerequisite for video and per-`MediaType` conformance enforcement.
> **References:** `docs/WZP-SPEC.md`, `docs/ROAD-TO-VIDEO.md` Phase V1, `docs/PROTOCOL-AUDIT.md`.

## Problem

v1 wire format has four structural problems that compound the moment video lands:

- 16-bit sequence wraps in ~21 min at 50 pps (W1)
- MiniHeader has no sequence delta, so a missed full header desyncs (W4)
- CodecID is 4 bits → 16 codec slots, 9 used; video will exhaust it (W9)
- No `MediaType` field → SFU cannot distinguish audio/video/data without a codec lookup (W10)

Fixing these post-deployment is a multi-client coordinated break. Fix once, before video.

## Goals

- One wire-format change resolves W1, W4, W9, W10 and reserves headroom for the next decade.
- v1 and v2 can co-exist briefly during rollout via explicit version handshake (typed rejection, not silent corruption).
- All 571 audio tests pass under v2.

## Non-goals

- Backward wire compatibility (we will not encode v2 atop v1 — it is a clean break).
- Video framing rules themselves (covered by PRD #5).
- New codec IDs beyond reservation (covered by PRDs #5, #6).

## Design

### `MediaHeader` v2 (16 bytes, byte-aligned)

```
Byte 0:    version          (u8)   0x02
Byte 1:    flags            (u8)   bit 7: T (FEC repair)
                                   bit 6: Q (QualityReport trailer present, inside AEAD)
                                   bit 5: KeyFrame (video I-frame packet)
                                   bit 4: FrameEnd (last packet of access unit)
                                   bits 3-0: reserved (must be 0)
Byte 2:    media_type       (u8)   0=audio, 1=video, 2=data, 3=control
Byte 3:    codec_id         (u8)
Byte 4:    stream_id        (u8)   0=base; simulcast layers 1..N
Byte 5:    fec_ratio        (u8)   0..200 → 0.0..2.0
Bytes 6-9:   sequence       (u32 BE)
Bytes 10-13: timestamp_ms   (u32 BE)
Bytes 14-15: fec_block_id   (u16 BE)
                                   audio: low 8 bits = block_id, high 8 = symbol_idx
                                   video: full u16 block_id (large FEC blocks for I-frames)
```

Justification for byte alignment (16 B over 12 B packed) is in `ROAD-TO-VIDEO.md` Phase V1; benchmarks showed ≤ 0.32 % stream overhead delta across all scenarios.

### `MiniHeader` v2 (5 bytes)

```
[FRAME_TYPE_MINI = 0x01]
Byte 0:    seq_delta            (u8)        ← new; resolves W4
Bytes 1-2: timestamp_delta_ms   (u16 BE)
Bytes 3-4: payload_len          (u16 BE)
```

Audio only. Video pays the full 16 B header per packet (no clean periodic structure to compress).

### Version negotiation

`CallOffer` and `CallAnswer` already carry supported profiles. Add:

```rust
struct CallOffer {
    ...
    protocol_version: u8,         // 2 in v2 clients
    supported_versions: Vec<u8>,  // e.g. [2]
}
```

Relay/peer side:
- If `protocol_version` is supported → proceed.
- If unsupported → close with `Hangup::ProtocolVersionMismatch { server_supported: Vec<u8> }`.

No silent fallback. No mixed-version session.

### Sequencing semantics

- `sequence` is per-stream, monotonic, u32, wraps at 2^32. At 1000 pps that is ~50 days — effectively no wrap.
- `timestamp_ms` is per-stream, milliseconds since session start, u32, ~49.7 days range. Rebase behavior at rekey: **does not reset** — kept monotonic across rekeys (documented as a separate hardening item in PRD #4, W3).
- `fec_block_id` is per-stream, u16, wraps at 2^16. With ≥ 5-frame blocks that is ~22 minutes at 50 pps — adequate but PRD #4 (W2) covers epoch counter if needed.

## Implementation outline

1. New types in `wzp-proto/src/packet.rs` behind a `proto-v2` feature flag.
2. Round-trip tests for `MediaHeader v2` and `MiniHeader v2` (encode → decode → assert equal).
3. Migrate `wzp-codec` encode path to emit v2 headers.
4. Migrate `wzp-client` and `wzp-relay` parse paths.
5. `CallOffer`/`CallAnswer` carry `protocol_version` and `supported_versions`.
6. Typed `Hangup::ProtocolVersionMismatch` reason.
7. Remove v1 emission path once all 571 tests pass under v2 (drop the feature flag default).
8. Add migration note to `WZP-SPEC.md`.

## Acceptance criteria

- All 571 audio tests pass with v2 headers.
- A v1 client connecting to a v2 relay receives `Hangup::ProtocolVersionMismatch` within 1 RTT.
- Wire-level capture confirms 16 B `MediaHeader` and 5 B `MiniHeader` on real audio calls.
- `media_type` byte readable by relay without parsing `codec_id` (enables PRD #2 Tier A separation).

## Risks

- **Stranding old clients.** Force-update prompt in UI; release notes; staged rollout (relays accept v1 for 2 weeks before flipping to reject).
- **MiniHeader 5 B vs 4 B regression check.** Trunking math reconfirmed (cap of 10 binds before MTU — no change).

## Effort

~2.5 engineer-days (Wave 1 tasks T1.1–T1.3 in the index).
