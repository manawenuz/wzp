# BUG-001: Android "Connecting…" Hangs / Join Voice Never Completes

**Severity:** P0 — renders the app non-functional for room joins on a fresh install
**Status:** Partially mitigated (5a13f12), narrowed by static review; Android repro/logcat still needed
**Branch:** `experimental-ui`
**Last investigated:** 2026-05-25
**Device confirmed affected:** Nothing Phone A059 (Android 15)

---

## Symptom

User taps "Join Voice". Button changes to "Connecting…" and stays there indefinitely. No error toast, no drawer, no progress. The only recovery is force-quitting the app.

## 2026-05-25 Static Review Update

The exact indefinite "Connecting…" symptom most likely came from an APK older than `5a13f12`, because current `desktop/src/main.ts` has a 15s JS-side timeout for manual room joins. The current branch can still produce closely related failures:

1. Native Oboe start can report false success when Android leaves capture/playout in `Starting` for 2s. That manifests as "joined but silent/dead audio", not a true JS hang.
2. First-run microphone permission can still race the first `openStream(Direction::Input)`, especially when the user joins immediately after granting permission.
3. Direct-call auto-connect did not have the 15s JS timeout even after `5a13f12`.
4. Toasts used `${e}`, so object-shaped Tauri errors could appear as `[object Object]`.

Working-tree diagnostic changes applied during this investigation:

- `crates/wzp-native/cpp/oboe_bridge.cpp`: return `-6` if both streams do not reach `Started` before the 2s poll deadline. This turns Oboe false-success into a visible Rust/JS error.
- `desktop/src/main.ts`: shared `connectWithTimeout()` for room joins and direct-call auto-connect; shared `errorMessage()` for useful toast text.

Next Android repro should distinguish:

| Toast / log | Meaning |
|---|---|
| `Join failed: wzp_native_audio_start failed: code -2` | mic permission / capture open failure |
| `Join failed: wzp_native_audio_start failed: code -6` | Oboe streams opened/requested start, but HAL never transitioned both to `Started` |
| `Join failed: connect timed out (15s) - check audio permissions` | Tauri command did not resolve to JS; collect Rust/Tauri logs around `connect:call_engine_starting` |

---

## Root Cause Chain

The `invoke("connect")` Tauri command runs the full `CallEngine::start` coroutine on Android. Execution order:

1. Parse relay address → QUIC dial → crypto handshake (~200ms, works — relay logs confirm room join succeeds)
2. `audio_stop()` (no-op on first launch)
3. `tokio::time::sleep(50ms)`
4. `set_audio_mode_communication()` (JNI into Kotlin)
5. **`tokio::task::spawn_blocking(crate::wzp_native::audio_start)`** ← primary hang point

`audio_start` calls `wzp_oboe_start()` (C++ FFI in `crates/wzp-native/cpp/oboe_bridge.cpp`), which:
- Opens capture stream (`captureBuilder.openStream`)
- Opens playout stream (`playoutBuilder.openStream`)
- `g_capture_stream->requestStart()`
- `g_playout_stream->requestStart()`
- **Polls up to 2 seconds** in a `std::this_thread::sleep_for(10ms)` busy-wait loop waiting for both streams to reach `Started` state (`oboe_bridge.cpp:404–423`)

Before the working-tree `-6` diagnostic change, if the HAL never transitioned to `Started`, `wzp_oboe_start` returned 0 (success!) after the 2s timeout even though streams were not functional. Rust saw `ret == 0`, considered it success, and `CallEngine::start` returned `Ok`.

The `invoke("connect")` promise resolves successfully, `enterVoice(false)` is called, the voice drawer appears — but audio streams are dead. The send task reads silence, the playout ring never drains.

**However**, relay log evidence shows the connection is established and then dropped 166ms later with `forwarded=0`, which means `CallEngine::start` did return to the `connect` command. If the user still sees "Connecting…" at that point, the JS `await connectRace` is not resolving — suggesting either the Rust command returned an error (which should show as a toast) or the `invoke` promise is hanging for a different reason.

---

## Evidence

**Relay log (pangolin, session at 06:40:04 UTC):**
```
room "general" join accepted
crypto handshake complete  t=+184ms
connection dropped          t=+350ms  forwarded=0
```

The relay sees a clean connection that self-terminates in ~350ms total. `forwarded=0` means no media was exchanged. Consistent with audio_start failing or the call task throwing before media loops start.

**Four rapid connects at 06:40:04** in the relay log suggest multiple taps (no `connectPending` guard in the APK installed at that time, or user was on an older build).

---

## Fixes Applied in `5a13f12`

| # | Problem | Fix | File |
|---|---------|-----|------|
| 1 | `wzp_oboe_start` called directly on tokio worker thread → froze entire runtime including timeouts | Changed to `spawn_blocking` | `desktop/src-tauri/src/engine.rs:609` |
| 2 | No JS-side timeout → "Connecting…" hangs forever if Rust never returns | Added 15s `Promise.race` | `desktop/src/main.ts:338` |
| 3 | No error feedback to user | Added `showToast()` in `catch` block | `desktop/src/main.ts:352` |
| 4 | Button disappeared on click | Changed to `disabled + "Connecting…"` text | `desktop/src/main.ts:335` |
| 5 | Handshake could hang forever waiting for `CallAnswer` | Added 10s `tokio::time::timeout` | `crates/wzp-client/src/handshake.rs:105` |

---

## Open Issues (Not Yet Fixed)

### Issue A: `g_running` flag race between `audio_stop` and `audio_start`

**Current status:** likely fixed in current branch. `crates/wzp-native/cpp/oboe_bridge.cpp:430` now clears `g_running` at the top of `wzp_oboe_stop`.

`oboe_bridge.cpp:244` checks `g_running.load()` at entry to `wzp_oboe_start`. The engine calls `audio_stop()` then waits 50ms then calls `audio_start()`. If `wzp_oboe_stop` does not synchronously clear `g_running` before returning, the next `wzp_oboe_start` sees `g_running == true` and returns `-1` immediately (line 246–247).

With `5a13f12`, Rust now propagates this as `"wzp_native_audio_start failed: code -1"` → toast. Confirm via logcat.

### Issue B: Mic permission granted at runtime causes audio HAL delay

After clearing app data, Android prompts for mic permission. The OS grants it but the audio HAL may not immediately honor it. The first `openStream(Direction::Input)` within ~1s of permission grant can fail with `ErrorPermissionDenied` → Oboe returns `-2`.

With `5a13f12` this should surface as toast: `"Join failed: wzp_native_audio_start failed: code -2"`.

### Issue C: `wzp_oboe_start` 2s poll timeout returns 0 (false success)

`oboe_bridge.cpp:404–423`: if streams don't reach `Started` state within 2s, the poll loop exits with no error — `wzp_oboe_start` returns 0. Rust treats this as success. The drawer appears but audio is dead. This is the "joined but silent" failure mode, distinct from "stuck on Connecting…".

**Fix:** return a distinct error code (e.g. `-6`) from `wzp_oboe_start` when the poll times out without both streams reaching `Started`.

**Working-tree status:** implemented as `-6`; needs Android NDK/device validation.

### Issue D: Error object serialization in JS toast

The `connect` command returns `Result<String, String>`. Tauri wraps the `Err` as a JS exception. If `e` in the `catch` block is a Tauri error object rather than a plain string, `${e}` renders as `"[object Object]"`. Should use `e?.message ?? String(e)` for robust stringification.

**Working-tree status:** implemented via `errorMessage(e)`.

---

## `wzp_oboe_start` Return Codes Reference

| Code | Meaning |
|------|---------|
| 0 | Success |
| -1 | Already running (`g_running == true` at entry) |
| -2 | `captureBuilder.openStream` failed |
| -3 | `playoutBuilder.openStream` failed |
| -4 | `g_capture_stream->requestStart()` failed |
| -5 | `g_playout_stream->requestStart()` failed |
| -6 | streams failed to reach `Started` before poll timeout |

---

## Reproduction Steps

1. Fresh install (or clear app data) on Nothing Phone A059
2. Grant microphone permission when prompted
3. Configure relay `193.180.213.68:4433`, room `general`
4. Tap "Join Voice"
5. Observe: button shows "Connecting…" indefinitely

---

## Diagnostic Steps

We have never captured `adb logcat` from a failing connect. This is the single highest-value diagnostic:

```bash
adb logcat -s "wzp-native" "wzp-desktop" "RustStd" | grep -E "audio|oboe|start|handshake|connect"
```

Key log lines to look for:

| Log line | Diagnosis |
|----------|-----------|
| `wzp_oboe_start: already running` | Issue A — g_running not cleared |
| `Failed to open capture stream: ErrorPermissionDenied` | Issue B — mic permission delay |
| `Failed to start capture` / `Failed to start playout` | Oboe HAL error, code -4 or -5 |
| `both streams Started after N polls` | audio_start succeeded |
| `audio_start task panic` | spawn_blocking panic (shouldn't happen) |
| `wzp_native_audio_start failed: code X` | Rust caught it, toast should be visible |

Alternatively: enable **Call debug logs** in Settings, reproduce, use the share button to extract logs without USB.

---

## Proposed Fixes (Prioritized)

1. **Validate `-6` from `wzp_oboe_start` on poll timeout** on Android builder/device — eliminates silent false-success
2. **Add mic permission pre-check** in Kotlin before calling into Rust — surface a cleaner error if permission is not yet effective
3. **If `-6` reproduces on Nothing A059, test startup sequencing:** request/start capture before `MODE_IN_COMMUNICATION`, add a short post-permission delay, or retry once after a full `wzp_oboe_stop`

---

## Related Files

- `crates/wzp-native/cpp/oboe_bridge.cpp` — `wzp_oboe_start` implementation
- `crates/wzp-native/src/lib.rs:238` — `audio_start_inner` (Rust FFI wrapper)
- `desktop/src-tauri/src/engine.rs:576–635` — `CallEngine::start` audio section
- `desktop/src/main.ts:328–360` — `joinVoiceBtn` click handler
- `crates/wzp-client/src/handshake.rs:105` — handshake timeout
