//! Acoustic Echo Cancellation using NLMS adaptive filter.
//! Processes 480-sample (10ms) sub-frames at 48kHz.

/// NLMS (Normalized Least Mean Squares) adaptive filter echo canceller.
///
/// Removes acoustic echo by modelling the echo path between the far-end
/// (speaker) signal and the near-end (microphone) signal, then subtracting
/// the estimated echo from the near-end in real time.
pub struct EchoCanceller {
    filter_coeffs: Vec<f32>,
    filter_len: usize,
    far_end_buf: Vec<f32>,
    far_end_pos: usize,
    mu: f32,
    enabled: bool,
}

impl EchoCanceller {
    /// Create a new echo canceller.
    ///
    /// * `sample_rate` — typically 48000
    /// * `filter_ms`   — echo-tail length in milliseconds (e.g. 100 for 100 ms)
    pub fn new(sample_rate: u32, filter_ms: u32) -> Self {
        let filter_len = (sample_rate as usize) * (filter_ms as usize) / 1000;
        Self {
            filter_coeffs: vec![0.0f32; filter_len],
            filter_len,
            far_end_buf: vec![0.0f32; filter_len],
            far_end_pos: 0,
            mu: 0.01,
            enabled: true,
        }
    }

    /// Feed far-end (speaker/playback) samples into the circular buffer.
    ///
    /// Must be called with the audio that was played out through the speaker
    /// *before* the corresponding near-end frame is processed.
    pub fn feed_farend(&mut self, farend: &[i16]) {
        for &s in farend {
            self.far_end_buf[self.far_end_pos] = s as f32;
            self.far_end_pos = (self.far_end_pos + 1) % self.filter_len;
        }
    }

    /// Process a near-end (microphone) frame, removing the estimated echo.
    ///
    /// Returns the echo-return-loss enhancement (ERLE) as a ratio: the RMS of
    /// the original near-end divided by the RMS of the residual.  Values > 1.0
    /// mean echo was reduced.
    pub fn process_frame(&mut self, nearend: &mut [i16]) -> f32 {
        if !self.enabled {
            return 1.0;
        }

        let n = nearend.len();
        let fl = self.filter_len;

        let mut sum_near_sq: f64 = 0.0;
        let mut sum_err_sq: f64 = 0.0;

        for i in 0..n {
            let near_f = nearend[i] as f32;

            // --- estimate echo as dot(coeffs, farend_window) ---
            // The far-end window for this sample starts at
            //   (far_end_pos - 1 - i) mod filter_len   (most recent)
            // and goes back filter_len samples.
            let mut echo_est: f32 = 0.0;
            let mut power: f32 = 0.0;

            // Position of the most-recent far-end sample for this near-end sample.
            // far_end_pos points to the *next write* position, so the most-recent
            // sample written is at far_end_pos - 1.  We have already called
            // feed_farend for this block, so the relevant samples are the last
            // filter_len entries ending just before the current write position,
            // offset by how far we are into this near-end frame.
            //
            // For sample i of the near-end frame, the corresponding far-end
            // "now" is far_end_pos - n + i  (wrapping).
            // far_end_pos points to next-write, so most recent sample is at
            // far_end_pos - 1.  For the i-th near-end sample we want the
            // far-end "now" to be at (far_end_pos - n + i).  We add fl
            // repeatedly to avoid underflow on the usize subtraction.
            let base = (self.far_end_pos + fl * ((n / fl) + 2) + i - n) % fl;

            for k in 0..fl {
                let fe_idx = (base + fl - k) % fl;
                let fe = self.far_end_buf[fe_idx];
                echo_est += self.filter_coeffs[k] * fe;
                power += fe * fe;
            }

            let error = near_f - echo_est;

            // --- NLMS coefficient update ---
            let norm = power + 1.0; // +1 regularisation to avoid div-by-zero
            let step = self.mu * error / norm;

            for k in 0..fl {
                let fe_idx = (base + fl - k) % fl;
                let fe = self.far_end_buf[fe_idx];
                self.filter_coeffs[k] += step * fe;
            }

            // Clamp output
            let out = error.max(-32768.0).min(32767.0);
            nearend[i] = out as i16;

            sum_near_sq += (near_f as f64) * (near_f as f64);
            sum_err_sq += (out as f64) * (out as f64);
        }

        // ERLE ratio
        if sum_err_sq < 1.0 {
            return 100.0; // near-perfect cancellation
        }
        (sum_near_sq / sum_err_sq).sqrt() as f32
    }

    /// Enable or disable echo cancellation.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Returns whether echo cancellation is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Reset the adaptive filter to its initial state.
    ///
    /// Zeroes out all filter coefficients and the far-end circular buffer.
    pub fn reset(&mut self) {
        self.filter_coeffs.iter_mut().for_each(|c| *c = 0.0);
        self.far_end_buf.iter_mut().for_each(|s| *s = 0.0);
        self.far_end_pos = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aec_creates_with_correct_filter_len() {
        let aec = EchoCanceller::new(48000, 100);
        assert_eq!(aec.filter_len, 4800);
        assert_eq!(aec.filter_coeffs.len(), 4800);
        assert_eq!(aec.far_end_buf.len(), 4800);
    }

    #[test]
    fn aec_passthrough_when_disabled() {
        let mut aec = EchoCanceller::new(48000, 100);
        aec.set_enabled(false);
        assert!(!aec.is_enabled());

        let original: Vec<i16> = (0..480).map(|i| (i * 10) as i16).collect();
        let mut frame = original.clone();
        let erle = aec.process_frame(&mut frame);
        assert_eq!(erle, 1.0);
        assert_eq!(frame, original);
    }

    #[test]
    fn aec_reset_zeroes_state() {
        let mut aec = EchoCanceller::new(48000, 10); // short for test speed
        let farend: Vec<i16> = (0..480).map(|i| ((i * 37) % 1000) as i16).collect();
        aec.feed_farend(&farend);

        aec.reset();

        assert!(aec.filter_coeffs.iter().all(|&c| c == 0.0));
        assert!(aec.far_end_buf.iter().all(|&s| s == 0.0));
        assert_eq!(aec.far_end_pos, 0);
    }

    #[test]
    fn aec_reduces_echo_of_known_signal() {
        // Use a small filter for speed.  Feed a known far-end signal, then
        // present the *same* signal as near-end (perfect echo, no room).
        // After adaptation the output energy should drop.
        let filter_ms = 5; // 240 taps at 48 kHz
        let mut aec = EchoCanceller::new(48000, filter_ms);

        // Generate a simple repeating pattern.
        let frame_len = 480usize;
        let make_frame = |offset: usize| -> Vec<i16> {
            (0..frame_len)
                .map(|i| {
                    let t = (offset + i) as f64 / 48000.0;
                    (5000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
                })
                .collect()
        };

        // Warm up the adaptive filter with several frames.
        let mut last_erle = 1.0f32;
        for frame_idx in 0..40 {
            let farend = make_frame(frame_idx * frame_len);
            aec.feed_farend(&farend);

            // Near-end = exact copy of far-end (pure echo).
            let mut nearend = farend.clone();
            last_erle = aec.process_frame(&mut nearend);
        }

        // After 40 frames the ERLE should be meaningfully > 1.
        assert!(
            last_erle > 1.0,
            "expected ERLE > 1.0 after adaptation, got {last_erle}"
        );
    }

    #[test]
    fn aec_silence_passthrough() {
        let mut aec = EchoCanceller::new(48000, 10);
        // Feed silence far-end
        aec.feed_farend(&vec![0i16; 480]);
        // Near-end is silence too
        let mut frame = vec![0i16; 480];
        let erle = aec.process_frame(&mut frame);
        assert!(erle >= 1.0);
        // Output should still be silence
        assert!(frame.iter().all(|&s| s == 0));
    }
}
