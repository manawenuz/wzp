# Road to Video

> Plan for adding video to WZP. Audio remains unchanged through Phase V1; video is additive. See `PROTOCOL-AUDIT.md` for the issues this plan addresses.

## Premise

The transport, crypto, session, federation, and SFU layers are codec-agnostic. The work is concentrated in:

1. Wire format (CodecID width, MediaType, MiniHeader seq, simulcast hooks)
2. Framer / depacketizer (NAL fragmentation, access-unit reassembly)
3. Bandwidth estimator (Quinn cwnd + transport feedback)
4. Keyframe semantics (PLI, NACK, keyframe cache at SFU)
5. Capture / encode pipeline (VideoToolbox / MediaCodec / NVENC)

## Implementation Status (as of 2026-05-25)

| Phase | Description | Status |
|---|---|---|
| V1 — Wire format | 16B MediaHeader v2, 5B MiniHeader v2, MediaType, u32 seq, 8-bit CodecID | ✅ Complete (T1.x) |
| V2 — Transport additions | BWE, NACK loop, TransportFeedback, dynamic FEC boost on I-frames | 🔲 Not started |
| V3 — `wzp-video` crate | H.264 baseline framer/depacketizer, VideoToolbox/MediaCodec/dav1d encoders | ✅ Substantially complete (T4.x, T5.x, T6.x) |
| V3 — H.264 Baseline | Single-layer H.264 | ✅ Complete |
| V3 — H.265 | VideoToolbox + MediaCodec H.265 | ✅ Complete (T5.x) |
| V3 — AV1 | dav1d + SVT-AV1 (non-Android), VideoToolbox AV1 (macOS M3+) | ✅ Complete; Android MediaCodec AV1 compile errors pending (T4.3.1.1) |
| V3 — Android MediaCodec | NDK 0.9 API migration for `mediacodec.rs` | 🔴 Blocked (31 compile errors) |
| V3 — Call engine wiring | `create_video_encoder()` integrated into active call negotiation | 🔴 Not started (T6.1.2 follow-up) |
| V4 — Keyframe & loss policy | NACK path, PLI, keyframe cache at SFU | 🟡 Framework present (`nack.rs`); not wired |
| V5 — Video adaptive controller | `VideoQualityController` + `PriorityMode` | 🟡 Controller built (`controller.rs`); not wired into call |
| V5 — Simulcast | Simulcast layer management | 🟡 `simulcast.rs` present; not wired |
| V6 — SFU changes | Keyframe cache, per-receiver layer selection, PLI suppression | 🟡 PLI suppression wired; keyframe cache + layer selection not started |
| V6 — Video scorer | `VideoScorer` legitimacy detection | 🟡 Built (`video_scorer.rs`); `observe()` not wired into room forwarding |
| V7 — Capture pipeline | Camera capture (AVCaptureSession, Camera2, NVENC) | 🔲 Not started |

**Legend:** ✅ Complete · 🟡 Partial/Framework only · 🔴 Blocked · 🔲 Not started

### Critical path to first video call

1. Fix Android MediaCodec compile errors (T4.3.1.1) — ~2h
2. Wire `create_video_encoder()` into call engine codec negotiation (T6.1.2) — ~2h
3. Fix crypto nonce bug (`decrypt()` must use `MediaHeader.seq`) — see `AUDIT-2026-05-25.md` C1 — ~1h
4. Wire `VideoScorer::observe()` into relay room forwarding (T6.2 follow-up) — ~2h
5. Implement Phase V2 BWE (mandatory for usable video) — ~3–4 days
6. Implement capture pipeline for at least one platform (V7) — ~1 week

## Phase V1 — Wire format & negotiation (no new code paths yet)

Bump protocol version. Land all wire changes together so compat breaks exactly once.

### Sizing decision (2026-05-11)

Hypothetical benchmarks on 12 B packed vs 16 B byte-aligned showed the overhead delta is invisible across every realistic scenario:

| Scenario | Δ overhead (12 B → 16 B) | Δ % of stream |
|---|---|---|
| Opus 24k audio (MiniHeader 49/50) | 4 B/s | 0.013 % |
| Codec2 1200 audio | 2 B/s | 0.13 % |
| H.264 SD 500 kbps video | 1.6 kbps | 0.32 % |
| H.264 HD 2.5 Mbps video | 7.1 kbps | 0.28 % |
| H.264 FHD 5 Mbps video | 14.1 kbps | 0.28 % |

Trunking cap (10) binds before MTU for audio, so TrunkFrame layout is unaffected. ChaCha20-Poly1305 cost is dominated by AEAD setup, not byte count — 4 extra bytes per packet is < 0.1 % of AEAD CPU on Cortex-A55.

**Decision: 16 B byte-aligned.** Bit-packing saves nothing material and costs recurring debug / fuzzer / evolution complexity. Reserves headroom for the next decade.

### `MediaHeader` v2 (16 B byte-aligned)

```
Byte 0:    version       (u8)   currently 0x02
Byte 1:    flags         (u8)   [T:1][Q:1][KeyFrame:1][FrameEnd:1][reserved:4]
                                T = FEC repair
                                Q = QualityReport trailer present
                                KeyFrame = packet belongs to an I-frame (video)
                                FrameEnd = last packet of an access unit (video)
Byte 2:    media_type    (u8)   0=audio, 1=video, 2=data, 3=control
Byte 3:    codec_id      (u8)   widened from 4-bit (room for 256)
Byte 4:    stream_id     (u8)   simulcast layer; 0=base
Byte 5:    fec_ratio     (u8)   0..200 → 0.0..2.0
Bytes 6-9:   sequence    (u32 BE)
Bytes 10-13: timestamp_ms (u32 BE)
Bytes 14-15: fec_block_id (u16 BE)
                                audio: low 8 bits block_id, high 8 bits symbol_idx
                                video: full u16 block_id (large blocks for I-frames)
```

- `version=2` is a hard switch — old clients receive a typed `Hangup::ProtocolVersionMismatch`.
- `media_type` (W10) lets the SFU drop video first under load without a codec lookup.
- `KeyFrame` lets a joining peer fast-forward to the next I-frame; SFU keyframe cache keys on it.
- `FrameEnd` lets the depacketizer fire an access unit without counting packets.
- `stream_id` is forward-compatible for simulcast (Phase V5).
- `sequence` widened to u32 (W1) — also benefits audio.

### `MiniHeader` v2 (5 B)

```
[FRAME_TYPE_MINI = 0x01]
Byte 0:    seq_delta            (u8)            ← new (W4)
Bytes 1-2: timestamp_delta_ms   (u16 BE)
Bytes 3-4: payload_len          (u16 BE)
```

Audio-only in V1. Video pays the full 16 B header per packet (every frame is a new access unit; no clean periodic structure to compress).

### New codec IDs

| ID | Codec | Notes |
|---|---|---|
| 9 | H.264 baseline | Universal HW encode coverage; ship first |
| 10 | H.264 main | Slight quality win over baseline; same HW |
| 11 | H.265 main | Apple A10+ universal, Snapdragon since ~2017, NVENC GTX 9xx+; ~30 % win vs H.264 |
| 12 | AV1 | Apple M3/A17+, Snapdragon 8 Gen 3+, RTX 40+, Arc, RX 7000+; best efficiency, narrow HW |
| 13 | VP9 | Reserved; may not implement |

Negotiation: `CallOffer.supported_codecs: Vec<CodecId>`. Both sides pick the highest mutually supported codec from preference cascade `[AV1, H.265, H.264 main, H.264 baseline]`.

### `QualityProfile` extension

Add:
- `video_bitrate_kbps: Option<u32>`
- `video_resolution: Option<(u16, u16)>`
- `video_fps: Option<u8>`
- `priority_mode: PriorityMode` (see Phase V5)

`CallOffer` / `CallAnswer` already negotiate profiles — slot video into the same path.

### Acceptance
- All 571 audio tests pass with `V=2` headers.
- Old v1 clients refused gracefully (clear error in `CallAnswer`).

## Phase V2 — Transport additions

**Decision (2026-05-11): all media on QUIC datagrams; no separate "reliable media" stream.**

A QUIC stream for I-frames was considered and rejected. A 200 KB I-frame on a 1 Mbps mobile link takes ~1.6 s to transit a stream, and the next I-frame queues behind it (HoL blocking by design). Datagrams + NACK + dynamic per-keyframe FEC degrade more gracefully on the lossy links we care about.

1. **All media on datagrams.** Uniform wire format; no HoL.
2. **NACK loop for video P-frames.** When `RTT < 2 × frame_interval`, receiver NACKs missing P-frame packets via `SignalMessage::Nack { stream_id, seqs }`. Otherwise (high RTT) skip NACK and request a keyframe via `PictureLossIndication`.
3. **Dynamic FEC boost on I-frames.** Encoder bumps `fec_ratio` to ~0.5 for keyframe packets (k=20 source → r=10 repair). Recovers most I-frame loss without a round trip.
4. **SPS/PPS / parameter sets on the existing signal stream.** Reliable, ordered, one-time at session start. Re-sent on codec switch. No new stream needed.
5. **`SignalMessage::TransportFeedback`** — `{ acked_seqs: Vec<u32>, nacked_seqs: Vec<u32>, remb_bps: u32, recv_time_us: u64 }`. Sent every 50 ms or every N packets, whichever first. Feeds BWE.
6. **`BandwidthEstimator` in `wzp-proto`** — consumes Quinn `cwnd`, `bytes_in_flight`, plus `TransportFeedback`. Output: `target_send_bps = min(cwnd_bps * 0.9, remb_bps)`.

### Acceptance
- Audio adapts to bandwidth (not just loss/RTT); fewer oscillations between 24 k and 32 k Opus on stable links.
- BWE output is on Prometheus.
- NACK round-trip recovery verified under 1–5 % packet loss at RTT ≤ 100 ms.

## Phase V3 — `wzp-video` crate

New crate parallel to `wzp-codec`:

```
wzp-video/
  src/
    encoder.rs       # trait VideoEncoder; VideoToolboxEncoder, MediaCodecEncoder,
                     # OpenH264Encoder fallback
    decoder.rs       # trait VideoDecoder
    framer.rs        # NAL unit fragmentation to MTU-sized chunks
                     # (simpler than RFC 6184 FU-A — we own both ends)
    depacketizer.rs  # Reassemble NALs, emit access units
    keyframe.rs      # Keyframe request handling
```

Framing rules:
- One access unit → N packets, each ≤ MTU − 12 (MediaHeader) − 16 (AEAD tag).
- `sequence` global per stream; `timestamp_ms` is presentation time.
- `KeyFrame` bit set on every packet of an I-frame.
- Last packet of frame: "frame end" bit (steal from `StreamId` or repurpose `reserved`).

Platform encoders:
- macOS / iOS: VideoToolbox
- Android: MediaCodec (surface texture path, no CPU copy)
- Windows: MediaFoundation → NVENC / QSV / AMF
- Linux: VAAPI / NVENC; OpenH264 software fallback

### Acceptance
- Unidirectional H.264 call working between two desktop clients.
- CPU usage on M1 < 5 % at 720p30; on Android mid-tier < 15 %.

## Phase V4 — Keyframe & loss policy

- On packet loss inside a P-frame: NACK if RTT < 2× frame interval, otherwise request keyframe via `SignalMessage::PictureLossIndication { stream_id }`.
- Joining peer: relay sends most recent keyframe from its cache.
- Tier downgrade: drop to lower simulcast layer, request keyframe for the new layer.

### Acceptance
- Black-screen-on-join < 200 ms when keyframe cache is warm.
- < 1 keyframe / 2 s on stable links; bursty on lossy links.

## Phase V5 — Video adaptive controller + PriorityMode

### `PriorityMode` on `QualityProfile`

```rust
pub enum PriorityMode {
    AudioFirst,    // default for calls: audio absolute priority, video elastic
    VideoFirst,    // user override: video priority, audio degrades second
    ScreenShare,   // video + slide-fallback; audio = intelligible speech only
    Balanced,      // proportional split, no absolute priority
}
```

Selected at call setup. Mutable mid-call via `SignalMessage::SetPriorityMode { mode }`. Defaults to `AudioFirst` for voice/video calls; presentation apps set `ScreenShare`; users can override to `VideoFirst` from settings.

### `VideoQualityController`

```
inputs:  bwe_bps, loss_pct, rtt_ms, encoder_queue_ms, priority_mode
outputs: target_bitrate, target_fps, target_resolution, simulcast_layer

allocation gate (per PriorityMode):

  AudioFirst:
    audio_budget = max(24 kbps, audio_tier_min)
    video_budget = bwe_bps - audio_budget
    Under congestion: video → 0 before audio degrades.

  VideoFirst:
    video_budget = max(video_floor, target_video_kbps)
    audio_budget = bwe_bps - video_budget
    Audio degrades first to Opus 16 k; video held at floor.

  ScreenShare:
    video_budget = bwe_bps - 16 kbps    // audio gets just Opus 16 k floor
    If video_budget < SD floor: switch encoder to slide mode
      (single high-quality I-frame every 2-5s instead of continuous video).
    Audio floor in this mode is Opus 16 k (speech only, no music).

  Balanced:
    audio_budget = bwe_bps * 0.15
    video_budget = bwe_bps * 0.85
    Both degrade proportionally.
```

Slide mode in `ScreenShare` is an encoder policy on the existing `wzp-video` framer (lower fps, higher per-frame quality, prefer HEVC/AV1 for text). No wire format change.

### Acceptance
- On a 100 kbps link in `AudioFirst`, audio stays at Opus 24 k and video drops to 0.
- On a 100 kbps link in `ScreenShare`, slide mode emits one I-frame every 3 s and audio holds Opus 16 k.
- On a 5 Mbps link, video ramps to top simulcast layer within 10 s.
- `SetPriorityMode` mid-call is honored within 1 s.

## Phase V6 — SFU changes

- **Per-room keyframe cache.** Latest I-frame per `(sender, stream_id)`. Sent to new joiners immediately. Eliminates "black screen for 2 seconds" on join.
- **Per-receiver layer selection.** Sender uploads ~3 simulcast layers; relay decides which to forward to each receiver based on their last `QualityReport`. Critical for N > 3 rooms.
- **PLI suppression.** If 10 receivers PLI within 200 ms, send one `KeyframeRequest` upstream, not 10.

### Acceptance
- 8-peer room with mixed link quality; high-quality peers see HD, low-quality peers see SD, no peer holds the room back.
- PLI traffic at SFU upstream < 1 / s under simulated mass packet loss.

## Phase V7 — Capture pipeline (platform-specific)

- macOS: `AVCaptureSession` → VideoToolbox → `wzp-video`. Wire into Tauri backend.
- Android: Camera2 → MediaCodec → JNI bridge into `wzp-native` or sibling cdylib. Surface texture path.
- Desktop Tauri (Windows): MediaFoundation → NVENC.

### Acceptance
- Camera permission flows on all platforms.
- < 50 ms end-to-end capture-to-encode latency on M1.

## Deferred

- **SVC** (per-layer temporal scalability in one bitstream). Simulcast (separate streams per layer) is enough for v1; wire format already supports it via `StreamId`.
- **Screen sharing.** Same codec path with a different capture source.
- **Group video keys.** Existing X25519 session key works; no protocol change needed.

## Suggested order of work

| Step | Effort | Output |
|---|---|---|
| 1. Wire format v2: 16 B MediaHeader, 5 B MiniHeader, MediaType, KeyFrame, FrameEnd, u32 seq, 8-bit CodecID | ~1 day | Audio still works under new header layout |
| 2. TransportFeedback + BandwidthEstimator (Quinn cwnd + remb) | 3–4 days | Audio adaptation improves; BWE on Prom |
| 3. `wzp-video` crate, H.264 baseline single-layer | 1–2 weeks | Unidirectional video call works |
| 4. NACK path + dynamic FEC boost on I-frames | 4–5 days | Loss recovery for video |
| 5. Keyframe cache at SFU + PLI suppression | 1 week | Fast join, low PLI traffic |
| 6. H.265 codec support (reuse framer) | 3 days | ~30 % quality win on Apple HW |
| 7. Simulcast + per-receiver layer selection | 1 week | Mixed-quality rooms work |
| 8. `VideoQualityController` + PriorityMode (incl. ScreenShare slide mode) | 1 week | Graceful degradation under congestion, user choice |
| 9. AV1 codec (gated on HW telemetry) | 4–5 days | Top-tier efficiency on capable devices |
| 10. Native capture pipelines (VideoToolbox / MediaCodec / NVENC) | 2 weeks | Production camera support per OS |

Step 1 is the lowest-regret, highest-leverage change and unlocks everything else.

Steps 3 + 6 + 9 form the codec rollout: ship H.264 first (works everywhere → unblocks integration testing on every device), add H.265 once framer is stable (low-effort, big Apple win), gate AV1 on real device telemetry. By 2028 we should be in a position to deprecate H.264 if telemetry says < 5 % of sessions still need it.
