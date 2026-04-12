//! Lock-free SPSC ring buffer — "Reader-Detects-Lap" architecture.
//!
//! SPSC invariant: the producer ONLY writes `write_pos`, the consumer
//! ONLY writes `read_pos`.  Neither thread touches the other's cursor.
//!
//! On overflow (writer laps the reader), the writer simply overwrites
//! old buffer data.  The reader detects the lap via `available() >
//! RING_CAPACITY` and snaps its own `read_pos` forward.
//!
//! Capacity is a power of 2 for bitmask indexing (no modulo).

use std::sync::atomic::{AtomicU64, AtomicUsize, Ordering};

/// Ring buffer capacity — power of 2 for bitmask indexing.
/// 16384 samples = 341.3ms at 48kHz mono.
const RING_CAPACITY: usize = 16384; // 2^14
const RING_MASK: usize = RING_CAPACITY - 1;

/// Lock-free single-producer single-consumer ring buffer for i16 PCM samples.
pub struct AudioRing {
    buf: Box<[i16]>,
    /// Monotonically increasing write cursor. ONLY written by producer.
    write_pos: AtomicUsize,
    /// Monotonically increasing read cursor. ONLY written by consumer.
    read_pos: AtomicUsize,
    /// Incremented by reader when it detects it was lapped (overflow).
    overflow_count: AtomicU64,
    /// Incremented by reader when ring is empty (underrun).
    underrun_count: AtomicU64,
}

// SAFETY: AudioRing is SPSC — one thread writes (producer), one reads (consumer).
// The producer only writes write_pos. The consumer only writes read_pos.
// Neither thread writes the other's cursor. Buffer indices are derived from
// the owning thread's cursor, ensuring no concurrent access to the same index.
unsafe impl Send for AudioRing {}
unsafe impl Sync for AudioRing {}

impl AudioRing {
    pub fn new() -> Self {
        debug_assert!(RING_CAPACITY.is_power_of_two());
        Self {
            buf: vec![0i16; RING_CAPACITY].into_boxed_slice(),
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
            overflow_count: AtomicU64::new(0),
            underrun_count: AtomicU64::new(0),
        }
    }

    /// Number of samples available to read (clamped to capacity).
    pub fn available(&self) -> usize {
        let w = self.write_pos.load(Ordering::Acquire);
        let r = self.read_pos.load(Ordering::Relaxed);
        w.wrapping_sub(r).min(RING_CAPACITY)
    }

    /// Write samples into the ring. Returns number of samples written.
    ///
    /// If the ring is full, old data is silently overwritten.  The reader
    /// will detect the lap and self-correct.  The writer NEVER touches
    /// `read_pos`.
    pub fn write(&self, samples: &[i16]) -> usize {
        let count = samples.len().min(RING_CAPACITY);
        let w = self.write_pos.load(Ordering::Relaxed);

        for i in 0..count {
            unsafe {
                let ptr = self.buf.as_ptr() as *mut i16;
                *ptr.add((w + i) & RING_MASK) = samples[i];
            }
        }

        self.write_pos
            .store(w.wrapping_add(count), Ordering::Release);
        count
    }

    /// Read samples from the ring into `out`. Returns number of samples read.
    ///
    /// If the writer has lapped the reader (overflow), `read_pos` is snapped
    /// forward to the oldest valid data.
    pub fn read(&self, out: &mut [i16]) -> usize {
        let w = self.write_pos.load(Ordering::Acquire);
        let mut r = self.read_pos.load(Ordering::Relaxed);

        let mut avail = w.wrapping_sub(r);

        // Lap detection: writer has overwritten our unread data.
        if avail > RING_CAPACITY {
            r = w.wrapping_sub(RING_CAPACITY);
            avail = RING_CAPACITY;
            self.overflow_count.fetch_add(1, Ordering::Relaxed);
        }

        let count = out.len().min(avail);
        if count == 0 {
            if w == r {
                self.underrun_count.fetch_add(1, Ordering::Relaxed);
            }
            return 0;
        }

        for i in 0..count {
            out[i] = unsafe { *self.buf.as_ptr().add((r + i) & RING_MASK) };
        }

        self.read_pos
            .store(r.wrapping_add(count), Ordering::Release);
        count
    }

    /// Number of overflow events (reader was lapped by writer).
    pub fn overflow_count(&self) -> u64 {
        self.overflow_count.load(Ordering::Relaxed)
    }

    /// Number of underrun events (reader found empty buffer).
    pub fn underrun_count(&self) -> u64 {
        self.underrun_count.load(Ordering::Relaxed)
    }
}
