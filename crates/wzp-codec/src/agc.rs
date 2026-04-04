//! Automatic Gain Control (AGC) with two-stage smoothing.
//!
//! Uses a fast attack / slow release envelope follower to keep the
//! output signal near a configurable target RMS level.  This prevents
//! both clipping (when the speaker is too loud) and inaudibility (when
//! the speaker is too quiet or far from the mic).

/// Two-stage automatic gain control.
///
/// The gain is adjusted per-frame based on the measured RMS energy,
/// with a fast attack (gain decreases quickly when signal gets louder)
/// and a slow release (gain increases gradually when signal gets quieter).
pub struct AutoGainControl {
    target_rms: f64,
    current_gain: f64,
    min_gain: f64,
    max_gain: f64,
    attack_alpha: f64,
    release_alpha: f64,
    enabled: bool,
}

impl AutoGainControl {
    /// Create a new AGC with sensible VoIP defaults.
    pub fn new() -> Self {
        Self {
            target_rms: 3000.0,   // ~-20 dBFS for i16
            current_gain: 1.0,
            min_gain: 0.5,
            max_gain: 32.0,
            attack_alpha: 0.3,    // fast attack
            release_alpha: 0.02,  // slow release
            enabled: true,
        }
    }

    /// Process a frame of PCM audio in-place, applying gain adjustment.
    pub fn process_frame(&mut self, pcm: &mut [i16]) {
        if !self.enabled {
            return;
        }

        // Compute RMS of the frame.
        let rms = Self::compute_rms(pcm);

        // Don't amplify near-silence — it would just boost noise.
        if rms < 10.0 {
            return;
        }

        // Desired instantaneous gain.
        let desired_gain = (self.target_rms / rms).clamp(self.min_gain, self.max_gain);

        // Smooth the gain transition.
        let alpha = if desired_gain < self.current_gain {
            // Signal is louder than target → reduce gain quickly (attack).
            self.attack_alpha
        } else {
            // Signal is quieter than target → raise gain slowly (release).
            self.release_alpha
        };

        self.current_gain = self.current_gain * (1.0 - alpha) + desired_gain * alpha;

        // Apply gain to each sample with hard limiting at ±31000 (~0.946 * i16::MAX).
        const LIMIT: f64 = 31000.0;
        let gain = self.current_gain;
        for sample in pcm.iter_mut() {
            let amplified = (*sample as f64) * gain;
            let clamped = amplified.clamp(-LIMIT, LIMIT);
            *sample = clamped as i16;
        }
    }

    /// Enable or disable the AGC.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Returns whether the AGC is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }

    /// Current gain expressed in dB.
    pub fn current_gain_db(&self) -> f64 {
        20.0 * self.current_gain.log10()
    }

    /// Compute the RMS (root mean square) of a PCM buffer.
    fn compute_rms(pcm: &[i16]) -> f64 {
        if pcm.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum_sq / pcm.len() as f64).sqrt()
    }
}

impl Default for AutoGainControl {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn agc_creates_with_defaults() {
        let agc = AutoGainControl::new();
        assert!(agc.is_enabled());
        assert!((agc.current_gain - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn agc_passthrough_when_disabled() {
        let mut agc = AutoGainControl::new();
        agc.set_enabled(false);

        let original: Vec<i16> = (0..960).map(|i| (i * 5) as i16).collect();
        let mut frame = original.clone();
        agc.process_frame(&mut frame);

        assert_eq!(frame, original);
    }

    #[test]
    fn agc_does_not_amplify_silence() {
        let mut agc = AutoGainControl::new();
        let mut frame = vec![0i16; 960];
        agc.process_frame(&mut frame);
        assert!(frame.iter().all(|&s| s == 0));
        // Gain should remain at initial value.
        assert!((agc.current_gain - 1.0).abs() < f64::EPSILON);
    }

    #[test]
    fn agc_amplifies_quiet_signal() {
        let mut agc = AutoGainControl::new();

        // Very quiet signal (RMS ~ 50).
        let mut frame: Vec<i16> = (0..960)
            .map(|i| {
                let t = i as f64 / 48000.0;
                (50.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect();

        // Process several frames to let the gain ramp up.
        for _ in 0..50 {
            let mut f = frame.clone();
            agc.process_frame(&mut f);
            frame = f;
        }

        // Gain should have increased past 1.0.
        assert!(
            agc.current_gain > 1.05,
            "expected gain > 1.05 for quiet signal, got {}",
            agc.current_gain
        );
    }

    #[test]
    fn agc_attenuates_loud_signal() {
        let mut agc = AutoGainControl::new();

        // Loud signal (RMS ~ 20000).
        let frame: Vec<i16> = (0..960)
            .map(|i| {
                let t = i as f64 / 48000.0;
                (28000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect();

        // Process several frames.
        for _ in 0..20 {
            let mut f = frame.clone();
            agc.process_frame(&mut f);
        }

        // Gain should have decreased below 1.0.
        assert!(
            agc.current_gain < 1.0,
            "expected gain < 1.0 for loud signal, got {}",
            agc.current_gain
        );
    }

    #[test]
    fn agc_output_within_limits() {
        let mut agc = AutoGainControl::new();
        // Force a high gain by processing many quiet frames first.
        for _ in 0..100 {
            let mut f: Vec<i16> = vec![100; 960];
            agc.process_frame(&mut f);
        }

        // Now send a louder frame — output should still be within ±31000.
        let mut frame: Vec<i16> = vec![20000; 960];
        agc.process_frame(&mut frame);
        assert!(
            frame.iter().all(|&s| s.abs() <= 31000),
            "output samples must be within ±31000"
        );
    }

    #[test]
    fn agc_gain_db_at_unity() {
        let agc = AutoGainControl::new();
        let db = agc.current_gain_db();
        assert!(
            db.abs() < 0.01,
            "expected ~0 dB at unity gain, got {db}"
        );
    }
}
