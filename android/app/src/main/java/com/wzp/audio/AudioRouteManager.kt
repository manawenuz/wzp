package com.wzp.audio

import android.content.Context
import android.media.AudioDeviceCallback
import android.media.AudioDeviceInfo
import android.media.AudioManager
import android.os.Handler
import android.os.Looper

/**
 * Manages audio routing between earpiece, speaker, and Bluetooth devices.
 *
 * Wraps [AudioManager] operations and listens for device connection changes
 * via [AudioDeviceCallback] (API 23+).
 *
 * Usage:
 * 1. Call [register] when the call starts
 * 2. Use [setSpeaker] and [setBluetoothSco] to switch routes
 * 3. Call [unregister] when the call ends
 */
class AudioRouteManager(context: Context) {

    private val audioManager = context.getSystemService(Context.AUDIO_SERVICE) as AudioManager
    private val mainHandler = Handler(Looper.getMainLooper())

    /** Listener for audio route changes. */
    var onRouteChanged: ((AudioRoute) -> Unit)? = null

    /** Current active route. */
    var currentRoute: AudioRoute = AudioRoute.EARPIECE
        private set

    // -- Device callback (API 23+) -------------------------------------------

    private val deviceCallback = object : AudioDeviceCallback() {
        override fun onAudioDevicesAdded(addedDevices: Array<out AudioDeviceInfo>) {
            for (device in addedDevices) {
                if (device.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO) {
                    // A Bluetooth headset was connected — optionally auto-switch
                    onRouteChanged?.invoke(AudioRoute.BLUETOOTH)
                }
            }
        }

        override fun onAudioDevicesRemoved(removedDevices: Array<out AudioDeviceInfo>) {
            for (device in removedDevices) {
                if (device.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO) {
                    // Bluetooth disconnected — fall back to earpiece or speaker
                    val fallback = if (audioManager.isSpeakerphoneOn) {
                        AudioRoute.SPEAKER
                    } else {
                        AudioRoute.EARPIECE
                    }
                    currentRoute = fallback
                    onRouteChanged?.invoke(fallback)
                }
            }
        }
    }

    // -- Public API -----------------------------------------------------------

    /** Register the device callback. Call when a call starts. */
    fun register() {
        audioManager.registerAudioDeviceCallback(deviceCallback, mainHandler)
    }

    /** Unregister the device callback and release Bluetooth SCO. Call when the call ends. */
    fun unregister() {
        audioManager.unregisterAudioDeviceCallback(deviceCallback)
        stopBluetoothSco()
    }

    /**
     * Enable or disable the loudspeaker.
     *
     * When enabling speaker, Bluetooth SCO is disconnected.
     */
    @Suppress("DEPRECATION")
    fun setSpeaker(enabled: Boolean) {
        if (enabled) {
            stopBluetoothSco()
        }
        audioManager.isSpeakerphoneOn = enabled
        currentRoute = if (enabled) AudioRoute.SPEAKER else AudioRoute.EARPIECE
        onRouteChanged?.invoke(currentRoute)
    }

    /**
     * Enable or disable Bluetooth SCO (Synchronous Connection Oriented) audio.
     *
     * When enabling Bluetooth, the speaker is turned off.
     */
    @Suppress("DEPRECATION")
    fun setBluetoothSco(enabled: Boolean) {
        if (enabled) {
            audioManager.isSpeakerphoneOn = false
            audioManager.startBluetoothSco()
            audioManager.isBluetoothScoOn = true
            currentRoute = AudioRoute.BLUETOOTH
        } else {
            stopBluetoothSco()
            currentRoute = AudioRoute.EARPIECE
        }
        onRouteChanged?.invoke(currentRoute)
    }

    /** Check whether a Bluetooth SCO device is currently connected. */
    fun isBluetoothAvailable(): Boolean {
        val devices = audioManager.getDevices(AudioManager.GET_DEVICES_OUTPUTS)
        return devices.any { it.type == AudioDeviceInfo.TYPE_BLUETOOTH_SCO }
    }

    /** List available output audio routes. */
    fun availableRoutes(): List<AudioRoute> {
        val routes = mutableListOf(AudioRoute.EARPIECE, AudioRoute.SPEAKER)
        if (isBluetoothAvailable()) {
            routes.add(AudioRoute.BLUETOOTH)
        }
        return routes
    }

    // -- Internal -------------------------------------------------------------

    @Suppress("DEPRECATION")
    private fun stopBluetoothSco() {
        if (audioManager.isBluetoothScoOn) {
            audioManager.isBluetoothScoOn = false
            audioManager.stopBluetoothSco()
        }
    }
}

/** Audio output route. */
enum class AudioRoute {
    /** Phone earpiece (default for calls). */
    EARPIECE,
    /** Built-in loudspeaker. */
    SPEAKER,
    /** Bluetooth SCO headset/headphones. */
    BLUETOOTH
}
