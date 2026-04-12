// Full Oboe implementation for Android
// This file is compiled only when targeting Android

#include "oboe_bridge.h"

#ifdef __ANDROID__
#include <oboe/Oboe.h>
#include <android/log.h>
#include <cstring>
#include <atomic>
#include <chrono>
#include <thread>

#define LOG_TAG "wzp-oboe"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO, LOG_TAG, __VA_ARGS__)
#define LOGW(...) __android_log_print(ANDROID_LOG_WARN, LOG_TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, LOG_TAG, __VA_ARGS__)

// ---------------------------------------------------------------------------
// Ring buffer helpers (SPSC, lock-free)
// ---------------------------------------------------------------------------

static inline int32_t ring_available_read(const wzp_atomic_int* write_idx,
                                           const wzp_atomic_int* read_idx,
                                           int32_t capacity) {
    int32_t w = std::atomic_load_explicit(write_idx, std::memory_order_acquire);
    int32_t r = std::atomic_load_explicit(read_idx, std::memory_order_relaxed);
    int32_t avail = w - r;
    if (avail < 0) avail += capacity;
    return avail;
}

static inline int32_t ring_available_write(const wzp_atomic_int* write_idx,
                                            const wzp_atomic_int* read_idx,
                                            int32_t capacity) {
    return capacity - 1 - ring_available_read(write_idx, read_idx, capacity);
}

static inline void ring_write(int16_t* buf, int32_t capacity,
                               wzp_atomic_int* write_idx, const wzp_atomic_int* read_idx,
                               const int16_t* src, int32_t count) {
    int32_t w = std::atomic_load_explicit(write_idx, std::memory_order_relaxed);
    for (int32_t i = 0; i < count; i++) {
        buf[w] = src[i];
        w++;
        if (w >= capacity) w = 0;
    }
    std::atomic_store_explicit(write_idx, w, std::memory_order_release);
}

static inline void ring_read(int16_t* buf, int32_t capacity,
                              const wzp_atomic_int* write_idx, wzp_atomic_int* read_idx,
                              int16_t* dst, int32_t count) {
    int32_t r = std::atomic_load_explicit(read_idx, std::memory_order_relaxed);
    for (int32_t i = 0; i < count; i++) {
        dst[i] = buf[r];
        r++;
        if (r >= capacity) r = 0;
    }
    std::atomic_store_explicit(read_idx, r, std::memory_order_release);
}

// ---------------------------------------------------------------------------
// Global state
// ---------------------------------------------------------------------------

static std::shared_ptr<oboe::AudioStream> g_capture_stream;
static std::shared_ptr<oboe::AudioStream> g_playout_stream;
// Value copy — the WzpOboeRings the Rust side passes us lives on the caller's
// stack frame and goes away as soon as wzp_oboe_start returns. The raw
// int16/atomic pointers INSIDE the struct point into the Rust-owned, leaked-
// for-the-lifetime-of-the-process AudioBackend singleton, so copying the
// struct by value is safe and keeps the inner pointers valid indefinitely.
// g_rings_valid guards the audio-callback-side read; clearing it in stop()
// signals "no backend" to the callbacks which then return silence + Stop.
static WzpOboeRings g_rings{};
static std::atomic<bool> g_rings_valid{false};
static std::atomic<bool> g_running{false};
static std::atomic<float> g_capture_latency_ms{0.0f};
static std::atomic<float> g_playout_latency_ms{0.0f};

// ---------------------------------------------------------------------------
// Capture callback
// ---------------------------------------------------------------------------

class CaptureCallback : public oboe::AudioStreamDataCallback {
public:
    uint64_t calls = 0;
    uint64_t total_frames = 0;
    uint64_t total_written = 0;
    uint64_t ring_full_drops = 0;

    oboe::DataCallbackResult onAudioReady(
            oboe::AudioStream* stream,
            void* audioData,
            int32_t numFrames) override {
        if (!g_running.load(std::memory_order_relaxed) ||
            !g_rings_valid.load(std::memory_order_acquire)) {
            return oboe::DataCallbackResult::Stop;
        }

        const int16_t* src = static_cast<const int16_t*>(audioData);
        int32_t avail = ring_available_write(g_rings.capture_write_idx,
                                              g_rings.capture_read_idx,
                                              g_rings.capture_capacity);
        int32_t to_write = (numFrames < avail) ? numFrames : avail;
        if (to_write > 0) {
            ring_write(g_rings.capture_buf, g_rings.capture_capacity,
                       g_rings.capture_write_idx, g_rings.capture_read_idx,
                       src, to_write);
        }
        total_frames += numFrames;
        total_written += to_write;
        if (to_write < numFrames) {
            ring_full_drops += (numFrames - to_write);
        }

        // Sample-range probe on the FIRST callback to prove we get real audio
        if (calls == 0 && numFrames > 0) {
            int16_t lo = src[0], hi = src[0];
            int32_t sumsq = 0;
            for (int32_t i = 0; i < numFrames; i++) {
                if (src[i] < lo) lo = src[i];
                if (src[i] > hi) hi = src[i];
                sumsq += (int32_t)src[i] * (int32_t)src[i];
            }
            int32_t rms = (int32_t) (numFrames > 0 ? (int32_t)__builtin_sqrt((double)sumsq / (double)numFrames) : 0);
            LOGI("capture cb#0: numFrames=%d sample_range=[%d..%d] rms=%d to_write=%d",
                 numFrames, lo, hi, rms, to_write);
        }
        // Heartbeat every 50 callbacks (~1s at 20ms/burst)
        calls++;
        if ((calls % 50) == 0) {
            LOGI("capture heartbeat: calls=%llu numFrames=%d ring_avail_write=%d to_write=%d full_drops=%llu total_written=%llu",
                 (unsigned long long)calls, numFrames, avail, to_write,
                 (unsigned long long)ring_full_drops, (unsigned long long)total_written);
        }

        // Update latency estimate
        auto result = stream->calculateLatencyMillis();
        if (result) {
            g_capture_latency_ms.store(static_cast<float>(result.value()),
                                        std::memory_order_relaxed);
        }

        return oboe::DataCallbackResult::Continue;
    }
};

// ---------------------------------------------------------------------------
// Playout callback
// ---------------------------------------------------------------------------

class PlayoutCallback : public oboe::AudioStreamDataCallback {
public:
    uint64_t calls = 0;
    uint64_t total_frames = 0;
    uint64_t total_played_real = 0;
    uint64_t underrun_frames = 0;
    uint64_t nonempty_calls = 0;

    oboe::DataCallbackResult onAudioReady(
            oboe::AudioStream* stream,
            void* audioData,
            int32_t numFrames) override {
        if (!g_running.load(std::memory_order_relaxed) ||
            !g_rings_valid.load(std::memory_order_acquire)) {
            memset(audioData, 0, numFrames * sizeof(int16_t));
            return oboe::DataCallbackResult::Stop;
        }

        int16_t* dst = static_cast<int16_t*>(audioData);
        int32_t avail = ring_available_read(g_rings.playout_write_idx,
                                             g_rings.playout_read_idx,
                                             g_rings.playout_capacity);
        int32_t to_read = (numFrames < avail) ? numFrames : avail;

        if (to_read > 0) {
            ring_read(g_rings.playout_buf, g_rings.playout_capacity,
                      g_rings.playout_write_idx, g_rings.playout_read_idx,
                      dst, to_read);
            nonempty_calls++;
        }
        // Fill remainder with silence on underrun
        if (to_read < numFrames) {
            memset(dst + to_read, 0, (numFrames - to_read) * sizeof(int16_t));
            underrun_frames += (numFrames - to_read);
        }
        total_frames += numFrames;
        total_played_real += to_read;

        // First callback: log requested config + prove we're being called
        if (calls == 0) {
            LOGI("playout cb#0: numFrames=%d ring_avail_read=%d to_read=%d",
                 numFrames, avail, to_read);
        }
        // On the first callback that actually has data, log the sample range
        // so we can tell if the samples coming out of the ring look like real
        // audio vs constant-zeroes vs garbage.
        if (to_read > 0 && nonempty_calls == 1) {
            int16_t lo = dst[0], hi = dst[0];
            int32_t sumsq = 0;
            for (int32_t i = 0; i < to_read; i++) {
                if (dst[i] < lo) lo = dst[i];
                if (dst[i] > hi) hi = dst[i];
                sumsq += (int32_t)dst[i] * (int32_t)dst[i];
            }
            int32_t rms = (to_read > 0) ? (int32_t)__builtin_sqrt((double)sumsq / (double)to_read) : 0;
            LOGI("playout FIRST nonempty read: to_read=%d sample_range=[%d..%d] rms=%d",
                 to_read, lo, hi, rms);
        }
        // Heartbeat every 50 callbacks (~1s at 20ms/burst)
        calls++;
        if ((calls % 50) == 0) {
            int state = (int)stream->getState();
            auto xrunRes = stream->getXRunCount();
            int xruns = xrunRes ? xrunRes.value() : -1;
            LOGI("playout heartbeat: calls=%llu nonempty=%llu numFrames=%d ring_avail_read=%d to_read=%d underrun_frames=%llu total_played_real=%llu state=%d xruns=%d",
                 (unsigned long long)calls, (unsigned long long)nonempty_calls,
                 numFrames, avail, to_read,
                 (unsigned long long)underrun_frames, (unsigned long long)total_played_real,
                 state, xruns);
        }

        // Update latency estimate
        auto result = stream->calculateLatencyMillis();
        if (result) {
            g_playout_latency_ms.store(static_cast<float>(result.value()),
                                        std::memory_order_relaxed);
        }

        return oboe::DataCallbackResult::Continue;
    }
};

static CaptureCallback g_capture_cb;
static PlayoutCallback g_playout_cb;

// ---------------------------------------------------------------------------
// Public C API
// ---------------------------------------------------------------------------

int wzp_oboe_start(const WzpOboeConfig* config, const WzpOboeRings* rings) {
    if (g_running.load(std::memory_order_relaxed)) {
        LOGW("wzp_oboe_start: already running");
        return -1;
    }

    // Deep-copy the rings struct into static storage BEFORE we publish it to
    // the audio callbacks — `rings` points at the caller's stack frame and
    // goes away as soon as this function returns.
    g_rings = *rings;
    g_rings_valid.store(true, std::memory_order_release);

    // Build capture stream
    oboe::AudioStreamBuilder captureBuilder;
    captureBuilder.setDirection(oboe::Direction::Input)
        ->setPerformanceMode(oboe::PerformanceMode::LowLatency)
        ->setSharingMode(oboe::SharingMode::Shared)
        ->setFormat(oboe::AudioFormat::I16)
        ->setChannelCount(config->channel_count)
        ->setSampleRateConversionQuality(oboe::SampleRateConversionQuality::Best)
        ->setDataCallback(&g_capture_cb);

    if (config->bt_active) {
        // BT SCO mode: do NOT set sample rate or input preset.
        // Requesting 48kHz against a BT SCO device fails with
        // "getInputProfile could not find profile". Letting the system
        // choose the native rate (8/16kHz) and relying on Oboe's
        // resampler (SampleRateConversionQuality::Best) to bridge
        // to our 48kHz ring buffer is the only path that works.
        // InputPreset::VoiceCommunication can also prevent BT SCO
        // routing on some devices — skip it for BT.
        LOGI("capture: BT mode — no sample rate or input preset set");
    } else {
        captureBuilder.setSampleRate(config->sample_rate)
            ->setFramesPerDataCallback(config->frames_per_burst)
            ->setInputPreset(oboe::InputPreset::VoiceCommunication);
    }

    oboe::Result result = captureBuilder.openStream(g_capture_stream);
    if (result != oboe::Result::OK) {
        LOGE("Failed to open capture stream: %s", oboe::convertToText(result));
        return -2;
    }
    LOGI("capture stream opened: actualSR=%d actualCh=%d actualFormat=%d actualFramesPerBurst=%d actualFramesPerDataCallback=%d bufferCapacityInFrames=%d sharing=%d perfMode=%d",
         g_capture_stream->getSampleRate(),
         g_capture_stream->getChannelCount(),
         (int)g_capture_stream->getFormat(),
         g_capture_stream->getFramesPerBurst(),
         g_capture_stream->getFramesPerDataCallback(),
         g_capture_stream->getBufferCapacityInFrames(),
         (int)g_capture_stream->getSharingMode(),
         (int)g_capture_stream->getPerformanceMode());

    // Build playout stream.
    //
    // Regression triangulation between builds:
    //   96be740 (Usage::Media, default API): playout callback DID drain
    //   the ring at steady 50Hz (playout heartbeat: calls=1100,
    //   total_played_real=1055040). Audio not audible because OS routing
    //   sent it to a silent output.
    //
    //   8c36fb5 (Usage::VoiceCommunication + setAudioApi(AAudio) +
    //   ContentType::Speech): playout callback fired cb#0 once then
    //   stopped draining the ring entirely. written_samples stuck at
    //   ring capacity (7679) across all subsequent heartbeats, so Oboe
    //   accepted zero samples after startup. Still inaudible.
    //
    // Hypothesis: forcing setAudioApi(AAudio) + VoiceCommunication on
    // Pixel 6 / Android 15 opens a stream that succeeds at cb#0 but
    // then detaches from the real audio driver. Reverting to the
    // config that at least drove callbacks correctly, plus the
    // Kotlin-side MODE_IN_COMMUNICATION + setSpeakerphoneOn(true)
    // handled in MainActivity.kt to route audio to the loud speaker.
    // Usage::VoiceCommunication is the correct Oboe usage for a VoIP app
    // — it respects Android's in-call audio routing and lets
    // AudioManager.setSpeakerphoneOn/setBluetoothScoOn actually switch
    // between earpiece, loudspeaker, and Bluetooth headset. Combined with
    // MODE_IN_COMMUNICATION set from MainActivity.kt and
    // speakerphoneOn=false by default, this produces handset/earpiece as
    // the default output.
    //
    // IMPORTANT: do NOT add setAudioApi(AAudio) here. Build 8c36fb5 proved
    // forcing AAudio with Usage::VoiceCommunication makes the playout
    // callback stop draining the ring after cb#0, even though the stream
    // opens successfully. Letting Oboe pick the API (which will be AAudio
    // on API ≥ 27 but via a different codepath) kept callbacks firing in
    // every other build.
    oboe::AudioStreamBuilder playoutBuilder;
    playoutBuilder.setDirection(oboe::Direction::Output)
        ->setPerformanceMode(oboe::PerformanceMode::LowLatency)
        ->setSharingMode(oboe::SharingMode::Shared)
        ->setFormat(oboe::AudioFormat::I16)
        ->setChannelCount(config->channel_count)
        ->setSampleRateConversionQuality(oboe::SampleRateConversionQuality::Best)
        ->setDataCallback(&g_playout_cb);

    if (config->bt_active) {
        LOGI("playout: BT mode — no sample rate set, using Usage::Media");
        // Usage::Media instead of VoiceCommunication for BT output
        // to avoid conflicts with the communication device routing.
        playoutBuilder.setUsage(oboe::Usage::Media);
    } else {
        playoutBuilder.setSampleRate(config->sample_rate)
            ->setFramesPerDataCallback(config->frames_per_burst)
            ->setUsage(oboe::Usage::VoiceCommunication);
    }

    result = playoutBuilder.openStream(g_playout_stream);
    if (result != oboe::Result::OK) {
        LOGE("Failed to open playout stream: %s", oboe::convertToText(result));
        g_capture_stream->close();
        g_capture_stream.reset();
        return -3;
    }
    LOGI("playout stream opened: actualSR=%d actualCh=%d actualFormat=%d actualFramesPerBurst=%d actualFramesPerDataCallback=%d bufferCapacityInFrames=%d sharing=%d perfMode=%d",
         g_playout_stream->getSampleRate(),
         g_playout_stream->getChannelCount(),
         (int)g_playout_stream->getFormat(),
         g_playout_stream->getFramesPerBurst(),
         g_playout_stream->getFramesPerDataCallback(),
         g_playout_stream->getBufferCapacityInFrames(),
         (int)g_playout_stream->getSharingMode(),
         (int)g_playout_stream->getPerformanceMode());

    g_running.store(true, std::memory_order_release);

    // Start both streams
    result = g_capture_stream->requestStart();
    if (result != oboe::Result::OK) {
        LOGE("Failed to start capture: %s", oboe::convertToText(result));
        g_running.store(false, std::memory_order_release);
        g_capture_stream->close();
        g_playout_stream->close();
        g_capture_stream.reset();
        g_playout_stream.reset();
        return -4;
    }

    result = g_playout_stream->requestStart();
    if (result != oboe::Result::OK) {
        LOGE("Failed to start playout: %s", oboe::convertToText(result));
        g_running.store(false, std::memory_order_release);
        g_capture_stream->requestStop();
        g_capture_stream->close();
        g_playout_stream->close();
        g_capture_stream.reset();
        g_playout_stream.reset();
        return -5;
    }

    // Log initial stream states right after requestStart() returns.
    // On well-behaved HALs both will already be Started; on others
    // (Nothing A059) they may still be in Starting state.
    LOGI("requestStart returned: capture_state=%d playout_state=%d",
         (int)g_capture_stream->getState(),
         (int)g_playout_stream->getState());

    // Poll until both streams report Started state, up to 2s timeout.
    // Some Android HALs (Nothing A059) delay transitioning from Starting
    // to Started; proceeding before the transition completes causes the
    // first capture/playout callbacks to be dropped silently.
    {
        auto deadline = std::chrono::steady_clock::now() + std::chrono::milliseconds(2000);
        int poll_count = 0;
        while (std::chrono::steady_clock::now() < deadline) {
            auto cap_state = g_capture_stream->getState();
            auto play_state = g_playout_stream->getState();
            if (cap_state == oboe::StreamState::Started &&
                play_state == oboe::StreamState::Started) {
                LOGI("both streams Started after %d polls", poll_count);
                break;
            }
            poll_count++;
            std::this_thread::sleep_for(std::chrono::milliseconds(10));
        }
        // Log final state even on timeout (helps diagnose HAL quirks)
        LOGI("stream states after poll: capture=%d playout=%d (polls=%d)",
             (int)g_capture_stream->getState(),
             (int)g_playout_stream->getState(),
             poll_count);
    }

    LOGI("Oboe started: sr=%d burst=%d ch=%d",
         config->sample_rate, config->frames_per_burst, config->channel_count);
    return 0;
}

void wzp_oboe_stop(void) {
    g_running.store(false, std::memory_order_release);
    // Tell the audio callbacks to stop touching g_rings BEFORE we tear down
    // the streams, so any in-flight callback returns Stop instead of reading
    // stale pointers.
    g_rings_valid.store(false, std::memory_order_release);

    if (g_capture_stream) {
        g_capture_stream->requestStop();
        g_capture_stream->close();
        g_capture_stream.reset();
    }
    if (g_playout_stream) {
        g_playout_stream->requestStop();
        g_playout_stream->close();
        g_playout_stream.reset();
    }

    LOGI("Oboe stopped");
}

float wzp_oboe_capture_latency_ms(void) {
    return g_capture_latency_ms.load(std::memory_order_relaxed);
}

float wzp_oboe_playout_latency_ms(void) {
    return g_playout_latency_ms.load(std::memory_order_relaxed);
}

int wzp_oboe_is_running(void) {
    return g_running.load(std::memory_order_relaxed) ? 1 : 0;
}

#else
// Non-Android fallback — should not be reached; oboe_stub.cpp is used instead.
// Provide empty implementations just in case.

int wzp_oboe_start(const WzpOboeConfig* config, const WzpOboeRings* rings) {
    (void)config; (void)rings;
    return -99;
}

void wzp_oboe_stop(void) {}
float wzp_oboe_capture_latency_ms(void) { return 0.0f; }
float wzp_oboe_playout_latency_ms(void) { return 0.0f; }
int wzp_oboe_is_running(void) { return 0; }

#endif // __ANDROID__
