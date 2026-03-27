//! Simple linear resampler for 48 kHz <-> 8 kHz conversion.
//!
//! These are basic implementations suitable for voice. For higher quality,
//! replace with the `rubato` crate later.

/// Downsample from 48 kHz to 8 kHz (6:1 decimation with averaging).
///
/// Each output sample is the average of 6 consecutive input samples,
/// providing basic anti-aliasing via a box filter.
pub fn resample_48k_to_8k(input: &[i16]) -> Vec<i16> {
    const RATIO: usize = 6;
    let out_len = input.len() / RATIO;
    let mut output = Vec::with_capacity(out_len);

    for chunk in input.chunks_exact(RATIO) {
        let sum: i32 = chunk.iter().map(|&s| s as i32).sum();
        output.push((sum / RATIO as i32) as i16);
    }

    output
}

/// Upsample from 8 kHz to 48 kHz (1:6 interpolation with linear interp).
///
/// Linearly interpolates between each pair of input samples to produce
/// 6 output samples per input sample.
pub fn resample_8k_to_48k(input: &[i16]) -> Vec<i16> {
    const RATIO: usize = 6;
    if input.is_empty() {
        return Vec::new();
    }

    let out_len = input.len() * RATIO;
    let mut output = Vec::with_capacity(out_len);

    for i in 0..input.len() {
        let current = input[i] as i32;
        let next = if i + 1 < input.len() {
            input[i + 1] as i32
        } else {
            current // hold last sample
        };

        for j in 0..RATIO {
            let interp = current + (next - current) * j as i32 / RATIO as i32;
            output.push(interp as i16);
        }
    }

    output
}

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
        // A constant signal should survive resampling
        let input = vec![1000i16; 960];
        let down = resample_48k_to_8k(&input);
        assert!(down.iter().all(|&s| s == 1000));
        let up = resample_8k_to_48k(&down);
        assert!(up.iter().all(|&s| s == 1000));
    }

    #[test]
    fn empty_input() {
        assert!(resample_48k_to_8k(&[]).is_empty());
        assert!(resample_8k_to_48k(&[]).is_empty());
    }
}
