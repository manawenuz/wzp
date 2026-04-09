//! wzp-native — standalone Android cdylib for all the C++ audio code.
//!
//! This crate is built with `cargo ndk`, NOT `cargo tauri android build`,
//! because the latter mispipes link flags in a way that causes bionic's
//! private `pthread_create` / `__init_tcb` symbols to land LOCALLY inside
//! any cdylib that also has a `cc::Build::new().cpp(true)` step. See
//! `docs/incident-tauri-android-init-tcb.md` for the full post-mortem.
//!
//! The Tauri desktop crate (`wzp-desktop`) has **no C++ at all**. At
//! runtime on Android, it `libloading::Library::new("libwzp_native.so")`'s
//! this crate's .so and calls the `wzp_native_*` functions below.
//!
//! Phase 1 (this file): a tiny smoke-test FFI surface so we can validate
//! that (a) cargo-ndk happily builds this crate standalone, (b) gradle
//! picks up the resulting .so from jniLibs, (c) the Tauri cdylib can
//! dlopen us at runtime and call exported functions. No C++, no Oboe, no
//! external deps. Phase 2 will add the Oboe cc::Build + audio FFI.

/// Smoke-test export #1 — returns a fixed magic number so the Tauri cdylib
/// can assert that `dlopen + dlsym` worked end-to-end. Always returns 42.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_version() -> i32 {
    42
}

/// Smoke-test export #2 — writes a fixed message into the caller's buffer
/// (NUL-terminated, capped at `cap`) and returns the number of bytes
/// written (not counting the NUL). Lets us verify we can move non-trivial
/// data across the FFI boundary without fighting ownership.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wzp_native_hello(out: *mut u8, cap: usize) -> usize {
    const MSG: &[u8] = b"hello from wzp-native\0";
    if out.is_null() || cap == 0 {
        return 0;
    }
    let n = MSG.len().min(cap);
    // SAFETY: caller provided a writable buffer of at least `cap` bytes.
    unsafe {
        core::ptr::copy_nonoverlapping(MSG.as_ptr(), out, n);
        // ensure last byte is a NUL even if we had to truncate
        *out.add(n - 1) = 0;
    }
    n - 1 // bytes written excluding the NUL
}
