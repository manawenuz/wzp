//! Automated clock-drift measurement tool.
//!
//! Sends N seconds of a known 440 Hz tone through the transport, records
//! received frame timestamps on the other side, and compares actual received
//! duration vs expected duration to quantify timing drift and packet loss.

use std::time::{Duration, Instant};

use tracing::info;

use wzp_proto::MediaTransport;

use crate::call::{CallConfig, CallDecoder, CallEncoder};

const FRAME_SAMPLES: usize = 960; // 20ms @ 48kHz
const SAMPLE_RATE: u32 = 48_000;

/// Configuration for a drift measurement run.
#[derive(Debug, Clone)]
pub struct DriftTestConfig {
    /// How many seconds of tone to send.
    pub duration_secs: u32,
    /// Frequency of the test tone (Hz).
    pub tone_freq_hz: f32,
}

impl Default for DriftTestConfig {
    fn default() -> Self {
        Self {
            duration_secs: 10,
            tone_freq_hz: 440.0,
        }
    }
}

/// Results from a drift measurement run.
#[derive(Debug, Clone)]
pub struct DriftResult {
    /// Expected duration in milliseconds (`duration_secs * 1000`).
    pub expected_duration_ms: u64,
    /// Actual measured duration in milliseconds (last_recv - first_recv).
    pub actual_duration_ms: u64,
    /// Drift: `actual - expected` (positive = receiver clock ran slow / packets delayed).
    pub drift_ms: i64,
    /// Drift as a percentage of expected duration.
    pub drift_pct: f64,
    /// Total frames sent by the sender.
    pub frames_sent: u64,
    /// Total frames successfully received and decoded.
    pub frames_received: u64,
    /// Packet loss percentage: `(1 - frames_received / frames_sent) * 100`.
    pub loss_pct: f64,
}

impl DriftResult {
    /// Compute a `DriftResult` from raw counters and timestamps.
    pub fn compute(
        expected_duration_ms: u64,
        actual_duration_ms: u64,
        frames_sent: u64,
        frames_received: u64,
    ) -> Self {
        let drift_ms = actual_duration_ms as i64 - expected_duration_ms as i64;
        let drift_pct = if expected_duration_ms > 0 {
            drift_ms as f64 / expected_duration_ms as f64 * 100.0
        } else {
            0.0
        };
        let loss_pct = if frames_sent > 0 {
            (1.0 - frames_received as f64 / frames_sent as f64) * 100.0
        } else {
            0.0
        };
        Self {
            expected_duration_ms,
            actual_duration_ms,
            drift_ms,
            drift_pct,
            frames_sent,
            frames_received,
            loss_pct,
        }
    }
}

/// Generate a sine wave frame at a given frequency.
fn sine_frame(freq_hz: f32, frame_offset: u64) -> Vec<i16> {
    let start = frame_offset * FRAME_SAMPLES as u64;
    (0..FRAME_SAMPLES)
        .map(|i| {
            let t = (start + i as u64) as f32 / SAMPLE_RATE as f32;
            (f32::sin(2.0 * std::f32::consts::PI * freq_hz * t) * 16000.0) as i16
        })
        .collect()
}

/// Run the drift measurement test.
///
/// 1. Spawns a send task that encodes `duration_secs` of tone at 20 ms intervals.
/// 2. Spawns a recv task that counts decoded frames and tracks first/last timestamps.
/// 3. After the sender finishes, waits 2 seconds for trailing packets.
/// 4. Computes and returns the `DriftResult`.
pub async fn run_drift_test(
    transport: &(dyn MediaTransport + Send + Sync),
    config: &DriftTestConfig,
) -> anyhow::Result<DriftResult> {
    let call_config = CallConfig::default();
    let mut encoder = CallEncoder::new(&call_config);
    let mut decoder = CallDecoder::new(&call_config);

    let total_frames: u64 = config.duration_secs as u64 * 50; // 50 frames/s at 20 ms
    let frame_duration = Duration::from_millis(20);
    let mut pcm_buf = vec![0i16; FRAME_SAMPLES];

    let mut frames_sent: u64 = 0;
    let mut frames_received: u64 = 0;
    let mut first_recv_time: Option<Instant> = None;
    let mut last_recv_time: Option<Instant> = None;

    info!(
        duration_secs = config.duration_secs,
        tone_hz = config.tone_freq_hz,
        total_frames = total_frames,
        "starting drift measurement"
    );

    let start = Instant::now();

    // Send + interleaved receive loop (same pattern as echo_test)
    for frame_idx in 0..total_frames {
        // --- send ---
        let pcm = sine_frame(config.tone_freq_hz, frame_idx);
        let packets = encoder.encode_frame(&pcm)?;
        for pkt in &packets {
            transport.send_media(pkt).await?;
        }
        frames_sent += 1;

        // --- try to receive (short window so we don't block the sender) ---
        let recv_deadline = Instant::now() + Duration::from_millis(5);
        loop {
            if Instant::now() >= recv_deadline {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(2), transport.recv_media()).await {
                Ok(Ok(Some(pkt))) => {
                    let is_repair = pkt.header.is_repair;
                    decoder.ingest(pkt);
                    if !is_repair {
                        if let Some(_n) = decoder.decode_next(&mut pcm_buf) {
                            let now = Instant::now();
                            if first_recv_time.is_none() {
                                first_recv_time = Some(now);
                            }
                            last_recv_time = Some(now);
                            frames_received += 1;
                        }
                    }
                }
                _ => break,
            }
        }

        if (frame_idx + 1) % 250 == 0 {
            info!(
                frame = frame_idx + 1,
                sent = frames_sent,
                recv = frames_received,
                elapsed = format!("{:.1}s", start.elapsed().as_secs_f64()),
                "drift-test progress"
            );
        }

        tokio::time::sleep(frame_duration).await;
    }

    // Drain trailing packets for 2 seconds
    info!("sender done, draining trailing packets for 2s...");
    let drain_deadline = Instant::now() + Duration::from_secs(2);
    while Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(100), transport.recv_media()).await {
            Ok(Ok(Some(pkt))) => {
                let is_repair = pkt.header.is_repair;
                decoder.ingest(pkt);
                if !is_repair {
                    if let Some(_n) = decoder.decode_next(&mut pcm_buf) {
                        let now = Instant::now();
                        if first_recv_time.is_none() {
                            first_recv_time = Some(now);
                        }
                        last_recv_time = Some(now);
                        frames_received += 1;
                    }
                }
            }
            _ => break,
        }
    }

    // Compute result
    let expected_duration_ms = config.duration_secs as u64 * 1000;
    let actual_duration_ms = match (first_recv_time, last_recv_time) {
        (Some(first), Some(last)) => last.duration_since(first).as_millis() as u64,
        _ => 0,
    };

    let result = DriftResult::compute(
        expected_duration_ms,
        actual_duration_ms,
        frames_sent,
        frames_received,
    );

    info!(
        expected_ms = result.expected_duration_ms,
        actual_ms = result.actual_duration_ms,
        drift_ms = result.drift_ms,
        drift_pct = format!("{:.4}%", result.drift_pct),
        loss_pct = format!("{:.1}%", result.loss_pct),
        "drift measurement complete"
    );

    Ok(result)
}

/// Pretty-print the drift measurement results.
pub fn print_drift_report(result: &DriftResult) {
    println!();
    println!("=== Drift Measurement Report ===");
    println!();
    println!("Frames sent:        {}", result.frames_sent);
    println!("Frames received:    {}", result.frames_received);
    println!("Packet loss:        {:.1}%", result.loss_pct);
    println!();
    println!("Expected duration:  {} ms", result.expected_duration_ms);
    println!("Actual duration:    {} ms", result.actual_duration_ms);
    println!("Drift:              {} ms ({:+.4}%)", result.drift_ms, result.drift_pct);
    println!();

    // Interpretation
    let abs_drift = result.drift_ms.unsigned_abs();
    if result.frames_received == 0 {
        println!("WARNING: No frames received. Transport may be non-functional.");
    } else if abs_drift < 5 {
        println!("Result: EXCELLENT -- drift is negligible (<5 ms).");
    } else if abs_drift < 20 {
        println!("Result: GOOD -- drift is within acceptable bounds (<20 ms).");
    } else if abs_drift < 100 {
        println!("Result: FAIR -- noticeable drift ({} ms). Clock sync may be needed.", abs_drift);
    } else {
        println!("Result: POOR -- significant drift ({} ms). Investigate clock sources.", abs_drift);
    }
    println!();
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn drift_result_calculations() {
        // Perfect case: no drift, no loss
        let r = DriftResult::compute(10_000, 10_000, 500, 500);
        assert_eq!(r.drift_ms, 0);
        assert!((r.drift_pct - 0.0).abs() < f64::EPSILON);
        assert!((r.loss_pct - 0.0).abs() < f64::EPSILON);

        // Positive drift (receiver duration longer than expected)
        let r = DriftResult::compute(10_000, 10_050, 500, 490);
        assert_eq!(r.drift_ms, 50);
        assert!((r.drift_pct - 0.5).abs() < 1e-9); // 50/10000 * 100 = 0.5%
        assert!((r.loss_pct - 2.0).abs() < 1e-9); // (1 - 490/500) * 100 = 2.0%

        // Negative drift (receiver duration shorter than expected)
        let r = DriftResult::compute(10_000, 9_900, 500, 450);
        assert_eq!(r.drift_ms, -100);
        assert!((r.drift_pct - (-1.0)).abs() < 1e-9); // -100/10000 * 100 = -1.0%
        assert!((r.loss_pct - 10.0).abs() < 1e-9); // (1 - 450/500) * 100 = 10.0%

        // Edge: zero frames sent (avoid division by zero)
        let r = DriftResult::compute(0, 0, 0, 0);
        assert_eq!(r.drift_ms, 0);
        assert!((r.drift_pct - 0.0).abs() < f64::EPSILON);
        assert!((r.loss_pct - 0.0).abs() < f64::EPSILON);
    }

    #[test]
    fn drift_config_defaults() {
        let cfg = DriftTestConfig::default();
        assert_eq!(cfg.duration_secs, 10);
        assert!((cfg.tone_freq_hz - 440.0).abs() < f32::EPSILON);
    }
}
