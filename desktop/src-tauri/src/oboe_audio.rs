//! Android audio backend for WarzonePhone Tauri mobile.
//!
//! Wraps the C++ Oboe bridge (cpp/oboe_bridge.cpp) — Oboe runs on high-priority
//! AAudio/OpenSL callback threads and reads/writes two shared SPSC ring buffers.
//! The Rust side drains the capture ring into the Opus encoder and fills the
//! playout ring with decoded PCM — no locking, no allocations, no cross-thread
//! channels on the hot path.
//!
//! Mirror of the proven implementation from `crates/wzp-android/src/audio_android.rs`
//! but stripped of the JNI-centric bits — this crate is a pure Tauri mobile lib.

#![cfg(target_os = "android")]

use std::sync::atomic::{AtomicI32, Ordering};
use std::sync::Arc;

use tracing::info;

/// 20 ms @ 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;

/// 8 frames × 960 samples = 160 ms headroom at 48 kHz.
const RING_CAPACITY: usize = 7680;

// ─── FFI to cpp/oboe_bridge.cpp ─────────────────────────────────────────────

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

// ─── Lock-free SPSC ring buffer (shared with C++ via AtomicI32) ────────────

/// Single-producer single-consumer ring buffer shared with the C++ Oboe callback.
///
/// The exposed method names (`available`, `read`, `write`) mirror
/// `wzp_client::audio_ring::AudioRing` so the CallEngine code can treat both
/// backends interchangeably via a cfg-switched type alias.
pub struct AudioRing {
    buf: Vec<i16>,
    capacity: usize,
    write_idx: AtomicI32,
    read_idx: AtomicI32,
}

// SAFETY: SPSC — producer owns write_idx, consumer owns read_idx, atomics
// provide acquire/release visibility between threads.
unsafe impl Send for AudioRing {}
unsafe impl Sync for AudioRing {}

impl AudioRing {
    pub fn new() -> Self {
        Self {
            buf: vec![0i16; RING_CAPACITY],
            capacity: RING_CAPACITY,
            write_idx: AtomicI32::new(0),
            read_idx: AtomicI32::new(0),
        }
    }

    /// Samples currently available to read.
    pub fn available(&self) -> usize {
        let w = self.write_idx.load(Ordering::Acquire);
        let r = self.read_idx.load(Ordering::Relaxed);
        let avail = w - r;
        if avail < 0 {
            (avail + self.capacity as i32) as usize
        } else {
            avail as usize
        }
    }

    /// Samples that can still be written without overflowing.
    pub fn available_write(&self) -> usize {
        self.capacity.saturating_sub(1).saturating_sub(self.available())
    }

    /// Producer side: write `data.len()` samples if room permits, else as many
    /// as fit. Returns the number of samples actually written.
    pub fn write(&self, data: &[i16]) -> usize {
        let avail = self.available_write();
        let count = data.len().min(avail);
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

    /// Consumer side: read up to `out.len()` samples. Returns count actually read.
    pub fn read(&self, out: &mut [i16]) -> usize {
        let avail = self.available();
        let count = out.len().min(avail);
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

// ─── Owning handle: starts/stops the Oboe streams and holds the rings alive ──

/// Owns both ring buffers and the active Oboe streams. Dropping this handle
/// stops the streams; the rings cannot outlive it because they're owned inside.
pub struct OboeHandle {
    capture: Arc<AudioRing>,
    playout: Arc<AudioRing>,
    started: bool,
}

impl OboeHandle {
    /// Allocate ring buffers and start the Oboe capture + playout streams.
    /// Returns the handle plus cloned ring Arcs for the CallEngine's send/recv tasks.
    pub fn start() -> Result<(Arc<AudioRing>, Arc<AudioRing>, Self), anyhow::Error> {
        let capture = Arc::new(AudioRing::new());
        let playout = Arc::new(AudioRing::new());

        let config = WzpOboeConfig {
            sample_rate: 48_000,
            frames_per_burst: FRAME_SAMPLES as i32,
            channel_count: 1,
        };
        let rings = WzpOboeRings {
            capture_buf: capture.buf_ptr(),
            capture_capacity: capture.capacity as i32,
            capture_write_idx: capture.write_idx_ptr(),
            capture_read_idx: capture.read_idx_ptr(),
            playout_buf: playout.buf_ptr(),
            playout_capacity: playout.capacity as i32,
            playout_write_idx: playout.write_idx_ptr(),
            playout_read_idx: playout.read_idx_ptr(),
        };

        let ret = unsafe { wzp_oboe_start(&config, &rings) };
        if ret != 0 {
            return Err(anyhow::anyhow!("wzp_oboe_start failed: code {ret}"));
        }
        info!(capture_latency_ms = unsafe { wzp_oboe_capture_latency_ms() },
              playout_latency_ms = unsafe { wzp_oboe_playout_latency_ms() },
              "oboe backend started");

        Ok((
            capture.clone(),
            playout.clone(),
            Self { capture, playout, started: true },
        ))
    }

    #[allow(unused)]
    pub fn is_running(&self) -> bool {
        unsafe { wzp_oboe_is_running() != 0 }
    }
}

impl Drop for OboeHandle {
    fn drop(&mut self) {
        if self.started {
            unsafe { wzp_oboe_stop() };
            info!("oboe backend stopped");
            self.started = false;
        }
        // Rings live as long as their Arcs do — the C++ side has already
        // stopped, so no more callbacks will touch the atomics.
        let _ = (&self.capture, &self.playout);
    }
}
