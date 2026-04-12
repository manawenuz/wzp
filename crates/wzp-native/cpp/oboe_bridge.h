#ifndef WZP_OBOE_BRIDGE_H
#define WZP_OBOE_BRIDGE_H

#include <stdint.h>

#ifdef __cplusplus
#include <atomic>
typedef std::atomic<int32_t> wzp_atomic_int;
extern "C" {
#else
#include <stdatomic.h>
typedef atomic_int wzp_atomic_int;
#endif

typedef struct {
    int32_t sample_rate;
    int32_t frames_per_burst;
    int32_t channel_count;
    int32_t bt_active;  /* nonzero = BT SCO mode: skip sample rate + input preset */
} WzpOboeConfig;

typedef struct {
    int16_t* capture_buf;
    int32_t  capture_capacity;
    wzp_atomic_int* capture_write_idx;
    wzp_atomic_int* capture_read_idx;

    int16_t* playout_buf;
    int32_t  playout_capacity;
    wzp_atomic_int* playout_write_idx;
    wzp_atomic_int* playout_read_idx;
} WzpOboeRings;

int wzp_oboe_start(const WzpOboeConfig* config, const WzpOboeRings* rings);
void wzp_oboe_stop(void);
float wzp_oboe_capture_latency_ms(void);
float wzp_oboe_playout_latency_ms(void);
int wzp_oboe_is_running(void);

#ifdef __cplusplus
}
#endif

#endif // WZP_OBOE_BRIDGE_H
