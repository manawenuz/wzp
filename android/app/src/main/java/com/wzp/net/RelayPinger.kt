package com.wzp.net

import android.util.Log
import java.net.DatagramPacket
import java.net.DatagramSocket
import java.net.InetSocketAddress

/**
 * Pure Kotlin UDP ping — no JNI, no native lib loading.
 * Sends a minimal packet to the relay and measures response time.
 * QUIC servers reply with Version Negotiation to unknown packets.
 */
object RelayPinger {
    private const val TAG = "RelayPinger"
    private const val TIMEOUT_MS = 2000

    // Minimal QUIC-like Initial packet (just enough to provoke a response)
    // First byte 0xC0 = long header, version 0x00000000 = version negotiation trigger
    private val PROBE = byteArrayOf(
        0xC0.toByte(), // long header form
        0x00, 0x00, 0x00, 0x00, // version 0 → triggers Version Negotiation
        0x08, // DCID length = 8
        0x01, 0x02, 0x03, 0x04, 0x05, 0x06, 0x07, 0x08, // fake DCID
        0x00, // SCID length = 0
        0x00, 0x00, // token length = 0 (for Initial)
        0x00, 0x04, // payload length = 4
        0x00, 0x00, 0x00, 0x00, // dummy payload
    )

    data class PingResult(
        val rttMs: Int,
        val reachable: Boolean,
    )

    /**
     * Ping a relay server via UDP. Returns RTT in ms, or unreachable.
     * Thread-safe, can be called from coroutine on Dispatchers.IO.
     */
    fun ping(address: String): PingResult {
        return try {
            val parts = address.split(":")
            if (parts.size != 2) return PingResult(0, false)
            val host = parts[0]
            val port = parts[1].toIntOrNull() ?: return PingResult(0, false)

            val socket = DatagramSocket()
            socket.soTimeout = TIMEOUT_MS
            val dest = InetSocketAddress(host, port)

            val sendPacket = DatagramPacket(PROBE, PROBE.size, dest)
            val recvBuf = ByteArray(1200)
            val recvPacket = DatagramPacket(recvBuf, recvBuf.size)

            val start = System.nanoTime()
            socket.send(sendPacket)
            socket.receive(recvPacket) // blocks until response or timeout
            val rttMs = ((System.nanoTime() - start) / 1_000_000).toInt()

            socket.close()
            PingResult(rttMs, true)
        } catch (e: Exception) {
            Log.w(TAG, "ping $address failed: ${e.message}")
            PingResult(0, false)
        }
    }
}
