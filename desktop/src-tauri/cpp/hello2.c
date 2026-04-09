/* hello2.c — identical content to hello.c, different file name + symbol.
 * Purpose: test if adding a THIRD trivial C static lib via cc::Build
 * regresses Step D regardless of what's in the file. Never called from Rust. */
#include <stdint.h>

int32_t wzp_hello2_stub(void) {
    return 43;
}
