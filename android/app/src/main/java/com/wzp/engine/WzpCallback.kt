package com.wzp.engine

/**
 * Callback interface for VoIP engine events.
 *
 * All callbacks are invoked on the main/UI thread.
 */
interface WzpCallback {

    /**
     * Called when the call state changes.
     *
     * @param state one of [CallStateConstants]: IDLE(0), CONNECTING(1), ACTIVE(2),
     *              RECONNECTING(3), CLOSED(4)
     */
    fun onCallStateChanged(state: Int)

    /**
     * Called when the network quality tier changes.
     *
     * @param tier 0 = Good, 1 = Degraded, 2 = Catastrophic
     */
    fun onQualityTierChanged(tier: Int)

    /**
     * Called when an error occurs in the native engine.
     *
     * @param code    numeric error code (negative)
     * @param message human-readable description
     */
    fun onError(code: Int, message: String)
}
