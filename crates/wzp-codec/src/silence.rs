//! Silence suppression and comfort noise generation.
//!
//! During silent periods (~50% of a typical call), full encoded frames waste
//! bandwidth. [`SilenceDetector`] detects silent audio based on RMS energy,
//! and [`ComfortNoise`] generates low-level background noise to fill gaps on
//! the decoder side.

use rand::Rng;

/// Detects silence in PCM audio using RMS energy with a hangover period.
///
/// The hangover prevents clipping the onset of speech: after silence is first
/// detected, the detector continues reporting "not silent" for `hangover_frames`
/// additional frames before transitioning to suppression.
pub struct SilenceDetector {
    /// RMS threshold below which audio is considered silent (for i16 samples).
    threshold_rms: f64,
    /// Number of frames to keep sending after silence starts (prevents speech clipping).
    hangover_frames: u32,
    /// Count of consecutive frames whose RMS is below the threshold.
    silent_frames: u32,
    /// Whether suppression is currently active.
    is_suppressing: bool,
}

impl SilenceDetector {
    /// Create a new silence detector.
    ///
    /// * `threshold_rms` — RMS energy below which a frame is silent (default: 100.0 for i16).
    /// * `hangover_frames` — frames to keep sending after silence onset (default: 5 = 100ms at 20ms frames).
    pub fn new(threshold_rms: f64, hangover_frames: u32) -> Self {
        Self {
            threshold_rms,
            hangover_frames,
            silent_frames: 0,
            is_suppressing: false,
        }
    }

    /// Compute the RMS (root mean square) energy of a PCM buffer.
    pub fn rms(pcm: &[i16]) -> f64 {
        if pcm.is_empty() {
            return 0.0;
        }
        let sum_sq: f64 = pcm.iter().map(|&s| (s as f64) * (s as f64)).sum();
        (sum_sq / pcm.len() as f64).sqrt()
    }

    /// Returns `true` if the frame should be suppressed (i.e. is silence past
    /// the hangover period).
    ///
    /// Call once per frame. The detector tracks consecutive silent frames
    /// internally and only reports suppression after the hangover expires.
    pub fn is_silent(&mut self, pcm: &[i16]) -> bool {
        let energy = Self::rms(pcm);

        if energy < self.threshold_rms {
            self.silent_frames = self.silent_frames.saturating_add(1);

            if self.silent_frames > self.hangover_frames {
                self.is_suppressing = true;
            }
        } else {
            // Speech detected — reset.
            self.silent_frames = 0;
            self.is_suppressing = false;
        }

        self.is_suppressing
    }

    /// Whether the detector is currently in the suppressing state.
    pub fn suppressing(&self) -> bool {
        self.is_suppressing
    }
}

/// Generates low-level comfort noise to fill silent periods.
///
/// When the decoder receives a comfort-noise descriptor (or detects a gap
/// caused by silence suppression), it uses this to produce a natural-sounding
/// background hiss instead of dead silence.
pub struct ComfortNoise {
    /// Peak amplitude of the generated noise (default: 50).
    level: i16,
}

impl ComfortNoise {
    /// Create a comfort noise generator with the given amplitude level.
    pub fn new(level: i16) -> Self {
        Self { level }
    }

    /// Fill `pcm` with low-level random noise in the range `[-level, level]`.
    pub fn generate(&self, pcm: &mut [i16]) {
        let mut rng = rand::thread_rng();
        for sample in pcm.iter_mut() {
            *sample = rng.gen_range(-self.level..=self.level);
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn silence_detector_detects_silence() {
        let mut det = SilenceDetector::new(100.0, 5);
        let silence = vec![0i16; 960];

        // First 5 frames are hangover — should NOT suppress yet.
        for _ in 0..5 {
            assert!(!det.is_silent(&silence));
        }
        // Frame 6 onward: past hangover, should suppress.
        assert!(det.is_silent(&silence));
        assert!(det.is_silent(&silence));
    }

    #[test]
    fn silence_detector_detects_speech() {
        let mut det = SilenceDetector::new(100.0, 5);

        // Generate a 1kHz sine wave at decent amplitude.
        let pcm: Vec<i16> = (0..960)
            .map(|i| {
                let t = i as f64 / 48000.0;
                (10000.0 * (2.0 * std::f64::consts::PI * 1000.0 * t).sin()) as i16
            })
            .collect();

        // Should never report silent.
        for _ in 0..20 {
            assert!(!det.is_silent(&pcm));
        }
    }

    #[test]
    fn silence_detector_hangover() {
        let mut det = SilenceDetector::new(100.0, 3);
        let silence = vec![0i16; 960];
        let speech: Vec<i16> = (0..960)
            .map(|i| {
                let t = i as f64 / 48000.0;
                (5000.0 * (2.0 * std::f64::consts::PI * 440.0 * t).sin()) as i16
            })
            .collect();

        // Feed silence past hangover to enter suppression.
        for _ in 0..4 {
            det.is_silent(&silence);
        }
        assert!(det.is_silent(&silence), "should be suppressing after hangover");

        // Speech arrives — should immediately stop suppressing.
        assert!(!det.is_silent(&speech));
        assert!(!det.is_silent(&speech));
    }

    #[test]
    fn comfort_noise_generates_nonzero() {
        let cn = ComfortNoise::new(50);
        let mut pcm = vec![0i16; 960];
        cn.generate(&mut pcm);

        // At least some samples should be non-zero.
        assert!(pcm.iter().any(|&s| s != 0), "CN output should not be all zeros");

        // All samples should be within [-50, 50].
        assert!(pcm.iter().all(|&s| s.abs() <= 50), "CN samples out of range");
    }

    #[test]
    fn rms_calculation() {
        // All zeros → RMS 0.
        assert_eq!(SilenceDetector::rms(&[0i16; 100]), 0.0);

        // Constant value: RMS of [v, v, v, ...] = |v|.
        let pcm = vec![100i16; 100];
        let rms = SilenceDetector::rms(&pcm);
        assert!((rms - 100.0).abs() < 0.01, "RMS of constant 100 should be 100, got {rms}");

        // Known pattern: [3, 4] → sqrt((9+16)/2) = sqrt(12.5) ≈ 3.5355
        let rms2 = SilenceDetector::rms(&[3, 4]);
        assert!((rms2 - 3.5355).abs() < 0.01, "RMS of [3,4] should be ~3.5355, got {rms2}");

        // Empty buffer → 0.
        assert_eq!(SilenceDetector::rms(&[]), 0.0);
    }
}
