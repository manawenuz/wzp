# Issue 001: App crashes on launch — missing libc++_shared.so

## Status: Fix committed, needs rebuild

## Symptom

App opens, shows splash screen, then immediately crashes back to home screen.
Crash occurs when user taps CALL (which triggers `System.loadLibrary("wzp_android")`),
or on any code path that first loads the native library.

## Device tested

- Nothing Phone, arm64-v8a, Android 15
- ADB device ID: `00142151B000973`

## Logcat crash trace

```
E AndroidRuntime: FATAL EXCEPTION: main
E AndroidRuntime: Process: com.wzp.phone, PID: 6048
E AndroidRuntime: java.lang.UnsatisfiedLinkError: dlopen failed: library "libc++_shared.so"
    not found: needed by /data/app/.../base.apk!/lib/arm64-v8a/libwzp_android.so
    in namespace clns-9
E AndroidRuntime:   at java.lang.Runtime.loadLibrary0(Runtime.java:1097)
E AndroidRuntime:   at java.lang.System.loadLibrary(System.java:1765)
E AndroidRuntime:   at com.wzp.engine.WzpEngine.<clinit>(WzpEngine.kt:115)
E AndroidRuntime:   at com.wzp.ui.call.CallViewModel.startCall(CallViewModel.kt:52)
E AndroidRuntime:   at com.wzp.ui.call.InCallScreenKt$InCallScreen$1$1$1.invoke(InCallScreen.kt:96)
```

## Root cause

`crates/wzp-android/build.rs` uses the `cc` crate to compile C++17 code
(the Oboe audio bridge). On Android targets, `cc::Build` defaults to dynamically
linking the C++ standard library (`libc++_shared.so`).

This means `libwzp_android.so` has a runtime dependency on `libc++_shared.so`.
However, the Gradle build (`cargoNdkBuild` task) only copies `libwzp_android.so`
into `jniLibs/arm64-v8a/`. The C++ runtime is never copied alongside it, so the
APK ships without `libc++_shared.so`.

At runtime, `dlopen("libwzp_android.so")` fails because the linker can't find
the missing shared library in the app's namespace.

### Why this wasn't caught earlier

The previous APK (pre-QUIC wiring) was 2.0MB release / 8.9MB debug. The Oboe
C++ bridge was likely being compiled but the native library may not have been
loaded on the code path that was tested, or the dependency was satisfied by a
different linking configuration at the time.

## Fix

In `crates/wzp-android/build.rs`, add `.cpp_link_stdlib(Some("c++_static"))` to
all `cc::Build` invocations targeting Android. This tells the `cc` crate to link
`libc++_static.a` instead of `libc++_shared.so`, baking the C++ runtime directly
into `libwzp_android.so`. No separate shared library needed at runtime.

```rust
cc::Build::new()
    .cpp(true)
    .std("c++17")
    .cpp_link_stdlib(Some("c++_static"))  // <-- this line
    .file("cpp/oboe_bridge.cpp")
    // ...
```

Applied to both the Oboe build path and the stub fallback path.

### Trade-off

Static linking increases `libwzp_android.so` size slightly (~200-400KB for
libc++ static). This is acceptable — the alternative (bundling libc++_shared.so
separately) adds complexity to the Gradle build and risks version mismatches if
multiple native libraries each bundle their own shared copy.

## Rebuild steps

```bash
cd android && ./gradlew assembleRelease
adb install -r app/build/outputs/apk/release/app-release.apk
```

## Verification

After install, the app should:
1. Open without crashing
2. Load `libwzp_android.so` successfully (check logcat for absence of UnsatisfiedLinkError)
3. Show the call UI with CALL button
