//! JNI bridge for Android — thin layer between Kotlin and the WzpEngine.
//!
//! Each function converts JNI types to Rust types, delegates to WzpEngine,
//! and converts results back. No audio processing happens here.
//!
//! # Safety
//!
//! All functions in this module are called from the JVM via JNI. They use raw
//! pointers for the JNI environment and object references. The `jni` crate is
//! not yet a dependency, so we use raw FFI types and placeholder string extraction.
//! When the `jni` crate is added, the `extract_jstring` helper should be replaced
//! with proper `JNIEnv::get_string()` calls.

use std::os::raw::{c_long, c_void};
use std::panic;

use tracing::{error, info};
use wzp_proto::QualityProfile;

use crate::engine::{CallStartConfig, WzpEngine};

/// Opaque engine handle passed to/from Kotlin as a `jlong`.
///
/// Boxed on the heap; the raw pointer is stored on the Kotlin side.
/// Only `nativeDestroy` frees it.
struct EngineHandle {
    engine: WzpEngine,
}

// ---------------------------------------------------------------------------
// JNI type aliases (mirrors the C JNI ABI without pulling in the `jni` crate)
// ---------------------------------------------------------------------------

/// JNI boolean — `u8` where 0 = false, non-zero = true.
type JBoolean = u8;

/// JNI int — `i32`.
type JInt = i32;

/// JNI long — `i64` / `c_long` on 64-bit.
type JLong = c_long;

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Recover the `EngineHandle` from a raw handle value **without** taking ownership.
///
/// # Safety
/// `handle` must be a value previously returned by `nativeInit` and not yet
/// passed to `nativeDestroy`.
unsafe fn handle_ref(handle: JLong) -> &'static mut EngineHandle {
    unsafe { &mut *(handle as *mut EngineHandle) }
}

/// Placeholder: extract a `String` from a JNI `jstring`.
///
/// When the `jni` crate is added this should be replaced with:
/// ```ignore
/// let env = JNIEnv::from_raw(env_ptr).unwrap();
/// env.get_string(jstring).unwrap().into()
/// ```
///
/// # Safety
/// `_env` and `_jstring` are raw JNI pointers.
#[allow(unused)]
unsafe fn extract_jstring(_env: *mut c_void, _jstring: *mut c_void) -> String {
    // TODO(jni): implement real string extraction once the `jni` crate is added.
    // For now return a default so the rest of the bridge compiles and can be tested
    // with hardcoded values from the Kotlin side.
    String::new()
}

/// Allocate a JNI `jstring` from a Rust `&str`.
///
/// # Safety
/// `_env` is a raw JNI pointer.
#[allow(unused)]
unsafe fn new_jstring(_env: *mut c_void, _s: &str) -> *mut c_void {
    // TODO(jni): implement via JNIEnv::new_string when jni crate is added.
    std::ptr::null_mut()
}

/// Map a Kotlin `profile` int to a `QualityProfile`.
fn profile_from_int(value: JInt) -> QualityProfile {
    match value {
        1 => QualityProfile::DEGRADED,
        2 => QualityProfile::CATASTROPHIC,
        _ => QualityProfile::GOOD,
    }
}

// ---------------------------------------------------------------------------
// JNI exports
// ---------------------------------------------------------------------------
// Function names follow JNI convention: Java_<package>_<Class>_<method>
// with underscores in the package replaced by `_1` in actual JNI but here we
// use the simplified form that matches javah output for the package `com.wzp.engine`.

/// Create a new `WzpEngine`, returning an opaque handle as `jlong`.
///
/// Kotlin signature: `private external fun nativeInit(): Long`
///
/// # Safety
/// Called from JNI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeInit(
    _env: *mut c_void,
    _class: *mut c_void,
) -> JLong {
    let result = panic::catch_unwind(|| {
        // Note: tracing on Android requires android_logger or similar.
        // fmt() subscriber writes to stdout which doesn't exist on Android.
        // Skip tracing init here — add android_logger later.

        let handle = Box::new(EngineHandle {
            engine: WzpEngine::new(),
        });
        info!("WzpEngine created via JNI");
        Box::into_raw(handle) as JLong
    });

    match result {
        Ok(h) => h,
        Err(_) => {
            error!("panic in nativeInit");
            0 // null handle — Kotlin side checks for 0
        }
    }
}

/// Start a call.
///
/// Kotlin signature:
/// ```kotlin
/// private external fun nativeStartCall(
///     handle: Long, relay: String, room: String, seed: String, token: String
/// ): Int
/// ```
///
/// Returns 0 on success, -1 on error.
///
/// # Safety
/// Called from JNI. `handle` must be a live engine handle.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeStartCall(
    env: *mut c_void,
    _class: *mut c_void,
    handle: JLong,
    relay_addr_ptr: *mut c_void,
    room_ptr: *mut c_void,
    seed_hex_ptr: *mut c_void,
    token_ptr: *mut c_void,
) -> JInt {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };

        // Extract strings from JNI. When the `jni` crate is available these
        // will use real JNI string conversion. For now, placeholders.
        let relay_addr = unsafe { extract_jstring(env, relay_addr_ptr) };
        let _room = unsafe { extract_jstring(env, room_ptr) };
        let seed_hex = unsafe { extract_jstring(env, seed_hex_ptr) };
        let token = unsafe { extract_jstring(env, token_ptr) };

        // Parse the hex-encoded 32-byte identity seed.
        let mut identity_seed = [0u8; 32];
        if seed_hex.len() == 64 {
            for i in 0..32 {
                if let Ok(byte) = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16) {
                    identity_seed[i] = byte;
                }
            }
        }

        let config = CallStartConfig {
            profile: QualityProfile::GOOD,
            relay_addr,
            auth_token: token.into_bytes(),
            identity_seed,
        };

        match h.engine.start_call(config) {
            Ok(()) => {
                info!("call started via JNI");
                0
            }
            Err(e) => {
                error!("start_call failed: {e}");
                -1
            }
        }
    }));

    match result {
        Ok(code) => code,
        Err(_) => {
            error!("panic in nativeStartCall");
            -1
        }
    }
}

/// Stop the active call.
///
/// Kotlin signature: `private external fun nativeStopCall(handle: Long)`
///
/// # Safety
/// Called from JNI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeStopCall(
    _env: *mut c_void,
    _class: *mut c_void,
    handle: JLong,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        h.engine.stop_call();
        info!("call stopped via JNI");
    }));
}

/// Set microphone mute state.
///
/// Kotlin signature: `private external fun nativeSetMute(handle: Long, muted: Boolean)`
///
/// # Safety
/// Called from JNI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeSetMute(
    _env: *mut c_void,
    _class: *mut c_void,
    handle: JLong,
    muted: JBoolean,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let muted = muted != 0;
        h.engine.set_mute(muted);
        info!(muted, "mute set via JNI");
    }));
}

/// Set speaker (loudspeaker) mode.
///
/// Kotlin signature: `private external fun nativeSetSpeaker(handle: Long, speaker: Boolean)`
///
/// # Safety
/// Called from JNI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeSetSpeaker(
    _env: *mut c_void,
    _class: *mut c_void,
    handle: JLong,
    speaker: JBoolean,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let speaker = speaker != 0;
        h.engine.set_speaker(speaker);
        info!(speaker, "speaker set via JNI");
    }));
}

/// Get call statistics as a JSON string.
///
/// Kotlin signature: `private external fun nativeGetStats(handle: Long): String`
///
/// Returns a JSON-serialized `CallStats` struct, or `"{}"` on error.
///
/// # Safety
/// Called from JNI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeGetStats(
    env: *mut c_void,
    _class: *mut c_void,
    handle: JLong,
) -> *mut c_void {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let stats = h.engine.get_stats();
        match serde_json::to_string(&stats) {
            Ok(json) => unsafe { new_jstring(env, &json) },
            Err(e) => {
                error!("failed to serialize stats: {e}");
                unsafe { new_jstring(env, "{}") }
            }
        }
    }));

    match result {
        Ok(ptr) => ptr,
        Err(_) => {
            error!("panic in nativeGetStats");
            unsafe { new_jstring(env, "{}") }
        }
    }
}

/// Force a specific quality profile, overriding adaptive logic.
///
/// Kotlin signature: `private external fun nativeForceProfile(handle: Long, profile: Int)`
///
/// Profile values: 0 = GOOD, 1 = DEGRADED, 2 = CATASTROPHIC.
///
/// # Safety
/// Called from JNI.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeForceProfile(
    _env: *mut c_void,
    _class: *mut c_void,
    handle: JLong,
    profile: JInt,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let qp = profile_from_int(profile);
        h.engine.force_profile(qp);
        info!(?qp, "profile forced via JNI");
    }));
}

/// Destroy the engine and free all associated memory.
///
/// After this call the handle is invalid and must not be reused.
///
/// Kotlin signature: `private external fun nativeDestroy(handle: Long)`
///
/// # Safety
/// Called from JNI. `handle` must be a live engine handle. After this call
/// the handle is dangling.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeDestroy(
    _env: *mut c_void,
    _class: *mut c_void,
    handle: JLong,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        // Retake ownership of the Box and drop it, which calls WzpEngine::drop()
        // and in turn stop_call().
        let h = unsafe { Box::from_raw(handle as *mut EngineHandle) };
        drop(h);
        info!("engine destroyed via JNI");
    }));
}
