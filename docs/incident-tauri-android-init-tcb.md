# Incident report — Tauri Android `__init_tcb+4` SIGSEGV

**Status:** Blocked. Reproducible crash with a known trigger at the cc::Build /
rustc-link-lib layer that we cannot yet explain. Writing this report to hand
off for external help.

**Project:** WarzonePhone (Rust + Tauri 2.x Mobile) Android rewrite
**Branch:** `feat/desktop-audio-rewrite`
**Target phone:** Pixel 6 (`oriole`), Android 16 (`BP3A.250905.014`), arm64-v8a
**Date range of investigation:** 2026-04-09 (one working session, ~27 builds)

---

## One-paragraph summary

We're porting the existing CPAL-backed desktop Tauri app (`desktop/src-tauri`)
to Tauri Mobile Android so the same Rust + Tauri + WebView codebase runs on
both platforms. The Android `.apk` launches, renders the home screen, and
registers on a relay for signal-only builds (no audio backend). The moment
we add **any** `cc::Build::new().cpp(true).cpp_link_stdlib("c++_shared")`
call to `build.rs` — even with a 6-line cpp file that just returns 42 and is
never called from Rust — the built `.so` crashes at launch inside
`__init_tcb(bionic_tcb*, pthread_internal_t*)+4` via `pthread_create` via
`std::thread::spawn` via `tao::ndk_glue::create` via
`Java_com_wzp_desktop_WryActivity_create`, before our Rust entry point has
a chance to run. The exact same NDK, exact same Rust toolchain, exact same
Docker image is used by the legacy `wzp-android` crate (via `cargo-ndk`)
which compiles Oboe and runs fine on the same phone.

---

## Environment

**Docker build image:** `wzp-android-builder` (Dockerfile at
`scripts/Dockerfile.android-builder`)

- Base: `debian:bookworm`
- JDK 17
- Android SDK:
  - cmdline-tools latest
  - `platforms;android-34`, `platforms;android-36`
  - `build-tools;34.0.0`, `build-tools;35.0.0`
  - `ndk;26.1.10909125` (last stable before scudo/MTE crash on NDK r27+)
  - `platform-tools`
- Node.js 20 LTS
- Rust stable `1.94.1 (e408947bf 2026-03-25)`
- Rust android targets: `aarch64-linux-android`, `armv7-linux-androideabi`,
  `i686-linux-android`, `x86_64-linux-android`
- `cargo-ndk` + `cargo tauri-cli 2.10.1` (latest 2.x)

**Host:** Docker on `SepehrHomeserverdk` (remote build server).

**Phone:** Pixel 6, Android 16, kernel 6.1.134-android14-11, on the same LAN
as the build machine and a local `wzp-relay` binary.

**Tauri crate:** `desktop/src-tauri/` in the workspace at the root of the
repo. Depends on `tauri = "2"`, `tauri-plugin-shell = "2"`, `tokio`, `rustls`,
`wzp-proto`, `wzp-codec`, `wzp-fec`, `wzp-crypto`, `wzp-transport`, and (on
non-Android only) `wzp-client` with `features = ["audio", "vpio"]`. The
crate's `[lib]` section is:

```toml
[lib]
name = "wzp_desktop_lib"
crate-type = ["staticlib", "cdylib", "rlib"]
```

The crate produces `libwzp_desktop_lib.so` which is `System.loadLibrary`'d by
Tauri's generated `WryActivity.onCreate` via JNI.

---

## The crash

Every failing build produces the same stack at launch, same pc offsets:

```
signal 11 (SIGSEGV), code 2 (SEGV_ACCERR), fault addr 0x00000072XXXXXX00f (write)

#00 pc 000000000130cc74 libwzp_desktop_lib.so (__init_tcb(bionic_tcb*, pthread_internal_t*)+4)
#01 pc 0000000001331cf0 libwzp_desktop_lib.so (pthread_create+360)
#02 pc 00000000012bee04 libwzp_desktop_lib.so (std::sys::thread::unix::Thread::new::h87be8e9feeaaaf84+184)
#03 pc 0000000000e37f5c libwzp_desktop_lib.so (std::thread::lifecycle::spawn_unchecked::h941f828f9a95150d+1504)
#04 pc 0000000000e461e8 libwzp_desktop_lib.so (std::thread::builder::Builder::spawn_unchecked::hec5f087680cb0248+112)
#05 pc 0000000000e441c8 libwzp_desktop_lib.so (std::thread::functions::spawn::ha3d3fbf2d9fe53e3+108)
#06 pc ...             libwzp_desktop_lib.so (tao::platform_impl::platform::ndk_glue::create::h254c68662718841a+1792)
#07 pc ...             libwzp_desktop_lib.so (Java_com_wzp_desktop_WryActivity_create+76)
```

The offsets are **byte-identical across every failing build**, even when the
cpp content changes drastically (cf. `cpp_smoke.cpp` at 6 lines, 20 lines,
200+ Oboe source files). We believe this is because cargo caches the Rust
compilation unit and only the build-script artifacts differ, and the final
link produces the same layout.

`__init_tcb` is defined locally inside our `.so` with C++ mangling:

```
_Z10__init_tcbP10bionic_tcbP18pthread_internal_t
```

It originates from bionic's `pthread_create.cpp`, which got pulled in
statically from the NDK's `sysroot/usr/lib/aarch64-linux-android/libc.a`.
Both failing and known-good (legacy `wzp_android.so`) builds contain this
same static symbol — the presence of the symbol is not the problem.

Fault address `0x72XXXXXX00f` with code `SEGV_ACCERR` (access permission
error, write). Aligned to `+4` inside `__init_tcb`, which is typically a
store into the passed-in `bionic_tcb*`. The pointer is either NULL-ish or
pointing into read-only memory.

---

## Bisection (the important part)

We started from a known-good commit (`5309938`) where the Tauri Android app
launches, registers on a relay, and behaves identically to the desktop app
modulo audio. Then we added features **one variable at a time**:

| Step | Commit | Change vs previous | Result |
|---|---|---|---|
| Baseline | `5309938` | — | ✅ launches, renders home, registers on relay |
| **A** | `f96d7ce` | Add `cc = "1"` build-dep + compile trivial `cpp/hello.c` via `cc::Build` (C, not C++). Static lib never linked in. | ✅ |
| **B** | `ae4f366` | Add `wzp-client` Android dep with `default-features = false` (no CPAL, no VPIO). No new imports. | ✅ |
| **C** | `19fd3dd` | Un-cfg-gate `mod engine;` in `lib.rs` so `engine.rs` compiles on Android. `CallEngine::start()` has an Android stub returning an error. | ✅ |
| **D** | `a852cad` | Compile `cpp/getauxval_fix.c` (legacy wzp-android shim). Still pure C. | ✅ |
| **E** | `4250f1b` | **Compile full Oboe C++ bridge** (200+ source files from `google/oboe@1.8.1`). `cc::Build::new().cpp(true).std("c++17").cpp_link_stdlib(Some("c++_shared"))` + `-llog` + `-lOpenSLES` link directives. Nothing called from Rust yet — the `extern "C"` bridge functions are exported but never referenced from the Rust side. | ❌ **crash** |
| E.4 | `aa240c6` | **Only change:** replace the entire Oboe compile with ONE tiny `cpp_smoke.cpp` file: `extern "C" int wzp_cpp_smoke(void) { std::lock_guard<std::mutex> lk(m); std::thread t([](){...}); t.join(); return g.load(); }`. Still `cpp(true) + cpp_link_stdlib("c++_shared")`. Drop `-llog`/`-lOpenSLES`. | ❌ **same crash, same offsets** |
| E.2 | `0224ce6` | Shrink `cpp_smoke.cpp` further: just `std::atomic<int>` + `fetch_add`, no mutex, no thread, no includes beyond `<atomic>`. | ❌ **same crash, same offsets** |
| E.1 | `0d74366` | **Absolute minimum:** `cpp_smoke.cpp` = `extern "C" int wzp_cpp_hello(void){return 42;}`. NO `#include`. NO STL. Just a function. Still compiled with `cpp(true) + cpp_link_stdlib("c++_shared")`. | ❌ **same crash, same offsets** |

### Additional confirming observations

1. **The cpp code is dead-stripped.** `llvm-nm -a libwzp_desktop_lib.so` shows
   zero matches for `wzp_cpp_hello`, `wzp_cpp_smoke`, or any Oboe symbol in
   builds E through E.1. The static archive (`libwzp_cpp_smoke.a` /
   `liboboe_bridge.a`) exists on disk under
   `target/aarch64-linux-android/debug/build/wzp-desktop-*/out/`, but because
   nothing in Rust ever references the exported C function, the final linker
   drops it.

2. **`build.rs` link directives are the real delta.** `cc::Build::new()
   .cpp(true).cpp_link_stdlib(Some("c++_shared"))` emits a
   `cargo:rustc-link-lib=c++_shared` directive that adds a `NEEDED` entry for
   `libc++_shared.so` to the final `.so`'s dynamic table. `readelf -d` on
   the crashing `.so` shows:

   ```
   NEEDED       Shared library: [libc++_shared.so]
   NEEDED       Shared library: [liblog.so]      (only in full Oboe build)
   NEEDED       Shared library: [libOpenSLES.so] (only in full Oboe build)
   ```

   The working baseline `.so` has no `NEEDED` entries beyond libc/liblog.

3. **Linker version doesn't matter.** We tried forcing
   `aarch64-linux-android26-clang` as the linker (API 26 has proper dynamic
   bindings to libc.so's runtime `pthread_create`/`__init_tcb`) via three
   different mechanisms:
   - `CARGO_TARGET_AARCH64_LINUX_ANDROID_LINKER` env var in `docker run`
   - `.cargo/config.toml` workspace-level linker override
   - **Binary replacement inside the image**: `mv
     aarch64-linux-android24-clang .orig` and replace with a shell script
     that `exec`s `aarch64-linux-android26-clang`. Verified by calling
     `--version` which prints `Target: aarch64-unknown-linux-android26`.

   All three made no difference. The `__init_tcb` symbol is pulled statically
   from the **same** `libc.a` regardless of which clang wrapper is used — the
   NDK ships ONE `libc.a` at
   `sysroot/usr/lib/aarch64-linux-android/libc.a` shared across all API
   levels. Only the per-API `libc.so` symlinks change (and we're linked
   statically, not dynamically, against libc).

4. **Legacy `wzp-android` crate works on the same phone, same image.** Run
   in the exact same Docker container, the legacy Kotlin app's JNI library
   (`crates/wzp-android` built via `cargo ndk`) compiles a subset of the
   same Oboe code, produces a `.so` that has the same static
   `_Z10__init_tcbP...` + `pthread_create` + `pthread_create.cpp` symbols,
   and launches cleanly on the Pixel 6. Key differences between the two
   build paths:

   | | `wzp-android` (works) | `wzp-desktop` Tauri (crashes) |
   |---|---|---|
   | Build driver | `cargo ndk -t arm64-v8a build --release -p wzp-android` | `cargo tauri android build --debug --target aarch64 --apk` |
   | Profile | release | debug (release crashes identically) |
   | Linker | `aarch64-linux-android26-clang` (via `.cargo/config.toml` which cargo-ndk honors) | `aarch64-linux-android24-clang` (tauri-cli hardcodes and ignores config; the shim redirect makes no difference) |
   | crate-type | `["cdylib", "rlib"]` | `["staticlib", "cdylib", "rlib"]` |
   | JNI entrypoint | direct Kotlin `System.loadLibrary` + our own `native fun` declarations; first `pthread_create` runs later from the tokio runtime inside a command | `WryActivity.onCreate` via Tauri's generated Java glue; first `pthread_create` runs **inside the JNI call** via `tao::ndk_glue::create` |
   | Other heavy deps | tokio, wzp-{proto,codec,fec,crypto,transport} | tokio, tauri, tauri-runtime-wry, tao, wry, webview2-com, soup3, webkit2gtk (all platform-specific ones cfg-gated out of android), and also all of the above |
   | Binary size | `libwzp_android.so` ≈ 14 MB (release) | `libwzp_desktop_lib.so` ≈ 160 MB (debug), 16 MB (release) |

5. **The crash happens in the JNI-callback thread during `onCreate`.** Frame
   #06 `tao::platform_impl::platform::ndk_glue::create+1792` is tao's Android
   event-loop bootstrap, which Tauri calls from inside
   `Java_com_wzp_desktop_WryActivity_create` in response to the Java-side
   activity lifecycle. This means the thread spawn is happening while the
   Java VM still holds the native onCreate call, before `onCreate` has
   returned to the Android runtime. Legacy `wzp-android` never spawns a
   thread from an onCreate JNI call — it spawns threads only from
   `nativeSignalConnect`/similar commands invoked later from Kotlin button
   clicks, after the activity is fully initialised.

---

## Current suspect

One of the two items below, probably (2):

1. **The `.cpp(true)` mode in cc-rs changes something invisible in the link
   pipeline** (for example, emitting a different `-x` flag to clang, or
   changing linker driver selection). We have not yet verified this by
   diffing the actual rustc linker invocation between a working and a
   crashing build with `--verbose` + `-Clink-arg=-Wl,-t`.

2. **Adding `libc++_shared.so` as a NEEDED entry causes Android's dynamic
   linker to load libc++_shared.so before our `.so`'s init runs, and
   something in libc++_shared's `.init_array` interacts badly with
   tao::ndk_glue's `pthread_create` call from inside the JNI onCreate
   window**. The legacy crate doesn't hit this because (a) it has no
   NEEDED libc++_shared when built without Oboe, and (b) even when it does
   build Oboe, its thread spawns happen outside the onCreate JNI call so
   whatever libc state is wrong at that moment is already stabilised.

We have not yet confirmed (2) with the obvious A/B test: keep `cpp_smoke.cpp`
but drop `.cpp_link_stdlib(Some("c++_shared"))` (and drop any manual
`cargo:rustc-link-lib=c++_shared`) so the NEEDED entry disappears but the
rest of the pipeline stays identical. That's the next experiment we were
going to run, but the user reasonably asked for this report first.

---

## What we've ruled out

- **NDK API level** — forcing API-26 linker via three independent mechanisms
  made zero difference.
- **Build profile** — release (`0x6b8000` offset, 21 MB unsigned APK) and
  debug (same 193 MB APK, same crash offsets) both crash identically.
- **Oboe specifically** — replacing the Oboe compile with 6 lines of C++
  that does nothing still reproduces the crash.
- **cpp code being executed at runtime** — dead-stripped, not in the final
  `.so` at all per `nm -a`.
- **minSdk in build.gradle** — bumped from 24 to 26, no effect.
- **libdl.a stub issue** — ruled out via logcat (`libdl.a is a stub --- use
  libdl.so instead` was only surfacing from our own `dlsym` shim that we
  subsequently deleted).
- **`pthread_create` interposition via `-Wl,--wrap=pthread_create`** — tried
  and reverted; the wrap target still resolved to the broken static stub.
- **Keystore / signing** — debug signing with persistent `~/.android/
  debug.keystore` works fine; no signature mismatch issues.

---

## The files involved

### `desktop/src-tauri/build.rs` (current state, E.1)

```rust
use std::path::PathBuf;
use std::process::Command;

fn main() {
    // Embedded git hash
    let git_hash = Command::new("git")
        .args(["rev-parse", "--short", "HEAD"])
        .output()
        .ok()
        .filter(|o| o.status.success())
        .and_then(|o| String::from_utf8(o.stdout).ok())
        .map(|s| s.trim().to_string())
        .unwrap_or_else(|| "unknown".into());
    println!("cargo:rustc-env=WZP_GIT_HASH={git_hash}");
    println!("cargo:rerun-if-changed=../../.git/HEAD");
    println!("cargo:rerun-if-changed=../../.git/refs/heads");

    let target = std::env::var("TARGET").unwrap_or_default();
    if target.contains("android") {
        // Step A: plain C sanity file
        println!("cargo:rerun-if-changed=cpp/hello.c");
        cc::Build::new().file("cpp/hello.c").compile("wzp_hello");

        // Step D: legacy getauxval shim
        println!("cargo:rerun-if-changed=cpp/getauxval_fix.c");
        cc::Build::new().file("cpp/getauxval_fix.c").compile("getauxval_fix");

        // Step E.1: minimal C++ smoke — THIS STEP BRINGS BACK THE CRASH
        println!("cargo:rerun-if-changed=cpp/cpp_smoke.cpp");
        cc::Build::new()
            .cpp(true)
            .std("c++17")
            .cpp_link_stdlib(Some("c++_shared"))
            .file("cpp/cpp_smoke.cpp")
            .compile("wzp_cpp_smoke");

        // Copy libc++_shared.so into gen/android jniLibs so the runtime
        // linker can find it when the NEEDED entry fires.
        if let Ok(ndk) = std::env::var("ANDROID_NDK_HOME").or_else(|_| std::env::var("NDK_HOME")) {
            let triple = "aarch64-linux-android";
            let abi = "arm64-v8a";
            let lib_dir = format!(
                "{ndk}/toolchains/llvm/prebuilt/linux-x86_64/sysroot/usr/lib/{triple}"
            );
            println!("cargo:rustc-link-search=native={lib_dir}");
            let shared_so = format!("{lib_dir}/libc++_shared.so");
            if std::path::Path::new(&shared_so).exists() {
                let manifest = std::env::var("CARGO_MANIFEST_DIR").unwrap_or_default();
                let jni_dir = format!("{manifest}/gen/android/app/src/main/jniLibs/{abi}");
                if std::fs::create_dir_all(&jni_dir).is_ok() {
                    let _ = std::fs::copy(&shared_so, format!("{jni_dir}/libc++_shared.so"));
                }
            }
        }
    }

    tauri_build::build()
}
```

### `desktop/src-tauri/cpp/cpp_smoke.cpp` (E.1)

```cpp
extern "C" int wzp_cpp_hello(void) {
    return 42;
}
```

### `desktop/src-tauri/Cargo.toml` (relevant excerpts)

```toml
[package]
name = "wzp-desktop"
version = "0.1.0"
edition = "2024"

[lib]
name = "wzp_desktop_lib"
crate-type = ["staticlib", "cdylib", "rlib"]

[[bin]]
name = "wzp-desktop"
path = "src/main.rs"

[build-dependencies]
tauri-build = { version = "2", features = [] }
cc = "1"

[dependencies]
tauri = { version = "2", features = [] }
tauri-plugin-shell = "2"
serde = { version = "1", features = ["derive"] }
serde_json = "1"
tokio = { version = "1", features = ["full"] }
tracing = "0.1"
tracing-subscriber = "0.3"
anyhow = "1"
rustls = { version = "0.23", default-features = false, features = ["ring", "std"] }

wzp-proto = { path = "../../crates/wzp-proto" }
wzp-codec = { path = "../../crates/wzp-codec" }
wzp-fec = { path = "../../crates/wzp-fec" }
wzp-crypto = { path = "../../crates/wzp-crypto" }
wzp-transport = { path = "../../crates/wzp-transport" }

[target.'cfg(not(target_os = "android"))'.dependencies]
wzp-client = { path = "../../crates/wzp-client", features = ["audio", "vpio"] }

[target.'cfg(target_os = "android")'.dependencies]
wzp-client = { path = "../../crates/wzp-client", default-features = false }
```

---

## Reproduction

A fresh clone on a Linux x86_64 host with:

```bash
git clone ssh://git@git.manko.yoga:222/manawenuz/wz-phone.git
cd wz-phone
git checkout feat/desktop-audio-rewrite
git reset --hard 0d74366   # <-- step E.1, smallest crashing commit

# Need: Android NDK r26.1.10909125, JDK 17, Node 20, Rust stable, cargo tauri 2.x
scripts/prep-linux-mint.sh    # installs all the above into /opt/android-sdk etc.

cd desktop
npm install
cd src-tauri
cargo tauri android build --debug --target aarch64 --apk
adb install -r gen/android/app/build/outputs/apk/universal/debug/app-universal-debug.apk
adb logcat -c && adb shell am start -n com.wzp.desktop/.MainActivity
adb logcat | grep -E "F DEBUG|__init_tcb|pthread_create"
```

Expected result: SIGSEGV at `__init_tcb+4` within ~500 ms of launch.

Reverting `cpp/cpp_smoke.cpp` + the `cc::Build` call for it in `build.rs`
(one git command: `git revert 0d74366 aa240c6 0224ce6 a852cad`) restores a
working build. Keeping the C sanity compile (`hello.c`, `getauxval_fix.c`)
is fine — only the `.cpp(true) + .cpp_link_stdlib("c++_shared")` combination
triggers the regression.

---

## What we'd like help with

1. **Is our suspect #2 actually the mechanism?** Is there a known issue
   where a Tauri/tao android cdylib crashes on load when it has a
   `libc++_shared.so` NEEDED entry and tries to spawn a thread from inside
   an onCreate JNI call?

2. **What's the correct way to link Oboe (or any C++ Android audio
   library) into a `cargo tauri android build` cdylib** without hitting
   this? Is there a known-good combination of cc-rs flags / linker
   arguments / cargo config?

3. **Is there a way to force `cargo tauri` to use the same linker setup
   as `cargo ndk`**, which reliably produces working Oboe-linked .so
   files from the exact same workspace? We've tried env var override,
   `.cargo/config.toml`, and image-level binary replacement — cargo
   tauri ignores all three and keeps using
   `aarch64-linux-android24-clang`.

4. **Is there a way to defer `tao::ndk_glue::create`'s thread spawn to
   after `onCreate` returns** so that whatever bionic state `__init_tcb`
   depends on is ready?

5. **Lastly** — is there a fundamentally different approach we should
   take (e.g., use the `oboe` Rust crate from crates.io instead of a
   hand-rolled C++ bridge, use Android's AAudio directly via the `ndk`
   crate's aaudio bindings, or even abandon the C++ audio path and
   implement mic/speaker via JNI into Java `AudioRecord`/`AudioTrack`)?
