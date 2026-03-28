//! ML-based noise suppression using nnnoiseless (pure-Rust RNNoise port).
//!
//! RNNoise operates on 480-sample frames at 48 kHz (10 ms). Our codec pipeline
//! uses 960-sample frames (20 ms), so each call processes two halves.

use nnnoiseless::DenoiseState;

/// Wraps [`DenoiseState`] to provide noise suppression on 960-sample (20 ms) PCM
/// frames at 48 kHz.
pub struct NoiseSupressor {
    state: Box<DenoiseState<'static>>,
    enabled: bool,
}

impl NoiseSupressor {
    /// Create a new noise suppressor (enabled by default).
    pub fn new() -> Self {
        Self {
            state: DenoiseState::new(),
            enabled: true,
        }
    }

    /// Process a 960-sample frame of 48 kHz mono PCM **in place**.
    ///
    /// nnnoiseless expects f32 samples in the range roughly [-32768, 32767].
    /// We convert i16 → f32, process two 480-sample halves, then convert back.
    pub fn process(&mut self, pcm: &mut [i16]) {
        if !self.enabled {
            return;
        }

        debug_assert!(
            pcm.len() >= 960,
            "NoiseSupressor::process expects at least 960 samples, got {}",
            pcm.len()
        );

        // Process in two 480-sample halves.
        for half in 0..2 {
            let offset = half * 480;
            let end = offset + 480;
            if end > pcm.len() {
                break;
            }

            // i16 → f32
            let mut float_buf = [0.0f32; 480];
            for (i, &sample) in pcm[offset..end].iter().enumerate() {
                float_buf[i] = sample as f32;
            }

            // nnnoiseless processes in-place, returns VAD probability (unused here).
            let mut output = [0.0f32; 480];
            let _vad = self.state.process_frame(&mut output, &float_buf);

            // f32 → i16 with clamping
            for (i, &val) in output.iter().enumerate() {
                let clamped = val.max(-32768.0).min(32767.0);
                pcm[offset + i] = clamped as i16;
            }
        }
    }

    /// Enable or disable noise suppression.
    pub fn set_enabled(&mut self, enabled: bool) {
        self.enabled = enabled;
    }

    /// Returns `true` if noise suppression is currently enabled.
    pub fn is_enabled(&self) -> bool {
        self.enabled
    }
}

impl Default for NoiseSupressor {
    fn default() -> Self {
        Self::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn denoiser_creates() {
        let ns = NoiseSupressor::new();
        assert!(ns.is_enabled());
    }

    #[test]
    fn denoiser_processes_frame() {
        let mut ns = NoiseSupressor::new();
        let mut pcm = vec![0i16; 960];
        // Fill with a simple pattern so we have something to process.
        for (i, s) in pcm.iter_mut().enumerate() {
            *s = ((i % 100) as i16).wrapping_mul(100);
        }
        let original_len = pcm.len();
        ns.process(&mut pcm);
        assert_eq!(pcm.len(), original_len, "output length must match input length");
    }

    #[test]
    fn denoiser_reduces_noise() {
        let mut ns = NoiseSupressor::new();

        // Generate a 440 Hz sine tone + white noise at 48 kHz.
        // We need multiple frames for the RNN to converge.
        let sample_rate = 48000.0f64;
        let freq = 440.0f64;
        let amplitude = 10000.0f64;
        let noise_amplitude = 3000.0f64;

        // Use a simple PRNG for reproducibility.
        let mut rng_state: u32 = 12345;
        let mut next_noise = || -> f64 {
            // xorshift32
            rng_state ^= rng_state << 13;
            rng_state ^= rng_state >> 17;
            rng_state ^= rng_state << 5;
            // Map to [-1, 1]
            (rng_state as f64 / u32::MAX as f64) * 2.0 - 1.0
        };

        // Feed several frames to let the RNN warm up, then measure the last one.
        let num_warmup_frames = 20;
        let mut last_input = vec![0i16; 960];
        let mut last_output = vec![0i16; 960];

        for frame_idx in 0..=num_warmup_frames {
            let mut pcm = vec![0i16; 960];
            for (i, s) in pcm.iter_mut().enumerate() {
                let t = (frame_idx * 960 + i) as f64 / sample_rate;
                let sine = amplitude * (2.0 * std::f64::consts::PI * freq * t).sin();
                let noise = noise_amplitude * next_noise();
                *s = (sine + noise).max(-32768.0).min(32767.0) as i16;
            }

            if frame_idx == num_warmup_frames {
                last_input = pcm.clone();
            }

            ns.process(&mut pcm);

            if frame_idx == num_warmup_frames {
                last_output = pcm;
            }
        }

        // Compute RMS of input and output.
        let rms = |buf: &[i16]| -> f64 {
            let sum: f64 = buf.iter().map(|&s| (s as f64) * (s as f64)).sum();
            (sum / buf.len() as f64).sqrt()
        };

        let input_rms = rms(&last_input);
        let output_rms = rms(&last_output);

        // The denoiser should not amplify the signal beyond input.
        // More importantly, the output should have measurably lower noise.
        // We verify the output RMS is less than the input RMS (noise was reduced).
        assert!(
            output_rms < input_rms,
            "expected output RMS ({output_rms:.1}) < input RMS ({input_rms:.1}); \
             denoiser should reduce noise"
        );
    }

    #[test]
    fn denoiser_passthrough_when_disabled() {
        let mut ns = NoiseSupressor::new();
        ns.set_enabled(false);
        assert!(!ns.is_enabled());

        let original: Vec<i16> = (0..960).map(|i| (i * 10) as i16).collect();
        let mut pcm = original.clone();
        ns.process(&mut pcm);

        assert_eq!(pcm, original, "disabled denoiser must not alter input");
    }
}
