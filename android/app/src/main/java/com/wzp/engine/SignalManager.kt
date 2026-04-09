package com.wzp.engine

import org.json.JSONObject

/**
 * Persistent signal connection for direct 1:1 calls.
 * Separate from WzpEngine — survives across calls.
 *
 * Lifecycle: connect() → [placeCall/answerCall] → destroy()
 */
class SignalManager {

    private var handle: Long = 0L

    val isConnected: Boolean get() = handle != 0L

    /**
     * Connect to relay and register for direct calls.
     * MUST be called from a thread with sufficient stack (8MB).
     * Blocks briefly during QUIC connect + register, then returns.
     */
    fun connect(relay: String, seedHex: String): Boolean {
        if (handle != 0L) return true // already connected
        handle = nativeSignalConnect(relay, seedHex)
        return handle != 0L
    }

    /** Get current signal state as parsed object. Non-blocking. */
    fun getState(): SignalState {
        if (handle == 0L) return SignalState()
        val json = nativeSignalGetState(handle) ?: return SignalState()
        return try {
            val obj = JSONObject(json)
            SignalState(
                status = obj.optString("status", "idle"),
                fingerprint = obj.optString("fingerprint", ""),
                incomingCallId = if (obj.isNull("incoming_call_id")) null else obj.optString("incoming_call_id"),
                incomingCallerFp = if (obj.isNull("incoming_caller_fp")) null else obj.optString("incoming_caller_fp"),
                incomingCallerAlias = if (obj.isNull("incoming_caller_alias")) null else obj.optString("incoming_caller_alias"),
                callSetupRelay = if (obj.isNull("call_setup_relay")) null else obj.optString("call_setup_relay"),
                callSetupRoom = if (obj.isNull("call_setup_room")) null else obj.optString("call_setup_room"),
                callSetupId = if (obj.isNull("call_setup_id")) null else obj.optString("call_setup_id"),
            )
        } catch (e: Exception) {
            SignalState()
        }
    }

    /** Place a direct call to a target fingerprint. */
    fun placeCall(targetFp: String): Int {
        if (handle == 0L) return -1
        return nativeSignalPlaceCall(handle, targetFp)
    }

    /** Answer an incoming call. mode: 0=Reject, 1=AcceptTrusted, 2=AcceptGeneric */
    fun answerCall(callId: String, mode: Int = 2): Int {
        if (handle == 0L) return -1
        return nativeSignalAnswerCall(handle, callId, mode)
    }

    /** Send hangup signal. */
    fun hangup() {
        if (handle != 0L) nativeSignalHangup(handle)
    }

    /** Destroy the signal manager. */
    fun destroy() {
        if (handle != 0L) {
            nativeSignalDestroy(handle)
            handle = 0L
        }
    }

    // JNI native methods
    private external fun nativeSignalConnect(relay: String, seed: String): Long
    private external fun nativeSignalGetState(handle: Long): String?
    private external fun nativeSignalPlaceCall(handle: Long, targetFp: String): Int
    private external fun nativeSignalAnswerCall(handle: Long, callId: String, mode: Int): Int
    private external fun nativeSignalHangup(handle: Long)
    private external fun nativeSignalDestroy(handle: Long)

    companion object {
        init { System.loadLibrary("wzp_android") }
    }
}

/** Signal connection state. */
data class SignalState(
    val status: String = "idle",
    val fingerprint: String = "",
    val incomingCallId: String? = null,
    val incomingCallerFp: String? = null,
    val incomingCallerAlias: String? = null,
    val callSetupRelay: String? = null,
    val callSetupRoom: String? = null,
    val callSetupId: String? = null,
)
