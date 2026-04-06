//! JNI bridge for Android — thin layer between Kotlin and the WzpEngine.

use std::panic;

use jni::objects::{JClass, JObject, JString};
use jni::sys::{jboolean, jint, jlong, jstring};
use jni::JNIEnv;
use tracing::{error, info};
use wzp_proto::QualityProfile;

use crate::engine::{CallStartConfig, WzpEngine};

/// Opaque engine handle passed to/from Kotlin as a `jlong`.
struct EngineHandle {
    engine: WzpEngine,
}

/// Recover the `EngineHandle` from a raw handle value.
unsafe fn handle_ref(handle: jlong) -> &'static mut EngineHandle {
    unsafe { &mut *(handle as *mut EngineHandle) }
}

fn profile_from_int(value: jint) -> QualityProfile {
    match value {
        1 => QualityProfile::DEGRADED,
        2 => QualityProfile::CATASTROPHIC,
        _ => QualityProfile::GOOD,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeInit(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    let result = panic::catch_unwind(|| {
        let handle = Box::new(EngineHandle {
            engine: WzpEngine::new(),
        });
        Box::into_raw(handle) as jlong
    });
    match result {
        Ok(h) => h,
        Err(_) => 0,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeStartCall(
    mut env: JNIEnv,
    _class: JClass,
    handle: jlong,
    relay_addr_j: JString,
    room_j: JString,
    seed_hex_j: JString,
    token_j: JString,
    alias_j: JString,
) -> jint {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let relay_addr: String = env.get_string(&relay_addr_j).map(|s| s.into()).unwrap_or_default();
        let room: String = env.get_string(&room_j).map(|s| s.into()).unwrap_or_default();
        let seed_hex: String = env.get_string(&seed_hex_j).map(|s| s.into()).unwrap_or_default();
        let token: String = env.get_string(&token_j).map(|s| s.into()).unwrap_or_default();
        let alias: String = env.get_string(&alias_j).map(|s| s.into()).unwrap_or_default();

        let h = unsafe { handle_ref(handle) };

        // Parse hex seed
        let mut identity_seed = [0u8; 32];
        if seed_hex.len() == 64 {
            for i in 0..32 {
                if let Ok(byte) = u8::from_str_radix(&seed_hex[i * 2..i * 2 + 2], 16) {
                    identity_seed[i] = byte;
                }
            }
        } else {
            // Generate random seed if not provided
            use rand::RngCore;
            rand::thread_rng().fill_bytes(&mut identity_seed);
        }

        let config = CallStartConfig {
            profile: QualityProfile::GOOD,
            relay_addr,
            room,
            auth_token: if token.is_empty() { Vec::new() } else { token.into_bytes() },
            identity_seed,
            alias: if alias.is_empty() { None } else { Some(alias) },
        };

        match h.engine.start_call(config) {
            Ok(()) => 0,
            Err(e) => {
                error!("start_call failed: {e}");
                -1
            }
        }
    }));

    match result {
        Ok(code) => code,
        Err(_) => -1,
    }
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeStopCall(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        h.engine.stop_call();
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeSetMute(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    muted: jboolean,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        h.engine.set_mute(muted != 0);
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeSetSpeaker(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    speaker: jboolean,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        h.engine.set_speaker(speaker != 0);
    }));
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeGetStats<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass,
    handle: jlong,
) -> jstring {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let stats = h.engine.get_stats();
        serde_json::to_string(&stats).unwrap_or_else(|_| "{}".to_string())
    }));

    let json = match result {
        Ok(s) => s,
        Err(_) => "{}".to_string(),
    };

    env.new_string(&json)
        .map(|s| s.into_raw())
        .unwrap_or(JObject::null().into_raw())
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeForceProfile(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
    profile: jint,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let qp = profile_from_int(profile);
        h.engine.force_profile(qp);
    }));
}

/// Write captured PCM samples from Kotlin AudioRecord into the engine's capture ring.
/// pcm is a Java short[] array.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeWriteAudio(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    pcm: jni::objects::JShortArray,
) -> jint {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let len = env.get_array_length(&pcm).unwrap_or(0) as usize;
        if len == 0 {
            return 0;
        }
        let mut buf = vec![0i16; len];
        // GetShortArrayRegion copies Java array into our buffer
        if env.get_short_array_region(&pcm, 0, &mut buf).is_err() {
            return 0;
        }
        h.engine.write_audio(&buf) as jint
    }));
    result.unwrap_or(0)
}

/// Read decoded PCM samples from the engine's playout ring for Kotlin AudioTrack.
/// pcm is a Java short[] array to fill. Returns number of samples actually read.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeReadAudio(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    pcm: jni::objects::JShortArray,
) -> jint {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let len = env.get_array_length(&pcm).unwrap_or(0) as usize;
        if len == 0 {
            return 0;
        }
        let mut buf = vec![0i16; len];
        let read = h.engine.read_audio(&mut buf);
        if read > 0 {
            let _ = env.set_short_array_region(&pcm, 0, &buf[..read]);
        }
        read as jint
    }));
    result.unwrap_or(0)
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeDestroy(
    _env: JNIEnv,
    _class: JClass,
    handle: jlong,
) {
    let _ = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { Box::from_raw(handle as *mut EngineHandle) };
        drop(h);
    }));
}
