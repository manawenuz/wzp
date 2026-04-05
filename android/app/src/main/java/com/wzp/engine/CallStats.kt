package com.wzp.engine

import org.json.JSONObject

/**
 * Snapshot of call statistics, mirroring the Rust `CallStats` struct.
 *
 * Constructed from the JSON string returned by [WzpEngine.getStats].
 */
data class CallStats(
    /** Current call state ordinal (see [CallStateConstants]). */
    val state: Int = 0,
    /** Call duration in seconds. */
    val durationSecs: Double = 0.0,
    /** Quality tier: 0 = Good, 1 = Degraded, 2 = Catastrophic. */
    val qualityTier: Int = 0,
    /** Observed packet loss percentage (0..100). */
    val lossPct: Float = 0f,
    /** Smoothed round-trip time in milliseconds. */
    val rttMs: Int = 0,
    /** Jitter in milliseconds. */
    val jitterMs: Int = 0,
    /** Current jitter buffer depth in packets. */
    val jitterBufferDepth: Int = 0,
    /** Total frames encoded since call start. */
    val framesEncoded: Long = 0,
    /** Total frames decoded since call start. */
    val framesDecoded: Long = 0,
    /** Number of playout underruns (buffer empty when audio was needed). */
    val underruns: Long = 0,
    /** Frames recovered by FEC. */
    val fecRecovered: Long = 0,
    /** Current mic audio level (RMS, 0-32767). */
    val audioLevel: Int = 0,
) {
    /** Human-readable quality label. */
    val qualityLabel: String
        get() = when (qualityTier) {
            0 -> "Good"
            1 -> "Degraded"
            2 -> "Catastrophic"
            else -> "Unknown"
        }

    companion object {
        /** Deserialise from the JSON string produced by the native engine. */
        fun fromJson(json: String): CallStats {
            return try {
                val obj = JSONObject(json)
                CallStats(
                    state = obj.optInt("state", 0),
                    durationSecs = obj.optDouble("duration_secs", 0.0),
                    qualityTier = obj.optInt("quality_tier", 0),
                    lossPct = obj.optDouble("loss_pct", 0.0).toFloat(),
                    rttMs = obj.optInt("rtt_ms", 0),
                    jitterMs = obj.optInt("jitter_ms", 0),
                    jitterBufferDepth = obj.optInt("jitter_buffer_depth", 0),
                    framesEncoded = obj.optLong("frames_encoded", 0),
                    framesDecoded = obj.optLong("frames_decoded", 0),
                    underruns = obj.optLong("underruns", 0),
                    fecRecovered = obj.optLong("fec_recovered", 0),
                    audioLevel = obj.optInt("audio_level", 0)
                )
            } catch (e: Exception) {
                CallStats()
            }
        }
    }
}
