package com.wzp.net

// Relay pinging is now done via WzpEngine.pingRelay() (instance method).
// This file kept for the data class only.

object RelayPinger {
    data class PingResult(
        val rttMs: Int,
        val reachable: Boolean,
        val serverFingerprint: String = "",
    )
}
