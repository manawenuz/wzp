package com.wzp.net

/**
 * Relay ping via native QUIC — requires loading the native .so.
 * After ping completes, the process must be restarted (System.exit)
 * because jemalloc initialization during .so load corrupts state
 * on Android 16 MTE devices.
 *
 * Flow: ping all servers → save results → exit → app restarts → load results
 */
object RelayPinger {

    data class PingResult(
        val rttMs: Int,
        val reachable: Boolean,
        val serverFingerprint: String = "",
    )

    /**
     * Ping a relay via the native QUIC stack.
     * WARNING: After calling this, the process must be restarted.
     */
    fun ping(address: String): PingResult {
        return try {
            val json = com.wzp.engine.WzpEngine.pingRelay(address) ?: return PingResult(0, false)
            val obj = org.json.JSONObject(json)
            PingResult(
                rttMs = obj.getInt("rtt_ms"),
                reachable = true,
                serverFingerprint = obj.optString("server_fingerprint", ""),
            )
        } catch (e: Exception) {
            PingResult(0, false)
        }
    }
}
