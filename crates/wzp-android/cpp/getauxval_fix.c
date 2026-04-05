// Override the broken static getauxval from compiler-rt/CRT.
// The static version reads from __libc_auxv which may be NULL in shared libs.
// This version calls the real bionic getauxval via dlsym.
#ifdef __ANDROID__
#include <dlfcn.h>
#include <stdint.h>

typedef unsigned long (*getauxval_fn)(unsigned long);

unsigned long getauxval(unsigned long type) {
    static getauxval_fn real_getauxval = (getauxval_fn)0;
    if (!real_getauxval) {
        real_getauxval = (getauxval_fn)dlsym((void*)-1 /* RTLD_DEFAULT */, "getauxval");
        if (!real_getauxval) {
            // Fallback: return 0 (no features detected)
            return 0;
        }
    }
    return real_getauxval(type);
}
#endif
