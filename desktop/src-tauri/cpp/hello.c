/* hello.c — minimal C file compiled via cc::Build on Android.
 *
 * Step A of the incremental Oboe integration: this file exists only to
 * exercise the cc::Build → static lib → rustc-link pipeline and prove
 * that introducing any C static library into our .so doesn't by itself
 * trigger the tao::ndk_glue pthread_create crash we hit on earlier
 * attempts. The function is deliberately never called from Rust.
 */

#include <stdint.h>

int32_t wzp_hello_stub(void) {
    return 42;
}
