# PRD: Adaptive Quality Control (Auto Codec)

## Problem

When a user selects "Auto" quality, the system currently just starts at Opus 24k (GOOD) and never changes. There is no runtime adaptation — if the network degrades mid-call, audio breaks up instead of gracefully stepping down to a lower bitrate codec. Conversely, if the network is excellent, the user stays on 24k when they could have studio-quality 64k.

The relay already sends `QualityReport` messages with loss % and RTT, and a `QualityAdapter` exists in `call.rs` that classifies network conditions into GOOD/DEGRADED/CATASTROPHIC — but none of this is wired into the Android or desktop engines.

## Solution

Wire the existing `QualityAdapter` into both engines so that "Auto" mode continuously monitors network quality and switches codecs mid-call. The full quality range should be used:

```
Excellent network  →  Studio 64k (best quality)
Good network       →  Opus 24k (default)
Degraded network   →  Opus 6k (lower bitrate, more FEC)
Poor network       →  Codec2 3.2k (vocoder, heavy FEC)
Catastrophic       →  Codec2 1.2k (minimum viable voice)
```

## Architecture

```
                    ┌─────────────────────┐
  Relay ──────────► │  QualityReport      │  loss %, RTT, jitter
                    │  (every ~1s)        │
                    └────────┬────────────┘
                             │
                             ▼
                    ┌─────────────────────┐
                    │  QualityAdapter     │  classify + hysteresis
                    │  (3-report window)  │
                    └────────┬────────────┘
                             │ recommend new profile
                             ▼
              ┌──────────────┴──────────────┐
              │                             │
              ▼                             ▼
     ┌────────────────┐           ┌────────────────┐
     │  Encoder        │           │  Decoder        │
     │  set_profile()  │           │  (auto-switch   │
     │  + FEC update   │           │   already works)│
     └────────────────┘           └────────────────┘
```

## Existing Infrastructure

### What already exists (in `crates/wzp-client/src/call.rs`)

1. **`QualityAdapter`** (lines 97-196):
   - Sliding window of `QualityReport` messages
   - `classify()`: loss > 15% or RTT > 200ms → CATASTROPHIC, loss > 5% or RTT > 100ms → DEGRADED, else → GOOD
   - `should_switch()`: hysteresis — requires 3 consecutive reports recommending the same profile before switching
   - Prevents oscillation between profiles

2. **`QualityReport`** (in `wzp-proto/src/packet.rs`):
   - Sent by relay piggy-backed on media packets
   - Fields: `loss_pct` (u8, 0-255 scaled), `rtt_4ms` (u8, RTT in 4ms units), `jitter_ms`, `bitrate_cap_kbps`

3. **`CallEncoder::set_profile()`** / **`CallDecoder` auto-switch**:
   - Encoder can switch codec mid-stream
   - Decoder already auto-detects incoming codec from packet headers

### What's missing

1. **QualityReport ingestion** — neither Android engine nor desktop engine reads quality reports from the relay
2. **Profile switch loop** — no periodic check that feeds reports to `QualityAdapter` and applies recommended switches
3. **Upward adaptation** — `QualityAdapter` only classifies into 3 tiers (GOOD/DEGRADED/CATASTROPHIC). Needs extension to recommend studio tiers when conditions are excellent (loss < 1%, RTT < 50ms)
4. **Notification to UI** — when quality changes, the UI should show the current active codec

## Requirements

### Phase 1: Basic Adaptive (3-tier)

**Both Android and Desktop:**

1. **Ingest QualityReports**: In the recv loop, extract `quality_report` from incoming `MediaPacket`s when present. Feed to `QualityAdapter`.

2. **Periodic quality check**: Every 1 second (or on each QualityReport), call `adapter.should_switch(&current_profile)`. If it returns `Some(new_profile)`:
   - Switch the encoder: `encoder.set_profile(new_profile)`
   - Update FEC encoder: `fec_enc = create_encoder(&new_profile)`
   - Update frame size if changed (e.g., 20ms → 40ms)
   - Log the switch

3. **Frame size adaptation on switch**: When switching from 20ms to 40ms frames (or vice versa):
   - Android: update `frame_samples` variable, resize `capture_buf`
   - Desktop: same — the send loop reads `frame_samples` dynamically

4. **UI indicator**: Show current active codec in the call screen stats line.
   - Android: add to `CallStats` and display in stats text
   - Desktop: add to `get_status` response and display in stats div

5. **Only in Auto mode**: Adaptive switching should only happen when the user selected "Auto". If they manually selected a profile, respect their choice.

### Phase 2: Extended Range (5-tier)

Extend `QualityAdapter::classify()` to use the full codec range:

| Condition | Profile | Codec |
|-----------|---------|-------|
| loss < 1% AND RTT < 30ms | STUDIO_64K | Opus 64k |
| loss < 1% AND RTT < 50ms | STUDIO_48K | Opus 48k |
| loss < 2% AND RTT < 80ms | STUDIO_32K | Opus 32k |
| loss < 5% AND RTT < 100ms | GOOD | Opus 24k |
| loss < 15% AND RTT < 200ms | DEGRADED | Opus 6k |
| loss >= 15% OR RTT >= 200ms | CATASTROPHIC | Codec2 1.2k |

With hysteresis:
- **Downgrade**: 3 consecutive reports (fast reaction to degradation)
- **Upgrade**: 5 consecutive reports (slow, cautious improvement)
- **Studio upgrade**: 10 consecutive reports (very conservative — avoid bouncing to 64k on brief good patches)

### Phase 3: Bandwidth Probing

Rather than relying solely on loss/RTT:
1. Start at GOOD
2. After 10 seconds of stable call, probe upward by switching to STUDIO_32K
3. If no quality degradation after 5 seconds, probe to STUDIO_48K
4. If degradation detected, immediately fall back
5. This discovers the true available bandwidth rather than guessing from loss stats

## Implementation Plan

### Android (`crates/wzp-android/src/engine.rs`)

```rust
// In the recv loop, after decoding:
if let Some(ref qr) = pkt.quality_report {
    quality_adapter.ingest(qr);
}

// Periodic check (every 50 frames ≈ 1 second):
if auto_profile && frames_decoded % 50 == 0 {
    if let Some(new_profile) = quality_adapter.should_switch(&current_profile) {
        info!(from = ?current_profile.codec, to = ?new_profile.codec, "auto: switching quality");
        let _ = encoder_ref.lock().set_profile(new_profile);
        fec_enc_ref.lock() = create_encoder(&new_profile);
        current_profile = new_profile;
        frame_samples = frame_samples_for(&new_profile);
        // Resize capture buffer if needed
    }
}
```

**Challenge**: The encoder is in the send task and the quality reports arrive in the recv task. Need shared state (AtomicU8 for profile index, or a channel).

**Recommended approach**: Use an `AtomicU8` that the recv task writes and the send task reads:
```rust
let pending_profile = Arc::new(AtomicU8::new(0xFF)); // 0xFF = no change

// Recv task: when adapter recommends switch
pending_profile.store(new_profile_index, Ordering::Release);

// Send task: check at frame boundary
let p = pending_profile.swap(0xFF, Ordering::Acquire);
if p != 0xFF { /* apply switch */ }
```

### Desktop (`desktop/src-tauri/src/engine.rs`)

Same pattern. The desktop engine already has separate send/recv tasks with shared atomics for mic_muted, etc. Add a `pending_profile: Arc<AtomicU8>` following the same pattern.

### Desktop CLI (`crates/wzp-client/src/call.rs`)

The `CallEncoder` already has `set_profile()`. The `CallDecoder` already auto-switches. Just need to:
1. Add `QualityAdapter` to `CallDecoder`
2. Feed quality reports in `ingest()`
3. Check `should_switch()` in `decode_next()`
4. Emit the recommendation via a callback or return value

## Testing

1. **Local test with tc/netem**: Use Linux traffic control to simulate loss/latency:
   ```bash
   # Simulate 10% loss, 150ms RTT
   tc qdisc add dev lo root netem loss 10% delay 75ms
   # Run 2 clients in auto mode, verify they switch to DEGRADED
   ```

2. **CLI test**: Run `wzp-client --profile auto` between two instances with simulated network conditions

3. **Relay quality reports**: Verify the relay actually sends QualityReport messages. If it doesn't yet, that needs to be implemented first (check relay code).

## Open Questions

1. **Does the relay currently send QualityReports?** If not, Phase 1 is blocked until the relay implements per-client loss/RTT tracking and report generation. The relay sees all packets and can compute loss % per sender.

2. **Codec2 3.2k placement**: Should auto mode use Codec2 3.2k between DEGRADED and CATASTROPHIC? It's 20ms frames (lower latency than Opus 6k's 40ms) but speech-only quality.

3. **Cross-client adaptation**: If client A is on GOOD and client B auto-adapts to CATASTROPHIC, client A still sends Opus 24k. Client B can decode it fine (auto-switch on recv). But should A also be told to lower quality to save B's bandwidth? This requires signaling between clients.

## Milestones

| Phase | Scope | Effort | Dependency |
|-------|-------|--------|------------|
| 0 | Verify relay sends QualityReports | 0.5 day | None |
| 1a | Wire QualityAdapter in Android engine | 1 day | Phase 0 |
| 1b | Wire QualityAdapter in desktop engine | 1 day | Phase 0 |
| 1c | UI indicator (current codec) | 0.5 day | Phase 1a/1b |
| 2 | Extended 5-tier classification | 0.5 day | Phase 1 |
| 3 | Bandwidth probing | 2 days | Phase 2 |
