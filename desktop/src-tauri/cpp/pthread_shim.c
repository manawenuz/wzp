/* pthread_shim.c
 *
 * Interpose pthread_create via linker --wrap to bypass Rust libstd's broken
 * static pthread_create stub (pulled in from an old NDK libc.a), which
 * transitively calls __init_tcb+4 and SIGSEGVs in any .so loaded via dlopen.
 *
 * Link flags (see build.rs):
 *   -Wl,--wrap=pthread_create
 *
 * The wrap flag makes the linker redirect every unresolved reference to
 * `pthread_create` → `__wrap_pthread_create` (below). Inside the shim we
 * explicitly open libc.so and look up the real, fully working runtime
 * pthread_create, bypassing libstd's bundled archive entirely.
 *
 * We deliberately do NOT call `__real_pthread_create` — that alias is the
 * SAME broken stub the wrap is designed to get around.
 */

#ifdef __ANDROID__

#define _GNU_SOURCE
#include <dlfcn.h>
#include <pthread.h>
#include <stddef.h>

typedef int (*pthread_create_fn)(pthread_t *, const pthread_attr_t *,
                                 void *(*)(void *), void *);

int __wrap_pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                          void *(*start_routine)(void *), void *arg) {
    static pthread_create_fn real = NULL;
    if (real == NULL) {
        /* Explicitly open libc.so and fetch the runtime pthread_create.
         * RTLD_NOLOAD would be wrong here — we want the mapping even if
         * bionic already loaded libc. The standard filename "libc.so" is
         * safe on Android; bionic's dynamic linker resolves it to
         * /apex/com.android.runtime/lib64/bionic/libc.so at runtime. */
        void *libc = dlopen("libc.so", RTLD_LAZY | RTLD_GLOBAL);
        if (libc != NULL) {
            real = (pthread_create_fn)dlsym(libc, "pthread_create");
        }
        if (real == NULL) {
            /* Fallback: try RTLD_DEFAULT — may or may not work depending on
             * link order but it's better than segfaulting. */
            real = (pthread_create_fn)dlsym(RTLD_DEFAULT, "pthread_create");
        }
        if (real == NULL) {
            return -1;
        }
    }
    return real(thread, attr, start_routine, arg);
}

#endif /* __ANDROID__ */
