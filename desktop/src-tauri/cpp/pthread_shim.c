/* pthread_shim.c
 *
 * Interpose pthread_create to bypass the broken static stub that Rust's
 * pre-compiled libstd for aarch64-linux-android drags in.
 *
 * The stub (from an old NDK libc.a) calls __init_tcb(bionic_tcb*, ...)+4,
 * which SIGSEGVs in .so libraries because __init_tcb expects TCB state that
 * only the static-libc init path sets up. In a dlopen-loaded shared lib
 * nothing ever initialises that state.
 *
 * By providing our own pthread_create at link time (which takes priority
 * over the one dragged in by libstd) and forwarding it via dlsym(RTLD_NEXT)
 * to the REAL pthread_create in libc.so, we completely sidestep the static
 * stub — libc.so's pthread_create is the fully working runtime version.
 *
 * The same trick handles `getauxval` via getauxval_fix.c.
 */

#ifdef __ANDROID__

#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stddef.h>

typedef int (*pthread_create_fn)(pthread_t *, const pthread_attr_t *,
                                 void *(*)(void *), void *);

int pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                   void *(*start_routine)(void *), void *arg) {
    static pthread_create_fn real_pthread_create = NULL;
    if (real_pthread_create == NULL) {
        /* RTLD_NEXT: skip the symbol we're currently defining and return
         * the next one in the search order — which is the real pthread_create
         * exported from libc.so. */
        real_pthread_create =
            (pthread_create_fn)dlsym(RTLD_NEXT, "pthread_create");
        if (real_pthread_create == NULL) {
            return -1;
        }
    }
    return real_pthread_create(thread, attr, start_routine, arg);
}

#endif /* __ANDROID__ */
