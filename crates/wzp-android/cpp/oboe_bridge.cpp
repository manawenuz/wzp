// Full Oboe implementation for Android
// This file is compiled only when targeting Android

#include "oboe_bridge.h"

#ifdef __ANDROID__
#include <oboe/Oboe.h>
#include <android/log.h>
#include <cstring>
#include <atomic>

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
static const WzpOboeRings* g_rings = nullptr;
static std::atomic<bool> g_running{false};
static std::atomic<float> g_capture_latency_ms{0.0f};
static std::atomic<float> g_playout_latency_ms{0.0f};

// ---------------------------------------------------------------------------
// Capture callback
// ---------------------------------------------------------------------------

class CaptureCallback : public oboe::AudioStreamDataCallback {
public:
    oboe::DataCallbackResult onAudioReady(
            oboe::AudioStream* stream,
            void* audioData,
            int32_t numFrames) override {
        if (!g_running.load(std::memory_order_relaxed) || !g_rings) {
            return oboe::DataCallbackResult::Stop;
        }

        const int16_t* src = static_cast<const int16_t*>(audioData);
        int32_t avail = ring_available_write(g_rings->capture_write_idx,
                                              g_rings->capture_read_idx,
                                              g_rings->capture_capacity);
        int32_t to_write = (numFrames < avail) ? numFrames : avail;
        if (to_write > 0) {
            ring_write(g_rings->capture_buf, g_rings->capture_capacity,
                       g_rings->capture_write_idx, g_rings->capture_read_idx,
                       src, to_write);
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
    oboe::DataCallbackResult onAudioReady(
            oboe::AudioStream* stream,
            void* audioData,
            int32_t numFrames) override {
        if (!g_running.load(std::memory_order_relaxed) || !g_rings) {
            memset(audioData, 0, numFrames * sizeof(int16_t));
            return oboe::DataCallbackResult::Stop;
        }

        int16_t* dst = static_cast<int16_t*>(audioData);
        int32_t avail = ring_available_read(g_rings->playout_write_idx,
                                             g_rings->playout_read_idx,
                                             g_rings->playout_capacity);
        int32_t to_read = (numFrames < avail) ? numFrames : avail;

        if (to_read > 0) {
            ring_read(g_rings->playout_buf, g_rings->playout_capacity,
                      g_rings->playout_write_idx, g_rings->playout_read_idx,
                      dst, to_read);
        }
        // Fill remainder with silence on underrun
        if (to_read < numFrames) {
            memset(dst + to_read, 0, (numFrames - to_read) * sizeof(int16_t));
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

    g_rings = rings;

    // Build capture stream
    oboe::AudioStreamBuilder captureBuilder;
    captureBuilder.setDirection(oboe::Direction::Input)
        ->setPerformanceMode(oboe::PerformanceMode::LowLatency)
        ->setSharingMode(oboe::SharingMode::Exclusive)
        ->setFormat(oboe::AudioFormat::I16)
        ->setChannelCount(config->channel_count)
        ->setSampleRate(config->sample_rate)
        ->setFramesPerDataCallback(config->frames_per_burst)
        ->setInputPreset(oboe::InputPreset::VoiceCommunication)
        ->setDataCallback(&g_capture_cb);

    oboe::Result result = captureBuilder.openStream(g_capture_stream);
    if (result != oboe::Result::OK) {
        LOGE("Failed to open capture stream: %s", oboe::convertToText(result));
        return -2;
    }

    // Build playout stream
    oboe::AudioStreamBuilder playoutBuilder;
    playoutBuilder.setDirection(oboe::Direction::Output)
        ->setPerformanceMode(oboe::PerformanceMode::LowLatency)
        ->setSharingMode(oboe::SharingMode::Exclusive)
        ->setFormat(oboe::AudioFormat::I16)
        ->setChannelCount(config->channel_count)
        ->setSampleRate(config->sample_rate)
        ->setFramesPerDataCallback(config->frames_per_burst)
        ->setUsage(oboe::Usage::VoiceCommunication)
        ->setDataCallback(&g_playout_cb);

    result = playoutBuilder.openStream(g_playout_stream);
    if (result != oboe::Result::OK) {
        LOGE("Failed to open playout stream: %s", oboe::convertToText(result));
        g_capture_stream->close();
        g_capture_stream.reset();
        return -3;
    }

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

    LOGI("Oboe started: sr=%d burst=%d ch=%d",
         config->sample_rate, config->frames_per_burst, config->channel_count);
    return 0;
}

void wzp_oboe_stop(void) {
    g_running.store(false, std::memory_order_release);

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

    g_rings = nullptr;
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
