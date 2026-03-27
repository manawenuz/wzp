//! Sliding window replay protection.
//!
//! Tracks seen sequence numbers using a bitmap. Window size is 1024 packets.
//! Sequence numbers that are too old (more than WINDOW_SIZE behind the highest
//! seen) are rejected.

use wzp_proto::CryptoError;

/// Window size in packets.
const WINDOW_SIZE: u16 = 1024;

/// Sliding window anti-replay detector.
///
/// Uses a bitmap to track which sequence numbers have been seen within
/// the current window. Handles u16 wrapping correctly.
pub struct AntiReplayWindow {
    /// Highest sequence number seen so far.
    highest: u16,
    /// Bitmap of seen packets. Bit i corresponds to (highest - i).
    bitmap: Vec<u64>,
    /// Whether any packet has been received yet.
    initialized: bool,
}

impl AntiReplayWindow {
    /// Number of u64 words needed for the bitmap.
    const BITMAP_WORDS: usize = (WINDOW_SIZE as usize + 63) / 64;

    /// Create a new anti-replay window.
    pub fn new() -> Self {
        Self {
            highest: 0,
            bitmap: vec![0u64; Self::BITMAP_WORDS],
            initialized: false,
        }
    }

    /// Check if a sequence number is valid (not a replay, not too old).
    /// If valid, marks it as seen.
    pub fn check_and_update(&mut self, seq: u16) -> Result<(), CryptoError> {
        if !self.initialized {
            self.initialized = true;
            self.highest = seq;
            self.set_bit(0);
            return Ok(());
        }

        let diff = seq.wrapping_sub(self.highest);

        if diff == 0 {
            // Duplicate of highest
            return Err(CryptoError::ReplayDetected { seq });
        }

        if diff < 0x8000 {
            // seq is ahead of highest (wrapping-aware: diff in [1, 0x7FFF])
            let shift = diff as usize;
            self.advance_window(shift);
            self.highest = seq;
            self.set_bit(0);
            Ok(())
        } else {
            // seq is behind highest (wrapping-aware: diff in [0x8000, 0xFFFF])
            let behind = self.highest.wrapping_sub(seq) as usize;
            if behind >= WINDOW_SIZE as usize {
                return Err(CryptoError::ReplayDetected { seq });
            }
            if self.get_bit(behind) {
                return Err(CryptoError::ReplayDetected { seq });
            }
            self.set_bit(behind);
            Ok(())
        }
    }

    /// Advance the window by `shift` positions (shift left = new bits at position 0).
    fn advance_window(&mut self, shift: usize) {
        if shift >= WINDOW_SIZE as usize {
            for word in &mut self.bitmap {
                *word = 0;
            }
            return;
        }

        // We need to shift the entire bitmap right by `shift` bits.
        // Bit 0 of word 0 is the most recent. Shifting right means
        // old entries move to higher bit positions.
        let word_shift = shift / 64;
        let bit_shift = shift % 64;

        // Move words
        let len = self.bitmap.len();
        for i in (0..len).rev() {
            let mut val = 0u64;
            if i >= word_shift {
                val = self.bitmap[i - word_shift] << bit_shift;
                if bit_shift > 0 && i > word_shift {
                    val |= self.bitmap[i - word_shift - 1] >> (64 - bit_shift);
                }
            }
            self.bitmap[i] = val;
        }
        // Clear the lower words that shifted in
        for word in &mut self.bitmap[..word_shift.min(len)] {
            *word = 0;
        }
        // Clear the lower bits of the first non-shifted word
        if word_shift < len && bit_shift > 0 {
            self.bitmap[word_shift] &= !((1u64 << bit_shift) - 1);
        }
    }

    fn set_bit(&mut self, offset: usize) {
        let word = offset / 64;
        let bit = offset % 64;
        if word < self.bitmap.len() {
            self.bitmap[word] |= 1u64 << bit;
        }
    }

    fn get_bit(&self, offset: usize) -> bool {
        let word = offset / 64;
        let bit = offset % 64;
        if word < self.bitmap.len() {
            (self.bitmap[word] >> bit) & 1 == 1
        } else {
            false
        }
    }
}

impl Default for AntiReplayWindow {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn first_packet_accepted() {
        let mut w = AntiReplayWindow::new();
        assert!(w.check_and_update(0).is_ok());
    }

    #[test]
    fn duplicate_rejected() {
        let mut w = AntiReplayWindow::new();
        assert!(w.check_and_update(100).is_ok());
        assert!(w.check_and_update(100).is_err());
    }

    #[test]
    fn sequential_accepted() {
        let mut w = AntiReplayWindow::new();
        for i in 0..200 {
            assert!(w.check_and_update(i).is_ok(), "seq {} should be accepted", i);
        }
    }

    #[test]
    fn out_of_order_within_window() {
        let mut w = AntiReplayWindow::new();
        assert!(w.check_and_update(100).is_ok());
        assert!(w.check_and_update(95).is_ok());
        assert!(w.check_and_update(98).is_ok());
        assert!(w.check_and_update(102).is_ok());
        assert!(w.check_and_update(99).is_ok());
    }

    #[test]
    fn old_packet_rejected() {
        let mut w = AntiReplayWindow::new();
        assert!(w.check_and_update(0).is_ok());
        // Advance well past the window
        assert!(w.check_and_update(2000).is_ok());
        // seq 0 is now too old
        assert!(w.check_and_update(0).is_err());
    }

    #[test]
    fn wrapping_works() {
        let mut w = AntiReplayWindow::new();
        assert!(w.check_and_update(65530).is_ok());
        assert!(w.check_and_update(65535).is_ok());
        assert!(w.check_and_update(0).is_ok()); // wrapped
        assert!(w.check_and_update(1).is_ok());
        assert!(w.check_and_update(65535).is_err()); // duplicate
    }

    #[test]
    fn within_window_boundary() {
        let mut w = AntiReplayWindow::new();
        assert!(w.check_and_update(1023).is_ok());
        // 1023 - 0 = 1023, exactly at window boundary
        assert!(w.check_and_update(0).is_ok());
        // But 1024 behind would be out
        assert!(w.check_and_update(1024).is_ok());
        // Now 0 is 1024 behind 1024, which is at the boundary limit
        assert!(w.check_and_update(0).is_err()); // already seen or too old
    }
}
