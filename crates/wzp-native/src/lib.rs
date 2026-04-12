//! wzp-native — standalone Android cdylib for all the C++ audio code.
//!
//! Built with `cargo ndk`, NOT `cargo tauri android build`. Loaded at
//! runtime by the Tauri desktop cdylib (`wzp-desktop`) via libloading.
//! See `docs/incident-tauri-android-init-tcb.md` for why the split exists.
//!
//! Phase 2: real Oboe audio backend.
//!
//! Architecture: Oboe runs capture + playout streams on its own high-
//! priority AAudio callback threads inside the C++ bridge. Two SPSC ring
//! buffers (capture and playout) are shared between the C++ callbacks
//! and the Rust side via atomic indices — no locks on the hot path.
//! `wzp-desktop` drains the capture ring into its Opus encoder and fills
//! the playout ring with decoded PCM.

use std::sync::atomic::{AtomicI32, Ordering};

// ─── Phase 1 smoke-test exports (kept for sanity checks) ─────────────────

/// Returns 42. Used by wzp-desktop's setup() to verify dlopen + dlsym
/// work before any audio code runs.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_version() -> i32 {
    42
}

/// Writes a NUL-terminated string into `out` (capped at `cap`) and
/// returns bytes written excluding the NUL.
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wzp_native_hello(out: *mut u8, cap: usize) -> usize {
    const MSG: &[u8] = b"hello from wzp-native\0";
    if out.is_null() || cap == 0 {
        return 0;
    }
    let n = MSG.len().min(cap);
    unsafe {
        core::ptr::copy_nonoverlapping(MSG.as_ptr(), out, n);
        *out.add(n - 1) = 0;
    }
    n - 1
}

// ─── C++ Oboe bridge FFI ─────────────────────────────────────────────────

#[repr(C)]
struct WzpOboeConfig {
    sample_rate: i32,
    frames_per_burst: i32,
    channel_count: i32,
    /// When nonzero, capture stream skips setSampleRate and setInputPreset
    /// so the system can route to BT SCO at its native rate (8/16kHz).
    /// Oboe's SampleRateConversionQuality::Best resamples to 48kHz.
    bt_active: i32,
}

#[repr(C)]
struct WzpOboeRings {
    capture_buf: *mut i16,
    capture_capacity: i32,
    capture_write_idx: *mut AtomicI32,
    capture_read_idx: *mut AtomicI32,
    playout_buf: *mut i16,
    playout_capacity: i32,
    playout_write_idx: *mut AtomicI32,
    playout_read_idx: *mut AtomicI32,
}

// SAFETY: atomics synchronise producer/consumer; raw pointers are owned
// by the AudioBackend singleton below whose lifetime covers all calls.
unsafe impl Send for WzpOboeRings {}
unsafe impl Sync for WzpOboeRings {}

unsafe extern "C" {
    fn wzp_oboe_start(config: *const WzpOboeConfig, rings: *const WzpOboeRings) -> i32;
    fn wzp_oboe_stop();
    fn wzp_oboe_capture_latency_ms() -> f32;
    fn wzp_oboe_playout_latency_ms() -> f32;
    fn wzp_oboe_is_running() -> i32;
}

// ─── SPSC ring buffer (shared with C++ via AtomicI32) ────────────────────

/// 20 ms @ 48 kHz mono = 960 samples.
const FRAME_SAMPLES: usize = 960;
/// ~160 ms headroom at 48 kHz.
const RING_CAPACITY: usize = 7680;

struct RingBuffer {
    buf: Vec<i16>,
    capacity: usize,
    write_idx: AtomicI32,
    read_idx: AtomicI32,
}

// SAFETY: SPSC with atomic read/write cursors; producer and consumer
// are always on different threads.
unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

impl RingBuffer {
    fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0i16; capacity],
            capacity,
            write_idx: AtomicI32::new(0),
            read_idx: AtomicI32::new(0),
        }
    }

    fn available_read(&self) -> usize {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Relaxed);
        let avail = w - r;
        if avail < 0 { (avail + self.capacity as i32) as usize } else { avail as usize }
    }

    fn available_write(&self) -> usize {
        self.capacity - 1 - self.available_read()
    }

    fn write(&self, data: &[i16]) -> usize {
        let count = data.len().min(self.available_write());
        if count == 0 {
            return 0;
        }
        let mut w = self.write_idx.load(Ordering::Relaxed) as usize;
        let cap = self.capacity;
        let buf_ptr = self.buf.as_ptr() as *mut i16;
        for sample in &data[..count] {
            unsafe { *buf_ptr.add(w) = *sample; }
            w += 1;
            if w >= cap { w = 0; }
        }
        self.write_idx.store(w as i32, Ordering::Release);
        count
    }

    fn read(&self, out: &mut [i16]) -> usize {
        let count = out.len().min(self.available_read());
        if count == 0 {
            return 0;
        }
        let mut r = self.read_idx.load(Ordering::Relaxed) as usize;
        let cap = self.capacity;
        let buf_ptr = self.buf.as_ptr();
        for slot in &mut out[..count] {
            unsafe { *slot = *buf_ptr.add(r); }
            r += 1;
            if r >= cap { r = 0; }
        }
        self.read_idx.store(r as i32, Ordering::Release);
        count
    }

    fn buf_ptr(&self) -> *mut i16 {
        self.buf.as_ptr() as *mut i16
    }
    fn write_idx_ptr(&self) -> *mut AtomicI32 {
        &self.write_idx as *const AtomicI32 as *mut AtomicI32
    }
    fn read_idx_ptr(&self) -> *mut AtomicI32 {
        &self.read_idx as *const AtomicI32 as *mut AtomicI32
    }
}

// ─── AudioBackend singleton ──────────────────────────────────────────────
//
// There is one global AudioBackend instance because Oboe's C++ side
// holds its own singleton of the streams. The `Box::leak`'d statics own
// the ring buffers for the lifetime of the process — dropping them while
// Oboe is still running would cause use-after-free in the audio callback.

use std::sync::OnceLock;

struct AudioBackend {
    capture: RingBuffer,
    playout: RingBuffer,
    started: std::sync::Mutex<bool>,
    /// Per-write logging throttle counter for wzp_native_audio_write_playout.
    playout_write_log_count: std::sync::atomic::AtomicU64,
    /// Fix A (task #35): the playout ring's read_idx at the last
    /// check. If audio_write_playout observes read_idx hasn't
    /// advanced after N writes, the Oboe playout callback has
    /// stopped firing → restart the streams.
    playout_last_read_idx: std::sync::atomic::AtomicI32,
    /// Number of writes since the last read_idx advance.
    playout_stall_writes: std::sync::atomic::AtomicU32,
}

static BACKEND: OnceLock<&'static AudioBackend> = OnceLock::new();

fn backend() -> &'static AudioBackend {
    BACKEND.get_or_init(|| {
        Box::leak(Box::new(AudioBackend {
            capture: RingBuffer::new(RING_CAPACITY),
            playout: RingBuffer::new(RING_CAPACITY),
            started: std::sync::Mutex::new(false),
            playout_write_log_count: std::sync::atomic::AtomicU64::new(0),
            playout_last_read_idx: std::sync::atomic::AtomicI32::new(0),
            playout_stall_writes: std::sync::atomic::AtomicU32::new(0),
        }))
    })
}

// ─── C FFI for wzp-desktop ───────────────────────────────────────────────

/// Start the Oboe audio streams. Returns 0 on success, non-zero on error.
/// Idempotent — calling while already running is a no-op that returns 0.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_audio_start() -> i32 {
    audio_start_inner(false)
}

/// Start Oboe in Bluetooth SCO mode — skips sample rate and input preset
/// on capture so the system can route to the BT SCO device natively.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_audio_start_bt() -> i32 {
    audio_start_inner(true)
}

fn audio_start_inner(bt: bool) -> i32 {
    let b = backend();
    let mut started = match b.started.lock() {
        Ok(g) => g,
        Err(_) => return -1,
    };
    if *started {
        return 0;
    }

    let config = WzpOboeConfig {
        sample_rate: 48_000,
        frames_per_burst: FRAME_SAMPLES as i32,
        channel_count: 1,
        bt_active: if bt { 1 } else { 0 },
    };
    let rings = WzpOboeRings {
        capture_buf: b.capture.buf_ptr(),
        capture_capacity: b.capture.capacity as i32,
        capture_write_idx: b.capture.write_idx_ptr(),
        capture_read_idx: b.capture.read_idx_ptr(),
        playout_buf: b.playout.buf_ptr(),
        playout_capacity: b.playout.capacity as i32,
        playout_write_idx: b.playout.write_idx_ptr(),
        playout_read_idx: b.playout.read_idx_ptr(),
    };
    let ret = unsafe { wzp_oboe_start(&config, &rings) };
    if ret != 0 {
        return ret;
    }
    *started = true;
    0
}

/// Stop Oboe. Idempotent. Safe to call from any thread.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_audio_stop() {
    let b = backend();
    if let Ok(mut started) = b.started.lock() {
        if *started {
            unsafe { wzp_oboe_stop() };
            *started = false;
        }
    }
}

/// Read captured PCM samples from the capture ring. Returns the number
/// of `i16` samples actually copied into `out` (may be less than
/// `out_len` if the ring is empty).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wzp_native_audio_read_capture(out: *mut i16, out_len: usize) -> usize {
    if out.is_null() || out_len == 0 {
        return 0;
    }
    let slice = unsafe { std::slice::from_raw_parts_mut(out, out_len) };
    backend().capture.read(slice)
}

/// Write PCM samples into the playout ring. Returns the number of
/// samples actually enqueued (may be less than `in_len` if the ring
/// is nearly full — in practice the caller should pace to 20 ms
/// frames and spin briefly if the ring is full).
#[unsafe(no_mangle)]
pub unsafe extern "C" fn wzp_native_audio_write_playout(input: *const i16, in_len: usize) -> usize {
    if input.is_null() || in_len == 0 {
        return 0;
    }
    let slice = unsafe { std::slice::from_raw_parts(input, in_len) };
    let b = backend();

    // Fix A (task #35): detect playout callback stall. If the
    // playout ring's read_idx hasn't advanced in 50+ writes
    // (~1 second at 50 writes/sec), the Oboe playout callback
    // has stopped firing → restart the streams. This is the
    // self-healing behavior that makes rejoin work: teardown +
    // rebuild clears whatever HAL state locked up the callback.
    let current_read_idx = b.playout.read_idx.load(std::sync::atomic::Ordering::Relaxed);
    let last_read_idx = b.playout_last_read_idx.load(std::sync::atomic::Ordering::Relaxed);
    if current_read_idx == last_read_idx {
        let stall = b.playout_stall_writes.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
        if stall >= 50 {
            // Callback hasn't drained anything in ~1 second.
            // Force a stream restart.
            unsafe {
                android_log("playout STALL detected (50 writes, read_idx unchanged) — restarting Oboe streams");
            }
            b.playout_stall_writes.store(0, std::sync::atomic::Ordering::Relaxed);
            // Release the started lock, stop, re-start.
            // This is the same logic as the Rust-side
            // audio_stop() + audio_start() but done inline
            // because we can't call the extern "C" fns
            // recursively. Just call the C++ side directly.
            {
                if let Ok(mut started) = b.started.lock() {
                    if *started {
                        unsafe { wzp_oboe_stop() };
                        *started = false;
                    }
                }
            }
            // Clear the rings so the restart doesn't read stale data
            b.playout.write_idx.store(0, std::sync::atomic::Ordering::Relaxed);
            b.playout.read_idx.store(0, std::sync::atomic::Ordering::Relaxed);
            b.capture.write_idx.store(0, std::sync::atomic::Ordering::Relaxed);
            b.capture.read_idx.store(0, std::sync::atomic::Ordering::Relaxed);
            // Re-start (stall detector — always non-BT mode)
            let config = WzpOboeConfig {
                sample_rate: 48_000,
                frames_per_burst: FRAME_SAMPLES as i32,
                channel_count: 1,
                bt_active: 0,
            };
            let rings = WzpOboeRings {
                capture_buf: b.capture.buf_ptr(),
                capture_capacity: b.capture.capacity as i32,
                capture_write_idx: b.capture.write_idx_ptr(),
                capture_read_idx: b.capture.read_idx_ptr(),
                playout_buf: b.playout.buf_ptr(),
                playout_capacity: b.playout.capacity as i32,
                playout_write_idx: b.playout.write_idx_ptr(),
                playout_read_idx: b.playout.read_idx_ptr(),
            };
            let ret = unsafe { wzp_oboe_start(&config, &rings) };
            if ret == 0 {
                if let Ok(mut started) = b.started.lock() {
                    *started = true;
                }
                unsafe { android_log("playout restart OK — Oboe streams rebuilt"); }
            } else {
                unsafe { android_log(&format!("playout restart FAILED: {ret}")); }
            }
            b.playout_last_read_idx.store(0, std::sync::atomic::Ordering::Relaxed);
            return 0; // caller will retry on next frame
        }
    } else {
        // read_idx advanced — callback is alive, reset counter
        b.playout_stall_writes.store(0, std::sync::atomic::Ordering::Relaxed);
        b.playout_last_read_idx.store(current_read_idx, std::sync::atomic::Ordering::Relaxed);
    }

    let before_w = b.playout.write_idx.load(std::sync::atomic::Ordering::Relaxed);
    let before_r = b.playout.read_idx.load(std::sync::atomic::Ordering::Relaxed);
    let written = b.playout.write(slice);
    // First few writes: log ring state + sample range so we can compare what
    // engine.rs hands us to what the C++ playout callback reads.
    let first_writes = b.playout_write_log_count.fetch_add(1, std::sync::atomic::Ordering::Relaxed);
    if first_writes < 3 || first_writes % 50 == 0 {
        let (mut lo, mut hi, mut sumsq) = (i16::MAX, i16::MIN, 0i64);
        for &s in slice.iter() {
            if s < lo { lo = s; }
            if s > hi { hi = s; }
            sumsq += (s as i64) * (s as i64);
        }
        let rms = (sumsq as f64 / slice.len() as f64).sqrt() as i32;
        let avail_w_after = b.playout.available_write();
        let avail_r_after = b.playout.available_read();
        let msg = format!(
            "playout WRITE #{first_writes}: in_len={} written={} range=[{lo}..{hi}] rms={rms} before_w={before_w} before_r={before_r} avail_read_after={avail_r_after} avail_write_after={avail_w_after}",
            slice.len(), written
        );
        unsafe {
            android_log(msg.as_str());
        }
    }
    written
}

// Minimal android logcat shim so we can print from the cdylib without pulling
// in android_logger crate (which would add another dep that has to build with
// cargo-ndk). Uses libc's __android_log_print via extern linkage.
#[cfg(target_os = "android")]
unsafe extern "C" {
    fn __android_log_write(prio: i32, tag: *const u8, text: *const u8) -> i32;
}

#[cfg(target_os = "android")]
unsafe fn android_log(msg: &str) {
    // ANDROID_LOG_INFO = 4. Tag and text must be NUL-terminated.
    let tag = b"wzp-native\0";
    let mut buf = Vec::with_capacity(msg.len() + 1);
    buf.extend_from_slice(msg.as_bytes());
    buf.push(0);
    unsafe { __android_log_write(4, tag.as_ptr(), buf.as_ptr()); }
}

#[cfg(not(target_os = "android"))]
#[allow(dead_code)]
unsafe fn android_log(_msg: &str) {}

/// Current capture latency reported by Oboe, in milliseconds. Returns
/// NaN / 0.0 if the stream isn't running.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_audio_capture_latency_ms() -> f32 {
    unsafe { wzp_oboe_capture_latency_ms() }
}

/// Current playout latency reported by Oboe, in milliseconds.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_audio_playout_latency_ms() -> f32 {
    unsafe { wzp_oboe_playout_latency_ms() }
}

/// Non-zero if both Oboe streams are currently running.
#[unsafe(no_mangle)]
pub extern "C" fn wzp_native_audio_is_running() -> i32 {
    unsafe { wzp_oboe_is_running() }
}
