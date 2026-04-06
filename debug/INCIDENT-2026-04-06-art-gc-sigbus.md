# Incident Report: SIGBUS in ART GC During Audio Thread JNI Calls

**Date:** 2026-04-06
**Severity:** High — app crash (SIGBUS) mid-call
**Status:** Root-caused, fix proposed
**Affects:** Android 16 (API 36) devices with concurrent mark-compact GC

## Summary

The app crashes with SIGBUS (signal 7, BUS_ADRERR) during an active call. The crash occurs in ART's garbage collector or JIT compiler, NOT in our Rust native code or AudioRing buffer. Both `wzp-capture` and `wzp-playout` Kotlin threads are affected.

## Crash Details

### Crash 1: wzp-capture (18:42, after 476s of call)

```
Fatal signal 7 (SIGBUS), code 2 (BUS_ADRERR), fault addr 0x720009be38
tid 19697 (wzp-capture), pid 17885 (com.wzp.phone)
```

**Backtrace:**
```
#00 art::StackVisitor::WalkStack
#01 art::Thread::VisitRoots
#02 art::gc::collector::MarkCompact::ThreadFlipVisitor::Run
#03 art::Thread::EnsureFlipFunctionStarted
#04 CheckJNI::ReleasePrimitiveArrayElements  ← JNI boundary
#05 android_media_AudioRecord_readInArray     ← AudioRecord.read()
#09 com.wzp.audio.AudioPipeline.runCapture
```

**Root cause:** ART's concurrent mark-compact GC (`MarkCompact::ThreadFlipVisitor`) is flipping thread roots while the capture thread is in the middle of a JNI call (`AudioRecord.read()`). The GC's `EnsureFlipFunctionStarted` triggers a stack walk that hits an invalid address.

### Crash 2: wzp-playout (19:17, mid-call)

```
Fatal signal 7 (SIGBUS), code 2 (BUS_ADRERR), fault addr 0x225eb98
tid 32574 (wzp-playout), pid 32479 (com.wzp.phone)
```

**Backtrace:**
```
#00 com.wzp.audio.AudioPipeline.runPlayout  ← JIT-compiled code
#01 art_quick_osr_stub                      ← On-Stack Replacement
#02 art::jit::Jit::MaybeDoOnStackReplacement
#03-#04 art::interpreter::ExecuteSwitchImplCpp
```

**Root cause:** ART's JIT compiler performed On-Stack Replacement (OSR) on the hot playout loop. The OSR stub references a code address (`0x225eb98`) that is no longer valid — likely because the GC moved the compiled code in memory during concurrent compaction.

## Why This Happens

Android 16 introduced a new **concurrent mark-compact GC** (CMC) that moves objects in memory while other threads are running. This is safe for normal Java code because ART uses read barriers. But our audio threads have specific properties that stress this:

1. **`Thread.MAX_PRIORITY`** — audio threads run at the highest priority, starving the GC thread of CPU time. The GC may not complete its thread-flip before the audio thread resumes.

2. **Tight JNI loops** — `runCapture()` and `runPlayout()` loop every 20ms calling `AudioRecord.read()` / `AudioTrack.write()` via JNI. Each JNI transition is a GC safepoint, but the thread spends most of its time in native code where the GC can't flip it.

3. **Long-running JIT-compiled code** — the hot loop gets JIT-compiled and may undergo OSR. If the GC compacts memory while OSR is in progress, the stub can reference stale addresses.

4. **Daemon threads that never exit** — our threads are parked with `Thread.sleep(Long.MAX_VALUE)` after the call ends (to avoid the libcrypto TLS destructor crash). These zombie threads accumulate GC root scan work.

## Evidence This Is Not Our Bug

| Component | Evidence |
|-----------|---------|
| **AudioRing** | Not in any backtrace. All crash frames are in `libart.so` (ART runtime) |
| **Rust native code** | `libwzp_android.so` not in any crash frame |
| **JNI bridge** | Crash happens during `ReleasePrimitiveArrayElements` (ART internal), not during our JNI calls |
| **Timing** | Crashes after 476s and mid-call — not during init or teardown |

## Proposed Fix

### Option A: Disable concurrent GC compaction for audio threads (recommended)

Use `dalvik.vm.gctype` or per-thread GC pinning to prevent the mark-compact collector from moving objects referenced by audio threads.

**Not directly controllable from app code.** But we can reduce GC pressure:

### Option B: Reduce JNI transitions in audio threads

Instead of calling `engine.writeAudio(pcm)` / `engine.readAudio(pcm)` via JNI on every 20ms frame, batch multiple frames or use `DirectByteBuffer` to share memory without JNI array copies.

**Implementation:**
- Allocate a `DirectByteBuffer` in Kotlin, share the pointer with Rust via JNI
- Audio threads write/read directly to the buffer (no JNI call per frame)
- Rust reads/writes from the same memory region
- Reduces JNI transitions from 100/sec to 0/sec per audio direction

### Option C: Use Android's Oboe (AAudio) natively from Rust

Skip the Kotlin AudioRecord/AudioTrack entirely. Use Oboe (which we already have as a dependency in `wzp-android/Cargo.toml`) to create native audio streams directly from Rust. The audio callbacks run in native code with no JNI, no GC interaction, no ART.

This is how the project was originally designed (see `audio_android.rs` with Oboe references) before switching to Kotlin AudioRecord for simplicity.

**Pros:** Eliminates the entire JNI audio path. No GC interaction. Lower latency.
**Cons:** Requires rewriting `AudioPipeline.kt` into Rust. Oboe setup is more complex.

### Option D: Pin audio thread objects to prevent GC movement

Use JNI `GetPrimitiveArrayCritical` instead of `GetShortArrayRegion` to pin the array in memory during the operation. This prevents the GC from moving the array while we're using it.

**Implementation:** Change `nativeWriteAudio` / `nativeReadAudio` JNI functions to use critical sections.

### Recommendation

**Short term: Option B** (DirectByteBuffer) — reduces JNI transitions without major refactoring.

**Long term: Option C** (Oboe from Rust) — eliminates the problem entirely. This is the architecturally correct solution and matches the original design intent.

## Data Files

- Logcat from Nothing A059 (Android 16, API 36)
- Two crashes in the same session: 18:42 (capture, after 476s) and 19:17 (playout)
- Both SIGBUS/BUS_ADRERR, both in ART internal frames
