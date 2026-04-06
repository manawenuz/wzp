package com.wzp.engine

/**
 * Native VoIP engine wrapper. Delegates all work to libwzp_android.so via JNI.
 *
 * Lifecycle:
 * 1. Construct with a [WzpCallback]
 * 2. Call [init] to create the native engine
 * 3. Call [startCall] to begin a VoIP session
 * 4. Use [setMute], [setSpeaker], [getStats], [forceProfile] during the call
 * 5. Call [stopCall] to end the session
 * 6. Call [destroy] when the engine is no longer needed
 *
 * Thread safety: all methods must be called from the same thread (typically main).
 */
class WzpEngine(private val callback: WzpCallback) {

    /** Opaque pointer to the native EngineHandle. 0 means not initialised. */
    private var nativeHandle: Long = 0L

    /** Whether the engine has been initialised. */
    val isInitialized: Boolean get() = nativeHandle != 0L

    /** Create the native engine. Must be called before any other method. */
    fun init() {
        check(nativeHandle == 0L) { "Engine already initialized" }
        nativeHandle = nativeInit()
        check(nativeHandle != 0L) { "Native engine creation failed" }
    }

    /**
     * Start a call.
     *
     * @param relayAddr relay server address (host:port)
     * @param room      room identifier (used as QUIC SNI)
     * @param seedHex   64-char hex-encoded 32-byte identity seed (empty = random)
     * @param token     authentication token (empty = no auth)
     * @param alias     display name sent to relay for room participant list
     * @return 0 on success, negative error code on failure
     */
    fun startCall(relayAddr: String, room: String, seedHex: String = "", token: String = "", alias: String = ""): Int {
        check(nativeHandle != 0L) { "Engine not initialized" }
        val result = nativeStartCall(nativeHandle, relayAddr, room, seedHex, token, alias)
        if (result == 0) {
            callback.onCallStateChanged(CallStateConstants.CONNECTING)
        } else {
            callback.onError(result, "Failed to start call")
        }
        return result
    }

    /** Stop the active call. Safe to call when no call is active. */
    fun stopCall() {
        if (nativeHandle != 0L) {
            nativeStopCall(nativeHandle)
            callback.onCallStateChanged(CallStateConstants.CLOSED)
        }
    }

    /** Mute or unmute the microphone. */
    fun setMute(muted: Boolean) {
        if (nativeHandle != 0L) nativeSetMute(nativeHandle, muted)
    }

    /** Enable or disable loudspeaker mode. */
    fun setSpeaker(speaker: Boolean) {
        if (nativeHandle != 0L) nativeSetSpeaker(nativeHandle, speaker)
    }


    /**
     * Get current call statistics as a JSON string.
     *
     * @return JSON-serialised [CallStats], or `"{}"` if the engine is not initialised.
     */
    fun getStats(): String {
        if (nativeHandle == 0L) return "{}"
        return try {
            nativeGetStats(nativeHandle) ?: "{}"
        } catch (_: Exception) {
            "{}"
        }
    }

    /**
     * Force a quality profile, overriding adaptive selection.
     *
     * @param profile 0 = GOOD, 1 = DEGRADED, 2 = CATASTROPHIC
     */
    fun forceProfile(profile: Int) {
        if (nativeHandle != 0L) nativeForceProfile(nativeHandle, profile)
    }

    /** Destroy the native engine and free all resources. The instance must not be reused. */
    fun destroy() {
        if (nativeHandle != 0L) {
            nativeDestroy(nativeHandle)
            nativeHandle = 0L
        }
    }

    /**
     * Write captured PCM samples into the engine's capture ring buffer.
     * Called from the AudioRecord capture thread.
     */
    fun writeAudio(pcm: ShortArray): Int {
        if (nativeHandle == 0L) return 0
        return nativeWriteAudio(nativeHandle, pcm)
    }

    /**
     * Read decoded PCM samples from the engine's playout ring buffer.
     * Called from the AudioTrack playout thread.
     */
    fun readAudio(pcm: ShortArray): Int {
        if (nativeHandle == 0L) return 0
        return nativeReadAudio(nativeHandle, pcm)
    }

    /**
     * Write captured PCM from a DirectByteBuffer — zero JNI array copy.
     * The buffer must be a direct ByteBuffer with native byte order containing i16 samples.
     * Called from the AudioRecord capture thread.
     */
    fun writeAudioDirect(buffer: java.nio.ByteBuffer, sampleCount: Int): Int {
        if (nativeHandle == 0L) return 0
        return nativeWriteAudioDirect(nativeHandle, buffer, sampleCount)
    }

    /**
     * Read decoded PCM into a DirectByteBuffer — zero JNI array copy.
     * The buffer must be a direct ByteBuffer with native byte order.
     * Called from the AudioTrack playout thread.
     */
    fun readAudioDirect(buffer: java.nio.ByteBuffer, maxSamples: Int): Int {
        if (nativeHandle == 0L) return 0
        return nativeReadAudioDirect(nativeHandle, buffer, maxSamples)
    }

    // -- JNI native methods --------------------------------------------------

    private external fun nativeInit(): Long
    private external fun nativeStartCall(
        handle: Long, relay: String, room: String, seed: String, token: String, alias: String
    ): Int
    private external fun nativeStopCall(handle: Long)
    private external fun nativeSetMute(handle: Long, muted: Boolean)
    private external fun nativeSetSpeaker(handle: Long, speaker: Boolean)
    private external fun nativeGetStats(handle: Long): String?
    private external fun nativeForceProfile(handle: Long, profile: Int)
    private external fun nativeWriteAudio(handle: Long, pcm: ShortArray): Int
    private external fun nativeReadAudio(handle: Long, pcm: ShortArray): Int
    private external fun nativeWriteAudioDirect(handle: Long, buffer: java.nio.ByteBuffer, sampleCount: Int): Int
    private external fun nativeReadAudioDirect(handle: Long, buffer: java.nio.ByteBuffer, maxSamples: Int): Int
    private external fun nativeDestroy(handle: Long)

    companion object {
        init {
            System.loadLibrary("wzp_android")
        }

        /**
         * Ping a relay server. Returns JSON `{"rtt_ms":N,"server_fingerprint":"hex"}`
         * or null if unreachable. Does not require an engine instance.
         */
        fun pingRelay(address: String): String? = nativePingRelay(address)

        @JvmStatic
        private external fun nativePingRelay(relay: String): String?
    }
}

/** Integer constants matching the Rust [CallState] enum ordinals. */
object CallStateConstants {
    const val IDLE = 0
    const val CONNECTING = 1
    const val ACTIVE = 2
    const val RECONNECTING = 3
    const val CLOSED = 4
}
