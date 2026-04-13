# PRD: Engine.rs Deduplication — Extract Shared Send/Recv Helpers

## Problem

`desktop/src-tauri/src/engine.rs` is 1,705 lines with two nearly identical `CallEngine::start()` implementations — one for Android (880 lines) and one for desktop (430 lines). ~350 lines are copy-pasted between them. Every change to the encode/decode/adaptive-quality pipeline requires editing both places, and they've already diverged in subtle ways (Android has extensive first-join diagnostics that desktop lacks).

## Scope

Extract the duplicated logic into shared helper functions. The Android and desktop paths should only differ in their audio I/O mechanism (Oboe ring via wzp-native vs CPAL capture_ring/playout_ring).

## What's Duplicated

| Block | Description | Lines (each) |
|-------|-------------|------|
| `build_call_config()` | Resolve quality string → CallConfig | 23 |
| Codec-to-profile match | Map CodecId → QualityProfile for decoder switch | 19 |
| Adaptive quality switch | Read AtomicU8, index_to_profile, set_profile, update frame_samples + dred_tuner | 15 |
| DRED tuner poll | Check frame counter, poll quinn stats, apply tuning | 15 |
| Quality report ingestion | Extract quality_report, feed to AdaptiveQualityController, store to AtomicU8 | 8 |
| Signal task | Accept signals, handle RoomUpdate/QualityDirective/Hangup | 48 |
| **Total** | | **~128 lines × 2 = 256 lines eliminated** |

## Implementation

### Phase 1: Top-Level Helper Functions

```rust
fn build_call_config(quality: &str) -> CallConfig {
    let profile = resolve_quality(quality);
    match profile {
        Some(p) => CallConfig {
            noise_suppression: false,
            suppression_enabled: false,
            ..CallConfig::from_profile(p)
        },
        None => CallConfig {
            noise_suppression: false,
            suppression_enabled: false,
            ..CallConfig::default()
        },
    }
}

fn codec_to_profile(codec: CodecId) -> QualityProfile {
    match codec {
        CodecId::Opus24k => QualityProfile::GOOD,
        CodecId::Opus6k => QualityProfile::DEGRADED,
        CodecId::Opus32k => QualityProfile::STUDIO_32K,
        CodecId::Opus48k => QualityProfile::STUDIO_48K,
        CodecId::Opus64k => QualityProfile::STUDIO_64K,
        CodecId::Codec2_1200 => QualityProfile::CATASTROPHIC,
        CodecId::Codec2_3200 => QualityProfile {
            codec: CodecId::Codec2_3200,
            fec_ratio: 0.5,
            frame_duration_ms: 20,
            frames_per_block: 5,
        },
        other => QualityProfile { codec: other, ..QualityProfile::GOOD },
    }
}

fn check_adaptive_switch(
    pending: &AtomicU8,
    encoder: &mut CallEncoder,
    tuner: &mut wzp_proto::DredTuner,
    frame_samples: &mut usize,
    tx_codec: &tokio::sync::Mutex<String>,
) -> bool {
    let p = pending.swap(PROFILE_NO_CHANGE, Ordering::Acquire);
    if p == PROFILE_NO_CHANGE { return false; }
    if let Some(new_profile) = index_to_profile(p) {
        let new_fs = (new_profile.frame_duration_ms as usize) * 48;
        if encoder.set_profile(new_profile).is_ok() {
            *frame_samples = new_fs;
            tuner.set_codec(new_profile.codec);
            // Caller updates tx_codec display string
            return true;
        }
    }
    false
}
```

### Phase 2: Shared Signal Task

Extract the signal task into a standalone async function:

```rust
async fn run_signal_task(
    transport: Arc<wzp_transport::QuinnTransport>,
    running: Arc<AtomicBool>,
    pending_profile: Arc<AtomicU8>,
    participants: Arc<Mutex<Vec<ParticipantInfo>>>,
) {
    loop {
        if !running.load(Ordering::Relaxed) { break; }
        match tokio::time::timeout(
            Duration::from_millis(SIGNAL_TIMEOUT_MS),
            transport.recv_signal(),
        ).await {
            Ok(Ok(Some(msg))) => {
                // Handle RoomUpdate, QualityDirective, Hangup...
            }
            _ => {}
        }
    }
}
```

### Phase 3: Shared DRED Poll + Quality Ingestion

These are small blocks but appear in both send and recv tasks. Extract as inline helpers or closures.

## Verification

1. `cargo check --workspace` — must compile
2. `cargo test -p wzp-proto -p wzp-relay -p wzp-client --lib` — must pass
3. Manual test: place a call Android↔Desktop, verify audio works in both directions
4. Verify adaptive quality still switches (set one side to auto, degrade network)

## Effort

- Phase 1: 1 hour (extract 3 functions, update 6 call sites)
- Phase 2: 30 min (extract signal task, update 2 spawn sites)
- Phase 3: 30 min (cleanup remaining small duplicates)
- Total: ~2 hours

## Not In Scope

- Audio I/O trait abstraction (Oboe vs CPAL) — different project, different risk profile
- Moving Android-specific diagnostics (first-join, PCM recorder) into a feature flag
- Splitting engine.rs into multiple files

## Implementation Status (2026-04-13)

All phases implemented:
- build_call_config(): shared CallConfig construction — DONE
- codec_to_profile(): shared CodecId → QualityProfile mapping — DONE
- run_signal_task(): shared signal handler — DONE
- Net reduction: ~39 lines, 6 duplicated blocks → single-line calls
