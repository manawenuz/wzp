//! Parameter sweep tool for jitter buffer configurations.
//!
//! Tests different (target_depth, max_depth) combinations in a local
//! encoder-to-decoder pipeline (no network) and reports frame loss,
//! estimated latency, underruns, and overruns for each configuration.

use crate::call::{CallConfig, CallDecoder, CallEncoder};
use wzp_proto::QualityProfile;

const FRAME_SAMPLES: usize = 960; // 20ms @ 48kHz
const SAMPLE_RATE: u32 = 48_000;
const FRAME_DURATION_MS: u32 = 20;

/// Configuration for a parameter sweep.
pub struct SweepConfig {
    /// Target jitter buffer depths to test (in packets).
    pub target_depths: Vec<usize>,
    /// Maximum jitter buffer depths to test (in packets).
    pub max_depths: Vec<usize>,
    /// Duration in seconds to run each configuration.
    pub test_duration_secs: u32,
    /// Frequency of the test tone in Hz.
    pub tone_freq_hz: f32,
}

impl Default for SweepConfig {
    fn default() -> Self {
        Self {
            target_depths: vec![10, 25, 50, 100, 200],
            max_depths: vec![50, 100, 250, 500],
            test_duration_secs: 2,
            tone_freq_hz: 440.0,
        }
    }
}

/// Result from one (target_depth, max_depth) configuration.
#[derive(Debug, Clone)]
pub struct SweepResult {
    /// Jitter buffer target depth used.
    pub target_depth: usize,
    /// Jitter buffer max depth used.
    pub max_depth: usize,
    /// Total frames sent into the encoder.
    pub frames_sent: u64,
    /// Total frames successfully decoded.
    pub frames_received: u64,
    /// Frame loss percentage.
    pub loss_pct: f64,
    /// Estimated latency in ms (target_depth * frame_duration).
    pub avg_latency_ms: f64,
    /// Number of jitter buffer underruns.
    pub underruns: u64,
    /// Number of jitter buffer overruns (packets dropped due to full buffer).
    pub overruns: u64,
}

/// Generate a sine wave frame at the given frequency and frame offset.
fn sine_frame(freq_hz: f32, frame_offset: u64) -> Vec<i16> {
    let start = frame_offset * FRAME_SAMPLES as u64;
    (0..FRAME_SAMPLES)
        .map(|i| {
            let t = (start + i as u64) as f32 / SAMPLE_RATE as f32;
            (f32::sin(2.0 * std::f32::consts::PI * freq_hz * t) * 16000.0) as i16
        })
        .collect()
}

/// Run a local parameter sweep (no network).
///
/// For each (target_depth, max_depth) combination, creates an encoder and
/// decoder, pushes frames through the pipeline, and collects statistics.
/// Combinations where `target_depth > max_depth` are skipped.
pub fn run_local_sweep(config: &SweepConfig) -> Vec<SweepResult> {
    let frames_per_config =
        (config.test_duration_secs as u64) * (1000 / FRAME_DURATION_MS as u64);

    let mut results = Vec::new();

    for &target in &config.target_depths {
        for &max in &config.max_depths {
            // Skip invalid combinations where target exceeds max.
            if target > max {
                continue;
            }

            let call_cfg = CallConfig {
                profile: QualityProfile::GOOD,
                jitter_target: target,
                jitter_max: max,
                jitter_min: target.min(3).max(1),
                ..Default::default()
            };

            let mut encoder = CallEncoder::new(&call_cfg);
            let mut decoder = CallDecoder::new(&call_cfg);

            let mut pcm_out = vec![0i16; FRAME_SAMPLES];
            let mut frames_decoded = 0u64;

            for frame_idx in 0..frames_per_config {
                // Encode a tone frame.
                let pcm_in = sine_frame(config.tone_freq_hz, frame_idx);
                let packets = match encoder.encode_frame(&pcm_in) {
                    Ok(p) => p,
                    Err(_) => continue,
                };

                // Feed all packets (source + repair) into the decoder.
                for pkt in packets {
                    decoder.ingest(pkt);
                }

                // Attempt to decode one frame.
                if decoder.decode_next(&mut pcm_out).is_some() {
                    frames_decoded += 1;
                }
            }

            // Drain: keep decoding until the jitter buffer is empty.
            for _ in 0..max {
                if decoder.decode_next(&mut pcm_out).is_some() {
                    frames_decoded += 1;
                } else {
                    break;
                }
            }

            let stats = decoder.stats().clone();

            let loss_pct = if frames_per_config > 0 {
                (1.0 - frames_decoded as f64 / frames_per_config as f64) * 100.0
            } else {
                0.0
            };

            results.push(SweepResult {
                target_depth: target,
                max_depth: max,
                frames_sent: frames_per_config,
                frames_received: frames_decoded,
                loss_pct: loss_pct.max(0.0),
                avg_latency_ms: target as f64 * FRAME_DURATION_MS as f64,
                underruns: stats.underruns,
                overruns: stats.overruns,
            });
        }
    }

    results
}

/// Print a formatted ASCII table of sweep results.
pub fn print_sweep_table(results: &[SweepResult]) {
    println!();
    println!("=== Jitter Buffer Parameter Sweep ===");
    println!();
    println!(
        " {:>6} | {:>4} | {:>6} | {:>6} | {:>6} | {:>10} | {:>9} | {:>8}",
        "target", "max", "sent", "recv", "loss%", "latency_ms", "underruns", "overruns"
    );
    println!(
        " {:-<6}-+-{:-<4}-+-{:-<6}-+-{:-<6}-+-{:-<6}-+-{:-<10}-+-{:-<9}-+-{:-<8}",
        "", "", "", "", "", "", "", ""
    );
    for r in results {
        println!(
            " {:>6} | {:>4} | {:>6} | {:>6} | {:>5.1}% | {:>10.0} | {:>9} | {:>8}",
            r.target_depth,
            r.max_depth,
            r.frames_sent,
            r.frames_received,
            r.loss_pct,
            r.avg_latency_ms,
            r.underruns,
            r.overruns,
        );
    }
    println!();
}

/// Run a default sweep and print the results.
///
/// This is the entry point for the `--sweep` CLI flag.
pub fn run_and_print_default_sweep() {
    let config = SweepConfig::default();
    let results = run_local_sweep(&config);
    print_sweep_table(&results);
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sweep_config_default() {
        let cfg = SweepConfig::default();
        assert_eq!(cfg.target_depths.len(), 5);
        assert_eq!(cfg.max_depths.len(), 4);
        assert!(cfg.test_duration_secs > 0);
        assert!(cfg.tone_freq_hz > 0.0);
        // All default targets should be positive.
        assert!(cfg.target_depths.iter().all(|&d| d > 0));
        assert!(cfg.max_depths.iter().all(|&d| d > 0));
    }

    #[test]
    fn local_sweep_runs() {
        let cfg = SweepConfig {
            target_depths: vec![3, 10],
            max_depths: vec![50, 100],
            test_duration_secs: 1,
            tone_freq_hz: 440.0,
        };
        let results = run_local_sweep(&cfg);
        // 2 targets x 2 maxes = 4 configs (all valid since targets < maxes).
        assert_eq!(results.len(), 4);
        for r in &results {
            assert!(r.frames_sent > 0, "frames_sent should be > 0");
            assert!(r.frames_received > 0, "frames_received should be > 0");
            assert!(r.avg_latency_ms > 0.0, "latency should be > 0");
        }
    }

    #[test]
    fn sweep_table_formats() {
        // Verify print_sweep_table doesn't panic with various inputs.
        print_sweep_table(&[]);

        let results = vec![
            SweepResult {
                target_depth: 10,
                max_depth: 50,
                frames_sent: 100,
                frames_received: 98,
                loss_pct: 2.0,
                avg_latency_ms: 200.0,
                underruns: 2,
                overruns: 0,
            },
            SweepResult {
                target_depth: 25,
                max_depth: 100,
                frames_sent: 100,
                frames_received: 100,
                loss_pct: 0.0,
                avg_latency_ms: 500.0,
                underruns: 0,
                overruns: 0,
            },
        ];
        print_sweep_table(&results);
    }
}
