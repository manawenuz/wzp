# PRD: Multi-Codec Video Negotiation (H.264 + H.265 + AV1)

> **Status:** proposed
> **Resolves:** Road-to-video Phase V3 codec rollout; reserves `CodecID` slots 9–13.
> **Depends on:** PRD #5 (video v1 working with H.264).

## Problem

H.264 baseline ships first because it has universal hardware encode coverage. H.265 offers ~30 % efficiency at equal quality and is now broadly supported in HW (Apple A10+, Snapdragon since ~2017, NVENC since GTX 9xx). AV1 is the long-term target but hardware encode is limited (Apple M3/A17+, Snapdragon 8 Gen 3+, RTX 40+).

We need codec negotiation so each session uses the best mutually-supported codec without manual configuration, and so we can roll AV1 in gated on real telemetry.

## Goals

- `CodecID` assignments for H.264 baseline (9), H.264 main (10), H.265 main (11), AV1 (12), VP9 reserved (13).
- Capability declaration in `CallOffer.supported_codecs`.
- Picker logic: highest mutually-supported codec from a deterministic preference cascade.
- Hardware-encode detection at session start; refuse codecs requiring SW encode on battery-powered devices.
- Existing framer/depacketizer reused — only the codec wrapper changes.

## Non-goals

- New codecs beyond this list.
- Per-receiver codec selection (one codec per stream for v1; could be revisited with simulcast).

## Design

### Codec capability declaration

```rust
pub struct CodecCapability {
    pub codec_id: u8,
    pub max_resolution: (u16, u16),
    pub max_fps: u8,
    pub hardware: bool,    // true if HW encode available
}

pub struct CallOffer {
    ...
    pub supported_codecs: Vec<CodecCapability>,
}
```

### Preference cascade

```
preference: [AV1, H.265 main, H.264 main, H.264 baseline]

pick = first codec in `preference` where:
    caller.supported.contains(codec)
    AND callee.supported.contains(codec)
    AND (codec.hardware on both sides OR codec.allow_software)
```

`allow_software` defaults to `false` for AV1 (battery cost too high), `true` for H.264 (cheap SW fallback).

### Per-codec details

| ID | Codec | Encoder priority |
|---|---|---|
| 9 | H.264 baseline | VideoToolbox / MediaCodec / NVENC / QSV / AMF / VAAPI; OpenH264 SW |
| 10 | H.264 main | Same HW; same SW |
| 11 | H.265 main | VideoToolbox A10+ / MediaCodec / NVENC GTX 9xx+ / QSV Skylake+; x265 SW (slow, disabled by default) |
| 12 | AV1 | VideoToolbox M3+/A17+ / MediaCodec SD8G3+ / NVENC RTX 40+; SVT-AV1 SW (gated) |
| 13 | VP9 | Reserved; may not implement |

### Framer reuse

The 16 B `MediaHeader` carries `codec_id`. The framer doesn't care which codec — it fragments NALs (for H.264/H.265) or OBUs (for AV1) into MTU-sized chunks, sets `KeyFrame`/`FrameEnd` bits, and passes payload through. Per-codec parameter sets (SPS/PPS for H.264/H.265, sequence header OBU for AV1) ship on the signal stream.

### Mid-call codec switch

Optional in v1. If implemented:
- Sender sends `SignalMessage::CodecSwitch { stream_id, new_codec_id, parameter_sets }`.
- Receiver swaps decoder and emits PLI to force a clean keyframe.

## Implementation outline

1. `CodecCapability` declaration + serde (additive change).
2. HW probe at session start (per platform).
3. Picker logic in `CallOffer`/`CallAnswer` flow.
4. H.265 encoder/decoder wrappers (VideoToolbox + MediaCodec).
5. AV1 encoder/decoder wrappers, gated on HW (SVT-AV1 fallback behind flag).
6. Prometheus: `wzp_session_codec_id_total{codec}` for telemetry on actual codec usage.

## Acceptance criteria

- Two macOS clients (M1 + M3) pick H.265 by default; M3 + iPhone 15 Pro pick AV1.
- M1 + Android device without H.265 HW picks H.264.
- Codec selection is deterministic given both sides' capabilities.
- AV1 refused on devices without HW unless `allow_software` flag explicitly set.

## Rollout gates

- H.264 baseline + main: ship with PRD #5.
- H.265: enable by default once HW probe accuracy verified on 5+ macOS + 5+ Android devices.
- AV1: 20 % of session-start probes must report HW encode capability before enabling by default. Until then, available only via debug flag.

## Risks

- **AV1 SW encode torches battery.** Mitigation: HW gate is mandatory; SW fallback off by default.
- **H.265 patent surface.** Mitigation: rely on platform-provided HW encoders (license covered upstream); avoid shipping x265 binary.
- **HW probe lies on some Android devices.** Mitigation: in-session fallback if encoder errors at start; degrade one codec tier.

## Effort

- H.265 wrappers: 3 d (T5.4)
- AV1 wrappers + HW gate: 5 d (T6.1)
- Picker + capability declaration: 1 d

Total: ~9 engineer-days, in Waves 5–6.
