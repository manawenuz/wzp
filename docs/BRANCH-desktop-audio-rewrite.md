# Branch: `feat/desktop-audio-rewrite`

Home of the Tauri desktop client for macOS, Windows, and Linux. Named "audio-rewrite" because the original driver was replacing a CPAL-only audio pipeline with platform-native backends that support OS-level echo cancellation (VoiceProcessingIO on macOS, WASAPI Communications on Windows), but the branch has grown into the full desktop story — Windows cross-compilation, vendored dependencies, history UI, direct calling, the whole thing.

## Purpose

The desktop client shares 100% of its frontend (`desktop/src/`) and Tauri command layer (`desktop/src-tauri/src/lib.rs`, `engine.rs`, `history.rs`) with the Android build on `android-rewrite`. Differences are limited to:

- **Audio backends**, which are platform-gated via Cargo target-dep sections in `desktop/src-tauri/Cargo.toml` and feature flags in `crates/wzp-client/Cargo.toml`.
- **Identity storage paths**, which resolve via Tauri's `app_data_dir()` (`~/Library/Application Support/…` on macOS, `%APPDATA%\…` on Windows, `~/.local/share/…` on Linux).
- **Build toolchains**: native `cargo build` on macOS/Linux, `cargo xwin` cross-compile from Linux for Windows via Docker on SepehrHomeserverdk.

## Audio backend matrix

| Target | Capture | Playback | AEC |
|---|---|---|---|
| macOS | CPAL (WASAPI/CoreAudio via cpal crate) OR VoiceProcessingIO (native Core Audio) | CPAL | VoiceProcessingIO native AEC (when `vpio` feature enabled) |
| Windows (default) | CPAL → WASAPI shared mode | CPAL → WASAPI shared mode | None |
| Windows (AEC build) | Direct WASAPI with `IAudioClient2::SetClientProperties(AudioCategory_Communications)` | CPAL → WASAPI shared mode | **OS-level**: Windows routes the capture stream through the driver's communications APO chain (AEC + NS + AGC) |
| Linux | CPAL → ALSA/PulseAudio | CPAL → ALSA/PulseAudio | None |

The macOS VPIO path is gated behind the `vpio` feature in `wzp-client` and the `coreaudio-rs` dep is itself `cfg(target_os = "macos")`, so enabling the feature on Windows or Linux is a no-op.

The Windows AEC path is gated behind the `windows-aec` feature, also target-gated (the `windows` crate dep is only pulled in on Windows), and re-exports `WasapiAudioCapture as AudioCapture` when enabled so downstream code doesn't need to know which backend is active. The current Windows build at `target/windows-exe/wzp-desktop.exe` has `windows-aec` on; a baseline noAEC build is preserved at `target/windows-exe/wzp-desktop-noAEC.exe` for A/B comparison on real hardware.

See [`BRANCH-android-rewrite.md`](BRANCH-android-rewrite.md) for Oboe audio on Android, which is its own story.

## Recent major work

### 1. Desktop direct calling feature (commit `2fd9465` and neighbors)

Brought direct 1:1 calls to macOS with full parity to the Android client:

- **Identity path fix**: the desktop `CallEngine::start` was loading seed from `$HOME/.wzp/identity` while `register_signal` used Tauri's `app_data_dir()`, producing two different fingerprints per run. Both now route through `load_or_create_seed()` which uses `app_data_dir()` everywhere.
- **Call history with dedup**: `history.rs` stores a `Vec<CallHistoryEntry>` with a `CallDirection` enum (`Placed | Received | Missed`). The `log` function dedupes by `call_id` so an outgoing call isn't logged twice as "missed" (when the signal loop's `DirectCallOffer` handler fires) and then again as "placed" (when `place_call` returns). Instead the entry is updated in place.
- **Recent contacts row**: a horizontal chip UI in the direct-call panel showing the last N peers with friendly aliases, clickable to re-dial.
- **Deregister button**: lets a user drop their signal registration without quitting the app, useful when switching identities.
- **Random alias derivation**: a new client sees a human-friendly alias like "silent-forest-41" derived deterministically from its seed, so it's identifiable in the UI before manual naming.
- **Default room "general"** instead of "android", since the desktop client is not Android.

### 2. macOS VoiceProcessingIO integration

`crates/wzp-client/src/audio_vpio.rs` — a native Core Audio implementation using `AUGraph` + `AudioComponentInstance` with the VPIO audio unit. Gives you hardware-accelerated AEC (same AEC Apple ships in FaceTime / iMessage audio / voice memos) at the cost of tight coupling to Apple frameworks. Lock-free ring pattern matches the CPAL path so the upper layers don't notice the difference.

Enabled by `features = ["audio", "vpio"]` in the macOS target section of `desktop/src-tauri/Cargo.toml`.

### 3. Windows cross-compilation via cargo-xwin

Cross-compiling Rust + Tauri to `x86_64-pc-windows-msvc` from Linux using `cargo-xwin`, which downloads the Microsoft CRT + Windows SDK on demand and drives `clang-cl` as the compiler. No Windows machine is needed for the build itself — only for runtime testing.

**Build infrastructure**:

- `scripts/Dockerfile.windows-builder` — Debian bookworm + Rust + cargo-xwin + Node 20 + cmake + ninja + llvm + clang + lld + nasm. Pre-warms the xwin MSVC CRT cache at image build time (saves ~4 minutes per cold build).
- `scripts/build-windows-docker.sh` — fire-and-forget remote build via Docker on SepehrHomeserverdk. Same pattern as `build-tauri-android.sh`. Uploads the `.exe` to rustypaste and fires an `ntfy.sh/wzp` notification on start and on completion.
- `scripts/build-windows-cloud.sh` — alternative pipeline using a temporary Hetzner Cloud VPS. Slower (full VM spin-up), more expensive, but useful when Docker image rebuilds would be disruptive.

**Two critical blockers resolved** on the way to a working `.exe`:

1. **libopus SSE4.1 / SSSE3 intrinsic compile failure**. `audiopus_sys` vendors libopus 1.3.1, whose `CMakeLists.txt` gates the per-file `-msse4.1` `COMPILE_FLAGS` behind `if(NOT MSVC)`. Under `clang-cl`, CMake sets `MSVC=1` (because `CMAKE_C_COMPILER_FRONTEND_VARIANT=MSVC` triggers `Platform/Windows-MSVC.cmake` which unconditionally sets the variable), so the per-file flag is never set and the SSE4.1 source files compile without the target feature — then fail with 20+ "always_inline function '_mm_cvtepi16_epi32' requires target feature 'sse4.1'" errors.

   Fixed by **vendoring audiopus_sys into `vendor/audiopus_sys/`** and patching its bundled libopus to introduce an `MSVC_CL` variable that is true only for real `cl.exe` (distinguished via `CMAKE_C_COMPILER_ID STREQUAL "MSVC"`). The eight `if(NOT MSVC)` SIMD guards are flipped to `if(NOT MSVC_CL)` and the global `/arch` block at line 445 becomes `if(MSVC_CL)`, so clang-cl gets the GCC-style per-file flags while real cl.exe keeps the `/arch:AVX` / `/arch:SSE2` globals.

   Wired in via `[patch.crates-io] audiopus_sys = { path = "vendor/audiopus_sys" }` at the workspace root.

   Upstream tracking: [xiph/opus#256](https://github.com/xiph/opus/issues/256), [xiph/opus PR #257](https://github.com/xiph/opus/pull/257) (both stale).

2. **tauri-build needs `icons/icon.ico` for the Windows PE resource**. The desktop only had `icon.png`. Generated a multi-size ICO (16/24/32/48/64/128/256) from the existing placeholder via Pillow and committed it. Placeholder quality — real branded icons can replace it later.

### 4. Windows `AudioCategory_Communications` capture path (task #24)

`crates/wzp-client/src/audio_wasapi.rs` — direct WASAPI capture via `IMMDeviceEnumerator → IAudioClient2 → SetClientProperties` with `AudioCategory_Communications`. This tells Windows "this is a VoIP call" and Windows routes the capture stream through the driver's registered communications APO chain, which on most Win10/11 consumer hardware includes AEC, NS, and AGC.

**Caveat**: quality is driver-dependent. On a machine with a good communications APO (Intel Smart Sound, Dolby, modern Realtek on Win11 24H2+, anything with Voice Clarity enabled) it's excellent. On generic class-compliant drivers with no communications APO registered, it's a no-op. For a guaranteed AEC regardless of driver, see task #26 which tracks implementing the classic Voice Capture DSP (`CLSID_CWMAudioAEC`) as a fallback.

Gated behind the `windows-aec` feature in `wzp-client`. Enabled by default in the Windows target section of `desktop/src-tauri/Cargo.toml`.

## Build pipelines

### Native macOS / Linux

```bash
cd desktop
npm install
npm run build
cd src-tauri
cargo build --release --bin wzp-desktop
```

### Windows x86_64 via Docker on SepehrHomeserverdk

```bash
./scripts/build-windows-docker.sh                 # Full: pull + build + download
./scripts/build-windows-docker.sh --no-pull       # Skip git fetch
./scripts/build-windows-docker.sh --rust          # Force-clean Rust target
./scripts/build-windows-docker.sh --image-build   # (Re)build the Docker image (fire-and-forget)
```

Output lands at `target/windows-exe/wzp-desktop.exe`. Both `wzp-desktop.exe` and `wzp-desktop-noAEC.exe` can coexist in that directory; the script writes `wzp-desktop.exe` so renaming the prior build to `-noAEC.exe` (or any other name) before rebuilding preserves it.

### Windows x86_64 via Hetzner Cloud (alternative)

```bash
./scripts/build-windows-cloud.sh                  # Full: create VM → build → download → destroy
./scripts/build-windows-cloud.sh --prepare        # Create VM and install deps only
./scripts/build-windows-cloud.sh --build          # Build on existing VM
./scripts/build-windows-cloud.sh --destroy        # Delete the VM
WZP_KEEP_VM=1 ./scripts/build-windows-cloud.sh    # Keep VM alive after build for debug
```

Remember to destroy the VM at end of day with `--destroy`.

### Linux x86_64 (relay + CLI + bench)

```bash
./scripts/build-linux-docker.sh                   # Fire-and-forget remote Docker build
./scripts/build-linux-docker.sh --install         # Wait for completion and download
```

Uses the same `wzp-android-builder` Docker image as Android (not a separate image), since the deps (Rust + cmake + ring prereqs) are the same.

## Testing

### Direct calling parity

1. Build on two machines (macOS + Windows, or two macOS, or any combination).
2. Both machines register on the same relay.
3. Copy one machine's fingerprint into the other's direct-call panel.
4. Place the call. Confirm ringing UI on the callee and "calling…" UI on the caller.
5. Answer. Confirm audio flows both ways.
6. Hang up from either side. Confirm call-history entries are labeled correctly (`Outgoing` on caller, `Incoming` on callee, never `Missed` on a successful call).

### Windows AEC A/B

1. Install `wzp-desktop-noAEC.exe` and `wzp-desktop.exe` on the same Windows box.
2. Join a call from each (separately) while a second machine plays known audio through the first machine's speakers.
3. On the remote (listening) side: the `noAEC` call should have clear audible echo; the AEC call should have minimal or no echo after a 1–2 s convergence period.
4. If both builds sound identical (with echo) → the `AudioCategory_Communications` switch isn't triggering the driver's APO chain. Investigate via task #26 (Voice Capture DSP fallback).

## Known quirks

1. **libopus vendor path is workspace-relative**. `[patch.crates-io] audiopus_sys = { path = "vendor/audiopus_sys" }` works from any crate in the workspace because Cargo resolves it against the root `Cargo.toml`'s directory. If the workspace is moved or vendored into another workspace, update the path.

2. **`cargo xwin` overwrites `override.cmake` on every invocation**. Any attempt to patch `~/.cache/cargo-xwin/cmake/clang-cl/override.cmake` at Docker image build time is inert because `src/compiler/clang_cl.rs` line ~444 writes the bundled file fresh on every run. All real fixes must land in the source tree (via the vendored audiopus_sys, as done here), not in the cargo-xwin cache.

3. **WebView2 runtime is a prerequisite on Windows 10**. Windows 11 ships with it. If the `.exe` launches and immediately exits with no error on a Win10 machine, that's the missing runtime — install it from [Microsoft's Evergreen bootstrapper](https://developer.microsoft.com/en-us/microsoft-edge/webview2/).

4. **Rust 2024 edition `unsafe_op_in_unsafe_fn` lint**. The WASAPI backend in `audio_wasapi.rs` emits ~18 of these warnings because Rust 2024 requires explicit `unsafe { ... }` blocks inside `unsafe fn` bodies. The warnings don't block the build and don't affect runtime behavior; cleaning them up is tracked informally as tech debt.

## Files of interest

| Path | Purpose |
|---|---|
| `desktop/src/` | Shared frontend (TypeScript + HTML + CSS) |
| `desktop/src-tauri/src/lib.rs` | Tauri commands shared with Android |
| `desktop/src-tauri/src/engine.rs` | `CallEngine` wrapper |
| `desktop/src-tauri/src/history.rs` | Persistent call history store with dedup |
| `crates/wzp-client/src/audio_io.rs` | CPAL capture + playback (baseline) |
| `crates/wzp-client/src/audio_vpio.rs` | macOS VoiceProcessingIO capture (AEC) |
| `crates/wzp-client/src/audio_wasapi.rs` | Windows WASAPI communications capture (AEC) |
| `vendor/audiopus_sys/opus/CMakeLists.txt` | Patched libopus for clang-cl SIMD |
| `scripts/Dockerfile.windows-builder` | Windows cross-compile Docker image |
| `scripts/build-windows-docker.sh` | Remote Docker build pipeline |
| `scripts/build-windows-cloud.sh` | Hetzner VPS alternative pipeline |
| `scripts/build-linux-docker.sh` | Linux x86_64 relay/CLI build pipeline |
