//! WZP-P2-T1-S5: 60-second long-session regression tests.
//!
//! Verifies that the full codec + FEC + jitter buffer pipeline does not drift
//! or degrade over a sustained 60-second (3000-frame) session. Runs entirely
//! in-process with no network — packets flow directly from encoder to decoder.

use wzp_client::call::{CallConfig, CallDecoder, CallEncoder};
use wzp_proto::QualityProfile;

const FRAME_SAMPLES: usize = 960; // 20ms @ 48kHz
const SAMPLE_RATE: f32 = 48_000.0;
const TOTAL_FRAMES: u64 = 3_000; // 60 seconds at 50 fps

/// Build a CallConfig tuned for direct-loopback testing (no network).
///
/// Disables silence suppression and noise suppression (which would mangle
/// or squelch the synthetic tone), uses a fixed (non-adaptive) jitter buffer
/// with min_depth=1 so that packets are played out as soon as they arrive.
fn test_config() -> CallConfig {
    CallConfig {
        profile: QualityProfile::GOOD,
        jitter_target: 4,
        jitter_max: 500,
        jitter_min: 1,
        suppression_enabled: false,
        noise_suppression: false,
        adaptive_jitter: false,
        ..Default::default()
    }
}

/// Generate a 20ms frame of 440 Hz sine tone.
fn sine_frame(frame_offset: u64) -> Vec<i16> {
    let start_sample = frame_offset * FRAME_SAMPLES as u64;
    (0..FRAME_SAMPLES)
        .map(|i| {
            let t = (start_sample + i as u64) as f32 / SAMPLE_RATE;
            (f32::sin(2.0 * std::f32::consts::PI * 440.0 * t) * 16000.0) as i16
        })
        .collect()
}

/// 60-second session with a perfect (lossless, in-order) channel.
///
/// Encodes 3000 frames of 440 Hz tone, feeds every packet directly into the
/// decoder, and verifies:
///   - frame loss < 5%  (>2850 of 3000 source frames decoded or PLC'd)
///   - no panics
///
/// Note: the encoder shares a single sequence counter between source and
/// repair packets.  Since repair packets are NOT pushed into the jitter
/// buffer, each FEC block creates a gap in the playout sequence.  GOOD
/// profile (5 frames/block, fec_ratio=0.2) generates 1 repair per block,
/// so every 6th seq number is a "phantom" Missing in the jitter buffer.
/// The jitter buffer correctly fills these gaps with PLC.  We call
/// `decode_next` once per encode tick; the buffer stays shallow because
/// PLC frames consume the phantom seqs at the same rate they're created.
#[test]
fn long_session_no_drift() {
    let config = test_config();
    let mut encoder = CallEncoder::new(&config);
    let mut decoder = CallDecoder::new(&config);

    let mut frames_decoded = 0u64;
    let mut pcm_buf = vec![0i16; FRAME_SAMPLES];

    for i in 0..TOTAL_FRAMES {
        let pcm = sine_frame(i);
        let packets = encoder.encode_frame(&pcm).expect("encode should not fail");

        for pkt in packets {
            decoder.ingest(pkt);
        }

        // Decode one frame per tick (mirrors real-time 50 fps cadence).
        if decoder.decode_next(&mut pcm_buf).is_some() {
            frames_decoded += 1;
        }
    }

    let stats = decoder.stats();

    println!(
        "long_session_no_drift: decoded={frames_decoded}/{TOTAL_FRAMES}, \
         underruns={}, overruns={}, depth={}, max_depth={}, late={}, lost={}",
        stats.underruns, stats.overruns, stats.current_depth, stats.max_depth_seen,
        stats.packets_late, stats.packets_lost,
    );

    // With 1 decode per tick over 3000 ticks, we expect ~3000 decoded frames
    // (some via PLC for repair-seq gaps).  Allow up to 5% gap.
    assert!(
        frames_decoded > 2850,
        "frame loss too high: decoded {frames_decoded}/3000 (need >2850 = <5% loss)"
    );
}

/// 60-second session with simulated 5% packet loss and reordering.
///
/// Every 20th source packet is dropped; pairs of adjacent packets are swapped
/// every 7 frames.  Verifies that FEC + jitter buffer recover gracefully:
///   - frame loss < 10% (FEC should recover some of the 5% artificial loss)
///   - no panics
#[test]
fn long_session_with_simulated_loss() {
    let config = test_config();
    let mut encoder = CallEncoder::new(&config);
    let mut decoder = CallDecoder::new(&config);

    let mut frames_decoded = 0u64;
    let mut pcm_buf = vec![0i16; FRAME_SAMPLES];

    for i in 0..TOTAL_FRAMES {
        let pcm = sine_frame(i);
        let packets = encoder.encode_frame(&pcm).expect("encode should not fail");

        let mut batch: Vec<_> = packets.into_iter().collect();

        // Simulate reordering: swap first two packets in the batch every 7 frames.
        if i % 7 == 0 && batch.len() >= 2 {
            batch.swap(0, 1);
        }

        for (j, pkt) in batch.into_iter().enumerate() {
            // Drop every 20th *source* (non-repair) packet to simulate ~5% loss.
            if !pkt.header.is_repair && i % 20 == 0 && j == 0 {
                continue; // drop this packet
            }
            decoder.ingest(pkt);
        }

        if decoder.decode_next(&mut pcm_buf).is_some() {
            frames_decoded += 1;
        }
    }

    let stats = decoder.stats();

    println!(
        "long_session_with_simulated_loss: decoded={frames_decoded}/{TOTAL_FRAMES}, \
         underruns={}, overruns={}, depth={}, max_depth={}, late={}, lost={}",
        stats.underruns, stats.overruns, stats.current_depth, stats.max_depth_seen,
        stats.packets_late, stats.packets_lost,
    );

    // With 5% artificial loss + FEC recovery + PLC, we should still get >90% decoded.
    assert!(
        frames_decoded > 2700,
        "frame loss too high under simulated loss: decoded {frames_decoded}/3000 (need >2700 = <10%)"
    );
}

/// Verify that the jitter buffer's decoded-frame count is consistent with its
/// own internal statistics over a long session.
#[test]
fn long_session_stats_consistency() {
    let config = test_config();
    let mut encoder = CallEncoder::new(&config);
    let mut decoder = CallDecoder::new(&config);

    let mut frames_decoded = 0u64;
    let mut pcm_buf = vec![0i16; FRAME_SAMPLES];

    for i in 0..TOTAL_FRAMES {
        let pcm = sine_frame(i);
        let packets = encoder.encode_frame(&pcm).expect("encode");

        for pkt in packets {
            decoder.ingest(pkt);
        }
        if decoder.decode_next(&mut pcm_buf).is_some() {
            frames_decoded += 1;
        }
    }

    let stats = decoder.stats();

    // total_decoded should match our manual counter.
    assert_eq!(
        stats.total_decoded, frames_decoded,
        "stats.total_decoded ({}) != manually counted frames_decoded ({frames_decoded})",
        stats.total_decoded,
    );

    // packets_received should be > 0.
    assert!(
        stats.packets_received > 0,
        "stats.packets_received should be > 0"
    );
}
