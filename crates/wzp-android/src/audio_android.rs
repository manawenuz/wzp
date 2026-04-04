//! Lock-free SPSC ring buffer audio backend for Android (Oboe).
//!
//! The ring buffers are shared between Rust and C++: the Oboe callbacks
//! (running on a high-priority audio thread) read/write directly into
//! the buffers via atomic indices, while the Rust codec thread on the
//! other side does the same.

use std::sync::atomic::{AtomicI32, Ordering};

use tracing::info;
#[allow(unused_imports)]
use tracing::warn;

/// Number of samples per 20 ms frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;

/// Default ring buffer capacity: 8 frames = 160 ms at 48 kHz.
const RING_CAPACITY: usize = 7680;

// ---------------------------------------------------------------------------
// FFI declarations matching oboe_bridge.h
// ---------------------------------------------------------------------------

#[repr(C)]
#[allow(non_snake_case)]
struct WzpOboeConfig {
    sample_rate: i32,
    frames_per_burst: i32,
    channel_count: i32,
}

#[repr(C)]
#[allow(non_snake_case)]
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

unsafe impl Send for WzpOboeRings {}
unsafe impl Sync for WzpOboeRings {}

unsafe extern "C" {
    fn wzp_oboe_start(config: *const WzpOboeConfig, rings: *const WzpOboeRings) -> i32;
    fn wzp_oboe_stop();
    fn wzp_oboe_capture_latency_ms() -> f32;
    fn wzp_oboe_playout_latency_ms() -> f32;
    fn wzp_oboe_is_running() -> i32;
}

// ---------------------------------------------------------------------------
// SPSC Ring Buffer
// ---------------------------------------------------------------------------

/// Single-producer single-consumer lock-free ring buffer.
///
/// The producer calls `write()` and the consumer calls `read()`.
/// Atomics use acquire/release ordering to ensure correct visibility
/// across the Oboe audio thread and the Rust codec thread.
pub struct RingBuffer {
    buf: Vec<i16>,
    capacity: usize,
    write_idx: AtomicI32,
    read_idx: AtomicI32,
}

impl RingBuffer {
    /// Create a new ring buffer with the given capacity (in samples).
    ///
    /// The actual usable capacity is `capacity - 1` to distinguish
    /// full from empty.
    pub fn new(capacity: usize) -> Self {
        Self {
            buf: vec![0i16; capacity],
            capacity,
            write_idx: AtomicI32::new(0),
            read_idx: AtomicI32::new(0),
        }
    }

    /// Number of samples available to read.
    pub fn available_read(&self) -> usize {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Relaxed);
        let avail = w - r;
        if avail < 0 {
            (avail + self.capacity as i32) as usize
        } else {
            avail as usize
        }
    }

    /// Number of samples that can be written before the buffer is full.
    pub fn available_write(&self) -> usize {
        self.capacity - 1 - self.available_read()
    }

    /// Write samples into the ring buffer (producer side).
    ///
    /// Returns the number of samples actually written (may be less than
    /// `data.len()` if the buffer is nearly full).
    pub fn write(&self, data: &[i16]) -> usize {
        let avail = self.available_write();
        let count = data.len().min(avail);
        if count == 0 {
            return 0;
        }

        let mut w = self.write_idx.load(Ordering::Relaxed) as usize;
        let cap = self.capacity;
        let buf_ptr = self.buf.as_ptr() as *mut i16;

        for i in 0..count {
            // SAFETY: w is always in [0, capacity) and we are the sole producer.
            unsafe {
                *buf_ptr.add(w) = data[i];
            }
            w += 1;
            if w >= cap {
                w = 0;
            }
        }

        self.write_idx.store(w as i32, Ordering::Release);
        count
    }

    /// Read samples from the ring buffer (consumer side).
    ///
    /// Returns the number of samples actually read (may be less than
    /// `out.len()` if the buffer doesn't have enough data).
    pub fn read(&self, out: &mut [i16]) -> usize {
        let avail = self.available_read();
        let count = out.len().min(avail);
        if count == 0 {
            return 0;
        }

        let mut r = self.read_idx.load(Ordering::Relaxed) as usize;
        let cap = self.capacity;
        let buf_ptr = self.buf.as_ptr();

        for i in 0..count {
            // SAFETY: r is always in [0, capacity) and we are the sole consumer.
            unsafe {
                out[i] = *buf_ptr.add(r);
            }
            r += 1;
            if r >= cap {
                r = 0;
            }
        }

        self.read_idx.store(r as i32, Ordering::Release);
        count
    }

    /// Get a raw pointer to the buffer data (for FFI).
    fn buf_ptr(&self) -> *mut i16 {
        self.buf.as_ptr() as *mut i16
    }

    /// Get a raw pointer to the write index atomic (for FFI).
    fn write_idx_ptr(&self) -> *mut AtomicI32 {
        &self.write_idx as *const AtomicI32 as *mut AtomicI32
    }

    /// Get a raw pointer to the read index atomic (for FFI).
    fn read_idx_ptr(&self) -> *mut AtomicI32 {
        &self.read_idx as *const AtomicI32 as *mut AtomicI32
    }
}

// SAFETY: The ring buffer is designed for SPSC use where producer and consumer
// are on different threads. The atomic indices provide the synchronization.
unsafe impl Send for RingBuffer {}
unsafe impl Sync for RingBuffer {}

// ---------------------------------------------------------------------------
// Oboe Backend
// ---------------------------------------------------------------------------

/// Oboe-based audio backend for Android.
///
/// Owns two SPSC ring buffers (capture and playout) that are shared with
/// the C++ Oboe callbacks via raw pointers. The Oboe callbacks run on
/// high-priority audio threads managed by the Android audio system.
pub struct OboeBackend {
    capture_ring: RingBuffer,
    playout_ring: RingBuffer,
    started: bool,
}

impl OboeBackend {
    /// Create a new backend with default ring buffer sizes (160 ms each).
    pub fn new() -> Self {
        Self {
            capture_ring: RingBuffer::new(RING_CAPACITY),
            playout_ring: RingBuffer::new(RING_CAPACITY),
            started: false,
        }
    }

    /// Start Oboe audio streams.
    ///
    /// This sets up the ring buffer pointers and calls into the C++ layer
    /// to open and start the capture and playout Oboe streams.
    pub fn start(&mut self) -> Result<(), anyhow::Error> {
        if self.started {
            return Ok(());
        }

        let config = WzpOboeConfig {
            sample_rate: 48_000,
            frames_per_burst: FRAME_SAMPLES as i32,
            channel_count: 1,
        };

        let rings = WzpOboeRings {
            capture_buf: self.capture_ring.buf_ptr(),
            capture_capacity: self.capture_ring.capacity as i32,
            capture_write_idx: self.capture_ring.write_idx_ptr(),
            capture_read_idx: self.capture_ring.read_idx_ptr(),

            playout_buf: self.playout_ring.buf_ptr(),
            playout_capacity: self.playout_ring.capacity as i32,
            playout_write_idx: self.playout_ring.write_idx_ptr(),
            playout_read_idx: self.playout_ring.read_idx_ptr(),
        };

        let ret = unsafe { wzp_oboe_start(&config, &rings) };
        if ret != 0 {
            return Err(anyhow::anyhow!("wzp_oboe_start failed with code {}", ret));
        }

        self.started = true;
        info!("Oboe backend started");
        Ok(())
    }

    /// Stop Oboe audio streams.
    pub fn stop(&mut self) {
        if !self.started {
            return;
        }
        unsafe { wzp_oboe_stop() };
        self.started = false;
        info!("Oboe backend stopped");
    }

    /// Read captured audio samples from the capture ring buffer.
    ///
    /// Returns the number of samples actually read. The caller should
    /// provide a buffer of at least `FRAME_SAMPLES` (960) samples.
    pub fn read_capture(&self, out: &mut [i16]) -> usize {
        self.capture_ring.read(out)
    }

    /// Write audio samples to the playout ring buffer.
    ///
    /// Returns the number of samples actually written.
    pub fn write_playout(&self, samples: &[i16]) -> usize {
        self.playout_ring.write(samples)
    }

    /// Get the current capture latency in milliseconds (from Oboe).
    #[allow(unused)]
    pub fn capture_latency_ms(&self) -> f32 {
        unsafe { wzp_oboe_capture_latency_ms() }
    }

    /// Get the current playout latency in milliseconds (from Oboe).
    #[allow(unused)]
    pub fn playout_latency_ms(&self) -> f32 {
        unsafe { wzp_oboe_playout_latency_ms() }
    }

    /// Check if the Oboe streams are currently running.
    #[allow(unused)]
    pub fn is_running(&self) -> bool {
        unsafe { wzp_oboe_is_running() != 0 }
    }
}

impl Drop for OboeBackend {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Thread affinity / priority helpers
// ---------------------------------------------------------------------------

/// Pin the current thread to the highest-numbered CPU cores (big cores on
/// ARM big.LITTLE architectures). Falls back silently on failure.
#[allow(unused)]
pub fn pin_to_big_core() {
    #[cfg(target_os = "android")]
    {
        unsafe {
            let num_cpus = libc::sysconf(libc::_SC_NPROCESSORS_ONLN);
            if num_cpus <= 0 {
                warn!("pin_to_big_core: could not determine CPU count");
                return;
            }
            let num_cpus = num_cpus as usize;

            // Target the upper half of CPUs (big cores on most big.LITTLE SoCs)
            let start = num_cpus / 2;
            let mut set: libc::cpu_set_t = std::mem::zeroed();
            libc::CPU_ZERO(&mut set);
            for cpu in start..num_cpus {
                libc::CPU_SET(cpu, &mut set);
            }

            let ret = libc::sched_setaffinity(
                0, // current thread
                std::mem::size_of::<libc::cpu_set_t>(),
                &set,
            );
            if ret != 0 {
                warn!("sched_setaffinity failed: {}", std::io::Error::last_os_error());
            } else {
                info!(start, num_cpus, "pinned to big cores");
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        // No-op on non-Android
    }
}

/// Attempt to set SCHED_FIFO real-time priority for the current thread.
/// Falls back silently on failure (requires appropriate permissions on Android).
#[allow(unused)]
pub fn set_realtime_priority() {
    #[cfg(target_os = "android")]
    {
        unsafe {
            let param = libc::sched_param {
                sched_priority: 2, // Low RT priority — enough for audio, safe
            };
            let ret = libc::sched_setscheduler(0, libc::SCHED_FIFO, &param);
            if ret != 0 {
                warn!(
                    "sched_setscheduler(SCHED_FIFO) failed: {}",
                    std::io::Error::last_os_error()
                );
            } else {
                info!("set SCHED_FIFO priority 2");
            }
        }
    }
    #[cfg(not(target_os = "android"))]
    {
        // No-op on non-Android
    }
}

// ---------------------------------------------------------------------------
// Tests
// ---------------------------------------------------------------------------

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn ring_buffer_write_read() {
        let ring = RingBuffer::new(16);
        let data = [1i16, 2, 3, 4, 5];
        assert_eq!(ring.write(&data), 5);
        assert_eq!(ring.available_read(), 5);

        let mut out = [0i16; 5];
        assert_eq!(ring.read(&mut out), 5);
        assert_eq!(out, [1, 2, 3, 4, 5]);
        assert_eq!(ring.available_read(), 0);
    }

    #[test]
    fn ring_buffer_wraparound() {
        let ring = RingBuffer::new(8);
        let data = [10i16, 20, 30, 40, 50, 60]; // 6 samples, capacity 8 (usable 7)
        assert_eq!(ring.write(&data), 6);

        let mut out = [0i16; 4];
        assert_eq!(ring.read(&mut out), 4);
        assert_eq!(out, [10, 20, 30, 40]);

        // Now write more, which should wrap around
        let data2 = [70i16, 80, 90, 100];
        assert_eq!(ring.write(&data2), 4);

        let mut out2 = [0i16; 6];
        assert_eq!(ring.read(&mut out2), 6);
        assert_eq!(out2, [50, 60, 70, 80, 90, 100]);
    }

    #[test]
    fn ring_buffer_full() {
        let ring = RingBuffer::new(4); // usable capacity = 3
        let data = [1i16, 2, 3, 4, 5];
        assert_eq!(ring.write(&data), 3); // Only 3 fit
        assert_eq!(ring.available_write(), 0);
    }

    #[test]
    fn oboe_backend_stub_start_stop() {
        let mut backend = OboeBackend::new();
        backend.start().expect("stub start should succeed");
        assert!(backend.started);
        backend.stop();
        assert!(!backend.started);
    }
}
