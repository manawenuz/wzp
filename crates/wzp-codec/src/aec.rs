//! Acoustic Echo Cancellation using NLMS adaptive filter.
//!
//! Improvements over naive NLMS:
//! - Double-talk detection: freezes adaptation when near-end speech dominates,
//!   preventing the filter from cancelling the local speaker's voice.
//! - Short default tail (30ms) tuned for laptops/phones where speaker and mic
//!   are close together.
//! - Residual suppression: attenuates output when echo estimate is confident.

/// NLMS (Normalized Least Mean Squares) adaptive filter echo canceller
/// with double-talk detection.
pub struct EchoCanceller {
    filter_coeffs: Vec<f32>,
    filter_len: usize,
    far_end_buf: Vec<f32>,
    far_end_pos: usize,
    /// NLMS step size (adaptation rate).
    mu: f32,
    enabled: bool,
    /// Running far-end power estimate (for double-talk detection).
    far_power_avg: f32,
    /// Running near-end power estimate (for double-talk detection).
    near_power_avg: f32,
    /// Smoothing factor for power estimates.
    power_alpha: f32,
    /// Double-talk threshold: if near/far power ratio exceeds this,
    /// freeze adaptation to protect near-end speech.
    dt_threshold: f32,
    /// Residual echo suppression factor (0.0 = none, 1.0 = full).
    suppress: f32,
}

impl EchoCanceller {
    /// Create a new echo canceller.
    ///
    /// * `sample_rate` — typically 48000
    /// * `filter_ms`   — echo-tail length in milliseconds (30ms recommended for laptops)
    pub fn new(sample_rate: u32, filter_ms: u32) -> Self {
        let filter_len = (sample_rate as usize) * (filter_ms as usize) / 1000;
        Self {
            filter_coeffs: vec![0.0f32; filter_len],
            filter_len,
            far_end_buf: vec![0.0f32; filter_len],
            far_end_pos: 0,
            mu: 0.005,
            enabled: true,
            far_power_avg: 0.0,
            near_power_avg: 0.0,
            power_alpha: 0.01,
            dt_threshold: 4.0,
            suppress: 0.6,
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
    /// Returns the echo-return-loss enhancement (ERLE) as a ratio.
    pub fn process_frame(&mut self, nearend: &mut [i16]) -> f32 {
        if !self.enabled {
            return 1.0;
        }

        let n = nearend.len();
        let fl = self.filter_len;

        // Compute frame-level power for double-talk detection.
        let near_power: f32 = nearend.iter().map(|&s| {
            let f = s as f32;
            f * f
        }).sum::<f32>() / n as f32;

        let far_start = (self.far_end_pos + fl * ((n / fl) + 1) - n) % fl;
        let far_power: f32 = (0..n).map(|i| {
            let fe = self.far_end_buf[(far_start + i) % fl];
            fe * fe
        }).sum::<f32>() / n as f32;

        // Smooth power estimates
        self.far_power_avg += self.power_alpha * (far_power - self.far_power_avg);
        self.near_power_avg += self.power_alpha * (near_power - self.near_power_avg);

        // Double-talk detection: if near-end is much louder than far-end,
        // the local speaker is active — freeze adaptation.
        let adapt = if self.far_power_avg < 1.0 {
            // No far-end signal — nothing to cancel, skip adaptation
            false
        } else {
            let ratio = self.near_power_avg / (self.far_power_avg + 1.0);
            ratio < self.dt_threshold
        };

        let mut sum_near_sq: f64 = 0.0;
        let mut sum_err_sq: f64 = 0.0;

        for i in 0..n {
            let near_f = nearend[i] as f32;

            // Estimate echo: dot(coeffs, farend_window)
            let base = (self.far_end_pos + fl * ((n / fl) + 2) + i - n) % fl;

            let mut echo_est: f32 = 0.0;
            let mut power: f32 = 0.0;

            for k in 0..fl {
                let fe_idx = (base + fl - k) % fl;
                let fe = self.far_end_buf[fe_idx];
                echo_est += self.filter_coeffs[k] * fe;
                power += fe * fe;
            }

            let error = near_f - echo_est;

            // NLMS coefficient update — only when not in double-talk
            if adapt && power > 1.0 {
                let norm = power + 1.0;
                let step = self.mu * error / norm;

                for k in 0..fl {
                    let fe_idx = (base + fl - k) % fl;
                    let fe = self.far_end_buf[fe_idx];
                    self.filter_coeffs[k] += step * fe;
                }
            }

            // Residual echo suppression: when far-end is active, attenuate
            // the residual to reduce perceptible echo.
            let out = if self.far_power_avg > 100.0 && !adapt {
                // Double-talk: pass through near-end with minimal suppression
                error
            } else if self.far_power_avg > 100.0 {
                // Far-end active, not double-talk: apply suppression
                error * (1.0 - self.suppress * (echo_est.abs() / (near_f.abs() + 1.0)).min(1.0))
            } else {
                // No far-end: pass through
                error
            };

            let out = out.max(-32768.0).min(32767.0);
            nearend[i] = out as i16;

            sum_near_sq += (near_f as f64) * (near_f as f64);
            sum_err_sq += (out as f64) * (out as f64);
        }

        if sum_err_sq < 1.0 {
            return 100.0;
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
    pub fn reset(&mut self) {
        self.filter_coeffs.iter_mut().for_each(|c| *c = 0.0);
        self.far_end_buf.iter_mut().for_each(|s| *s = 0.0);
        self.far_end_pos = 0;
        self.far_power_avg = 0.0;
        self.near_power_avg = 0.0;
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn aec_creates_with_correct_filter_len() {
        let aec = EchoCanceller::new(48000, 30);
        assert_eq!(aec.filter_len, 1440);
        assert_eq!(aec.filter_coeffs.len(), 1440);
        assert_eq!(aec.far_end_buf.len(), 1440);
    }

    #[test]
    fn aec_passthrough_when_disabled() {
        let mut aec = EchoCanceller::new(48000, 30);
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
        let mut aec = EchoCanceller::new(48000, 10);
        let farend: Vec<i16> = (0..480).map(|i| ((i * 37) % 1000) as i16).collect();
        aec.feed_farend(&farend);

        aec.reset();

        assert!(aec.filter_coeffs.iter().all(|&c| c == 0.0));
        assert!(aec.far_end_buf.iter().all(|&s| s == 0.0));
        assert_eq!(aec.far_end_pos, 0);
    }

    #[test]
    fn aec_reduces_echo_of_known_signal() {
        let filter_ms = 5;
        let mut aec = EchoCanceller::new(48000, filter_ms);

        let frame_len = 480usize;
        let make_frame = |offset: usize| -> Vec<i16> {
            (0..frame_len)
                .map(|i| {
                    let t = (offset + i) as f64 / 48000.0;
                    (5000.0 * (2.0 * std::f64::consts::PI * 300.0 * t).sin()) as i16
                })
                .collect()
        };

        let mut last_erle = 1.0f32;
        for frame_idx in 0..40 {
            let farend = make_frame(frame_idx * frame_len);
            aec.feed_farend(&farend);

            let mut nearend = farend.clone();
            last_erle = aec.process_frame(&mut nearend);
        }

        assert!(
            last_erle > 1.0,
            "expected ERLE > 1.0 after adaptation, got {last_erle}"
        );
    }

    #[test]
    fn aec_silence_passthrough() {
        let mut aec = EchoCanceller::new(48000, 10);
        aec.feed_farend(&vec![0i16; 480]);
        let mut frame = vec![0i16; 480];
        let erle = aec.process_frame(&mut frame);
        assert!(erle >= 1.0);
        assert!(frame.iter().all(|&s| s == 0));
    }

    #[test]
    fn aec_preserves_nearend_during_doubletalk() {
        // When only near-end is active (no far-end), output should
        // closely match input — the AEC should not suppress speech.
        let mut aec = EchoCanceller::new(48000, 30);

        let frame_len = 960;
        let nearend_signal: Vec<i16> = (0..frame_len)
            .map(|i| {
                let t = i as f64 / 48000.0;
                (10000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect();

        // Feed silence as far-end
        aec.feed_farend(&vec![0i16; frame_len]);

        let mut frame = nearend_signal.clone();
        aec.process_frame(&mut frame);

        // Output energy should be close to input energy (not suppressed)
        let input_energy: f64 = nearend_signal.iter().map(|&s| (s as f64).powi(2)).sum();
        let output_energy: f64 = frame.iter().map(|&s| (s as f64).powi(2)).sum();
        let ratio = output_energy / input_energy;

        assert!(
            ratio > 0.8,
            "near-end speech should be preserved, energy ratio = {ratio:.3}"
        );
    }
}
