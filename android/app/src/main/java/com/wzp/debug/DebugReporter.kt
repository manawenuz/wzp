package com.wzp.debug

import android.content.Context
import android.util.Log
import kotlinx.coroutines.Dispatchers
import kotlinx.coroutines.withContext
import java.io.BufferedOutputStream
import java.io.ByteArrayOutputStream
import java.io.File
import java.io.FileInputStream
import java.io.FileOutputStream
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.text.SimpleDateFormat
import java.util.Date
import java.util.Locale
import java.util.zip.ZipEntry
import java.util.zip.ZipOutputStream

/**
 * Collects call debug data (audio recordings, logs, histograms, stats)
 * into a zip file for email sharing.
 */
class DebugReporter(private val context: Context) {

    companion object {
        private const val TAG = "DebugReporter"
        private const val SAMPLE_RATE = 48000
    }

    /**
     * Build a zip with all debug data.
     * Returns the zip File on success, or null on failure.
     */
    suspend fun collectZip(
        callDurationSecs: Double,
        finalStatsJson: String,
        aecEnabled: Boolean,
        alias: String,
        server: String,
        room: String
    ): File? = withContext(Dispatchers.IO) {
        try {
            val debugDir = File(context.cacheDir, "wzp_debug")
            val timestamp = SimpleDateFormat("yyyyMMdd_HHmmss", Locale.US).format(Date())
            val zipFile = File(context.cacheDir, "wzp_debug_${timestamp}.zip")

            ZipOutputStream(BufferedOutputStream(FileOutputStream(zipFile))).use { zos ->
                // 1. Call metadata
                val meta = buildString {
                    appendLine("=== WZ Phone Debug Report ===")
                    appendLine("Timestamp: $timestamp")
                    appendLine("Alias: $alias")
                    appendLine("Server: $server")
                    appendLine("Room: $room")
                    appendLine("Duration: ${"%.1f".format(callDurationSecs)}s")
                    appendLine("AEC: ${if (aecEnabled) "ON" else "OFF"}")
                    appendLine("Device: ${android.os.Build.MANUFACTURER} ${android.os.Build.MODEL}")
                    appendLine("Android: ${android.os.Build.VERSION.RELEASE} (API ${android.os.Build.VERSION.SDK_INT})")
                    appendLine()
                    appendLine("=== Final Stats ===")
                    appendLine(finalStatsJson)
                }
                addTextEntry(zos, "meta.txt", meta)

                // 2. Logcat — WZP-related tags
                val logcat = collectLogcat()
                addTextEntry(zos, "logcat.txt", logcat)

                // 3. Capture audio (mic) → WAV
                val captureRaw = File(debugDir, "capture.pcm")
                if (captureRaw.exists() && captureRaw.length() > 0) {
                    addWavEntry(zos, "capture.wav", captureRaw)
                    Log.i(TAG, "capture.pcm: ${captureRaw.length()} bytes -> WAV")
                }

                // 4. Playout audio (speaker) → WAV
                val playoutRaw = File(debugDir, "playout.pcm")
                if (playoutRaw.exists() && playoutRaw.length() > 0) {
                    addWavEntry(zos, "playout.wav", playoutRaw)
                    Log.i(TAG, "playout.pcm: ${playoutRaw.length()} bytes -> WAV")
                }

                // 5. RMS histogram CSV
                val captureHist = File(debugDir, "capture_rms.csv")
                if (captureHist.exists()) addFileEntry(zos, "capture_rms.csv", captureHist)
                val playoutHist = File(debugDir, "playout_rms.csv")
                if (playoutHist.exists()) addFileEntry(zos, "playout_rms.csv", playoutHist)
            }

            Log.i(TAG, "zip created: ${zipFile.length()} bytes (${zipFile.length() / 1024}KB)")

            // Clean up raw debug files (keep zip)
            debugDir.listFiles()?.forEach { it.delete() }

            zipFile
        } catch (e: Exception) {
            Log.e(TAG, "debug report failed", e)
            null
        }
    }

    /** Clean up any leftover debug files from a previous session. */
    fun prepareForCall() {
        val debugDir = File(context.cacheDir, "wzp_debug")
        if (debugDir.exists()) {
            debugDir.listFiles()?.forEach { it.delete() }
        }
        debugDir.mkdirs()
        // Also clean up old zip files
        context.cacheDir.listFiles()?.filter { it.name.startsWith("wzp_debug_") }?.forEach { it.delete() }
    }

    private fun collectLogcat(): String {
        return try {
            val process = Runtime.getRuntime().exec(
                arrayOf(
                    "logcat", "-d",
                    "-t", "5000",
                    "--format", "threadtime"
                )
            )
            val output = process.inputStream.bufferedReader().readText()
            process.waitFor()
            output.lines()
                .filter { line ->
                    line.contains("wzp", ignoreCase = true) ||
                    line.contains("WzpEngine") ||
                    line.contains("AudioPipeline") ||
                    line.contains("WzpCall") ||
                    line.contains("CallService") ||
                    line.contains("AudioTrack") ||
                    line.contains("AudioRecord") ||
                    line.contains("AcousticEchoCanceler") ||
                    line.contains("NoiseSuppressor") ||
                    line.contains("FATAL") ||
                    line.contains("ANR") ||
                    line.contains("AudioFlinger") ||
                    line.contains("DebugReporter") ||
                    line.contains("QUIC") ||
                    line.contains("quinn")
                }
                .joinToString("\n")
        } catch (e: Exception) {
            "Failed to collect logcat: ${e.message}"
        }
    }

    private fun addWavEntry(zos: ZipOutputStream, name: String, pcmFile: File) {
        val dataSize = pcmFile.length().toInt()
        val byteRate = SAMPLE_RATE * 1 * 16 / 8
        val blockAlign = 1 * 16 / 8

        zos.putNextEntry(ZipEntry(name))

        // Write WAV header (44 bytes)
        val header = ByteBuffer.allocate(44).order(ByteOrder.LITTLE_ENDIAN)
        header.put("RIFF".toByteArray())
        header.putInt(36 + dataSize)
        header.put("WAVE".toByteArray())
        header.put("fmt ".toByteArray())
        header.putInt(16)
        header.putShort(1)   // PCM
        header.putShort(1)   // mono
        header.putInt(SAMPLE_RATE)
        header.putInt(byteRate)
        header.putShort(blockAlign.toShort())
        header.putShort(16)  // bits per sample
        header.put("data".toByteArray())
        header.putInt(dataSize)
        zos.write(header.array())

        // Stream PCM data directly (avoids loading entire file into memory)
        FileInputStream(pcmFile).use { it.copyTo(zos) }
        zos.closeEntry()
    }

    private fun addTextEntry(zos: ZipOutputStream, name: String, content: String) {
        zos.putNextEntry(ZipEntry(name))
        zos.write(content.toByteArray())
        zos.closeEntry()
    }

    private fun addFileEntry(zos: ZipOutputStream, name: String, file: File) {
        zos.putNextEntry(ZipEntry(name))
        FileInputStream(file).use { it.copyTo(zos) }
        zos.closeEntry()
    }
}
