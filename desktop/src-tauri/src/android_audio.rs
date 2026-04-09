//! Runtime bridge to Android's `AudioManager` for in-call audio routing.
//!
//! We own a quinn+Oboe VoIP pipeline entirely from Rust, but routing the
//! playout stream between earpiece / loudspeaker / Bluetooth headset has to
//! happen at the JVM level because those toggles are AudioManager-only.
//! This module uses the global JavaVM handle that `ndk_context` exposes
//! (populated by Tauri's mobile runtime) + the `jni` crate to reach into
//! the Android framework without needing a Tauri plugin.
//!
//! All callers must be inside an Android target (`#[cfg(target_os = "android")]`).

#![cfg(target_os = "android")]

use jni::objects::{JObject, JString, JValue};
use jni::JavaVM;

/// Grab the JavaVM + current Activity from the ndk_context that Tauri's
/// mobile runtime sets up at process startup.
fn jvm_and_activity() -> Result<(JavaVM, JObject<'static>), String> {
    let ctx = ndk_context::android_context();
    let vm_ptr = ctx.vm() as *mut jni::sys::JavaVM;
    if vm_ptr.is_null() {
        return Err("ndk_context: JavaVM pointer is null".into());
    }
    let vm = unsafe { JavaVM::from_raw(vm_ptr) }
        .map_err(|e| format!("JavaVM::from_raw: {e}"))?;
    let activity_ptr = ctx.context() as jni::sys::jobject;
    if activity_ptr.is_null() {
        return Err("ndk_context: activity pointer is null".into());
    }
    // SAFETY: ndk_context guarantees the pointer lives for the process
    // lifetime; we wrap it as a JObject<'static> for convenience.
    let activity: JObject<'static> = unsafe { JObject::from_raw(activity_ptr) };
    Ok((vm, activity))
}

/// Get Android's `AudioManager` via `activity.getSystemService("audio")`.
fn audio_manager<'local>(
    env: &mut jni::AttachGuard<'local>,
    activity: &JObject<'local>,
) -> Result<JObject<'local>, String> {
    let svc_name: JString<'local> = env
        .new_string("audio")
        .map_err(|e| format!("new_string(audio): {e}"))?;
    let am = env
        .call_method(
            activity,
            "getSystemService",
            "(Ljava/lang/String;)Ljava/lang/Object;",
            &[JValue::Object(&svc_name)],
        )
        .and_then(|v| v.l())
        .map_err(|e| format!("getSystemService(audio): {e}"))?;
    if am.is_null() {
        return Err("getSystemService returned null".into());
    }
    Ok(am)
}

/// Switch between loud speaker (`true`) and earpiece/handset (`false`).
///
/// Calls `AudioManager.setSpeakerphoneOn(on)` on the JVM. Requires that
/// the audio mode is already `MODE_IN_COMMUNICATION` — MainActivity.kt
/// sets this at startup, so by the time a call is up this is always true.
pub fn set_speakerphone(on: bool) -> Result<(), String> {
    let (vm, activity) = jvm_and_activity()?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let am = audio_manager(&mut env, &activity)?;

    env.call_method(
        &am,
        "setSpeakerphoneOn",
        "(Z)V",
        &[JValue::Bool(if on { 1 } else { 0 })],
    )
    .map_err(|e| format!("setSpeakerphoneOn({on}): {e}"))?;

    tracing::info!(on, "AudioManager.setSpeakerphoneOn");
    Ok(())
}

/// Query the current speakerphone state. Returns true if routing is on the
/// loud speaker, false if on earpiece / BT headset / wired headset.
pub fn is_speakerphone_on() -> Result<bool, String> {
    let (vm, activity) = jvm_and_activity()?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let am = audio_manager(&mut env, &activity)?;

    let on = env
        .call_method(&am, "isSpeakerphoneOn", "()Z", &[])
        .and_then(|v| v.z())
        .map_err(|e| format!("isSpeakerphoneOn: {e}"))?;
    Ok(on)
}
