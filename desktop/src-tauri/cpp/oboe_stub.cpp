// Stub implementation for non-Android host builds (testing, cargo check, etc.)

#include "oboe_bridge.h"
#include <stdio.h>

int wzp_oboe_start(const WzpOboeConfig* config, const WzpOboeRings* rings) {
    (void)config;
    (void)rings;
    fprintf(stderr, "wzp_oboe_start: stub (not on Android)\n");
    return 0;
}

void wzp_oboe_stop(void) {
    fprintf(stderr, "wzp_oboe_stop: stub (not on Android)\n");
}

float wzp_oboe_capture_latency_ms(void) {
    return 0.0f;
}

float wzp_oboe_playout_latency_ms(void) {
    return 0.0f;
}

int wzp_oboe_is_running(void) {
    return 0;
}
