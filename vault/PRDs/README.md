---
tags: [prd, wzp]
type: prd
---

# PRD Index — Protocol v2, Video, Abuse Mitigation

> Coordinated worklist that addresses (a) the P0/P1 findings in `docs/PROTOCOL-AUDIT.md`, (b) the video roadmap in `docs/ROAD-TO-VIDEO.md`, and (c) the relay abuse vectors in `docs/ATTACK-SURFACE-RELAY-ABUSE.md`. Each item below links to its own PRD.

## Why a combined plan

The three documents share substantial structure:

- **Wire format v2** (audit P0: W1, W4, W9, W10) is the prerequisite for video framing **and** for per-`MediaType` conformance enforcement against abuse. One change resolves three pressures.
- **TransportFeedback + BWE** (audit P1: W6, W14) is mandatory for video, materially improves audio adaptation, and gives the relay another observable for abuse detection.
- **Relay conformance enforcement** (attack surface Tiers A–G) is independently valuable for audio today, and the v2 `MediaType` bit lets it scale cleanly to video.

Sequencing matters. Implementing v2 wire format **before** any video work or any deep abuse mitigation avoids two compatibility breaks.

## PRD catalog

| # | PRD | Resolves | Status |
|---|---|---|---|
| 1 | [PRD-wire-format-v2](./PRD-wire-format-v2.md) | Audit W1, W4, W9, W10; prereq for #5/#6/#7/#8 and Tier F of #2 | proposed |
| 2 | [PRD-relay-conformance](./PRD-relay-conformance.md) | Attack-surface Tiers A–G | proposed |
| 3 | [PRD-transport-feedback-bwe](./PRD-transport-feedback-bwe.md) | Audit W6, W14 | proposed |
| 4 | [PRD-protocol-hardening](./PRD-protocol-hardening.md) | Audit W2, W3, W5, W11, W12, W13 (security + correctness batch) | proposed |
| 5 | [PRD-video-v1](./PRD-video-v1.md) | Road-to-video Phases V3 + V4 (H.264 single-layer, NACK, keyframe cache) | proposed |
| 6 | [PRD-video-multicodec](./PRD-video-multicodec.md) | H.265 + AV1 negotiation (road-to-video Phase V3 codec rollout) | proposed |
| 7 | [PRD-video-quality-priority](./PRD-video-quality-priority.md) | Road-to-video Phase V5 (VideoQualityController + PriorityMode + ScreenShare) | proposed |
| 8 | [PRD-video-simulcast](./PRD-video-simulcast.md) | Road-to-video Phases V5 + V6 (simulcast, per-receiver layer selection at SFU) | proposed |

Native capture pipelines (road-to-video Phase V7) are out of scope here — they sit downstream of #5 and are platform team work; tracked separately.

## Dependency graph

```
                      ┌───────────────────────────────┐
                      │  #1 Wire format v2 (keystone) │
                      └────────┬──────────────────────┘
                               │
        ┌──────────────────────┼────────────────────────┐
        │                      │                        │
        ▼                      ▼                        ▼
┌──────────────┐    ┌──────────────────┐    ┌──────────────────────┐
│ #2 Conformance│    │ #3 Transport     │    │ #4 Protocol          │
│  Tier A-G     │    │   Feedback + BWE │    │   Hardening          │
└──────┬────────┘    └────────┬─────────┘    └──────────────────────┘
       │ Tier A-D first       │
       │ Tier F needs traffic │
       │ baseline             │
       │                      │
       │              ┌───────▼────────┐
       │              │ #5 Video v1    │
       │              │ (H.264 + NACK) │
       │              └───────┬────────┘
       │                      │
       │       ┌──────────────┼──────────────┐
       │       │              │              │
       │       ▼              ▼              ▼
       │  ┌────────┐  ┌──────────────┐  ┌──────────────┐
       │  │ #6     │  │ #7 Video     │  │ #8 Simulcast │
       │  │ Multi- │  │   Quality +  │  │              │
       │  │ codec  │  │   Priority   │  │              │
       │  └────────┘  └──────────────┘  └──────────────┘
       │
       └──> #2 Tier F (video) — needs #5 in production traffic to baseline
```

## Combined task list

Ordered by dependency and risk. Each task references its PRD.

### Wave 1 — Foundation (week 1)

| Task | PRD | Effort | Output |
|---|---|---|---|
| T1.1 Land 16 B MediaHeader v2 + 5 B MiniHeader v2 in `wzp-proto` | #1 | 1 d | New types behind feature flag; old paths still work |
| T1.2 Update `wzp-codec` + `wzp-client` + `wzp-relay` to emit v2 | #1 | 1 d | All audio tests pass under v2 |
| T1.3 Protocol version negotiation in `CallOffer/CallAnswer` (typed `Hangup::ProtocolVersionMismatch`) | #1 + #4 (W12) | 0.5 d | v1 clients rejected with clear reason |
| T1.4 `QualityReport` trailer moved inside AEAD payload (or AAD-bound) | #4 (W5) | 0.5 d | Security fix, audit log |
| T1.5 Anti-replay window made per-stream and per-MediaType configurable | #4 (W11) | 0.5 d | Audio=64, video=1024 ready |

### Wave 2 — Feedback + abuse mitigation (week 2)

| Task | PRD | Effort | Output |
|---|---|---|---|
| T2.1 `SignalMessage::TransportFeedback` variant | #3 | 1 d | Wire path; not yet consumed |
| T2.2 `BandwidthEstimator` in `wzp-proto` (cwnd + remb fusion) | #3 | 2 d | Prometheus output |
| T2.3 `AdaptiveQualityController` consumes BWE | #3 | 1 d | Audio upgrade decisions use bandwidth, not just loss |
| T2.4 `wzp-relay/src/conformance.rs` — Tier A (bitrate ceilings per CodecID) | #2 | 1 d | Bulk-tunnel abuse killed |
| T2.5 Tier B (packet-rate cap) + Tier C (timestamp consistency) | #2 | 1 d | Loud abuse caught |
| T2.6 Prometheus: `relay_conformance_*` counters + observable histograms | #2 | 0.5 d | Baseline data collection starts |

### Wave 3 — Protocol hardening (week 3)

| Task | PRD | Effort | Output |
|---|---|---|---|
| T3.1 `fec_block_id` widened to u16 in v2 | #4 (W2) | 0.5 d | No FEC collisions on slow joiners |
| T3.2 Document `timestamp_ms` rebase behavior at rekey | #4 (W3) | 0.5 d | Spec clarity |
| T3.3 `SignalMessage` variants prefixed with `version: u8` | #4 (W12) | 0.5 d | Future-proof signaling |
| T3.4 `RoomManager` migrated to `DashMap<RoomId, Arc<RwLock<Room>>>` | #4 (W13) | 2 d | No per-packet global lock |
| T3.5 Tier E (per-fingerprint / per-IP token bucket) wired to featherChat auth | #2 | 1.5 d | Aggregate quota enforced |
| T3.6 Tier D (per-codec packet-size sanity) | #2 | 0.5 d | Sneaky-payload class caught |

### Wave 4 — Video v1 (weeks 4–6)

| Task | PRD | Effort | Output |
|---|---|---|---|
| T4.1 `wzp-video` crate scaffold; H.264 framer + depacketizer | #5 | 4 d | NAL fragmentation, access-unit reassembly |
| T4.2 VideoToolbox encoder + decoder (macOS) | #5 | 3 d | Unidirectional video macOS↔macOS |
| T4.3 MediaCodec encoder + decoder (Android, via JNI) | #5 | 5 d | Android video path |
| T4.4 NACK loop (`SignalMessage::Nack`) + RTT-gated policy | #5 | 2 d | P-frame loss recovery |
| T4.5 Dynamic FEC ratio on I-frames (encoder hint to FEC layer) | #5 | 1 d | I-frame survivability without round trip |
| T4.6 SFU keyframe cache per (room, sender, stream) | #5 | 2 d | < 200 ms join-to-first-frame |
| T4.7 PLI suppression at SFU | #5 | 1 d | Bounded upstream PLI rate |

### Wave 5 — Quality, codecs, simulcast (weeks 7–9)

| Task | PRD | Effort | Output |
|---|---|---|---|
| T5.1 `PriorityMode` enum on `QualityProfile` + `SignalMessage::SetPriorityMode` | #7 | 1 d | Wire path |
| T5.2 `VideoQualityController` with per-mode allocation gates | #7 | 3 d | AudioFirst / VideoFirst / Balanced live |
| T5.3 ScreenShare mode: slide-fallback encoder policy | #7 | 2 d | Presentation use case viable |
| T5.4 H.265 encoder/decoder (reuse framer) | #6 | 3 d | Codec negotiation cascade live |
| T5.5 Simulcast: encoder emits 3 layers; `stream_id` carries layer | #8 | 4 d | Layer-tagged uplink |
| T5.6 Per-receiver layer selection at SFU | #8 | 3 d | Mixed-quality rooms work |
| T5.7 Tier F (entropy scorer) — audio variant first, baselined from Wave 2/3 data | #2 | 3 d | Covert-tunnel pressure |
| T5.8 Tier G (response policy + audit log) | #2 | 1 d | Operational |

### Wave 6 — AV1 + Tier F video (weeks 10+)

| Task | PRD | Effort | Output |
|---|---|---|---|
| T6.1 AV1 encoder/decoder with HW detection (SVT-AV1 fallback) | #6 | 5 d | Top-tier efficiency on capable HW |
| T6.2 Tier F video scorer (keyframe periodicity, I/P frame-size ratio, BWE responsiveness) | #2 | 3 d | Video abuse detection |
| T6.3 Federated reputation gossip (optional) | #2 | 4 d | Cross-relay abuse mitigation |

## Risk register

| Risk | Likelihood | Impact | Mitigation |
|---|---|---|---|
| v2 wire format break strands old clients | High | High | Typed `Hangup::ProtocolVersionMismatch`, clear UI, force update prompt |
| BWE oscillation regresses audio adaptation | Med | Med | Behind feature flag; A/B with shadow Prometheus before flipping default |
| Conformance Tier A false positives | Low | High | Math-derived ceilings × 1.5; counter-only mode for 1 week before enforcement |
| `DashMap` migration regresses room semantics | Med | Med | Integration tests for federation + trunking before merging |
| Android MediaCodec edge cases (Nothing A059 baseline) | High | Med | Per-device test matrix; software fallback path |
| AV1 software encode torches battery | High | Low | HW probe at session start; refuse AV1 if no HW encode |
| Tier F false-positives on edge cases (e.g., long silences in lectures) | Med | High | Verdict-only mode + 30 s window minimum + Suspect tier escalation |

## Open product questions (not blocking)

- Anonymous vs. authenticated quota split — numbers TBD pending Prometheus baseline.
- Whether to expose `PriorityMode` UI for end users or only via product preset (call vs. screen-share).
- AV1 rollout gate: 5 %? 20 %? of sessions reporting HW support before enabling by default.
- Federated reputation gossip is powerful but introduces a poisoning surface; decision deferred to after Wave 5.
