# Incident Report: Playout Ring Buffer Cursor Desync — Bidirectional Audio Loss

**Date:** 2026-04-06
**Severity:** Critical — causes 10-16 seconds of complete bidirectional silence mid-call
**Status:** Root-caused, fix pending
**Affects:** All clients using `AudioRing` (Android, potentially desktop)

## Summary

Both participants in a call experience simultaneous, prolonged audio silence (10-16 seconds) despite the QUIC transport, relay, and Rust codec pipeline all functioning normally. The root cause is a cursor desynchronization in the lock-free SPSC ring buffer (`AudioRing`) that transfers decoded PCM from the Rust recv task to the Kotlin AudioTrack playout thread.

## How We Know It's the Ring Buffer

### Evidence that eliminates other components

| Component | Evidence it's healthy | Source |
|-----------|----------------------|--------|
| **QUIC send path** | `frames_dropped=0, send_errors=0` on both clients | Engine send stats log |
| **QUIC recv path** | `max_recv_gap_ms=82, recv_errors=0` — no gaps >82ms | Engine recv stats log |
| **Relay forwarding** | `max_forward_ms=0, send_errors=0` in previous relay-instrumented test | Relay debug logging |
| **Opus codec** | `frames_decoded=2442` over 51.9s = 47 frames/sec (correct for 20ms) | Final stats JSON |
| **FEC** | `fec_recovered=4870` — FEC working normally | Final stats JSON |
| **Audio capture** | Pixel 6 capture has 0% silence; Nothing has gaps but those are expected mic pauses | capture_rms.csv |

### Evidence pointing to the ring buffer

1. **Both clients go silent at the exact same wall-clock moment (26.66s into call)** — rules out per-device issues; the common factor is the relay, but the relay was proven healthy in prior tests.

2. **`playout_avail=8640` at stats dump time** — the playout ring reports 8640 samples available (180ms, nearly full at the 9600 capacity). The recv task believes it has successfully written data into the ring. But the AudioTrack playout thread is reading silence (RMS=0 for 12+ seconds).

3. **Recv task continued receiving packets with no gaps** — `max_recv_gap_ms=82` across the entire call. The decoded audio was written to the ring continuously.

4. **Silence starts and ends cleanly** — the transition from audio → silence happens within a single 20ms frame (frame 1332: rms=101, frame 1333: rms=0). This is not network degradation (which shows gradual quality loss). It's a discrete state change — the reader suddenly stops seeing data.

5. **Recovery is also discrete** — at ~38.8s (Sharp Hawk) and ~42.7s (Pixel 6), audio snaps back with high-energy frames (rms=3296+). Not a gradual reconnection.

## The Ring Buffer Code

**File:** `crates/wzp-android/src/audio_ring.rs`

```rust
const RING_CAPACITY: usize = 960 * 10; // 9600 samples = 200ms at 48kHz

pub struct AudioRing {
    buf: Box<[i16; RING_CAPACITY]>,
    write_pos: AtomicUsize,  // monotonically increasing, wraps at usize::MAX
    read_pos: AtomicUsize,   // monotonically increasing, wraps at usize::MAX
}
```

### `available()` — how many samples can be read
```rust
pub fn available(&self) -> usize {
    let w = self.write_pos.load(Ordering::Acquire);
    let r = self.read_pos.load(Ordering::Acquire);
    w.wrapping_sub(r)  // relies on usize wrapping arithmetic
}
```

### `write()` — producer (Rust recv task thread, inside tokio block_on)
```rust
pub fn write(&self, samples: &[i16]) -> usize {
    let w = self.write_pos.load(Ordering::Relaxed);
    let count = samples.len().min(RING_CAPACITY);
    // ... write samples at (w + i) % RING_CAPACITY ...
    self.write_pos.store(w.wrapping_add(count), Ordering::Release);

    // If we overwrote unread data, advance read_pos
    if self.available() > RING_CAPACITY {
        let new_read = self.write_pos.load(Ordering::Relaxed).wrapping_sub(RING_CAPACITY);
        self.read_pos.store(new_read, Ordering::Release);
    }
}
```

### `read()` — consumer (Kotlin AudioTrack JVM thread, via JNI)
```rust
pub fn read(&self, out: &mut [i16]) -> usize {
    let avail = self.available();
    let count = out.len().min(avail);
    let r = self.read_pos.load(Ordering::Relaxed);
    // ... read samples at (r + i) % RING_CAPACITY ...
    self.read_pos.store(r.wrapping_add(count), Ordering::Release);
    count
}
```

## Suspected Failure Modes

### 1. Writer advances `read_pos` while reader is mid-read (data race)

The `write()` method at lines 68-72 modifies `read_pos` from the writer thread when it detects overflow. But the `read()` method on the consumer thread also modifies `read_pos`. This violates the SPSC contract — `read_pos` is supposed to be owned by the consumer.

**Scenario:**
1. Reader loads `read_pos = R` (line 82)
2. Writer detects overflow, stores `read_pos = R'` (line 71) where `R' > R`
3. Reader finishes reading, stores `read_pos = R + count` (line 88) — **overwrites** the writer's `R'` with a stale, smaller value

After step 3, the ring's `read_pos` has gone backwards. Now `available()` returns `write_pos.wrapping_sub(old_read_pos)` which is larger than `RING_CAPACITY`. Every subsequent `write()` call hits the overflow branch and keeps advancing `read_pos`, but the reader keeps overwriting it back. The ring is in a corrupted state where the reader and writer are fighting over `read_pos`.

### 2. `wrapping_sub` returns astronomically large values

`available()` uses `w.wrapping_sub(r)`. On a 64-bit platform, if due to the race above `r > w`, `wrapping_sub` returns `usize::MAX - (r - w) + 1` — an enormous number. The `read()` method caps this with `out.len().min(avail)` so it reads `out.len()` samples. But those samples are from indices calculated as `(r + i) % RING_CAPACITY` which wraps correctly. The samples read would be whatever was in the buffer at those positions — potentially stale/old data, or zeros from initialization.

However, the playout RMS CSV shows clean zeros (RMS=0), not garbage. This suggests the ring is returning the zeroed-out initial buffer contents, meaning `read_pos` has jumped far ahead of `write_pos`, pointing to memory that was never written to (or was written long ago and has since been zeroed by the overflow advance logic).

### 3. Why silence lasts exactly 12-16 seconds

After the desync, each `write()` call (every 20ms when a packet is decoded) enters the overflow branch and resets `read_pos`. But the reader immediately overwrites it back in its next `read()` call. This tug-of-war continues until one of two things happens:
- The cursors happen to realign through wrapping arithmetic
- A timing coincidence where the writer's store to `read_pos` happens to "win" the race

The 12-16 second duration is non-deterministic and depends on exact thread scheduling and cursor values.

## Reproduction Pattern

The bug manifests after roughly 25-30 seconds of a call. This timing is suspicious:
- At 48kHz mono, 20ms frames = 50 frames/sec
- Each decoded frame writes 960 samples to the ring
- After 25 seconds: `write_pos ≈ 25 * 50 * 960 = 1,200,000`
- The ring capacity is 9600, so `write_pos` has wrapped around `RING_CAPACITY` about 125 times

The wrapping of the monotonic cursors past certain thresholds, combined with the reader/writer `read_pos` race, likely triggers the desync at this scale.

## Data Files

All data from two independent test sessions (3 calls total) is in `/workspace/wzp/debug/`:

| File | Contents |
|------|----------|
| `wzp_debug_20260406_120546.zip` | Sharp Hawk (Nothing A059) — 51.9s call |
| `wzp_debug_20260406_120549.zip` | Bright Viper (Pixel 6) — 51.9s call |
| `wzp_debug_20260406_111733.zip` | Sharp Hawk — earlier 72.0s call, same pattern |
| `wzp_debug_20260406_111735.zip` | Bright Viper — earlier 72.0s call, same pattern |
| `wzp_debug_20260406_105858.zip` | First session (pre-logging fix), 39.8s call |
| `wzp_debug_20260406_105900.zip` | First session, 39.7s call |

### Key fields in each zip

- `meta.txt` — device, duration, final stats JSON
- `playout_rms.csv` — per-frame (20ms) RMS of AudioTrack output; silence = RMS 0
- `capture_rms.csv` — per-frame RMS of AudioRecord input
- `logcat.txt` — Android logcat filtered to WZP + audio tags

### How to reproduce the analysis

```python
import csv
with open("playout_rms.csv") as f:
    for row in csv.DictReader(f):
        if int(row['rms']) == 0 and int(row['time_ms']) > 2000:
            print(f"SILENCE at {row['time_ms']}ms")
```

## Affected Code

- `crates/wzp-android/src/audio_ring.rs` — the `AudioRing` struct, specifically the `write()` method's overflow handling that mutates `read_pos` from the producer thread
- Any client using `AudioRing` for playout (currently only Android; desktop uses `cpal` directly)

## Constraints for the Fix

1. Must remain lock-free — AudioTrack thread runs at `Thread.MAX_PRIORITY` and cannot block
2. Must handle overflow gracefully — if the reader falls behind, old audio should be dropped, not cause a desync
3. The writer (Rust recv task) and reader (Kotlin AudioTrack via JNI) run on different threads with different scheduling priorities
4. Ring capacity is 200ms which is tight — any fix must not increase latency significantly
5. The `write_pos` and `read_pos` are the only synchronization mechanism (no mutex, no condvar)
