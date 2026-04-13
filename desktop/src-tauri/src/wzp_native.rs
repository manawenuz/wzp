//! Runtime binding to the standalone `wzp-native` cdylib.
//!
//! See `docs/incident-tauri-android-init-tcb.md` and the top of
//! `crates/wzp-native/src/lib.rs` for the full story on why this split
//! exists. Short version: Tauri's desktop cdylib cannot have any C++
//! compiled into it (via cc::Build) without landing in rust-lang/rust#104707's
//! staticlib symbol leak, which makes bionic's private `pthread_create`
//! symbols bind locally and SIGSEGV in `__init_tcb+4` at launch. So all
//! the Oboe + audio code lives in a standalone `wzp-native` .so built
//! with `cargo-ndk`, and we dlopen it here at runtime.
//!
//! The Library handle lives in a `'static` `OnceLock` for the lifetime of
//! the process; all function pointers cached below borrow from it safely.

#![cfg(target_os = "android")]

use std::sync::OnceLock;

// ─── Library handle (kept alive forever) ─────────────────────────────────

static LIB: OnceLock<libloading::Library> = OnceLock::new();

// Cached function pointers, resolved once at init(). Each is a raw
// `extern "C"` fn pointer with effectively `'static` lifetime because
// LIB is a OnceLock that never drops.
static VERSION: OnceLock<unsafe extern "C" fn() -> i32> = OnceLock::new();
static HELLO: OnceLock<unsafe extern "C" fn(*mut u8, usize) -> usize> = OnceLock::new();
static AUDIO_START: OnceLock<unsafe extern "C" fn() -> i32> = OnceLock::new();
static AUDIO_START_BT: OnceLock<unsafe extern "C" fn() -> i32> = OnceLock::new();
static AUDIO_STOP: OnceLock<unsafe extern "C" fn()> = OnceLock::new();
static AUDIO_CAPTURE_AVAILABLE: OnceLock<extern "C" fn() -> usize> = OnceLock::new();
static AUDIO_READ_CAPTURE: OnceLock<unsafe extern "C" fn(*mut i16, usize) -> usize> = OnceLock::new();
static AUDIO_WRITE_PLAYOUT: OnceLock<unsafe extern "C" fn(*const i16, usize) -> usize> = OnceLock::new();
static AUDIO_IS_RUNNING: OnceLock<unsafe extern "C" fn() -> i32> = OnceLock::new();
static AUDIO_CAPTURE_LATENCY: OnceLock<unsafe extern "C" fn() -> f32> = OnceLock::new();
static AUDIO_PLAYOUT_LATENCY: OnceLock<unsafe extern "C" fn() -> f32> = OnceLock::new();

/// Load `libwzp_native.so` and resolve every exported function we use.
/// Call this once at app startup (from the Tauri `setup()` callback).
/// Subsequent calls are no-ops.
pub fn init() -> Result<(), String> {
    if LIB.get().is_some() {
        return Ok(());
    }

    // Open the sibling cdylib. The Android dynamic linker searches
    // /data/app/<pkg>/lib/arm64/ which gradle populates from jniLibs.
    let lib = unsafe { libloading::Library::new("libwzp_native.so") }
        .map_err(|e| format!("dlopen libwzp_native.so: {e}"))?;

    // Stash the Library into the OnceLock first so all Symbol lookups
    // below borrow from the 'static reference rather than a local.
    LIB.set(lib).map_err(|_| "wzp_native::LIB already set")?;
    let lib_ref: &'static libloading::Library = LIB.get().unwrap();

    unsafe {
        macro_rules! resolve {
            ($cell:expr, $ty:ty, $name:expr) => {{
                let sym: libloading::Symbol<$ty> = lib_ref.get($name)
                    .map_err(|e| format!("dlsym {}: {e}", core::str::from_utf8($name).unwrap_or("?")))?;
                // Dereference the Symbol to extract the raw fn pointer;
                // it stays valid because lib_ref is 'static.
                $cell.set(*sym).map_err(|_| format!("{} already set", core::str::from_utf8($name).unwrap_or("?")))?;
            }};
        }

        resolve!(VERSION, unsafe extern "C" fn() -> i32, b"wzp_native_version");
        resolve!(HELLO, unsafe extern "C" fn(*mut u8, usize) -> usize, b"wzp_native_hello");
        resolve!(AUDIO_START, unsafe extern "C" fn() -> i32, b"wzp_native_audio_start");
        resolve!(AUDIO_START_BT, unsafe extern "C" fn() -> i32, b"wzp_native_audio_start_bt");
        resolve!(AUDIO_STOP, unsafe extern "C" fn(), b"wzp_native_audio_stop");
        resolve!(AUDIO_CAPTURE_AVAILABLE, extern "C" fn() -> usize, b"wzp_native_audio_capture_available");
        resolve!(AUDIO_READ_CAPTURE, unsafe extern "C" fn(*mut i16, usize) -> usize, b"wzp_native_audio_read_capture");
        resolve!(AUDIO_WRITE_PLAYOUT, unsafe extern "C" fn(*const i16, usize) -> usize, b"wzp_native_audio_write_playout");
        resolve!(AUDIO_IS_RUNNING, unsafe extern "C" fn() -> i32, b"wzp_native_audio_is_running");
        resolve!(AUDIO_CAPTURE_LATENCY, unsafe extern "C" fn() -> f32, b"wzp_native_audio_capture_latency_ms");
        resolve!(AUDIO_PLAYOUT_LATENCY, unsafe extern "C" fn() -> f32, b"wzp_native_audio_playout_latency_ms");
    }

    Ok(())
}

/// Is `init()` done and all symbols cached?
pub fn is_loaded() -> bool {
    AUDIO_START.get().is_some()
}

// ─── Smoke-test accessors ────────────────────────────────────────────────

pub fn version() -> i32 {
    VERSION.get().map(|f| unsafe { f() }).unwrap_or(-1)
}

pub fn hello() -> String {
    let Some(f) = HELLO.get() else { return String::new(); };
    let mut buf = [0u8; 64];
    let n = unsafe { f(buf.as_mut_ptr(), buf.len()) };
    String::from_utf8_lossy(&buf[..n]).into_owned()
}

// ─── Audio accessors ─────────────────────────────────────────────────────

/// Start the Oboe capture + playout streams. Returns `Err(code)` on
/// failure. Idempotent on the wzp-native side.
pub fn audio_start() -> Result<(), i32> {
    let f = AUDIO_START.get().ok_or(-100_i32)?;
    let ret = unsafe { f() };
    if ret == 0 { Ok(()) } else { Err(ret) }
}

/// Start Oboe in Bluetooth SCO mode — capture skips sample rate and
/// input preset so the system routes to the BT SCO device natively.
pub fn audio_start_bt() -> Result<(), i32> {
    let f = AUDIO_START_BT.get().ok_or(-100_i32)?;
    let ret = unsafe { f() };
    if ret == 0 { Ok(()) } else { Err(ret) }
}

/// Stop both streams. Safe to call even if not running.
pub fn audio_stop() {
    if let Some(f) = AUDIO_STOP.get() {
        unsafe { f() };
    }
}

/// Number of capture samples available to read without blocking.
pub fn audio_capture_available() -> usize {
    let Some(f) = AUDIO_CAPTURE_AVAILABLE.get() else { return 0; };
    f()
}

/// Read captured i16 PCM into `out`. Returns bytes actually copied.
pub fn audio_read_capture(out: &mut [i16]) -> usize {
    let Some(f) = AUDIO_READ_CAPTURE.get() else { return 0; };
    unsafe { f(out.as_mut_ptr(), out.len()) }
}

/// Write i16 PCM into the playout ring. Returns samples enqueued.
pub fn audio_write_playout(input: &[i16]) -> usize {
    let Some(f) = AUDIO_WRITE_PLAYOUT.get() else { return 0; };
    unsafe { f(input.as_ptr(), input.len()) }
}

pub fn audio_is_running() -> bool {
    AUDIO_IS_RUNNING.get().map(|f| unsafe { f() } != 0).unwrap_or(false)
}

#[allow(dead_code)]
pub fn audio_capture_latency_ms() -> f32 {
    AUDIO_CAPTURE_LATENCY.get().map(|f| unsafe { f() }).unwrap_or(0.0)
}

#[allow(dead_code)]
pub fn audio_playout_latency_ms() -> f32 {
    AUDIO_PLAYOUT_LATENCY.get().map(|f| unsafe { f() }).unwrap_or(0.0)
}
