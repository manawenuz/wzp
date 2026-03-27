//! Automated echo quality test.
//!
//! Sends a known test signal through a relay (echo mode), records the return,
//! and analyzes quality over time to detect degradation, jitter buffer drift,
//! and packet loss patterns.

use std::time::{Duration, Instant};

use bytes::Bytes;
use tracing::{debug, info, warn};

use wzp_proto::packet::{MediaHeader, MediaPacket};
use wzp_proto::traits::{AudioDecoder, AudioEncoder, FecDecoder, FecEncoder};
use wzp_proto::MediaTransport;

use crate::call::{CallConfig, CallDecoder, CallEncoder};

const FRAME_SAMPLES: usize = 960; // 20ms @ 48kHz
const SAMPLE_RATE: u32 = 48_000;

/// Results from one analysis window.
#[derive(Debug, Clone)]
pub struct WindowResult {
    /// Window index (0-based).
    pub index: usize,
    /// Time offset from start (seconds).
    pub time_offset_secs: f64,
    /// Number of frames sent in this window.
    pub frames_sent: u32,
    /// Number of frames received (decoded) in this window.
    pub frames_received: u32,
    /// Packet loss percentage for this window.
    pub loss_pct: f32,
    /// Signal-to-noise ratio (dB) — higher is better.
    pub snr_db: f32,
    /// Cross-correlation with original signal (0.0-1.0).
    pub correlation: f32,
    /// Max absolute sample value in received audio.
    pub peak_amplitude: i16,
    /// Whether the window contains silence (no signal detected).
    pub is_silent: bool,
}

/// Full echo test results.
#[derive(Debug)]
pub struct EchoTestResult {
    pub duration_secs: f64,
    pub total_frames_sent: u64,
    pub total_frames_received: u64,
    pub total_packets_sent: u64,
    pub total_packets_received: u64,
    pub overall_loss_pct: f32,
    pub windows: Vec<WindowResult>,
    /// Jitter buffer stats at end.
    pub jitter_depth_final: usize,
    pub jitter_packets_lost: u64,
    pub jitter_packets_late: u64,
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

/// Compute signal-to-noise ratio between original and received PCM.
fn compute_snr(original: &[i16], received: &[i16]) -> f32 {
    if original.is_empty() || received.is_empty() {
        return 0.0;
    }
    let len = original.len().min(received.len());
    let mut signal_power: f64 = 0.0;
    let mut noise_power: f64 = 0.0;
    for i in 0..len {
        let s = original[i] as f64;
        let n = (received[i] as f64) - s;
        signal_power += s * s;
        noise_power += n * n;
    }
    if noise_power < 1.0 {
        return 99.0; // essentially perfect
    }
    (10.0 * (signal_power / noise_power).log10()) as f32
}

/// Compute normalized cross-correlation between two signals.
fn cross_correlation(a: &[i16], b: &[i16]) -> f32 {
    if a.is_empty() || b.is_empty() {
        return 0.0;
    }
    let len = a.len().min(b.len());
    let mut sum_ab: f64 = 0.0;
    let mut sum_aa: f64 = 0.0;
    let mut sum_bb: f64 = 0.0;
    for i in 0..len {
        let x = a[i] as f64;
        let y = b[i] as f64;
        sum_ab += x * y;
        sum_aa += x * x;
        sum_bb += y * y;
    }
    let denom = (sum_aa * sum_bb).sqrt();
    if denom < 1.0 {
        return 0.0;
    }
    (sum_ab / denom) as f32
}

/// Run an automated echo quality test.
///
/// Sends `duration_secs` of 440Hz tone through the transport (expects echo mode relay),
/// records the response, and analyzes quality in `window_secs`-second windows.
pub async fn run_echo_test(
    transport: &(dyn MediaTransport + Send + Sync),
    duration_secs: u32,
    window_secs: f64,
) -> anyhow::Result<EchoTestResult> {
    let config = CallConfig::default();
    let mut encoder = CallEncoder::new(&config);
    let mut decoder = CallDecoder::new(&config);

    let total_frames = (duration_secs as u64) * 50; // 50 fps at 20ms
    let frames_per_window = ((window_secs * 50.0) as u64).max(1);

    // Storage for sent and received PCM per window
    let mut sent_pcm: Vec<i16> = Vec::new();
    let mut recv_pcm: Vec<i16> = Vec::new();
    let mut windows: Vec<WindowResult> = Vec::new();
    let mut pcm_buf = vec![0i16; FRAME_SAMPLES];

    let mut total_packets_sent = 0u64;
    let mut total_packets_received = 0u64;
    let mut window_frames_sent = 0u32;
    let mut window_frames_received = 0u32;
    let mut window_idx = 0usize;

    let start = Instant::now();
    let frame_duration = Duration::from_millis(20);

    info!(
        duration = duration_secs,
        window = format!("{window_secs}s"),
        "starting echo quality test"
    );

    for frame_idx in 0..total_frames {
        // Generate and send tone
        let pcm = sine_frame(440.0, frame_idx);
        sent_pcm.extend_from_slice(&pcm);

        let packets = encoder.encode_frame(&pcm)?;
        for pkt in &packets {
            transport.send_media(pkt).await?;
            total_packets_sent += 1;
        }
        window_frames_sent += 1;

        // Try to receive echo (non-blocking-ish: short timeout)
        let recv_deadline = Instant::now() + Duration::from_millis(5);
        loop {
            if Instant::now() >= recv_deadline {
                break;
            }
            match tokio::time::timeout(Duration::from_millis(2), transport.recv_media()).await {
                Ok(Ok(Some(pkt))) => {
                    total_packets_received += 1;
                    let is_repair = pkt.header.is_repair;
                    decoder.ingest(pkt);
                    if !is_repair {
                        if let Some(n) = decoder.decode_next(&mut pcm_buf) {
                            recv_pcm.extend_from_slice(&pcm_buf[..n]);
                            window_frames_received += 1;
                        }
                    }
                }
                _ => break,
            }
        }

        // Analyze window
        if (frame_idx + 1) % frames_per_window == 0 || frame_idx == total_frames - 1 {
            let time_offset = start.elapsed().as_secs_f64();

            // Compare sent vs received for this window
            let sent_start = (window_idx as u64 * frames_per_window * FRAME_SAMPLES as u64) as usize;
            let sent_end = sent_start + (window_frames_sent as usize * FRAME_SAMPLES);
            let sent_window = if sent_end <= sent_pcm.len() {
                &sent_pcm[sent_start..sent_end]
            } else {
                &sent_pcm[sent_start..]
            };

            let recv_start = recv_pcm.len().saturating_sub(window_frames_received as usize * FRAME_SAMPLES);
            let recv_window = &recv_pcm[recv_start..];

            let peak = recv_window.iter().map(|s| s.abs()).max().unwrap_or(0);
            let is_silent = peak < 100;

            let snr = if !is_silent && !sent_window.is_empty() && !recv_window.is_empty() {
                compute_snr(sent_window, recv_window)
            } else {
                0.0
            };

            let corr = if !is_silent && !sent_window.is_empty() && !recv_window.is_empty() {
                cross_correlation(sent_window, recv_window)
            } else {
                0.0
            };

            let loss = if window_frames_sent > 0 {
                (1.0 - window_frames_received as f32 / window_frames_sent as f32) * 100.0
            } else {
                0.0
            };

            let result = WindowResult {
                index: window_idx,
                time_offset_secs: time_offset,
                frames_sent: window_frames_sent,
                frames_received: window_frames_received,
                loss_pct: loss.max(0.0),
                snr_db: snr,
                correlation: corr,
                peak_amplitude: peak,
                is_silent,
            };

            info!(
                window = window_idx,
                time = format!("{:.1}s", time_offset),
                sent = window_frames_sent,
                recv = window_frames_received,
                loss = format!("{:.1}%", result.loss_pct),
                snr = format!("{:.1}dB", snr),
                corr = format!("{:.3}", corr),
                peak = peak,
                "window analysis"
            );

            windows.push(result);
            window_idx += 1;
            window_frames_sent = 0;
            window_frames_received = 0;
        }

        tokio::time::sleep(frame_duration).await;
    }

    // Drain remaining received packets
    info!("draining remaining packets...");
    let drain_deadline = Instant::now() + Duration::from_secs(3);
    while Instant::now() < drain_deadline {
        match tokio::time::timeout(Duration::from_millis(100), transport.recv_media()).await {
            Ok(Ok(Some(pkt))) => {
                total_packets_received += 1;
                let is_repair = pkt.header.is_repair;
                decoder.ingest(pkt);
                if !is_repair {
                    decoder.decode_next(&mut pcm_buf);
                }
            }
            _ => break,
        }
    }

    let jitter_stats = decoder.jitter_stats();
    let total_frames_received = recv_pcm.len() as u64 / FRAME_SAMPLES as u64;
    let overall_loss = if total_frames > 0 {
        (1.0 - total_frames_received as f32 / total_frames as f32) * 100.0
    } else {
        0.0
    };

    Ok(EchoTestResult {
        duration_secs: start.elapsed().as_secs_f64(),
        total_frames_sent: total_frames,
        total_frames_received,
        total_packets_sent,
        total_packets_received,
        overall_loss_pct: overall_loss.max(0.0),
        windows,
        jitter_depth_final: jitter_stats.current_depth,
        jitter_packets_lost: jitter_stats.packets_lost,
        jitter_packets_late: jitter_stats.packets_late,
    })
}

/// Print a summary report of the echo test.
pub fn print_report(result: &EchoTestResult) {
    println!();
    println!("=== Echo Quality Test Report ===");
    println!();
    println!("Duration:           {:.1}s", result.duration_secs);
    println!("Frames sent:        {}", result.total_frames_sent);
    println!("Frames received:    {}", result.total_frames_received);
    println!("Packets sent:       {}", result.total_packets_sent);
    println!("Packets received:   {}", result.total_packets_received);
    println!("Overall loss:       {:.1}%", result.overall_loss_pct);
    println!("Jitter buf depth:   {}", result.jitter_depth_final);
    println!("Jitter buf lost:    {}", result.jitter_packets_lost);
    println!("Jitter buf late:    {}", result.jitter_packets_late);
    println!();
    println!("┌───────┬─────────┬──────┬──────┬─────────┬───────┬───────┐");
    println!("│ Win   │ Time    │ Sent │ Recv │ Loss    │ SNR   │ Corr  │");
    println!("├───────┼─────────┼──────┼──────┼─────────┼───────┼───────┤");
    for w in &result.windows {
        let status = if w.is_silent { " !" } else { "  " };
        println!(
            "│ {:>3}{} │ {:>5.1}s  │ {:>4} │ {:>4} │ {:>5.1}%  │ {:>5.1} │ {:.3} │",
            w.index, status, w.time_offset_secs, w.frames_sent, w.frames_received,
            w.loss_pct, w.snr_db, w.correlation
        );
    }
    println!("└───────┴─────────┴──────┴──────┴─────────┴───────┴───────┘");

    // Detect degradation trend
    if result.windows.len() >= 4 {
        let first_half: Vec<_> = result.windows[..result.windows.len() / 2].to_vec();
        let second_half: Vec<_> = result.windows[result.windows.len() / 2..].to_vec();

        let avg_loss_first = first_half.iter().map(|w| w.loss_pct).sum::<f32>() / first_half.len() as f32;
        let avg_loss_second = second_half.iter().map(|w| w.loss_pct).sum::<f32>() / second_half.len() as f32;
        let avg_corr_first = first_half.iter().map(|w| w.correlation).sum::<f32>() / first_half.len() as f32;
        let avg_corr_second = second_half.iter().map(|w| w.correlation).sum::<f32>() / second_half.len() as f32;

        println!();
        if avg_loss_second > avg_loss_first + 5.0 {
            println!("WARNING: Quality degradation detected!");
            println!("  Loss increased from {:.1}% to {:.1}% over time", avg_loss_first, avg_loss_second);
        }
        if avg_corr_second < avg_corr_first - 0.1 {
            println!("WARNING: Signal correlation dropped from {:.3} to {:.3}", avg_corr_first, avg_corr_second);
        }
        if avg_loss_second <= avg_loss_first + 5.0 && avg_corr_second >= avg_corr_first - 0.1 {
            println!("Quality is STABLE over the test duration.");
        }
    }
    println!();
}
