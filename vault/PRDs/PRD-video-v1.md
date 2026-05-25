---
tags: [prd, wzp]
type: prd
---

# PRD: Video v1 — H.264 Single-Layer

> **Status:** proposed
> **Resolves:** Road-to-video Phases V3 + V4 (encoder/decoder, framer, NACK, keyframe cache).
> **Depends on:** PRD #1 (wire format v2), PRD #3 (TransportFeedback + BWE).

## Problem

WZP has no video path. Add a working unidirectional video call (macOS↔macOS first, then Android↔macOS) using H.264 baseline, with loss recovery appropriate for lossy mobile links.

## Goals

- New `wzp-video` crate parallel to `wzp-codec`.
- H.264 baseline encode/decode using platform hardware encoders.
- NAL fragmentation and access-unit reassembly conformant to our 16 B `MediaHeader` v2.
- NACK loop for P-frame loss (RTT-gated).
- Dynamic FEC ratio boost on I-frame packets.
- SFU keyframe cache for fast join-to-first-frame.
- PLI suppression at SFU to bound upstream keyframe-request traffic.

## Non-goals

- Multi-codec negotiation (PRD #6).
- Simulcast or per-receiver layer selection (PRD #8).
- VideoQualityController logic beyond a fixed bitrate target (PRD #7).
- Native camera capture pipelines (separate platform work).

## Design

### `wzp-video` crate

```
wzp-video/
  src/
    encoder.rs       # trait VideoEncoder
                     # VideoToolboxEncoder (macOS)
                     # MediaCodecEncoder (Android, JNI)
                     # OpenH264Encoder (software fallback)
    decoder.rs       # trait VideoDecoder; mirror per-platform
    framer.rs        # H.264 NAL fragmentation to MTU-sized chunks
    depacketizer.rs  # Reassemble NALs, emit access units
    keyframe.rs      # Keyframe request handling, sender + receiver
    config.rs        # SPS/PPS shipment over signal stream
```

### Framing

One access unit (frame) → N packets, each ≤ `MTU - 16 (header) - 16 (AEAD tag)`.

- `sequence` global per (session, stream_id), advances per packet.
- `timestamp_ms` is presentation time, equal across all packets of a single access unit.
- `KeyFrame` bit set on every packet of an I-frame.
- `FrameEnd` bit set on the last packet of the access unit.
- `fec_block_id` per access unit (u16 in v2, large blocks).

Parameter sets (SPS/PPS) ride on the **signal stream**, not media datagrams. Sent at session start and on codec change. Reliable, ordered, one-time.

### NACK loop

```
SignalMessage::Nack {
    version: u8,
    stream_id: u8,
    seqs: Vec<u32>,    // missing P-frame packets
}
```

Receiver behavior:
- If access unit incomplete after `frame_interval` ms:
  - If `RTT < 2 × frame_interval`: emit `Nack`.
  - Else: emit `PictureLossIndication`.
- Backoff: max 1 Nack per (stream, seq) per 2 × RTT.

Sender behavior:
- On `Nack`: re-transmit if packet is still in send buffer (last 500 ms).
- On `PictureLossIndication`: emit a fresh I-frame within 200 ms.

### Dynamic FEC on I-frames

Encoder marks packets belonging to I-frames. FEC layer applies a higher ratio (default 0.5) to I-frame blocks, vs. nominal (0.1) for P-frames. Configurable.

### SFU keyframe cache

`RoomManager` maintains per `(room, sender, stream_id)`:

```rust
struct KeyframeCache {
    packets: Vec<Bytes>,        // most recent complete I-frame
    timestamp_ms: u32,
    sequence_first: u32,
}
```

On new participant join, cache is replayed before live forwarding starts. Eliminates 2 s black-screen-on-join.

Cache TTL: replaced whenever a new complete I-frame arrives.

### PLI suppression

If ≥ 2 receivers PLI within 200 ms for the same `(sender, stream_id)`, the SFU emits one `KeyframeRequest` upstream, not N. Tracked per-(sender, stream).

## Implementation outline

1. `wzp-video` crate scaffold (T4.1).
2. Framer/depacketizer with property tests (T4.1).
3. VideoToolbox encoder/decoder (macOS) (T4.2).
4. MediaCodec encoder/decoder (Android, JNI) (T4.3).
5. NACK signal + sender/receiver state machines (T4.4).
6. I-frame FEC ratio hint plumbed from encoder to FEC layer (T4.5).
7. SFU keyframe cache (T4.6).
8. PLI suppression (T4.7).
9. End-to-end test: macOS sender → relay → macOS receiver, 5 min call, < 1 % loss network.

## Acceptance criteria

- Unidirectional H.264 720p30 call macOS↔macOS, CPU < 5 % on M1.
- Android↔macOS works with MediaCodec (surface-texture path).
- Black-screen-on-join < 200 ms when keyframe cache is warm.
- Under 5 % synthetic packet loss at 50 ms RTT: NACK recovery keeps video smooth, < 1 keyframe / 2 s.
- Under 5 % synthetic packet loss at 300 ms RTT: PLI fallback fires, keyframe rate ~ 1 / s.
- Upstream PLI traffic at SFU < 2 / s under simulated mass packet loss with 8 receivers.

## Risks

- **MediaCodec surface-texture edge cases.** Per-device matrix; software fallback path mandatory.
- **VideoToolbox H.264 baseline restrictions** (some profiles are main-only in HW). Mitigation: profile detection at session start.
- **NACK storm under heavy loss.** Mitigation: rate cap (max 50 Nacks/s/receiver) and exponential backoff.
- **Keyframe cache memory footprint** (one I-frame per active stream per room). Mitigation: cap cache at 200 KB; if exceeded, drop and rely on PLI.

## Effort

~3 weeks (Wave 4 tasks T4.1–T4.7).
