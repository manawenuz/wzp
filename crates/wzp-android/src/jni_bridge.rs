//! JNI bridge for Android — thin layer between Kotlin and the WzpEngine.

use std::panic;
use std::sync::Once;

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

/// 7 = auto (use relay's chosen profile)
const PROFILE_AUTO: jint = 7;

fn profile_from_int(value: jint) -> QualityProfile {
    match value {
        0 => QualityProfile::GOOD,            // Opus 24k
        1 => QualityProfile::DEGRADED,        // Opus 6k
        2 => QualityProfile::CATASTROPHIC,    // Codec2 1.2k
        3 => QualityProfile {                 // Codec2 3.2k
            codec: wzp_proto::CodecId::Codec2_3200,
            fec_ratio: 0.5,
            frame_duration_ms: 20,
            frames_per_block: 5,
        },
        4 => QualityProfile::STUDIO_32K,      // Opus 32k
        5 => QualityProfile::STUDIO_48K,      // Opus 48k
        6 => QualityProfile::STUDIO_64K,      // Opus 64k
        _ => QualityProfile::GOOD,            // auto falls back to GOOD
    }
}

static INIT_LOGGING: Once = Once::new();

/// Initialize tracing → Android logcat (tag "wzp_android").
/// Safe to call multiple times — only the first call takes effect.
fn init_logging() {
    INIT_LOGGING.call_once(|| {
        // Wrap in catch_unwind — sharded_slab allocation inside
        // tracing_subscriber::registry() can crash on some Android
        // devices if scudo malloc fails during early initialization.
        let _ = std::panic::catch_unwind(|| {
            use tracing_subscriber::layer::SubscriberExt;
            use tracing_subscriber::util::SubscriberInitExt;
            use tracing_subscriber::EnvFilter;
            if let Ok(layer) = tracing_android::layer("wzp_android") {
                // Filter: INFO for our crates, WARN for everything else.
                // The jni crate emits VERBOSE logs for every method lookup
                // (~10 lines per JNI call, 100+ calls/sec) which floods logcat
                // and causes the system to kill the app.
                let filter = EnvFilter::new("warn,wzp_android=info,wzp_proto=info,wzp_transport=info,wzp_codec=info,wzp_fec=info,wzp_crypto=info");
                let _ = tracing_subscriber::registry()
                    .with(layer)
                    .with(filter)
                    .try_init();
            }
        });
    });
}

#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeInit(
    _env: JNIEnv,
    _class: JClass,
) -> jlong {
    let result = panic::catch_unwind(|| {
        init_logging();
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
    profile_j: jint,
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
            profile: profile_from_int(profile_j),
            auto_profile: profile_j == PROFILE_AUTO,
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

/// Write captured PCM from a DirectByteBuffer — zero JNI array copies.
/// The ByteBuffer must contain little-endian i16 samples.
/// Called from the AudioRecord capture thread.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeWriteAudioDirect(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    buffer: jni::objects::JByteBuffer,
    sample_count: jint,
) -> jint {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let ptr = env.get_direct_buffer_address(&buffer).unwrap_or(std::ptr::null_mut());
        if ptr.is_null() || sample_count <= 0 {
            return 0;
        }
        let samples = unsafe {
            std::slice::from_raw_parts(ptr as *const i16, sample_count as usize)
        };
        h.engine.write_audio(samples) as jint
    }));
    result.unwrap_or(0)
}

/// Read decoded PCM into a DirectByteBuffer — zero JNI array copies.
/// The ByteBuffer will be filled with little-endian i16 samples.
/// Called from the AudioTrack playout thread.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeReadAudioDirect(
    env: JNIEnv,
    _class: JClass,
    handle: jlong,
    buffer: jni::objects::JByteBuffer,
    max_samples: jint,
) -> jint {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let ptr = env.get_direct_buffer_address(&buffer).unwrap_or(std::ptr::null_mut());
        if ptr.is_null() || max_samples <= 0 {
            return 0;
        }
        let samples = unsafe {
            std::slice::from_raw_parts_mut(ptr as *mut i16, max_samples as usize)
        };
        h.engine.read_audio(samples) as jint
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

/// Ping a relay server — instance method, requires engine handle.
/// Returns JSON `{"rtt_ms":N,"server_fingerprint":"hex"}` or null on failure.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativePingRelay<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass,
    handle: jlong,
    relay_j: JString,
) -> jstring {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let relay: String = env.get_string(&relay_j).map(|s| s.into()).unwrap_or_default();
        match h.engine.ping_relay(&relay) {
            Ok(json) => Some(json),
            Err(_) => None,
        }
    }));

    let json = match result {
        Ok(Some(s)) => s,
        _ => return JObject::null().into_raw(),
    };
    env.new_string(&json)
        .map(|s| s.into_raw())
        .unwrap_or(JObject::null().into_raw())
}

// ── Direct calling JNI functions ──

/// Start persistent signaling connection to relay for direct calls.
/// Returns 0 on success, -1 on error.
/// NOTE: No panic::catch_unwind here — Android's Kotlin dispatcher thread has
/// very limited stack (~1MB) and the unwind machinery overflows it.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeStartSignaling<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass,
    handle: jlong,
    relay_addr_j: JString,
    seed_hex_j: JString,
    token_j: JString,
    alias_j: JString,
) -> jint {
    let h = unsafe { handle_ref(handle) };
    let relay_addr: String = env.get_string(&relay_addr_j).map(|s| s.into()).unwrap_or_default();
    let seed_hex: String = env.get_string(&seed_hex_j).map(|s| s.into()).unwrap_or_default();
    let token: String = env.get_string(&token_j).map(|s| s.into()).unwrap_or_default();
    let alias: String = env.get_string(&alias_j).map(|s| s.into()).unwrap_or_default();

    match h.engine.start_signaling(
        &relay_addr,
        &seed_hex,
        if token.is_empty() { None } else { Some(&token) },
        if alias.is_empty() { None } else { Some(&alias) },
    ) {
        Ok(()) => 0,
        Err(e) => { error!("start_signaling failed: {e}"); -1 }
    }
}

/// Place a direct call to a target fingerprint.
/// Returns 0 on success, -1 on error.
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativePlaceCall<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass,
    handle: jlong,
    target_fp_j: JString,
) -> jint {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let target: String = env.get_string(&target_fp_j).map(|s| s.into()).unwrap_or_default();
        h.engine.place_call(&target)
    }));

    match result {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => { error!("place_call failed: {e}"); -1 }
        Err(_) => { error!("place_call panicked"); -1 }
    }
}

/// Answer an incoming direct call.
/// mode: 0=Reject, 1=AcceptTrusted, 2=AcceptGeneric
#[unsafe(no_mangle)]
pub unsafe extern "system" fn Java_com_wzp_engine_WzpEngine_nativeAnswerCall<'a>(
    mut env: JNIEnv<'a>,
    _class: JClass,
    handle: jlong,
    call_id_j: JString,
    mode: jint,
) -> jint {
    let result = panic::catch_unwind(panic::AssertUnwindSafe(|| {
        let h = unsafe { handle_ref(handle) };
        let call_id: String = env.get_string(&call_id_j).map(|s| s.into()).unwrap_or_default();
        let accept_mode = match mode {
            0 => wzp_proto::CallAcceptMode::Reject,
            1 => wzp_proto::CallAcceptMode::AcceptTrusted,
            _ => wzp_proto::CallAcceptMode::AcceptGeneric,
        };
        h.engine.answer_call(&call_id, accept_mode)
    }));

    match result {
        Ok(Ok(())) => 0,
        Ok(Err(e)) => { error!("answer_call failed: {e}"); -1 }
        Err(_) => { error!("answer_call panicked"); -1 }
    }
}
