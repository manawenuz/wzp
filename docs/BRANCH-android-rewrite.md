# Branch: `android-rewrite`

Pivot away from the legacy Kotlin + JNI Android client to a pure-Rust **Tauri 2.x Mobile** app that shares the same frontend and backend code as the desktop client.

## Why this branch exists

The Kotlin + JNI stack was a crash factory. Every failure mode we hit was at the Kotlin ↔ Rust boundary, and each fix uncovered the next layer of the onion:

| Symptom | Root cause | Fix |
|---|---|---|
| App crashed on launch before `onCreate` returned | `__init_tcb` / `pthread_create` bionic private symbols leaking out of `libwzp_android.so` because the Rust crate used `crate-type = ["cdylib", "staticlib"]`. rust-lang/rust#104707 documents that staticlib alongside cdylib leaks non-exported symbols from the staticlib into the cdylib, and Bionic's private internal pthread symbols got bound LOCALLY inside our `.so` instead of resolved against `libc.so` at `dlopen` time | Dropped `staticlib` from the crate-type list. `crate-type = ["cdylib", "rlib"]` only. |
| Stack overflow on `place_call` | `Dispatchers.IO` threads have a ~512 KB stack, too small for the Rust signal-connect path that does TLS handshake + quinn setup inside one closure | Launched JNI calls from a dedicated `java.lang.Thread` with an explicit 8 MB stack |
| `ring` / `libcrypto` TLS reuse crash on second call | tokio runtime got dropped between calls, but `ring` keeps a TLS-stored SSL context that is invalidated when the runtime thread is reused by a new runtime — `ring` sees stale context and segfaults | Single long-lived tokio runtime for the entire signal client lifetime; split `start()` into an inline `connect+register` path and a `run()` path on a separate thread to avoid the `thread::spawn` closure's stack overflow |
| Null dereference on register with fresh install | Identity seed file empty when it existed-but-was-blank, Rust side deref'd the zero-length slice | Generate seed if empty on register |

Every fix kept the app limping along but the fundamental design problem remained: **state management was split across a Kotlin ViewModel and a Rust engine, with a hand-rolled JNI bridge in between that had to be perfect to not crash**. The working desktop Tauri client (with the same Rust backend) had none of these problems because it spoke to the Rust code via in-process `invoke()` from a WebView, not JNI.

So: rewrite the Android app as a **Tauri 2.x Mobile app**, reusing the entire desktop codebase verbatim (`main.ts`, `style.css`, `index.html`, `main.rs`, `engine.rs` — everything). Tauri Mobile added Android support in v2, it's production-ready, and it eliminates the JNI boundary entirely.

The incident postmortem lives at [`docs/incident-tauri-android-init-tcb.md`](incident-tauri-android-init-tcb.md).

## Architecture

```
┌─────────────────────────────────────────────────┐
│                Tauri 2.x Mobile                  │
│                                                  │
│  Android WebView  ──────────  HTML/JS/CSS       │  ← Shared with desktop
│         │                      (main.ts)         │
│         │                                        │
│    invoke() ─────────────── Rust Commands       │  ← Shared with desktop
│                             (main.rs)            │
│                               │                  │
│              ┌───────────────┼────────────┐     │
│              │               │            │     │
│         SignalMgr        CallEngine   Identity  │  ← Shared crates
│        (signal_hub)     (wzp-client) (wzp-crypto)│
│              │               │                  │
│              │               │                  │
│              ▼               ▼                  │
│        QUIC to relay    Oboe audio (Android)    │
│                         via wzp-native cdylib   │
└─────────────────────────────────────────────────┘
```

**What is reused from desktop verbatim** (zero rewrite):

- `desktop/src/main.ts` — entire frontend
- `desktop/src/style.css` — all styling
- `desktop/src/identicon.ts` — identicon rendering
- `desktop/index.html` — HTML structure
- `desktop/src-tauri/src/main.rs` — all Tauri commands (`connect`, `disconnect`, `register_signal`, `place_call`, …)
- `desktop/src-tauri/src/engine.rs` — `CallEngine` wrapper

**What is Android-specific**:

- `desktop/src-tauri/src/android_audio.rs` — JVM-side audio routing (`AudioManager.setSpeakerphoneOn` for earpiece/speaker toggle). Runs from Tauri's existing JNI context — no hand-rolled bridge, Tauri owns the JVM hookup.
- `desktop/src-tauri/src/wzp_native.rs` — runtime `dlopen` of `libwzp_native.so`, a standalone cdylib crate (`crates/wzp-native`) that owns all C++ (Oboe bridge). Kept in its own crate so its C/C++ static archives never get statically linked into `wzp-desktop`'s `.so`, which would re-trigger the `__init_tcb` / pthread leak.
- `crates/wzp-native/` — the standalone C++/Oboe bridge cdylib. Loaded via `libloading` at runtime from `wzp_native.rs`. Provides capture + playout streams using Oboe's `Usage::VoiceCommunication` + `MODE_IN_COMMUNICATION` combo.
- Android-specific target dependencies in `desktop/src-tauri/Cargo.toml` (`jni`, `ndk-context`, `libloading`) — no CPAL, no VPIO.

## Key architectural decisions

### 1. `wzp-native` as a standalone cdylib loaded via `libloading`

The alternative — linking `wzp-native` as a regular Rust dep with C++ static archives — would cause the same `__init_tcb` crash that killed the Kotlin version. By making `wzp-native` its own cdylib and `dlopen`-ing it at runtime, Bionic's `libc.so` resolves every symbol at load time the way it's supposed to, and no private TCB symbols leak.

### 2. `crate-type = ["cdylib", "rlib"]` only (no `staticlib`)

Same reason. The `rlib` output is needed so the `wzp-desktop` binary target can link against the library; `cdylib` is needed for Android's `System.loadLibrary`; `staticlib` would reintroduce the symbol-leak bug.

### 3. Oboe audio config

`Usage::VoiceCommunication` + Java-side `MODE_IN_COMMUNICATION`. **Never** call `setAudioApi(AAudio)` explicitly — on some devices (Nothing Phone in particular) it causes Oboe to open the wrong stream type and audio goes silent. Let Oboe pick the audio API automatically. This is documented in the auto-memory `project_tauri_android_audio.md`.

### 4. Speaker/earpiece toggle uses `tokio::task::spawn_blocking`

Oboe's `stop()` + `start()` cycle is synchronous and can block for 50–200 ms. Calling it on the tokio executor stalls every other async task (including the QUIC datagram loop), dropping audio packets. Wrapping the toggle in `spawn_blocking` isolates it to a dedicated thread pool. Fixed in commit `76a4c53`.

## Build pipeline

Docker on SepehrHomeserverdk, same pattern as the Android legacy pipeline and the Windows pipeline:

```
./scripts/build-tauri-android.sh         # Full: pull + build + ntfy + rustypaste
./scripts/build-tauri-android.sh --pull  # Explicit git pull (default)
./scripts/build-tauri-android.sh --clean # Blow away the Rust target cache
```

**Image**: `wzp-android-builder` (shared with the legacy Kotlin pipeline). The Dockerfile was extended to install Node.js 20 LTS, Android API level 36, build-tools 35.0.0, tauri-cli 2.x, and all four Android Rust targets on top of the legacy NDK 26.1 + cargo-ndk + Gradle setup. Both pipelines coexist in the same image.

**Output**: `wzp-release.apk` uploaded to rustypaste, URL delivered via `ntfy.sh/wzp`.

## Known quirks (Tauri Mobile specific)

1. **tauri-cli `android init` writes absolute paths** into `gradle.properties` for the NDK path. Those paths are local to wherever `android init` was run, so they break any cross-machine build unless overridden with `ANDROID_NDK_HOME` at build time. The build script exports `ANDROID_NDK_HOME` explicitly to work around this.

2. **API 36 vs API 34 coexistence**: the legacy Kotlin pipeline targets API 34, Tauri Mobile 2.x wants compileSdk 36. The shared Docker image installs both SDK levels so neither pipeline needs to reinstall.

3. **Identity seed lives in Android-specific app data dir**: `/data/data/com.wzp.phone/files/.wzp/identity` instead of `$HOME/.wzp/identity`. The shared `load_or_create_seed()` function in `desktop/src-tauri/src/lib.rs` uses Tauri's `app_data_dir()` which resolves correctly on both Android and desktop — no per-platform code needed.

4. **Direct calls on macOS previously hit an identity mismatch bug** — the `CallEngine` was using `$HOME/.wzp/identity` directly while `register_signal` used Tauri's `app_data_dir()`. Fixed by routing both through `load_or_create_seed()` (commit `2fd9465`). This was important for cross-platform consistency.

## Current state (snapshot)

What works:

- Tauri Mobile scaffold builds and runs on Android
- Signal hub connect + register works
- Room mode (SFU group calls) works with Oboe audio
- Direct 1:1 calls work with full parity to desktop
- Speaker/earpiece toggle works without stalling the audio pipeline
- Call history, recent contacts, deregister UI all present (inherited from desktop)

What remains (task list refs in parens):

- Background service for keeping signal alive when app is backgrounded (#19)
- Proper permission requests (microphone, notifications) on first launch (#19)
- Incoming call notification while backgrounded (#19)
- App icon + splash screen (#19)

## Testing

- **Build**: `./scripts/build-tauri-android.sh` — verify the APK lands on rustypaste and installs on device.
- **Smoke test**: Install → open app → Register → Place call → Receive call. No crashes, audio flows both ways.
- **Speaker toggle**: During a call, toggle speaker/earpiece several times in rapid succession. Audio should never stop, and the toggle should respond within ~200 ms.
- **Stress test**: Call for 10+ minutes continuous. No memory growth, no packet loss beyond what's attributable to the network.

## Files of interest

| Path | Purpose |
|---|---|
| `desktop/src-tauri/src/lib.rs` | Shared Tauri commands (desktop + Android) |
| `desktop/src-tauri/src/android_audio.rs` | JVM-side speaker/earpiece routing |
| `desktop/src-tauri/src/wzp_native.rs` | Runtime dlopen of libwzp_native.so |
| `crates/wzp-native/` | Standalone C++/Oboe cdylib, loaded at runtime |
| `scripts/build-tauri-android.sh` | Remote Docker build pipeline |
| `scripts/Dockerfile.android-builder` | Shared Android Docker image (legacy + Tauri) |
| `docs/incident-tauri-android-init-tcb.md` | Postmortem of the Kotlin+JNI crash cascade |
