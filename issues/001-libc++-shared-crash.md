# Issue 001: App crashes on launch — C++ runtime not linked correctly

## Status: Fix v2 committed, needs rebuild

## Symptom

App opens, shows splash screen, then immediately crashes back to home screen.
Crash occurs when user taps CALL (which triggers `System.loadLibrary("wzp_android")`),
or on any code path that first loads the native library.

## Device tested

- Nothing Phone, arm64-v8a, Android 15
- ADB device ID: `00142151B000973`

## Crash history

### Attempt 1: `libc++_shared.so` not found

```
E AndroidRuntime: java.lang.UnsatisfiedLinkError: dlopen failed: library "libc++_shared.so"
    not found: needed by .../libwzp_android.so
    at com.wzp.engine.WzpEngine.<clinit>(WzpEngine.kt:115)
```

**Cause**: `cc::Build` defaults to dynamic C++ linking. `libc++_shared.so` never
packaged into APK.

**Attempted fix**: `.cpp_link_stdlib(Some("c++_static"))` — link STL statically.

### Attempt 2: missing `__class_type_info` vtable (RTTI)

```
E AndroidRuntime: java.lang.UnsatisfiedLinkError: dlopen failed: cannot locate symbol
    "_ZTVN10__cxxabiv117__class_type_infoE" referenced by .../libwzp_android.so
    at com.wzp.engine.WzpEngine.<clinit>(WzpEngine.kt:115)
```

**Cause**: Android NDK splits the static C++ runtime into two archives:
- `libc++_static.a` — STL (containers, strings, algorithms)
- `libc++abi.a` — ABI layer (RTTI typeinfo vtables, exception handling)

The `cc` crate's `.cpp_link_stdlib(Some("c++_static"))` only emits
`cargo:rustc-link-lib=static=c++_static`. It does NOT pull in `libc++abi.a`,
so all RTTI symbols (`__class_type_info`, `__si_class_type_info`, etc.)
are unresolved at dlopen time.

## Root cause (full)

`crates/wzp-android/build.rs` uses the `cc` crate to compile C++17 code
(the Oboe audio bridge). Two things go wrong:

1. Dynamic linking by default → `libc++_shared.so` not in APK
2. Even with `.cpp_link_stdlib("c++_static")`, the ABI half (`libc++abi.a`)
   is not linked, leaving RTTI symbols unresolved

## Fix (v2)

Suppress the `cc` crate's automatic C++ stdlib linking with `.cpp_link_stdlib(None)`,
then explicitly link both static archives:

```rust
cc::Build::new()
    .cpp(true)
    .std("c++17")
    .cpp_link_stdlib(None)       // suppress cc crate's automatic linking
    .file("cpp/oboe_bridge.cpp")
    // ...
    .compile("oboe_bridge");

// Manually link both halves of the Android NDK static C++ runtime
println!("cargo:rustc-link-lib=static=c++_static");
println!("cargo:rustc-link-lib=static=c++abi");
```

This is placed once after the match block (applies to both Oboe and stub paths).

### Trade-off

Static linking increases `libwzp_android.so` by ~300-500KB. Acceptable for
avoiding shared library packaging complexity.

## Rebuild steps

```bash
cd android && ./gradlew clean assembleRelease
adb install -r app/build/outputs/apk/release/app-release.apk
```

Use `clean` to ensure the native library is fully relinked.

## Verification

After install, the app should:
1. Open without crashing
2. Load `libwzp_android.so` successfully (no UnsatisfiedLinkError in logcat)
3. Show the call UI with CALL button
