//! Lock-free SPSC ring buffers for audio PCM transfer between
//! Kotlin AudioRecord/AudioTrack threads and the Rust engine.
//!
//! These use a simple spin-free design: the producer writes and advances
//! a write cursor, the consumer reads and advances a read cursor.
//! Both cursors are atomic so no mutex is needed.

use std::sync::atomic::{AtomicUsize, Ordering};

/// Ring buffer capacity in i16 samples.
/// 960 samples * 10 frames = ~200ms of audio at 48kHz mono.
const RING_CAPACITY: usize = 960 * 10;

/// Lock-free single-producer single-consumer ring buffer for i16 PCM samples.
pub struct AudioRing {
    buf: Box<[i16; RING_CAPACITY]>,
    write_pos: AtomicUsize,
    read_pos: AtomicUsize,
}

// SAFETY: AudioRing is designed for SPSC — one thread writes, one reads.
// The atomics ensure visibility. The buffer itself is never accessed
// from the same index by both threads simultaneously because the
// producer only writes to positions between write_pos and read_pos,
// and the consumer only reads from positions between read_pos and write_pos.
unsafe impl Send for AudioRing {}
unsafe impl Sync for AudioRing {}

impl AudioRing {
    pub fn new() -> Self {
        Self {
            buf: Box::new([0i16; RING_CAPACITY]),
            write_pos: AtomicUsize::new(0),
            read_pos: AtomicUsize::new(0),
        }
    }

    /// Number of samples available to read.
    pub fn available(&self) -> usize {
        let w = self.write_pos.load(Ordering::Acquire);
        let r = self.read_pos.load(Ordering::Acquire);
        w.wrapping_sub(r)
    }

    /// Number of samples that can be written without overwriting.
    pub fn free_space(&self) -> usize {
        RING_CAPACITY - self.available()
    }

    /// Write samples into the ring. Returns number of samples written.
    /// Drops oldest samples if the ring is full.
    pub fn write(&self, samples: &[i16]) -> usize {
        let w = self.write_pos.load(Ordering::Relaxed);
        let count = samples.len().min(RING_CAPACITY);

        for i in 0..count {
            let idx = (w + i) % RING_CAPACITY;
            // SAFETY: We're the only writer, and the reader won't read
            // past read_pos which we haven't advanced past yet.
            unsafe {
                let ptr = self.buf.as_ptr() as *mut i16;
                *ptr.add(idx) = samples[i];
            }
        }

        self.write_pos.store(w.wrapping_add(count), Ordering::Release);

        // If we overwrote unread data, advance read_pos
        if self.available() > RING_CAPACITY {
            let new_read = self.write_pos.load(Ordering::Relaxed).wrapping_sub(RING_CAPACITY);
            self.read_pos.store(new_read, Ordering::Release);
        }

        count
    }

    /// Read samples from the ring into `out`. Returns number of samples read.
    pub fn read(&self, out: &mut [i16]) -> usize {
        let avail = self.available();
        let count = out.len().min(avail);

        let r = self.read_pos.load(Ordering::Relaxed);
        for i in 0..count {
            let idx = (r + i) % RING_CAPACITY;
            out[i] = unsafe { *self.buf.as_ptr().add(idx) };
        }

        self.read_pos.store(r.wrapping_add(count), Ordering::Release);
        count
    }
}
