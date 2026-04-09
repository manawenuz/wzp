/* pthread_shim.c
 *
 * Interpose pthread_create via `--wrap=pthread_create` to bypass Rust libstd's
 * broken static pthread_create stub. The stub (from an old bundled libc.a)
 * calls __init_tcb+4 and SIGSEGVs in dlopen-loaded .so libraries.
 *
 * Approach:
 *   1. The linker rewrites every `pthread_create` reference inside libstd
 *      (and everywhere else) into `__wrap_pthread_create` — our function.
 *   2. Inside __wrap_pthread_create we call libc.so's real pthread_create
 *      via dlsym(RTLD_DEFAULT). Because --wrap redirects only references,
 *      the symbol `pthread_create` itself is NOT defined in our .so, so
 *      RTLD_DEFAULT finds the one exported by libc.so (always loaded).
 *   3. We log every step via __android_log_print so we can diagnose any
 *      lookup failure from logcat (tag: WZP_pthread_shim).
 */

#ifdef __ANDROID__

#define _GNU_SOURCE
#include <android/log.h>
#include <dlfcn.h>
#include <pthread.h>
#include <stddef.h>
#include <stdio.h>

#define TAG "WZP_pthread_shim"
#define LOGI(...) __android_log_print(ANDROID_LOG_INFO,  TAG, __VA_ARGS__)
#define LOGE(...) __android_log_print(ANDROID_LOG_ERROR, TAG, __VA_ARGS__)

typedef int (*pthread_create_fn)(pthread_t *, const pthread_attr_t *,
                                 void *(*)(void *), void *);

static pthread_create_fn resolve_real_pthread_create(void) {
    /* RTLD_DEFAULT: search the global symbol table starting with the main
     * executable (app_process64), then every shared library loaded into the
     * process in load order. libc.so is always loaded first, so its
     * pthread_create export is the first match — and since we never define
     * a symbol literally named "pthread_create" (only __wrap_pthread_create),
     * there's no ambiguity. */
    pthread_create_fn fn = (pthread_create_fn)dlsym(RTLD_DEFAULT, "pthread_create");
    if (fn != NULL) {
        LOGI("resolved pthread_create via RTLD_DEFAULT at %p", (void *)fn);
        return fn;
    }
    LOGE("dlsym(RTLD_DEFAULT, pthread_create) returned NULL: %s", dlerror());

    /* Fall back to explicit dlopen of libc.so — bionic is normally happy
     * to hand back the already-loaded mapping. */
    void *libc = dlopen("libc.so", RTLD_LAZY | RTLD_NOLOAD);
    if (libc == NULL) {
        libc = dlopen("libc.so", RTLD_LAZY);
    }
    if (libc == NULL) {
        LOGE("dlopen(libc.so) failed: %s", dlerror());
        return NULL;
    }
    fn = (pthread_create_fn)dlsym(libc, "pthread_create");
    if (fn == NULL) {
        LOGE("dlsym(libc, pthread_create) returned NULL: %s", dlerror());
        return NULL;
    }
    LOGI("resolved pthread_create via dlopen(libc.so) at %p", (void *)fn);
    return fn;
}

int __wrap_pthread_create(pthread_t *thread, const pthread_attr_t *attr,
                          void *(*start_routine)(void *), void *arg) {
    static pthread_create_fn real = NULL;
    if (real == NULL) {
        real = resolve_real_pthread_create();
        if (real == NULL) {
            LOGE("__wrap_pthread_create: no real pthread_create found, returning EAGAIN");
            return 11; /* EAGAIN — best we can do */
        }
    }
    return real(thread, attr, start_routine, arg);
}

#endif /* __ANDROID__ */
