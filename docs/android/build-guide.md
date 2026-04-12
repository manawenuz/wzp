# Build Guide

## Prerequisites

| Tool | Version | Purpose |
|------|---------|---------|
| JDK | 17 | Android Gradle builds |
| Android SDK | 34 | Compile SDK |
| Android NDK | 26.1.10909125 | Native C++/Rust compilation |
| Rust | 1.85+ | Native engine (edition 2024) |
| cargo-ndk | latest | Cross-compile Rust → Android |
| `aarch64-linux-android` target | - | Rust target for ARM64 |

### Install Rust Android target

```bash
rustup target add aarch64-linux-android
cargo install cargo-ndk
```

### Environment Variables

```bash
export JAVA_HOME="/usr/lib/jvm/java-17-openjdk-amd64"
export ANDROID_HOME="$HOME/android-sdk"
export ANDROID_NDK_HOME="$ANDROID_HOME/ndk/26.1.10909125"

# For manual cargo-ndk builds (Gradle sets these automatically):
export CC_aarch64_linux_android="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android21-clang"
export CXX_aarch64_linux_android="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/aarch64-linux-android21-clang++"
export AR_aarch64_linux_android="$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-ar"
```

## Build Commands

### Full Build (Gradle drives everything)

```bash
cd android
./gradlew assembleRelease
```

This runs:
1. `cargoNdkBuild` task: invokes `cargo ndk -t arm64-v8a -o app/src/main/jniLibs build --release -p wzp-android`
2. Compiles Kotlin/Compose code
3. Packages APK with signing

### Native Library Only

```bash
cargo ndk -t arm64-v8a -o android/app/src/main/jniLibs build --release -p wzp-android
```

Output: `android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so`

### Skip Native Rebuild

If the `.so` hasn't changed:

```bash
cd android
./gradlew assembleRelease -x cargoNdkBuild
```

### Debug Build

```bash
cd android
./gradlew assembleDebug
```

Debug APK is ~8.9 MB (unstripped `.so`), release is ~6.9 MB.

## Signing

### Debug

```
Keystore: android/keystore/wzp-debug.jks
Password: android
Key alias: wzp-debug
```

### Release

```
Keystore: android/keystore/wzp-release.jks
Password: wzphone2024
Key alias: wzp-release
```

Both keystores are checked into the repo for development convenience. For production, replace with proper key management.

## Build Artifacts

| Artifact | Path | Size |
|----------|------|------|
| Debug APK | `android/app/build/outputs/apk/debug/app-debug.apk` | ~8.9 MB |
| Release APK | `android/app/build/outputs/apk/release/app-release.apk` | ~6.9 MB |
| Native lib | `android/app/src/main/jniLibs/arm64-v8a/libwzp_android.so` | ~5 MB |

## ABI Support

Currently only `arm64-v8a` (ARM64) is built. This covers 95%+ of modern Android devices.

To add more ABIs, edit `build.gradle.kts`:

```kotlin
ndk { abiFilters += listOf("arm64-v8a", "armeabi-v7a") }
```

And update the cargo-ndk command in `cargoNdkBuild` task:

```kotlin
commandLine("cargo", "ndk", "-t", "arm64-v8a", "-t", "armeabi-v7a", ...)
```

## Oboe Dependency

The Oboe C++ audio library is fetched at build time by `build.rs`:

1. Attempts `git clone` of Oboe 1.8.1 into `$OUT_DIR/oboe`
2. If successful, compiles `oboe_bridge.cpp` with Oboe headers
3. If clone fails (no network), falls back to `oboe_stub.cpp` (no-op audio)

This means **first build requires internet** to fetch Oboe. Subsequent builds use the cached checkout.

## Common Build Issues

### `cargo ndk` not found

```bash
cargo install cargo-ndk
```

### Missing Android target

```bash
rustup target add aarch64-linux-android
```

### NDK not found

Ensure `ANDROID_NDK_HOME` points to the NDK directory containing `toolchains/llvm/`.

### C++ compilation errors

Check that `CXX_aarch64_linux_android` points to a valid clang++ from the NDK.

### Gradle daemon issues

```bash
./gradlew --stop
./gradlew assembleRelease --no-daemon
```
