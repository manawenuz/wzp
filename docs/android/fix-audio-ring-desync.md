# Fix: AudioRing SPSC Buffer Cursor Desync

## Problem

A critical bug causes 10-16 seconds of bidirectional audio silence mid-call (~25-30s in). Both participants go silent at the exact same moment. The QUIC transport, relay, Opus codec, and FEC are all healthy — the bug is in the lock-free ring buffer that transfers decoded PCM from the Rust recv task to the Kotlin AudioTrack playout thread.

**Root cause:** `AudioRing::write()` modifies `read_pos` from the producer thread during overflow handling (lines 68-72 of `audio_ring.rs`). This violates the SPSC invariant — only the consumer should own `read_pos`. When both threads write to `read_pos`, a race corrupts the cursor state, causing the reader to see an empty or stale buffer for 12-16 seconds.

**Full forensics:** `debug/INCIDENT-2026-04-06-playout-ring-desync.md`

---

## Solution: Reader-Detects-Lap Architecture

The writer NEVER touches `read_pos`. On overflow, the writer simply overwrites old buffer data and advances `write_pos`. The reader detects it was lapped and self-corrects by snapping its own `read_pos` forward.

---

## Implementation Steps

### Step 1: Rewrite `AudioRing`

**File:** `crates/wzp-android/src/audio_ring.rs`

Replace the entire implementation with:

**Constants:**
```rust
/// Ring buffer capacity — must be a power of 2 for bitmask indexing.
/// 16384 samples = 341.3ms at 48kHz mono. Provides 70% more headroom
/// than the previous 9600 (200ms) for surviving Android GC pauses.
const RING_CAPACITY: usize = 16384; // 2^14
const RING_MASK: usize = RING_CAPACITY - 1;
```

**Struct:**
```rust
pub struct AudioRing {
    buf: Box<[i16; RING_CAPACITY]>,
    write_pos: AtomicUsize,     // monotonically increasing, ONLY written by producer
    read_pos: AtomicUsize,      // monotonically increasing, ONLY written by consumer
    overflow_count: AtomicU64,  // incremented by reader when it detects a lap
    underrun_count: AtomicU64,  // incremented by reader when ring is empty
}
```

**`write()` — producer. Does NOT touch `read_pos`:**
```rust
pub fn write(&self, samples: &[i16]) -> usize {
    let count = samples.len().min(RING_CAPACITY);
    let w = self.write_pos.load(Ordering::Relaxed);

    for i in 0..count {
        unsafe {
            let ptr = self.buf.as_ptr() as *mut i16;
            *ptr.add((w + i) & RING_MASK) = samples[i];
        }
    }

    self.write_pos.store(w.wrapping_add(count), Ordering::Release);
    count
}
```

**`read()` — consumer. Detects lap, self-corrects:**
```rust
pub fn read(&self, out: &mut [i16]) -> usize {
    let w = self.write_pos.load(Ordering::Acquire);
    let mut r = self.read_pos.load(Ordering::Relaxed);

    let mut avail = w.wrapping_sub(r);

    // Lap detection: writer has overwritten our unread data.
    // Snap read_pos forward to oldest valid data in the buffer.
    // Safe because we (the reader) are the sole owner of read_pos.
    if avail > RING_CAPACITY {
        r = w.wrapping_sub(RING_CAPACITY);
        avail = RING_CAPACITY;
        self.overflow_count.fetch_add(1, Ordering::Relaxed);
    }

    let count = out.len().min(avail);
    if count == 0 {
        if w == r {
            self.underrun_count.fetch_add(1, Ordering::Relaxed);
        }
        return 0;
    }

    for i in 0..count {
        out[i] = unsafe { *self.buf.as_ptr().add((r + i) & RING_MASK) };
    }

    self.read_pos.store(r.wrapping_add(count), Ordering::Release);
    count
}
```

**`available()` — clamped for external callers:**
```rust
pub fn available(&self) -> usize {
    let w = self.write_pos.load(Ordering::Acquire);
    let r = self.read_pos.load(Ordering::Relaxed);
    w.wrapping_sub(r).min(RING_CAPACITY)
}
```

**`free_space()` — keep for API compat:**
```rust
pub fn free_space(&self) -> usize {
    RING_CAPACITY.saturating_sub(self.available())
}
```

**Diagnostic accessors:**
```rust
pub fn overflow_count(&self) -> u64 {
    self.overflow_count.load(Ordering::Relaxed)
}

pub fn underrun_count(&self) -> u64 {
    self.underrun_count.load(Ordering::Relaxed)
}
```

**Constructor:**
```rust
pub fn new() -> Self {
    debug_assert!(RING_CAPACITY.is_power_of_two());
    Self {
        buf: Box::new([0i16; RING_CAPACITY]),
        write_pos: AtomicUsize::new(0),
        read_pos: AtomicUsize::new(0),
        overflow_count: AtomicU64::new(0),
        underrun_count: AtomicU64::new(0),
    }
}
```

**Imports to add:** `use std::sync::atomic::AtomicU64;`

**Safety comment update:**
```rust
// SAFETY: AudioRing is SPSC — one thread writes (producer), one reads (consumer).
// The producer only writes write_pos. The consumer only writes read_pos.
// Neither thread writes the other's cursor. Buffer indices are derived from
// the owning thread's cursor, ensuring no concurrent access to the same index.
```

---

### Step 2: Add counter fields to `CallStats`

**File:** `crates/wzp-android/src/stats.rs`

Add three fields to the `CallStats` struct (after `fec_recovered`):

```rust
/// Playout ring overflow count (reader was lapped by writer).
pub playout_overflows: u64,
/// Playout ring underrun count (reader found empty buffer).
pub playout_underruns: u64,
/// Capture ring overflow count.
pub capture_overflows: u64,
```

These derive `Default` (= 0) automatically via the existing `#[derive(Default)]`.

---

### Step 3: Wire ring diagnostics into engine stats + logging

**File:** `crates/wzp-android/src/engine.rs`

**3a.** In `get_stats()` (~line 181), populate the new fields:

```rust
stats.playout_overflows = self.state.playout_ring.overflow_count();
stats.playout_underruns = self.state.playout_ring.underrun_count();
stats.capture_overflows = self.state.capture_ring.overflow_count();
```

**3b.** In the recv task periodic stats log, add ring health:

```rust
info!(
    frames_decoded,
    fec_recovered,
    recv_errors,
    max_recv_gap_ms,
    playout_avail = state.playout_ring.available(),
    playout_overflows = state.playout_ring.overflow_count(),
    playout_underruns = state.playout_ring.underrun_count(),
    "recv stats"
);
```

**3c.** In the send task periodic stats log, add capture ring health:

```rust
info!(
    seq = s,
    block_id,
    frames_sent,
    frames_dropped,
    send_errors,
    ring_avail = state.capture_ring.available(),
    capture_overflows = state.capture_ring.overflow_count(),
    "send stats"
);
```

---

### Step 4: Parse new stats in Kotlin

**File:** `android/app/src/main/java/com/wzp/engine/CallStats.kt`

Add fields to the data class:

```kotlin
val playoutOverflows: Long = 0,
val playoutUnderruns: Long = 0,
val captureOverflows: Long = 0,
```

Add parsing in `fromJson()`:

```kotlin
playoutOverflows = obj.optLong("playout_overflows", 0),
playoutUnderruns = obj.optLong("playout_underruns", 0),
captureOverflows = obj.optLong("capture_overflows", 0),
```

No UI changes needed — these fields will appear in debug report JSON automatically.

---

### Step 5: Unit tests

**File:** `crates/wzp-android/src/audio_ring.rs` — add `#[cfg(test)] mod tests`

```rust
#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn capacity_is_power_of_two() {
        assert!(RING_CAPACITY.is_power_of_two());
    }

    #[test]
    fn basic_write_read() {
        let ring = AudioRing::new();
        let input: Vec<i16> = (0..960).map(|i| i as i16).collect();
        ring.write(&input);
        assert_eq!(ring.available(), 960);

        let mut output = vec![0i16; 960];
        let read = ring.read(&mut output);
        assert_eq!(read, 960);
        assert_eq!(output, input);
        assert_eq!(ring.available(), 0);
    }

    #[test]
    fn wraparound() {
        let ring = AudioRing::new();
        let frame = vec![42i16; 960];
        // Write enough to wrap the buffer multiple times
        for _ in 0..20 {
            ring.write(&frame);
            let mut out = vec![0i16; 960];
            ring.read(&mut out);
            assert!(out.iter().all(|&s| s == 42));
        }
    }

    #[test]
    fn overflow_detected_by_reader() {
        let ring = AudioRing::new();
        // Write more than RING_CAPACITY without reading
        let big = vec![7i16; RING_CAPACITY + 960];
        ring.write(&big[..RING_CAPACITY]);
        ring.write(&big[RING_CAPACITY..]);

        // Reader should detect lap
        let mut out = vec![0i16; 960];
        let read = ring.read(&mut out);
        assert!(read > 0);
        assert_eq!(ring.overflow_count(), 1);
        // Data should be from the most recent writes
        assert!(out.iter().all(|&s| s == 7));
    }

    #[test]
    fn writer_never_modifies_read_pos() {
        let ring = AudioRing::new();
        // Read pos should stay at 0 until read() is called
        let data = vec![1i16; RING_CAPACITY + 960];
        ring.write(&data);
        // read_pos is private, but we can check available() > CAPACITY
        // which proves write() didn't advance read_pos
        let w = ring.write_pos.load(std::sync::atomic::Ordering::Relaxed);
        let r = ring.read_pos.load(std::sync::atomic::Ordering::Relaxed);
        assert_eq!(r, 0, "write() must not modify read_pos");
        assert!(w.wrapping_sub(r) > RING_CAPACITY);
    }

    #[test]
    fn underrun_counted() {
        let ring = AudioRing::new();
        let mut out = vec![0i16; 960];
        let read = ring.read(&mut out);
        assert_eq!(read, 0);
        assert_eq!(ring.underrun_count(), 1);
    }

    #[test]
    fn overflow_recovery_reads_recent_data() {
        let ring = AudioRing::new();
        // Fill with old data
        let old = vec![1i16; RING_CAPACITY];
        ring.write(&old);
        // Overwrite with new data (lapping the reader)
        let new_data = vec![99i16; 960];
        ring.write(&new_data);

        // Reader should snap forward and get recent data
        let mut out = vec![0i16; RING_CAPACITY];
        let read = ring.read(&mut out);
        assert_eq!(read, RING_CAPACITY);
        // The last 960 samples should be 99
        assert!(out[RING_CAPACITY - 960..].iter().all(|&s| s == 99));
        assert_eq!(ring.overflow_count(), 1);
    }
}
```

---

## Memory Ordering Reference

| Operation | Ordering | Rationale |
|-----------|----------|-----------|
| `write_pos.store` in `write()` | Release | Buffer writes visible before cursor advances |
| `write_pos.load` in `read()` | Acquire | Pairs with Release above — sees all buffer writes |
| `write_pos.load` in `write()` | Relaxed | Writer is sole owner of write_pos |
| `read_pos.load` in `read()` | Relaxed | Reader is sole owner of read_pos |
| `read_pos.store` in `read()` | Release | Makes available() consistent from any thread |
| `read_pos.load` in `available()` | Relaxed | Informational only, slight staleness OK |
| All counters | Relaxed | Diagnostic only |

---

## Capacity Tradeoff

| Capacity | Duration | Memory | Verdict |
|----------|----------|--------|---------|
| 8192 (2^13) | 170ms | 16KB | Less than current 200ms — risky |
| **16384 (2^14)** | **341ms** | **32KB** | **70% more headroom, bitmask indexing** |
| 32768 (2^15) | 682ms | 64KB | Excessive latency on overflow recovery |

---

## Verification

1. `cargo test -p wzp-android` — new unit tests pass
2. `cargo ndk -t arm64-v8a build --release -p wzp-android` — ARM cross-compile succeeds
3. Build APK, install on both test devices (Nothing A059 + Pixel 6)
4. 2+ minute call — verify no audio gaps
5. Check debug report JSON: `playout_overflows` should be 0 or very small
6. Check logcat `wzp_android` tag: send/recv stats show healthy ring state
7. Stress test: play music through one device speaker while on call — forces high ring throughput

---

## Files to Modify

| File | What changes |
|------|-------------|
| `crates/wzp-android/src/audio_ring.rs` | Complete rewrite — the core fix |
| `crates/wzp-android/src/stats.rs` | Add 3 counter fields |
| `crates/wzp-android/src/engine.rs` | Wire counters into get_stats() + periodic logs |
| `android/app/src/main/java/com/wzp/engine/CallStats.kt` | Parse 3 new JSON fields |

## What Does NOT Change

- `AudioPipeline.kt` — calls `readAudio()`/`writeAudio()` unchanged; ring fix is transparent
- `jni_bridge.rs` — JNI bridge passes through unchanged
- `audio_android.rs` — separate Oboe-based ring, currently unused, different design
- Relay code — relay is confirmed healthy
- Desktop client — uses `Mutex + mpsc`, not `AudioRing`
