//! Acoustic Echo Cancellation — delay-compensated leaky NLMS with
//! Geigel double-talk detection.
//!
//! Key insight: on a laptop, the round-trip audio latency (playout → speaker
//! → air → mic → capture) is 30–50ms.  The far-end reference must be delayed
//! by this amount so the adaptive filter models the *echo path*, not the
//! *system delay + echo path*.
//!
//! The leaky coefficient decay prevents the filter from diverging when the
//! echo path changes (e.g. hand near laptop) or when the delay estimate
//! is slightly off.

/// Delay-compensated leaky NLMS echo canceller with Geigel DTD.
pub struct EchoCanceller {
    // --- Adaptive filter ---
    filter: Vec<f32>,
    filter_len: usize,
    /// Circular buffer of far-end reference samples (after delay).
    far_buf: Vec<f32>,
    far_pos: usize,
    /// NLMS step size.
    mu: f32,
    /// Leakage factor: coefficients are multiplied by (1 - leak) each frame.
    /// Prevents unbounded growth / divergence.  0.0001 is gentle.
    leak: f32,
    enabled: bool,

    // --- Delay buffer ---
    /// Raw far-end samples before delay compensation.
    delay_ring: Vec<f32>,
    delay_write: usize,
    delay_read: usize,
    /// Delay in samples (e.g. 1920 = 40ms at 48kHz).
    delay_samples: usize,
    /// Capacity of the delay ring.
    delay_cap: usize,

    // --- Double-talk detection (Geigel) ---
    /// Peak far-end level over the last filter_len samples.
    far_peak: f32,
    /// Geigel threshold: if |near| > threshold * far_peak, assume double-talk.
    geigel_threshold: f32,
    /// Holdover counter: keep DTD active for a few frames after detection.
    dtd_holdover: u32,
    dtd_hold_frames: u32,
}

impl EchoCanceller {
    /// Create a new echo canceller.
    ///
    /// * `sample_rate` — typically 48000
    /// * `filter_ms`   — echo-tail length in milliseconds (60ms recommended)
    /// * `delay_ms`    — far-end delay compensation in milliseconds (40ms for laptops)
    pub fn new(sample_rate: u32, filter_ms: u32) -> Self {
        Self::with_delay(sample_rate, filter_ms, 40)
    }

    pub fn with_delay(sample_rate: u32, filter_ms: u32, delay_ms: u32) -> Self {
        let filter_len = (sample_rate as usize) * (filter_ms as usize) / 1000;
        let delay_samples = (sample_rate as usize) * (delay_ms as usize) / 1000;
        // Delay ring must hold at least delay_samples + one frame (960) of headroom.
        let delay_cap = delay_samples + (sample_rate as usize / 10); // +100ms headroom
        Self {
            filter: vec![0.0; filter_len],
            filter_len,
            far_buf: vec![0.0; filter_len],
            far_pos: 0,
            mu: 0.01,
            leak: 0.0001,
            enabled: true,

            delay_ring: vec![0.0; delay_cap],
            delay_write: 0,
            delay_read: 0,
            delay_samples,
            delay_cap,

            far_peak: 0.0,
            geigel_threshold: 0.7,
            dtd_holdover: 0,
            dtd_hold_frames: 5,
        }
    }

    /// Feed far-end (speaker) samples.  These go into the delay buffer first;
    /// once enough samples have accumulated, they are released to the filter's
    /// circular buffer with the correct delay offset.
    pub fn feed_farend(&mut self, farend: &[i16]) {
        // Write raw samples into the delay ring.
        for &s in farend {
            self.delay_ring[self.delay_write % self.delay_cap] = s as f32;
            self.delay_write += 1;
        }

        // Release delayed samples to the filter's far-end buffer.
        while self.delay_available() >= 1 {
            let sample = self.delay_ring[self.delay_read % self.delay_cap];
            self.delay_read += 1;

            self.far_buf[self.far_pos] = sample;
            self.far_pos = (self.far_pos + 1) % self.filter_len;

            // Track peak far-end level for Geigel DTD.
            let abs_s = sample.abs();
            if abs_s > self.far_peak {
                self.far_peak = abs_s;
            }
        }

        // Decay far_peak slowly (avoids stale peak from a loud burst long ago).
        self.far_peak *= 0.9995;
    }

    /// Number of delayed samples available to release.
    fn delay_available(&self) -> usize {
        let buffered = self.delay_write - self.delay_read;
        if buffered > self.delay_samples {
            buffered - self.delay_samples
        } else {
            0
        }
    }

    /// Process a near-end (microphone) frame, removing the estimated echo.
    pub fn process_frame(&mut self, nearend: &mut [i16]) -> f32 {
        if !self.enabled {
            return 1.0;
        }

        let n = nearend.len();
        let fl = self.filter_len;

        // --- Geigel double-talk detection ---
        // If any near-end sample exceeds threshold * far_peak, assume
        // the local speaker is active and freeze adaptation.
        let mut is_doubletalk = self.dtd_holdover > 0;
        if !is_doubletalk {
            let threshold_level = self.geigel_threshold * self.far_peak;
            for &s in nearend.iter() {
                if (s as f32).abs() > threshold_level && self.far_peak > 100.0 {
                    is_doubletalk = true;
                    self.dtd_holdover = self.dtd_hold_frames;
                    break;
                }
            }
        }
        if self.dtd_holdover > 0 {
            self.dtd_holdover -= 1;
        }

        // Check if far-end is active (otherwise nothing to cancel).
        let far_active = self.far_peak > 100.0;

        // --- Leaky coefficient decay ---
        // Applied once per frame for efficiency.
        let decay = 1.0 - self.leak;
        for c in self.filter.iter_mut() {
            *c *= decay;
        }

        let mut sum_near_sq: f64 = 0.0;
        let mut sum_err_sq: f64 = 0.0;

        for i in 0..n {
            let near_f = nearend[i] as f32;

            // Position of far-end "now" for this near-end sample.
            let base = (self.far_pos + fl * ((n / fl) + 2) + i - n) % fl;

            // --- Echo estimation: dot(filter, far_end_window) ---
            let mut echo_est: f32 = 0.0;
            let mut power: f32 = 0.0;

            for k in 0..fl {
                let fe_idx = (base + fl - k) % fl;
                let fe = self.far_buf[fe_idx];
                echo_est += self.filter[k] * fe;
                power += fe * fe;
            }

            let error = near_f - echo_est;

            // --- NLMS adaptation (only when far-end active & no double-talk) ---
            if far_active && !is_doubletalk && power > 10.0 {
                let step = self.mu * error / (power + 1.0);
                for k in 0..fl {
                    let fe_idx = (base + fl - k) % fl;
                    self.filter[k] += step * self.far_buf[fe_idx];
                }
            }

            let out = error.clamp(-32768.0, 32767.0);
            nearend[i] = out as i16;

            sum_near_sq += (near_f as f64).powi(2);
            sum_err_sq += (out as f64).powi(2);
        }

        if sum_err_sq < 1.0 {
            100.0
        } else {
            (sum_near_sq / sum_err_sq).sqrt() as f32
        }
    }

    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    pub fn reset(&mut self) {
        self.filter.iter_mut().for_each(|c| *c = 0.0);
        self.far_buf.iter_mut().for_each(|s| *s = 0.0);
        self.far_pos = 0;
        self.far_peak = 0.0;
        self.delay_ring.iter_mut().for_each(|s| *s = 0.0);
        self.delay_write = 0;
        self.delay_read = 0;
        self.dtd_holdover = 0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn creates_with_correct_sizes() {
        let aec = EchoCanceller::with_delay(48000, 60, 40);
        assert_eq!(aec.filter_len, 2880); // 60ms @ 48kHz
        assert_eq!(aec.delay_samples, 1920); // 40ms @ 48kHz
    }

    #[test]
    fn passthrough_when_disabled() {
        let mut aec = EchoCanceller::new(48000, 60);
        aec.set_enabled(false);

        let original: Vec<i16> = (0..960).map(|i| (i * 10) as i16).collect();
        let mut frame = original.clone();
        aec.process_frame(&mut frame);
        assert_eq!(frame, original);
    }

    #[test]
    fn silence_passthrough() {
        let mut aec = EchoCanceller::with_delay(48000, 30, 0);
        aec.feed_farend(&vec![0i16; 960]);
        let mut frame = vec![0i16; 960];
        aec.process_frame(&mut frame);
        assert!(frame.iter().all(|&s| s == 0));
    }

    #[test]
    fn reduces_echo_with_no_delay() {
        // Simulate: far-end plays, echo arrives at mic attenuated by ~50%
        // (realistic — speaker to mic on laptop loses volume).
        let mut aec = EchoCanceller::with_delay(48000, 10, 0);

        let frame_len = 480;
        let make_tone = |offset: usize| -> Vec<i16> {
            (0..frame_len)
                .map(|i| {
                    let t = (offset + i) as f64 / 48000.0;
                    (5000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
                })
                .collect()
        };

        let mut last_erle = 1.0f32;
        for frame_idx in 0..100 {
            let farend = make_tone(frame_idx * frame_len);
            aec.feed_farend(&farend);

            // Near-end = attenuated copy of far-end (echo at ~50% volume).
            let mut nearend: Vec<i16> = farend.iter().map(|&s| s / 2).collect();
            last_erle = aec.process_frame(&mut nearend);
        }

        assert!(
            last_erle > 1.0,
            "expected ERLE > 1.0 after adaptation, got {last_erle}"
        );
    }

    #[test]
    fn preserves_nearend_during_doubletalk() {
        let mut aec = EchoCanceller::with_delay(48000, 30, 0);

        let frame_len = 960;
        let nearend: Vec<i16> = (0..frame_len)
            .map(|i| {
                let t = i as f64 / 48000.0;
                (10000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect();

        // Feed silence as far-end (no echo source).
        aec.feed_farend(&vec![0i16; frame_len]);

        let mut frame = nearend.clone();
        aec.process_frame(&mut frame);

        let input_energy: f64 = nearend.iter().map(|&s| (s as f64).powi(2)).sum();
        let output_energy: f64 = frame.iter().map(|&s| (s as f64).powi(2)).sum();
        let ratio = output_energy / input_energy;

        assert!(
            ratio > 0.8,
            "near-end speech should be preserved, energy ratio = {ratio:.3}"
        );
    }

    #[test]
    fn delay_buffer_holds_samples() {
        let mut aec = EchoCanceller::with_delay(48000, 10, 20);
        // 20ms delay = 960 samples @ 48kHz.
        // After feeding, feed_farend auto-drains available samples to far_buf.
        // So delay_available() is always 0 after feed_farend returns.
        // Instead, verify far_pos advances only after the delay is filled.

        // Feed 960 samples (= delay amount). No samples released yet.
        aec.feed_farend(&vec![1i16; 960]);
        // far_buf should still be all zeros (nothing released).
        assert!(aec.far_buf.iter().all(|&s| s == 0.0), "nothing should be released yet");

        // Feed 480 more. 480 should be released to far_buf.
        aec.feed_farend(&vec![2i16; 480]);
        let non_zero = aec.far_buf.iter().filter(|&&s| s != 0.0).count();
        assert!(non_zero > 0, "samples should have been released to far_buf");
    }
}
