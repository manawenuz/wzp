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
import kotlin.math.pow

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
    private var captureThread: Thread? = null
    private var playoutThread: Thread? = null

    fun start(engine: WzpEngine) {
        if (running) return
        running = true

        captureThread = Thread({
            runCapture(engine)
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
        // Don't join — threads are parked as daemons to avoid native TLS crash
        captureThread = null
        playoutThread = null
        Log.i(TAG, "audio pipeline stopped")
    }

    private fun applyGain(pcm: ShortArray, count: Int, db: Float) {
        if (db == 0f) return
        val linear = 10f.pow(db / 20f)
        for (i in 0 until count) {
            pcm[i] = (pcm[i] * linear).toInt().coerceIn(-32000, 32000).toShort()
        }
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

        // Attach hardware AEC if available
        var aec: AcousticEchoCanceler? = null
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
        var ns: NoiseSuppressor? = null
        if (NoiseSuppressor.isAvailable()) {
            try {
                ns = NoiseSuppressor.create(recorder.audioSessionId)
                ns?.enabled = true
                Log.i(TAG, "NoiseSuppressor enabled")
            } catch (e: Exception) {
                Log.w(TAG, "NoiseSuppressor init failed: ${e.message}")
            }
        }

        recorder.startRecording()
        Log.i(TAG, "capture started: ${SAMPLE_RATE}Hz mono, buf=$bufSize, aec=${aec?.enabled}, ns=${ns?.enabled}")

        val pcm = ShortArray(FRAME_SAMPLES)
        try {
            while (running) {
                val read = recorder.read(pcm, 0, FRAME_SAMPLES)
                if (read > 0) {
                    applyGain(pcm, read, captureGainDb)
                    engine.writeAudio(pcm)
                } else if (read < 0) {
                    Log.e(TAG, "AudioRecord.read error: $read")
                    break
                }
            }
        } finally {
            recorder.stop()
            aec?.release()
            ns?.release()
            recorder.release()
            Log.i(TAG, "capture stopped")
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
        val silence = ShortArray(FRAME_SAMPLES) // pre-allocated silence
        try {
            while (running) {
                val read = engine.readAudio(pcm)
                if (read >= FRAME_SAMPLES) {
                    applyGain(pcm, read, playoutGainDb)
                    track.write(pcm, 0, read)
                } else {
                    // Not enough decoded audio — write silence to keep stream alive
                    track.write(silence, 0, FRAME_SAMPLES)
                    // Sleep briefly to avoid busy-spinning
                    Thread.sleep(5)
                }
            }
        } finally {
            track.stop()
            track.release()
            Log.i(TAG, "playout stopped")
        }
    }
}
