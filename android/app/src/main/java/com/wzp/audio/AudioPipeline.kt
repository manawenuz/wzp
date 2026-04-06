package com.wzp.audio

import android.Manifest
import android.content.Context
import android.content.pm.PackageManager
import android.media.AudioAttributes
import android.media.AudioFormat
import android.media.AudioRecord
import android.media.AudioTrack
import android.media.MediaRecorder
import android.media.audiofx.AcousticEchoCanceler
import android.media.audiofx.NoiseSuppressor
import android.util.Log
import androidx.core.content.ContextCompat
import com.wzp.engine.WzpEngine
import java.io.BufferedOutputStream
import java.io.File
import java.io.FileOutputStream
import java.io.OutputStreamWriter
import java.nio.ByteBuffer
import java.nio.ByteOrder
import java.util.concurrent.CountDownLatch
import java.util.concurrent.TimeUnit
import kotlin.math.pow
import kotlin.math.sqrt

/**
 * Audio pipeline that captures mic audio and plays received audio using
 * Android AudioRecord/AudioTrack APIs running on JVM threads.
 *
 * PCM samples are shuttled to/from the Rust engine via JNI ring buffers:
 * - Capture: AudioRecord → WzpEngine.writeAudio() → Rust encoder → network
 * - Playout: network → Rust decoder → WzpEngine.readAudio() → AudioTrack
 *
 * All audio is 48kHz, mono, 16-bit PCM (matching Opus codec requirements).
 */
class AudioPipeline(private val context: Context) {

    companion object {
        private const val TAG = "AudioPipeline"
        private const val SAMPLE_RATE = 48000
        private const val CHANNEL_IN = AudioFormat.CHANNEL_IN_MONO
        private const val CHANNEL_OUT = AudioFormat.CHANNEL_OUT_MONO
        private const val ENCODING = AudioFormat.ENCODING_PCM_16BIT
        /** 20ms frame at 48kHz = 960 samples */
        private const val FRAME_SAMPLES = 960
    }

    @Volatile
    private var running = false
    /** Playout (incoming voice) gain in dB. 0 = unity. */
    @Volatile
    var playoutGainDb: Float = 0f
    /** Capture (mic) gain in dB. 0 = unity. */
    @Volatile
    var captureGainDb: Float = 0f
    /** Whether to attach hardware AEC. Must be set before start(). */
    var aecEnabled: Boolean = true
    /** Enable debug recording of PCM + RMS histogram to cache dir. */
    var debugRecording: Boolean = true
    private var captureThread: Thread? = null
    private var playoutThread: Thread? = null
    /** Latch counted down by each audio thread after exiting its loop.
     *  stop() does NOT wait on this — teardown waits via awaitDrain(). */
    private var drainLatch: CountDownLatch? = null

    private val debugDir: File by lazy {
        File(context.cacheDir, "wzp_debug").also { it.mkdirs() }
    }

    fun start(engine: WzpEngine) {
        if (running) return
        running = true
        drainLatch = CountDownLatch(2) // one for capture, one for playout

        captureThread = Thread({
            runCapture(engine)
            drainLatch?.countDown() // signal: capture loop exited, no more JNI calls
            // Park thread forever — exiting triggers a libcrypto TLS destructor
            // crash (SIGSEGV in OPENSSL_free) on Android when a JNI-calling thread exits.
            parkThread()
        }, "wzp-capture").apply {
            isDaemon = true
            priority = Thread.MAX_PRIORITY
            start()
        }

        playoutThread = Thread({
            runPlayout(engine)
            drainLatch?.countDown() // signal: playout loop exited
            parkThread()
        }, "wzp-playout").apply {
            isDaemon = true
            priority = Thread.MAX_PRIORITY
            start()
        }

        Log.i(TAG, "audio pipeline started")
    }

    fun stop() {
        running = false
        // Don't join threads — they are parked as daemons to avoid native TLS crash.
        // Don't null thread refs or drainLatch — teardown() needs awaitDrain().
        Log.i(TAG, "audio pipeline stopped (running=false)")
    }

    /** Block until both audio threads have exited their loops (max 200ms).
     *  After this returns, no more JNI calls to the engine will be made. */
    fun awaitDrain(): Boolean {
        val ok = drainLatch?.await(200, TimeUnit.MILLISECONDS) ?: true
        if (!ok) Log.w(TAG, "awaitDrain: audio threads did not drain in 200ms")
        captureThread = null
        playoutThread = null
        drainLatch = null
        return ok
    }

    private fun applyGain(pcm: ShortArray, count: Int, db: Float) {
        if (db == 0f) return
        val linear = 10f.pow(db / 20f)
        for (i in 0 until count) {
            pcm[i] = (pcm[i] * linear).toInt().coerceIn(-32000, 32000).toShort()
        }
    }

    private fun computeRms(pcm: ShortArray, count: Int): Int {
        var sumSq = 0.0
        for (i in 0 until count) {
            val s = pcm[i].toDouble()
            sumSq += s * s
        }
        return sqrt(sumSq / count).toInt()
    }

    private fun parkThread() {
        try {
            Thread.sleep(Long.MAX_VALUE)
        } catch (_: InterruptedException) {
            // process exiting
        }
    }

    private fun runCapture(engine: WzpEngine) {
        if (ContextCompat.checkSelfPermission(context, Manifest.permission.RECORD_AUDIO)
            != PackageManager.PERMISSION_GRANTED
        ) {
            Log.e(TAG, "RECORD_AUDIO permission not granted, capture disabled")
            return
        }

        val minBuf = AudioRecord.getMinBufferSize(SAMPLE_RATE, CHANNEL_IN, ENCODING)
        val bufSize = maxOf(minBuf, FRAME_SAMPLES * 2 * 4) // at least 4 frames

        val recorder = try {
            AudioRecord(
                MediaRecorder.AudioSource.VOICE_COMMUNICATION,
                SAMPLE_RATE,
                CHANNEL_IN,
                ENCODING,
                bufSize
            )
        } catch (e: SecurityException) {
            Log.e(TAG, "AudioRecord SecurityException: ${e.message}")
            return
        }

        if (recorder.state != AudioRecord.STATE_INITIALIZED) {
            Log.e(TAG, "AudioRecord failed to initialize")
            recorder.release()
            return
        }

        // Attach hardware AEC if available and enabled in settings
        var aec: AcousticEchoCanceler? = null
        var ns: NoiseSuppressor? = null
        if (aecEnabled) {
            if (AcousticEchoCanceler.isAvailable()) {
                try {
                    aec = AcousticEchoCanceler.create(recorder.audioSessionId)
                    aec?.enabled = true
                    Log.i(TAG, "AEC enabled (session=${recorder.audioSessionId})")
                } catch (e: Exception) {
                    Log.w(TAG, "AEC init failed: ${e.message}")
                }
            } else {
                Log.w(TAG, "AEC not available on this device")
            }

            // Attach hardware noise suppressor if available
            if (NoiseSuppressor.isAvailable()) {
                try {
                    ns = NoiseSuppressor.create(recorder.audioSessionId)
                    ns?.enabled = true
                    Log.i(TAG, "NoiseSuppressor enabled")
                } catch (e: Exception) {
                    Log.w(TAG, "NoiseSuppressor init failed: ${e.message}")
                }
            }
        } else {
            Log.i(TAG, "AEC disabled by user setting")
        }

        recorder.startRecording()
        Log.i(TAG, "capture started: ${SAMPLE_RATE}Hz mono, buf=$bufSize, aec=${aec?.enabled}, ns=${ns?.enabled}")

        val pcm = ShortArray(FRAME_SAMPLES)
        // Debug: PCM file + RMS CSV
        var pcmOut: BufferedOutputStream? = null
        var rmsCsv: OutputStreamWriter? = null
        val byteConv = ByteBuffer.allocate(FRAME_SAMPLES * 2).order(ByteOrder.LITTLE_ENDIAN)
        var frameIdx = 0L
        if (debugRecording) {
            try {
                pcmOut = BufferedOutputStream(FileOutputStream(File(debugDir, "capture.pcm")), 65536)
                rmsCsv = OutputStreamWriter(FileOutputStream(File(debugDir, "capture_rms.csv")))
                rmsCsv.write("frame,time_ms,rms\n")
            } catch (e: Exception) {
                Log.w(TAG, "debug recording init failed: ${e.message}")
            }
        }
        try {
            while (running) {
                val read = recorder.read(pcm, 0, FRAME_SAMPLES)
                if (read > 0) {
                    applyGain(pcm, read, captureGainDb)
                    engine.writeAudio(pcm)

                    // Debug: write raw PCM + RMS
                    if (pcmOut != null) {
                        byteConv.clear()
                        for (i in 0 until read) byteConv.putShort(pcm[i])
                        pcmOut.write(byteConv.array(), 0, read * 2)
                    }
                    if (rmsCsv != null) {
                        val rms = computeRms(pcm, read)
                        val timeMs = frameIdx * FRAME_SAMPLES * 1000L / SAMPLE_RATE
                        rmsCsv.write("$frameIdx,$timeMs,$rms\n")
                    }
                    frameIdx++
                } else if (read < 0) {
                    Log.e(TAG, "AudioRecord.read error: $read")
                    break
                }
            }
        } finally {
            pcmOut?.close()
            rmsCsv?.close()
            recorder.stop()
            aec?.release()
            ns?.release()
            recorder.release()
            Log.i(TAG, "capture stopped (frames=$frameIdx)")
        }
    }

    private fun runPlayout(engine: WzpEngine) {
        val minBuf = AudioTrack.getMinBufferSize(SAMPLE_RATE, CHANNEL_OUT, ENCODING)
        val bufSize = maxOf(minBuf, FRAME_SAMPLES * 2 * 4)

        val track = AudioTrack.Builder()
            .setAudioAttributes(
                AudioAttributes.Builder()
                    .setUsage(AudioAttributes.USAGE_VOICE_COMMUNICATION)
                    .setContentType(AudioAttributes.CONTENT_TYPE_SPEECH)
                    .build()
            )
            .setAudioFormat(
                AudioFormat.Builder()
                    .setSampleRate(SAMPLE_RATE)
                    .setChannelMask(CHANNEL_OUT)
                    .setEncoding(ENCODING)
                    .build()
            )
            .setBufferSizeInBytes(bufSize)
            .setTransferMode(AudioTrack.MODE_STREAM)
            .build()

        if (track.state != AudioTrack.STATE_INITIALIZED) {
            Log.e(TAG, "AudioTrack failed to initialize")
            track.release()
            return
        }

        track.play()
        Log.i(TAG, "playout started: ${SAMPLE_RATE}Hz mono, buf=$bufSize")

        val pcm = ShortArray(FRAME_SAMPLES)
        val silence = ShortArray(FRAME_SAMPLES)
        // Debug: PCM file + RMS CSV for playout
        var pcmOut: BufferedOutputStream? = null
        var rmsCsv: OutputStreamWriter? = null
        val byteConv = ByteBuffer.allocate(FRAME_SAMPLES * 2).order(ByteOrder.LITTLE_ENDIAN)
        var frameIdx = 0L
        if (debugRecording) {
            try {
                pcmOut = BufferedOutputStream(FileOutputStream(File(debugDir, "playout.pcm")), 65536)
                rmsCsv = OutputStreamWriter(FileOutputStream(File(debugDir, "playout_rms.csv")))
                rmsCsv.write("frame,time_ms,rms\n")
            } catch (e: Exception) {
                Log.w(TAG, "debug playout recording init failed: ${e.message}")
            }
        }
        try {
            while (running) {
                val read = engine.readAudio(pcm)
                if (read >= FRAME_SAMPLES) {
                    applyGain(pcm, read, playoutGainDb)
                    track.write(pcm, 0, read)

                    // Debug: write raw PCM + RMS
                    if (pcmOut != null) {
                        byteConv.clear()
                        for (i in 0 until read) byteConv.putShort(pcm[i])
                        pcmOut.write(byteConv.array(), 0, read * 2)
                    }
                    if (rmsCsv != null) {
                        val rms = computeRms(pcm, read)
                        val timeMs = frameIdx * FRAME_SAMPLES * 1000L / SAMPLE_RATE
                        rmsCsv.write("$frameIdx,$timeMs,$rms\n")
                    }
                    frameIdx++
                } else {
                    track.write(silence, 0, FRAME_SAMPLES)
                    // Log silence frames to RMS as 0
                    if (rmsCsv != null) {
                        val timeMs = frameIdx * FRAME_SAMPLES * 1000L / SAMPLE_RATE
                        rmsCsv.write("$frameIdx,$timeMs,0\n")
                    }
                    frameIdx++
                    Thread.sleep(5)
                }
            }
        } finally {
            pcmOut?.close()
            rmsCsv?.close()
            track.stop()
            track.release()
            Log.i(TAG, "playout stopped (frames=$frameIdx)")
        }
    }
}
