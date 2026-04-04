//! Windowed-sinc FIR resampler for 48 kHz <-> 8 kHz conversion.
//!
//! Provides both stateless free functions (backward-compatible) and stateful
//! `Downsampler48to8` / `Upsampler8to48` structs that maintain overlap history
//! between frames for glitch-free streaming.

use std::f64::consts::PI;

// ─── FIR kernel parameters ─────────────────────────────────────────────────

/// Number of FIR taps in the anti-alias / interpolation filter.
const FIR_TAPS: usize = 48;
/// Kaiser window beta parameter — controls sidelobe attenuation.
const KAISER_BETA: f64 = 8.0;
/// Cutoff frequency in Hz for the low-pass filter (just below 4 kHz Nyquist of 8 kHz).
const CUTOFF_HZ: f64 = 3800.0;
/// Working sample rate in Hz.
const SAMPLE_RATE: f64 = 48000.0;
/// Decimation / interpolation ratio between 48 kHz and 8 kHz.
const RATIO: usize = 6;

// ─── Kaiser window helpers ─────────────────────────────────────────────────

/// Zeroth-order modified Bessel function of the first kind, I₀(x).
///
/// Computed via the well-known power-series expansion, converging rapidly
/// for the moderate values of x used in Kaiser window design.
fn bessel_i0(x: f64) -> f64 {
    let mut sum = 1.0f64;
    let mut term = 1.0f64;
    let half_x = x / 2.0;
    for k in 1..=25 {
        term *= (half_x / k as f64) * (half_x / k as f64);
        sum += term;
        if term < 1e-12 * sum {
            break;
        }
    }
    sum
}

/// Build a windowed-sinc low-pass FIR kernel.
///
/// Returns `FIR_TAPS` coefficients normalised so that the DC gain is exactly 1.0.
fn build_fir_kernel() -> [f64; FIR_TAPS] {
    let mut kernel = [0.0f64; FIR_TAPS];
    let m = (FIR_TAPS - 1) as f64;
    let fc = CUTOFF_HZ / SAMPLE_RATE; // normalised cutoff (0..0.5)
    let beta_denom = bessel_i0(KAISER_BETA);

    for i in 0..FIR_TAPS {
        // Sinc
        let n = i as f64 - m / 2.0;
        let sinc = if n.abs() < 1e-12 {
            2.0 * fc
        } else {
            (2.0 * PI * fc * n).sin() / (PI * n)
        };

        // Kaiser window
        let t = 2.0 * i as f64 / m - 1.0; // range [-1, 1]
        let kaiser = bessel_i0(KAISER_BETA * (1.0 - t * t).max(0.0).sqrt()) / beta_denom;

        kernel[i] = sinc * kaiser;
    }

    // Normalise to unity DC gain.
    let sum: f64 = kernel.iter().sum();
    if sum.abs() > 1e-15 {
        for k in kernel.iter_mut() {
            *k /= sum;
        }
    }

    kernel
}

// ─── Stateful Downsampler 48→8 ─────────────────────────────────────────────

/// Stateful polyphase FIR downsampler from 48 kHz to 8 kHz.
///
/// Maintains `FIR_TAPS - 1` samples of history between successive calls to
/// `process()` for seamless frame boundaries.
pub struct Downsampler48to8 {
    kernel: [f64; FIR_TAPS],
    history: Vec<f64>,
}

impl Downsampler48to8 {
    pub fn new() -> Self {
        Self {
            kernel: build_fir_kernel(),
            history: vec![0.0; FIR_TAPS - 1],
        }
    }

    /// Downsample a block of 48 kHz samples to 8 kHz.
    ///
    /// The input length should be a multiple of 6; any trailing samples that
    /// don't form a complete output sample are consumed into the history.
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let hist_len = self.history.len(); // FIR_TAPS - 1
        let total_len = hist_len + input.len();

        // Build a working buffer: history ++ input (as f64).
        let mut work = Vec::with_capacity(total_len);
        work.extend_from_slice(&self.history);
        work.extend(input.iter().map(|&s| s as f64));

        let out_len = input.len() / RATIO;
        let mut output = Vec::with_capacity(out_len);

        for i in 0..out_len {
            // The centre of the filter for output sample i sits at
            // position hist_len + i*RATIO in the work buffer (aligning
            // with the first new input sample at decimation phase 0).
            let centre = hist_len + i * RATIO;
            let start = centre + 1 - FIR_TAPS; // may be 0 for the first few

            let mut acc = 0.0f64;
            for k in 0..FIR_TAPS {
                let idx = start + k;
                if idx < work.len() {
                    acc += work[idx] * self.kernel[k];
                }
            }
            output.push(acc.round().clamp(-32768.0, 32767.0) as i16);
        }

        // Update history: keep the last (FIR_TAPS - 1) samples from work.
        if work.len() >= hist_len {
            self.history
                .copy_from_slice(&work[work.len() - hist_len..]);
        } else {
            // Input was shorter than history — shift.
            let shift = hist_len - work.len();
            self.history.copy_within(shift.., 0);
            for (i, &v) in work.iter().enumerate() {
                self.history[hist_len - work.len() + i] = v;
            }
        }

        output
    }
}

impl Default for Downsampler48to8 {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Stateful Upsampler 8→48 ───────────────────────────────────────────────

/// Stateful FIR upsampler from 8 kHz to 48 kHz.
///
/// Inserts zeros between input samples (zero-stuffing), then applies the
/// low-pass FIR to remove imaging, with gain compensation of `RATIO`.
pub struct Upsampler8to48 {
    kernel: [f64; FIR_TAPS],
    history: Vec<f64>,
}

impl Upsampler8to48 {
    pub fn new() -> Self {
        Self {
            kernel: build_fir_kernel(),
            history: vec![0.0; FIR_TAPS - 1],
        }
    }

    /// Upsample a block of 8 kHz samples to 48 kHz.
    pub fn process(&mut self, input: &[i16]) -> Vec<i16> {
        let hist_len = self.history.len(); // FIR_TAPS - 1

        // Zero-stuff: insert RATIO-1 zeros between each input sample.
        let stuffed_len = input.len() * RATIO;
        let total_len = hist_len + stuffed_len;

        let mut work = Vec::with_capacity(total_len);
        work.extend_from_slice(&self.history);
        for &s in input {
            work.push(s as f64);
            for _ in 1..RATIO {
                work.push(0.0);
            }
        }

        let out_len = stuffed_len;
        let mut output = Vec::with_capacity(out_len);

        // The gain factor compensates for the zeros introduced by stuffing.
        let gain = RATIO as f64;

        for i in 0..out_len {
            let centre = hist_len + i;
            let start = centre + 1 - FIR_TAPS;

            let mut acc = 0.0f64;
            for k in 0..FIR_TAPS {
                let idx = start + k;
                if idx < work.len() {
                    acc += work[idx] * self.kernel[k];
                }
            }
            acc *= gain;
            output.push(acc.round().clamp(-32768.0, 32767.0) as i16);
        }

        // Update history.
        if work.len() >= hist_len {
            self.history
                .copy_from_slice(&work[work.len() - hist_len..]);
        } else {
            let shift = hist_len - work.len();
            self.history.copy_within(shift.., 0);
            for (i, &v) in work.iter().enumerate() {
                self.history[hist_len - work.len() + i] = v;
            }
        }

        output
    }
}

impl Default for Upsampler8to48 {
    fn default() -> Self {
        Self::new()
    }
}

// ─── Backward-compatible free functions ─────────────────────────────────────

/// Downsample from 48 kHz to 8 kHz (6:1 decimation with FIR anti-alias filter).
///
/// This is a convenience wrapper that creates a temporary [`Downsampler48to8`].
/// For streaming use, prefer the stateful struct to avoid edge artefacts between
/// frames.
pub fn resample_48k_to_8k(input: &[i16]) -> Vec<i16> {
    let mut ds = Downsampler48to8::new();
    ds.process(input)
}

/// Upsample from 8 kHz to 48 kHz (1:6 interpolation with FIR imaging filter).
///
/// This is a convenience wrapper that creates a temporary [`Upsampler8to48`].
/// For streaming use, prefer the stateful struct to avoid edge artefacts between
/// frames.
pub fn resample_8k_to_48k(input: &[i16]) -> Vec<i16> {
    let mut us = Upsampler8to48::new();
    us.process(input)
}

// ─── Tests ──────────────────────────────────────────────────────────────────

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn roundtrip_length() {
        // 960 samples at 48kHz (20ms) -> 160 samples at 8kHz -> 960 samples at 48kHz
        let input_48k = vec![0i16; 960];
        let down = resample_48k_to_8k(&input_48k);
        assert_eq!(down.len(), 160);
        let up = resample_8k_to_48k(&down);
        assert_eq!(up.len(), 960);
    }

    #[test]
    fn dc_signal_preserved() {
        // A constant signal should survive resampling (approximately).
        let input = vec![1000i16; 960];
        let down = resample_48k_to_8k(&input);
        // Allow some edge transient — check that the middle samples are close.
        let mid_start = down.len() / 4;
        let mid_end = 3 * down.len() / 4;
        for &s in &down[mid_start..mid_end] {
            assert!(
                (s - 1000).abs() < 50,
                "DC downsampled sample {s} too far from 1000"
            );
        }

        let up = resample_8k_to_48k(&down);
        let mid_start_up = up.len() / 4;
        let mid_end_up = 3 * up.len() / 4;
        for &s in &up[mid_start_up..mid_end_up] {
            assert!(
                (s - 1000).abs() < 100,
                "DC upsampled sample {s} too far from 1000"
            );
        }
    }

    #[test]
    fn empty_input() {
        assert!(resample_48k_to_8k(&[]).is_empty());
        assert!(resample_8k_to_48k(&[]).is_empty());
    }

    #[test]
    fn stateful_downsampler_produces_correct_length() {
        let mut ds = Downsampler48to8::new();
        let out = ds.process(&vec![0i16; 960]);
        assert_eq!(out.len(), 160);
        let out2 = ds.process(&vec![0i16; 960]);
        assert_eq!(out2.len(), 160);
    }

    #[test]
    fn stateful_upsampler_produces_correct_length() {
        let mut us = Upsampler8to48::new();
        let out = us.process(&vec![0i16; 160]);
        assert_eq!(out.len(), 960);
        let out2 = us.process(&vec![0i16; 160]);
        assert_eq!(out2.len(), 960);
    }

    #[test]
    fn fir_kernel_has_unity_dc_gain() {
        let kernel = build_fir_kernel();
        let sum: f64 = kernel.iter().sum();
        assert!(
            (sum - 1.0).abs() < 1e-10,
            "FIR kernel DC gain should be 1.0, got {sum}"
        );
    }

    #[test]
    fn bessel_i0_known_values() {
        // I₀(0) = 1
        assert!((bessel_i0(0.0) - 1.0).abs() < 1e-12);
        // I₀(1) ≈ 1.2660658
        assert!((bessel_i0(1.0) - 1.2660658).abs() < 1e-5);
    }
}
