# PRD: Simulcast + Per-Receiver Layer Selection

> **Status:** proposed
> **Resolves:** Road-to-video Phases V5 + V6 (simulcast at sender, layer selection at SFU).
> **Depends on:** PRD #5 (video v1), PRD #7 (VideoQualityController).

## Problem

In a multi-peer video room, peers have wildly different link quality. A single uplink stream forces a choice: encode for the worst peer (everyone sees SD) or encode for the best peer (poor peers drop out). Simulcast solves this — sender uploads multiple independent layers, and the SFU forwards the appropriate layer to each receiver based on their current quality.

WZP's v2 wire format already reserves `stream_id: u8` for this. This PRD wires it up.

## Goals

- Sender emits 2–3 simultaneous H.264/H.265/AV1 streams per source (different bitrate/resolution).
- Each layer tagged by `stream_id` (0 = base/SD, 1 = mid/HD, 2 = high/FHD).
- SFU selects per-receiver which layer to forward, based on that receiver's last `QualityReport` / BWE.
- Layer switches are seamless (next keyframe boundary) and don't require sender involvement.
- Mixed-quality rooms work: best peer gets FHD, worst peer gets SD, no peer holds the room back.

## Non-goals

- SVC (per-layer temporal scalability within one bitstream). Simulcast achieves the same outcome with simpler encoder.
- Audio simulcast (audio is small; not worth the encode cost).

## Design

### Sender side

Three encoder instances per source:

| `stream_id` | Resolution | Target bitrate | Frame rate |
|---|---|---|---|
| 0 (low) | 480×270 | 150 kbps | 15 fps |
| 1 (mid) | 960×540 | 600 kbps | 30 fps |
| 2 (high) | 1920×1080 | 2.5 Mbps | 30 fps |

Resolution/bitrate ladder configurable per profile. Encoders share input frames (downsample for low/mid).

Each layer is an independent stream with its own `sequence`, `timestamp_ms`, and FEC blocks. Identified on the wire by `stream_id` byte in `MediaHeader` v2.

### SFU forwarding

`RoomManager` per-receiver state:

```rust
pub struct ReceiverState {
    fingerprint: Fingerprint,
    bwe_kbps: AtomicU32,
    loss_pct: AtomicU8,
    selected_layer: AtomicU8,  // per (sender, source_stream)
}
```

Layer selection logic (run periodically per receiver):

```
if receiver.bwe_kbps > HIGH_THRESHOLD && receiver.loss_pct < 2:
    selected_layer = high
elif receiver.bwe_kbps > MID_THRESHOLD:
    selected_layer = mid
else:
    selected_layer = low
```

Hysteresis: must hold new tier for 3 s before switching.

On layer switch:
- SFU continues forwarding the old layer until the next keyframe arrives on the new layer.
- If no keyframe on the new layer within 500 ms, SFU emits PLI to sender for that layer.

### Per-layer keyframe cache

PRD #5 keyframe cache extended: one cache entry per `(room, sender, stream_id)`. New joiner gets the most recent keyframe from the layer matched to their BWE.

### Layer-aware PLI suppression

PLI is layer-scoped. Sender refreshes only the requested layer, not all three.

## Implementation outline

1. `VideoQualityController` extended to drive 3 encoder instances per source (T5.5).
2. Frame distributor: downsample input frame for low/mid layers before encode.
3. Per-layer state on `MediaHeader` (already in v2 via `stream_id`).
4. SFU `ReceiverState` and selection logic (T5.6).
5. Per-layer keyframe cache (extension of PRD #5).
6. Per-layer PLI plumbing.
7. Telemetry: `wzp_room_layer_distribution{stream_id}` histogram.

## Acceptance criteria

- 3-encoder uplink works on M1 within 8 % CPU at 1080p30 / 540p30 / 270p15.
- 4-peer room with shaped links (5 Mbps, 1 Mbps, 500 kbps, 100 kbps): each peer receives the highest layer their link supports.
- Layer switch under improving link conditions occurs within 5 s of bandwidth recovery.
- No peer's bandwidth degradation holds back any other peer.

## Risks

- **3-encoder CPU cost on mid/low-end Android.** Mitigation: dynamic layer count — drop high layer if encoder queue grows; some devices may only support 2 layers.
- **Frame-rate drift between layers** (independent encoders running). Mitigation: shared frame clock; low/mid layers drop frames if needed to stay aligned.
- **SFU per-receiver state bloat.** Mitigation: only allocate state for active receivers; 80 B/receiver/sender bound.
- **Layer switch causing brief visible flicker.** Mitigation: switch only at keyframes; UI may show momentary resolution change but no glitch.

## Effort

~7 engineer-days (Wave 5 tasks T5.5 + T5.6).
