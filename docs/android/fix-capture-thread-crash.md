# Fix: Capture/Playout Thread Use-After-Free on Hangup

## Problem

App crashes (SIGSEGV) when hanging up a call. The capture thread (`wzp-capture`) calls `engine.writeAudio()` via JNI after `teardown()` has freed the native engine handle. Same race exists for the playout thread's `readAudio()`.

**Root cause:** TOCTOU race between the `nativeHandle == 0L` check in `WzpEngine.writeAudio()`/`readAudio()` and `destroy()` freeing the native memory on the ViewModel thread. Audio threads can't be joined (libcrypto TLS destructor crash), so there's no synchronization between `stopAudio()` and `destroy()`.

**Full forensics:** `debug/INCIDENT-2026-04-06-capture-thread-use-after-free.md`

---

## Solution: Destroy Latch

Add a `CountDownLatch(2)` that both audio threads count down after exiting their loops. `teardown()` awaits the latch (with timeout) before calling `destroy()`, guaranteeing no in-flight JNI calls.

---

## Implementation Steps

### Step 1: Add a drain latch to `AudioPipeline`

**File:** `android/app/src/main/java/com/wzp/audio/AudioPipeline.kt`

Add a `CountDownLatch` field:

```kotlin
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit

class AudioPipeline(private val context: Context) {
    // ... existing fields ...

    /** Latch counted down by each audio thread after exiting its loop.
     *  stop() does NOT wait on this — teardown waits via awaitDrain(). */
    private var drainLatch: CountDownLatch? = null
```

In `start()`, create the latch before spawning threads:

```kotlin
fun start(engine: WzpEngine) {
    if (running) return
    running = true
    drainLatch = CountDownLatch(2)  // one for capture, one for playout

    captureThread = Thread({
        runCapture(engine)
        drainLatch?.countDown()  // signal: capture loop exited
        parkThread()
    }, "wzp-capture").apply { ... }

    playoutThread = Thread({
        runPlayout(engine)
        drainLatch?.countDown()  // signal: playout loop exited
        parkThread()
    }, "wzp-playout").apply { ... }
    // ...
}
```

Add `awaitDrain()` — called by ViewModel before `destroy()`:

```kotlin
/** Block until both audio threads have exited their loops (max 200ms).
 *  After this returns, no more JNI calls to the engine will be made. */
fun awaitDrain(): Boolean {
    return drainLatch?.await(200, TimeUnit.MILLISECONDS) ?: true
}
```

`stop()` remains unchanged (non-blocking, sets `running = false`).

### Step 2: Update `CallViewModel.teardown()` to await drain

**File:** `android/app/src/main/java/com/wzp/ui/call/CallViewModel.kt`

Change teardown to wait for audio threads before destroying:

```kotlin
private fun teardown(stopService: Boolean = true) {
    Log.i(TAG, "teardown: stopping audio, stopService=$stopService")
    val hadCall = audioStarted
    CallService.onStopFromNotification = null
    stopAudio()             // sets running=false (non-blocking)
    stopStatsPolling()

    // Wait for audio threads to exit their loops before destroying the engine.
    // This guarantees no in-flight JNI calls to writeAudio/readAudio.
    val drained = audioPipeline?.awaitDrain() ?: true
    if (!drained) {
        Log.w(TAG, "teardown: audio threads did not drain in time")
    }
    audioPipeline = null

    Log.i(TAG, "teardown: stopping engine")
    try { engine?.stopCall() } catch (e: Exception) { Log.w(TAG, "stopCall err: $e") }
    try { engine?.destroy() } catch (e: Exception) { Log.w(TAG, "destroy err: $e") }
    engine = null
    engineInitialized = false
    // ... rest unchanged
}
```

**Key change:** `awaitDrain()` is called AFTER `stopAudio()` (which sets `running=false`) but BEFORE `engine?.destroy()`. The latch guarantees both threads have exited their `while(running)` loops and will never call `writeAudio`/`readAudio` again.

Also move `audioPipeline = null` to after `awaitDrain()` to keep the reference alive for the latch call.

### Step 3: Move `stopAudio()` pipeline nulling

**File:** `android/app/src/main/java/com/wzp/ui/call/CallViewModel.kt`

In `stopAudio()`, do NOT null out the pipeline — let `teardown()` handle it after drain:

```kotlin
private fun stopAudio() {
    if (!audioStarted) return
    audioPipeline?.stop()    // sets running=false
    // DON'T null audioPipeline here — teardown() needs it for awaitDrain()
    audioRouteManager?.unregister()
    audioRouteManager?.setSpeaker(false)
    _isSpeaker.value = false
    audioStarted = false
}
```

---

## Files to Modify

| File | What changes |
|------|-------------|
| `android/.../audio/AudioPipeline.kt` | Add `CountDownLatch`, `countDown()` in threads, `awaitDrain()` method |
| `android/.../ui/call/CallViewModel.kt` | `teardown()` calls `awaitDrain()` before `destroy()`; `stopAudio()` doesn't null pipeline |

## What Does NOT Change

- `WzpEngine.kt` — the `nativeHandle == 0L` guard stays as defense-in-depth
- `jni_bridge.rs` — `panic::catch_unwind` stays as last resort
- `AudioPipeline.stop()` — remains non-blocking
- Thread parking — still needed to avoid libcrypto TLS crash

## Verification

1. Build APK, install on test device
2. Make a call, hang up — verify no crash in logcat (`adb logcat -s AndroidRuntime:E DEBUG:F`)
3. Rapid call/hangup/call/hangup cycles — stress the teardown path
4. Check logcat for `teardown: audio threads did not drain in time` — should never appear under normal conditions
5. Verify debug report still works after hangup (latch doesn't interfere with report collection)
