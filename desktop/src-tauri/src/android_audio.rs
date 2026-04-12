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

// ─── Bluetooth SCO routing ──────────────────────────────────────────────────

/// Start Bluetooth SCO audio routing.
///
/// On API 31+ uses `setCommunicationDevice()` which is the modern way to
/// route voice audio to a specific device. Falls back to the deprecated
/// `startBluetoothSco()` path on older APIs.
///
/// The caller must restart Oboe streams after this call.
pub fn start_bluetooth_sco() -> Result<(), String> {
    let (vm, activity) = jvm_and_activity()?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let am = audio_manager(&mut env, &activity)?;

    // Ensure speaker is off — mutually exclusive with BT.
    env.call_method(
        &am,
        "setSpeakerphoneOn",
        "(Z)V",
        &[JValue::Bool(0)],
    )
    .map_err(|e| format!("setSpeakerphoneOn(false): {e}"))?;

    // Try modern API first (API 31+): setCommunicationDevice(AudioDeviceInfo)
    // Find a BT SCO or BLE device from getAvailableCommunicationDevices()
    let used_modern = try_set_communication_device(&mut env, &am, true)?;

    if !used_modern {
        // Fallback: deprecated startBluetoothSco (API < 31)
        tracing::info!("start_bluetooth_sco: falling back to deprecated startBluetoothSco");
        env.call_method(&am, "startBluetoothSco", "()V", &[])
            .map_err(|e| format!("startBluetoothSco: {e}"))?;
    }

    tracing::info!(used_modern, "AudioManager: Bluetooth SCO started");
    Ok(())
}

/// Stop Bluetooth SCO audio routing, returning audio to the earpiece.
///
/// The caller must restart Oboe streams after this call.
pub fn stop_bluetooth_sco() -> Result<(), String> {
    let (vm, activity) = jvm_and_activity()?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let am = audio_manager(&mut env, &activity)?;

    // Modern API: clearCommunicationDevice() (API 31+)
    let cleared = try_set_communication_device(&mut env, &am, false)?;

    if !cleared {
        // Fallback: deprecated stopBluetoothSco
        env.call_method(&am, "stopBluetoothSco", "()V", &[])
            .map_err(|e| format!("stopBluetoothSco: {e}"))?;
    }

    tracing::info!(cleared, "AudioManager: Bluetooth SCO stopped");
    Ok(())
}

/// Try to use the modern `setCommunicationDevice` / `clearCommunicationDevice`
/// API (Android 12 / API 31+). Returns `true` if the modern API was used.
fn try_set_communication_device(
    env: &mut jni::AttachGuard<'_>,
    am: &JObject<'_>,
    enable: bool,
) -> Result<bool, String> {
    // Check SDK_INT >= 31 (Android 12)
    let sdk_int = env
        .get_static_field(
            "android/os/Build$VERSION",
            "SDK_INT",
            "I",
        )
        .and_then(|v| v.i())
        .unwrap_or(0);

    if sdk_int < 31 {
        return Ok(false);
    }

    if !enable {
        // clearCommunicationDevice()
        env.call_method(am, "clearCommunicationDevice", "()V", &[])
            .map_err(|e| format!("clearCommunicationDevice: {e}"))?;
        tracing::info!("clearCommunicationDevice: done");
        return Ok(true);
    }

    // getAvailableCommunicationDevices() → List<AudioDeviceInfo>
    let device_list = env
        .call_method(
            am,
            "getAvailableCommunicationDevices",
            "()Ljava/util/List;",
        )
        .and_then(|v| v.l())
        .map_err(|e| format!("getAvailableCommunicationDevices: {e}"))?;

    let size = env
        .call_method(&device_list, "size", "()I", &[])
        .and_then(|v| v.i())
        .unwrap_or(0);

    // Find first BT device: TYPE_BLUETOOTH_SCO (7), TYPE_BLUETOOTH_A2DP (8),
    // TYPE_BLE_HEADSET (26), TYPE_BLE_SPEAKER (27)
    for i in 0..size {
        let device = env
            .call_method(
                &device_list,
                "get",
                "(I)Ljava/lang/Object;",
                &[JValue::Int(i)],
            )
            .and_then(|v| v.l())
            .map_err(|e| format!("list.get({i}): {e}"))?;

        let device_type = env
            .call_method(&device, "getType", "()I", &[])
            .and_then(|v| v.i())
            .unwrap_or(0);

        // BT SCO = 7, A2DP = 8, BLE headset = 26, BLE speaker = 27
        if matches!(device_type, 7 | 8 | 26 | 27) {
            let ok = env
                .call_method(
                    am,
                    "setCommunicationDevice",
                    "(Landroid/media/AudioDeviceInfo;)Z",
                    &[JValue::Object(&device)],
                )
                .and_then(|v| v.z())
                .unwrap_or(false);

            tracing::info!(
                device_type,
                ok,
                "setCommunicationDevice: set BT device"
            );
            return Ok(ok);
        }
    }

    tracing::warn!("setCommunicationDevice: no BT device in available list");
    Ok(false)
}

/// Query whether Bluetooth audio is currently the active communication device.
///
/// On API 31+ checks `getCommunicationDevice()` type. Falls back to the
/// deprecated `isBluetoothScoOn()` on older APIs.
pub fn is_bluetooth_sco_on() -> Result<bool, String> {
    let (vm, activity) = jvm_and_activity()?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let am = audio_manager(&mut env, &activity)?;

    let sdk_int = env
        .get_static_field("android/os/Build$VERSION", "SDK_INT", "I")
        .and_then(|v| v.i())
        .unwrap_or(0);

    if sdk_int >= 31 {
        // getCommunicationDevice() → AudioDeviceInfo (nullable)
        let device = env
            .call_method(am, "getCommunicationDevice", "()Landroid/media/AudioDeviceInfo;", &[])
            .and_then(|v| v.l())
            .unwrap_or(JObject::null());
        if device.is_null() {
            return Ok(false);
        }
        let device_type = env
            .call_method(&device, "getType", "()I", &[])
            .and_then(|v| v.i())
            .unwrap_or(0);
        // BT SCO = 7, A2DP = 8, BLE headset = 26, BLE speaker = 27
        return Ok(matches!(device_type, 7 | 8 | 26 | 27));
    }

    // Fallback: deprecated API
    env.call_method(&am, "isBluetoothScoOn", "()Z", &[])
        .and_then(|v| v.z())
        .map_err(|e| format!("isBluetoothScoOn: {e}"))
}

/// Check whether a Bluetooth audio device is currently connected.
///
/// Iterates `AudioManager.getDevices(GET_DEVICES_OUTPUTS)` and looks for
/// any Bluetooth device type. Many headsets only register as A2DP until
/// SCO is explicitly started, so we check for both SCO and A2DP types.
pub fn is_bluetooth_available() -> Result<bool, String> {
    let (vm, activity) = jvm_and_activity()?;
    let mut env = vm
        .attach_current_thread()
        .map_err(|e| format!("attach_current_thread: {e}"))?;
    let am = audio_manager(&mut env, &activity)?;

    // AudioManager.GET_DEVICES_OUTPUTS = 2
    let devices = env
        .call_method(
            &am,
            "getDevices",
            "(I)[Landroid/media/AudioDeviceInfo;",
            &[JValue::Int(2)],
        )
        .and_then(|v| v.l())
        .map_err(|e| format!("getDevices(OUTPUTS): {e}"))?;

    let arr = jni::objects::JObjectArray::from(devices);
    let len = env
        .get_array_length(&arr)
        .map_err(|e| format!("get_array_length: {e}"))?;

    for i in 0..len {
        let device = env
            .get_object_array_element(&arr, i)
            .map_err(|e| format!("get_object_array_element({i}): {e}"))?;
        let device_type = env
            .call_method(&device, "getType", "()I", &[])
            .and_then(|v| v.i())
            .unwrap_or(0);
        // TYPE_BLUETOOTH_SCO = 7, TYPE_BLUETOOTH_A2DP = 8
        if device_type == 7 || device_type == 8 {
            tracing::info!(device_type, idx = i, "is_bluetooth_available: found BT device");
            return Ok(true);
        }
    }
    Ok(false)
}
