# Debugging Guide

## Crash on Launch

### Symptom: App crashes immediately after opening

**Most likely cause: Namespace mismatch in AndroidManifest.xml**

The Gradle namespace is `com.wzp.phone` but all Kotlin classes are in package `com.wzp.*`. If the manifest uses shorthand names (`.WzpApplication`, `.ui.call.CallActivity`), Android resolves them as `com.wzp.phone.WzpApplication` which doesn't exist.

**Fix**: Always use fully-qualified class names in the manifest:

```xml
<!-- WRONG -->
<application android:name=".WzpApplication">
    <activity android:name=".ui.call.CallActivity">

<!-- CORRECT -->
<application android:name="com.wzp.WzpApplication">
    <activity android:name="com.wzp.ui.call.CallActivity">
```

### Symptom: Crash in `System.loadLibrary("wzp_android")`

The native `.so` is missing or incompatible. Check:

```bash
# Verify the .so exists in the APK
unzip -l app-release.apk | grep libwzp
# Should show: lib/arm64-v8a/libwzp_android.so

# Verify ABI matches device
adb shell getprop ro.product.cpu.abi
# Should return: arm64-v8a
```

### Symptom: Crash when calling `nativeGetStats()` (returns null jstring)

The JNI bridge must return a valid `jstring`, not a null pointer. The Kotlin side declares the return as `String?` (nullable) and wraps in try/catch:

```kotlin
fun getStats(): String {
    if (nativeHandle == 0L) return "{}"
    return try {
        nativeGetStats(nativeHandle) ?: "{}"
    } catch (_: Exception) {
        "{}"
    }
}
```

### Symptom: Tracing subscriber panic

`tracing_subscriber::fmt()` writes to stdout, which doesn't exist on Android. The init was removed. If you need logging, use `android_logger` crate instead.

## Logcat Filters

### View all WZP logs

```bash
adb logcat -s wzp-android:V wzp-codec:V wzp-net:V
```

### View Rust tracing output (if android_logger is added)

```bash
adb logcat | grep -E "(wzp|WzpEngine|CallActivity)"
```

### View Oboe audio logs

```bash
adb logcat -s AAudio:V oboe:V
```

### View native crashes

```bash
adb logcat -s DEBUG:V libc:V
```

Look for `signal 11 (SIGSEGV)` or `signal 6 (SIGABRT)` with a backtrace in `libwzp_android.so`.

### Symbolicate native crash

```bash
# Find the .so with debug symbols (before stripping)
SO_PATH="target/aarch64-linux-android/release/libwzp_android.so"

# Use addr2line from NDK
$ANDROID_NDK_HOME/toolchains/llvm/prebuilt/linux-x86_64/bin/llvm-addr2line \
    -e $SO_PATH -f 0x<address_from_crash>
```

## Network Issues

### Call stuck on "Connecting..."

The QUIC handshake to the relay is failing. Common causes:

1. **Relay not running**: Verify the relay is listening:
   ```bash
   nc -zvu 172.16.81.125 4433
   ```

2. **Wrong relay address**: Hardcoded in `CallViewModel.kt`:
   ```kotlin
   const val DEFAULT_RELAY = "172.16.81.125:4433"
   ```

3. **QUIC blocked by firewall**: QUIC uses UDP. Many networks block UDP traffic. Ensure UDP port 4433 is open.

4. **TLS handshake failure**: The client uses `client_config()` which disables certificate verification. If the relay's QUIC config changed, this may fail.

### Connected but no audio

1. **Microphone permission denied**: Check Android settings. The app requests `RECORD_AUDIO` on first launch.

2. **Oboe failed to start**: The codec thread logs this. Check logcat for "failed to start audio".

3. **Ring buffer underrun**: The stats overlay shows "Under" count. High underruns mean the codec thread isn't keeping up.

4. **Network not forwarding**: If both phones show "Active" but frame counters aren't increasing, the relay may not be forwarding. Check relay logs.

### High packet loss

The stats overlay shows loss percentage. Common causes:

- Wi-Fi congestion (try cellular or move closer to AP)
- UDP throttling by carrier/ISP
- Relay overloaded (check relay metrics)

## Audio Issues

### Echo

AEC (Acoustic Echo Cancellation) is enabled by default with a 100ms tail. If echo persists:

- The AEC may need a longer tail for the specific acoustic environment
- Speaker volume too high overwhelms the canceller
- Check that `last_decoded_farend` is being set (playout path working)

### Robot voice / glitching

Usually caused by jitter buffer underruns. The jitter buffer adapts between 10-250 packets. Check:

- `jitter_buffer_depth` in stats (should be > 0 during active call)
- `underruns` counter (should not climb rapidly)
- Network jitter (high jitter_ms causes adaptation)

### No sound from speaker

1. Check `isSpeaker` state in the UI
2. Oboe playout stream may have failed — check logcat for Oboe errors
3. Ring buffer might be empty — check `framesDecoded` counter

## JNI Issues

### `UnsatisfiedLinkError: No implementation found for...`

The JNI function name doesn't match. JNI names must follow the pattern:
```
Java_com_wzp_engine_WzpEngine_<methodName>
```

If the package structure changes, all JNI function names must be updated in `jni_bridge.rs`.

### Panic across FFI boundary

All JNI functions wrap their body in `panic::catch_unwind()`. If a Rust panic escapes to Java, it causes a `SIGABRT`. The catch_unwind returns safe defaults:

| Function | Panic return |
|----------|--------------|
| `nativeInit` | 0 (null handle) |
| `nativeStartCall` | -1 (error) |
| `nativeGetStats` | `JObject::null()` |
| Others | void (silently swallowed) |

### Thread safety

All JNI methods must be called from the same thread (Android main thread). The `EngineHandle` is a raw pointer — concurrent access is undefined behavior.

## Stats JSON Format

The `nativeGetStats()` returns JSON matching this Rust struct:

```json
{
  "state": "Active",
  "duration_secs": 42.5,
  "quality_tier": 0,
  "loss_pct": 0.5,
  "rtt_ms": 45,
  "jitter_ms": 12,
  "jitter_buffer_depth": 3,
  "frames_encoded": 2125,
  "frames_decoded": 2100,
  "underruns": 5
}
```

Kotlin deserializes this via `CallStats.fromJson()` using `org.json.JSONObject` (Android built-in, no library needed).

## Diagnostic Checklist

When something doesn't work, check in this order:

1. **APK installed for correct ABI?** (`arm64-v8a` only)
2. **Manifest class names fully qualified?** (no dots prefix)
3. **Relay running and reachable?** (`nc -zvu <host> <port>`)
4. **Microphone permission granted?**
5. **Stats polling working?** (check if frame counters increment)
6. **Logcat for native crashes?** (`adb logcat -s DEBUG:V`)
7. **Network connectivity?** (UDP port open, no firewall)
