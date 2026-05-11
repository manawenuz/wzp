# PRD: Video Quality Controller + PriorityMode

> **Status:** proposed
> **Resolves:** Road-to-video Phase V5 (video adaptive controller, audio-priority gate, ScreenShare slide-mode).
> **Depends on:** PRD #3 (BWE), PRD #5 (video v1).

## Problem

Audio and video share a finite bandwidth budget. The FaceTime model — audio absolute priority, video elastic on top — is right for the default voice/video call, but it's wrong for screen-share / presentation where a frozen slide deck is worse than slightly degraded audio.

We need: a single `VideoQualityController` consuming BWE, with a policy gate driven by a user/product-selectable `PriorityMode`.

## Goals

- `PriorityMode` enum carried on `QualityProfile`.
- Per-mode allocation gates: `AudioFirst`, `VideoFirst`, `ScreenShare`, `Balanced`.
- Mid-call `SetPriorityMode` signal for runtime override.
- ScreenShare slide-fallback: when bandwidth drops below SD video floor, encoder switches to single-I-frame-every-N-seconds mode (no wire format change).
- Sensible defaults per call type (voice/video call → AudioFirst; presentation app → ScreenShare).

## Non-goals

- Multi-stream priority (e.g., one HD + one screen-share in the same session — separate work).
- Custom user-defined modes; only the four enum variants.

## Design

### `PriorityMode`

```rust
#[derive(Debug, Clone, Copy, PartialEq, Eq, Serialize, Deserialize)]
pub enum PriorityMode {
    AudioFirst,    // default for voice/video calls
    VideoFirst,    // user override
    ScreenShare,   // video + slide fallback; audio = intelligible speech only
    Balanced,      // proportional split
}
```

Carried on `QualityProfile`:

```rust
pub struct QualityProfile {
    ...
    pub priority_mode: PriorityMode,    // default AudioFirst
    pub video_bitrate_kbps: Option<u32>,
    pub video_resolution: Option<(u16, u16)>,
    pub video_fps: Option<u8>,
}
```

Mid-call change:

```rust
SignalMessage::SetPriorityMode {
    version: u8,
    mode: PriorityMode,
}
```

### Allocation gates

```
let bwe = bandwidth_estimator.target_send_bps();

match priority_mode {
    AudioFirst => {
        audio_budget = max(24_kbps, audio_tier_min);  // audio floor first
        video_budget = bwe.saturating_sub(audio_budget);
        // video → 0 before audio degrades below floor
    }
    VideoFirst => {
        video_budget = max(video_floor, target_video_bps);
        audio_budget = bwe.saturating_sub(video_budget);
        // audio degrades to Opus 16k floor first
    }
    ScreenShare => {
        // Audio gets just enough for intelligible speech.
        audio_budget = 16_kbps;
        video_budget = bwe.saturating_sub(audio_budget);
        if video_budget < SD_VIDEO_FLOOR {
            encoder.set_mode(EncoderMode::SlideFallback);
        }
    }
    Balanced => {
        audio_budget = (bwe as f64 * 0.15) as u64;
        video_budget = bwe - audio_budget;
    }
}
```

### `VideoQualityController`

```rust
pub struct VideoQualityController {
    bwe: Arc<BandwidthEstimator>,
    mode: AtomicU8,    // PriorityMode
    encoder: Arc<dyn VideoEncoder>,
    loss_pct: AtomicU8,
    rtt_ms: AtomicU32,
    encoder_queue_ms: AtomicU32,
}

impl VideoQualityController {
    pub fn tick(&self) {
        let budget = self.allocate();
        let target = self.derive_target(budget);  // (bitrate, fps, resolution, layer)
        self.encoder.set_target(target);
    }
}
```

`derive_target` maps `(budget, loss, rtt, queue)` to encoder parameters via a step table. Smoothed; no jumps larger than 2× per second.

### ScreenShare slide-fallback

Pure encoder policy:
- Normal video: continuous frames, target fps (5–15 for screen content).
- When `video_budget < SD_VIDEO_FLOOR` (e.g., 150 kbps): switch to slide mode.
- Slide mode: emit one high-quality I-frame every 2–5 s. No P-frames. Encoder prefers H.265 or AV1 (text legibility).
- Wire format: `KeyFrame=1` on every packet, `FrameEnd=1` on last packet of slide. No new fields.

Receiver doesn't know slide mode is on — just sees keyframes arriving slowly.

### Defaults

| Product flow | Default mode |
|---|---|
| Voice call | AudioFirst (no video) |
| Video call | AudioFirst |
| Screen share | ScreenShare |
| User toggle in settings | VideoFirst or Balanced |

## Implementation outline

1. `PriorityMode` enum + serde + `QualityProfile` field (T5.1).
2. `SetPriorityMode` signal variant (T5.1).
3. `VideoQualityController::new` + `tick` (T5.2).
4. Per-mode allocation gates (T5.2).
5. `EncoderMode::SlideFallback` in `wzp-video` (T5.3).
6. Integration: `CallEngine` honors `SetPriorityMode` within 1 s.
7. UI plumbing for runtime toggle (out of scope here; tracked by platform team).

## Acceptance criteria

- 100 kbps shaped link, `AudioFirst`: audio holds Opus 24 k, video drops to 0.
- 100 kbps shaped link, `ScreenShare`: audio holds Opus 16 k, video in slide mode emits 1 I-frame / 3 s.
- 100 kbps shaped link, `VideoFirst`: audio drops to Opus 16 k, video holds floor.
- 5 Mbps link, `AudioFirst`: video reaches HD within 10 s.
- `SetPriorityMode` mid-call applied within 1 s.

## Risks

- **Mode flapping under unstable BWE.** Mitigation: 10 s dwell time before allowing mode-driven encoder reconfiguration.
- **Slide mode mistaken for poor connection by users.** Mitigation: UI indicator distinguishing "slide mode active" from "poor connection".
- **AudioFirst floor too aggressive for low-bandwidth music calls.** Mitigation: when audio profile is `Opus 64k music`, floor raised to 48 k.

## Effort

~6 engineer-days (Wave 5 tasks T5.1–T5.3).
