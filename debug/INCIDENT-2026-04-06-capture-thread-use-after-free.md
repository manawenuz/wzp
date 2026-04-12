# Incident Report: Native Crash in Capture Thread — Use-After-Free on Engine Handle

**Date:** 2026-04-06
**Severity:** Critical — app crash (SIGSEGV) on call hangup
**Status:** Root-caused, fix pending
**Affects:** Android client only

## Summary

The app crashes with a native SIGSEGV during or shortly after call hangup. The crash occurs in JIT-compiled code inside `AudioPipeline.runCapture()`. The root cause is a use-after-free: the capture thread calls `engine.writeAudio()` via JNI after the engine's native handle has been freed by `teardown()` on the ViewModel thread.

## Crash Stacktrace

```
04-06 13:05:42.707 F DEBUG: #09 pc 000000000250696c /memfd:jit-cache (deleted) (com.wzp.audio.AudioPipeline.runCapture+3228)
04-06 13:05:42.707 F DEBUG: #14 pc 0000000000005270 <anonymous:730900d000> (com.wzp.audio.AudioPipeline.start$lambda$0+0)
04-06 13:05:42.708 F DEBUG: #19 pc 00000000000044cc <anonymous:730900d000> (com.wzp.audio.AudioPipeline.$r8$lambda$0rYcivupwvyN4SgBXhsroKmTlo8+0)
04-06 13:05:42.708 F DEBUG: #24 pc 00000000000042e4 <anonymous:730900d000> (com.wzp.audio.AudioPipeline$$ExternalSyntheticLambda0.run+0)
```

This is a tombstone (signal crash), not a Java exception. The `F DEBUG` tag indicates a native crash handler (debuggerd) captured the signal.

## Root Cause

### The Race Condition

Two threads operate on the engine concurrently without synchronization:

**Thread 1: `wzp-capture` (AudioRecord thread, MAX_PRIORITY)**
```kotlin
// AudioPipeline.runCapture() — runs in a tight loop
while (running) {
    val read = recorder.read(pcm, 0, FRAME_SAMPLES)
    if (read > 0) {
        engine.writeAudio(pcm)  // <-- JNI call to native engine
    }
}
```

**Thread 2: ViewModel/UI thread (normal priority)**
```kotlin
// CallViewModel.teardown()
stopAudio()                     // sets AudioPipeline.running = false
engine?.stopCall()              // tells Rust to stop
engine?.destroy()               // frees native memory, sets nativeHandle = 0L
engine = null
```

### The Kotlin Guard is Insufficient

`WzpEngine.writeAudio()` has a guard:
```kotlin
fun writeAudio(pcm: ShortArray): Int {
    if (nativeHandle == 0L) return 0      // check
    return nativeWriteAudio(nativeHandle, pcm)  // use
}
```

This is a **TOCTOU (time-of-check/time-of-use) race**:
1. Capture thread checks `nativeHandle != 0L` → true
2. ViewModel thread calls `destroy()`, which calls `nativeDestroy(handle)` then sets `nativeHandle = 0L`
3. Capture thread calls `nativeWriteAudio(handle, pcm)` with the now-freed handle
4. The JNI function dereferences `handle` as a pointer → **SIGSEGV**

The same race exists for `readAudio()` on the `wzp-playout` thread.

### Why `stopAudio()` Doesn't Prevent This

`AudioPipeline.stop()` sets `running = false` but does **NOT join or wait** for the threads:
```kotlin
fun stop() {
    running = false
    // Don't join — threads are parked as daemons to avoid native TLS crash
    captureThread = null
    playoutThread = null
}
```

The threads are intentionally not joined because of a separate bug: exiting a JNI-calling thread triggers a `SIGSEGV in OPENSSL_free` due to libcrypto TLS destructors on Android. The threads instead "park" with `Thread.sleep(Long.MAX_VALUE)` after the loop exits.

But the problem is the **window between `running = false` and the thread actually checking it**. The capture thread may be blocked in `recorder.read()` (which blocks for 20ms per frame) or in the middle of `engine.writeAudio()` when `destroy()` is called.

### Timeline of the Crash

```
T=0ms    ViewModel: stopAudio() → sets running=false
T=0ms    ViewModel: stopStatsPolling()
T=0ms    ViewModel: engine.stopCall()    — Rust stops internal tasks
T=1ms    ViewModel: engine.destroy()     — frees native memory
         ↑ nativeHandle = 0L

T=0-20ms Capture thread: still in recorder.read() or writeAudio()
         → if in writeAudio(), the nativeHandle check passed BEFORE destroy()
         → JNI dereferences freed pointer → SIGSEGV
```

## Affected Code

### Files with the race

| File | Line(s) | Issue |
|------|---------|-------|
| `android/.../WzpEngine.kt` | 107-108, 116-117 | TOCTOU on `nativeHandle` in `writeAudio()` / `readAudio()` |
| `android/.../CallViewModel.kt` | 257-262 | `stopAudio()` + `destroy()` without waiting for audio threads to quiesce |
| `android/.../AudioPipeline.kt` | 80-82 | `stop()` doesn't synchronize with running threads |

### Files with the thread parking workaround

| File | Line(s) | Context |
|------|---------|---------|
| `android/.../AudioPipeline.kt` | 57-58, 69-70 | Threads parked after loop exit to avoid libcrypto TLS crash |
| `android/.../AudioPipeline.kt` | 96-101 | `parkThread()` — `Thread.sleep(Long.MAX_VALUE)` |

## Constraints for the Fix

1. **Cannot join audio threads** — joining triggers a separate SIGSEGV in `OPENSSL_free` when the thread's TLS destructors fire (documented in `AudioPipeline.kt` comments). The parking workaround must be preserved.

2. **Must guarantee no JNI calls after `destroy()`** — the native handle is a raw pointer; any dereference after free is undefined behavior.

3. **Must not add blocking waits on the UI thread** — `teardown()` runs on the ViewModel thread which must remain responsive.

4. **The `@Volatile running` flag is necessary but not sufficient** — it prevents new loop iterations but doesn't help with in-flight JNI calls.

5. **Both `writeAudio` and `readAudio` have the same race** — the fix must cover both the capture and playout paths.

## Reproduction

The crash is timing-dependent. It's most likely to occur when:
- The capture thread is in the middle of a `writeAudio()` JNI call when `destroy()` is called
- More likely on slower devices or under CPU pressure (GC, thermal throttling)
- Can happen on every hangup, but only crashes ~10-30% of the time due to the timing window

## Analysis of Possible Fix Approaches

### Approach A: Add a synchronization gate in the JNI bridge

Use a `ReentrantReadWriteLock` or `AtomicBoolean` in `WzpEngine.kt`:
- Audio threads acquire a read lock / check the flag before JNI calls
- `destroy()` acquires a write lock / sets the flag and waits for in-flight calls to drain

**Pro:** Clean, solves the race directly.
**Con:** Adding a lock to the audio hot path (every 20ms). `ReentrantReadWriteLock` is not lock-free. However, the read-lock path is uncontended 99.99% of the time (write-lock only during destroy), so contention is negligible.

### Approach B: Defer `destroy()` until audio threads have stopped

Instead of calling `destroy()` in `teardown()`, set a flag and have the audio threads call `destroy()` after they exit the loop (before parking).

**Pro:** No locks on hot path.
**Con:** Complex lifecycle — which thread calls destroy? What if both threads race to destroy? Need a `CountDownLatch` or similar.

### Approach C: Make the JNI handle atomically invalidated

Use `AtomicLong` for `nativeHandle` and use `compareAndExchange` in `destroy()` + `getAndCheck` pattern in audio calls.

**Pro:** Lock-free.
**Con:** Still has a TOCTOU window — the thread can load the handle, then it gets CAS'd to 0, then the thread uses the stale handle. Doesn't fully solve the race without combining with a reference count or epoch.

### Approach D: Introduce a destroy latch

Add a `CountDownLatch(1)` that audio threads wait on before parking. `teardown()` sets `running=false`, then `await`s the latch (with timeout), then calls `destroy()`. Each audio thread counts down the latch after exiting the loop.

Actually this needs a `CountDownLatch(2)` — one for each thread (capture + playout).

**Pro:** Guarantees no in-flight JNI calls at destroy time. No locks on hot path.
**Con:** `teardown()` blocks for up to one frame duration (~20ms) waiting for threads to exit their loops. Acceptable for a hangup path.

### Recommendation

**Approach D (destroy latch)** is the cleanest. The 20ms worst-case wait is imperceptible on the hangup path, and it provides a hard guarantee that no JNI calls are in flight when `destroy()` runs. Combined with the existing `running` volatile flag, the audio threads exit their loops within one frame and count down the latch.

If the latch times out (e.g., AudioRecord.read() is stuck), `destroy()` proceeds anyway — the `panic::catch_unwind` in the JNI bridge will catch the invalid access as a panic rather than a SIGSEGV (though this is best-effort; a true SIGSEGV from freed memory is not catchable).

## Data Files

The crash was captured from the Nothing A059 device at 13:05:42 on 2026-04-06. The tombstone is in the device's `/data/tombstones/` directory. The logcat output shows the crash frames.
