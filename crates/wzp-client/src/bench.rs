//! Benchmark functions for measuring WarzonePhone protocol performance.
//!
//! Covers codec roundtrip, FEC recovery, encryption throughput, and the full pipeline.

use std::time::{Duration, Instant};

use wzp_crypto::ChaChaSession;
use wzp_fec::{RaptorQFecDecoder, RaptorQFecEncoder};
use wzp_proto::traits::{CryptoSession, FecDecoder, FecEncoder};
use wzp_proto::QualityProfile;

use crate::call::{CallConfig, CallDecoder, CallEncoder};

// ─── Results ────────────────────────────────────────────────────────────────

/// Results from the codec roundtrip benchmark.
#[derive(Debug)]
pub struct CodecResult {
    pub frames: usize,
    pub total_encode: Duration,
    pub total_decode: Duration,
    pub avg_encode_us: f64,
    pub avg_decode_us: f64,
    pub frames_per_sec: f64,
    pub compression_ratio: f64,
}

/// Results from the FEC recovery benchmark.
#[derive(Debug)]
pub struct FecResult {
    pub blocks_attempted: usize,
    pub blocks_recovered: usize,
    pub recovery_rate_pct: f64,
    pub total_source_bytes: usize,
    pub total_repair_bytes: usize,
    pub overhead_bytes: usize,
    pub total_time: Duration,
}

/// Results from the crypto benchmark.
#[derive(Debug)]
pub struct CryptoResult {
    pub packets: usize,
    pub total_time: Duration,
    pub packets_per_sec: f64,
    pub megabytes_per_sec: f64,
    pub avg_latency_us: f64,
}

/// Results from the full pipeline benchmark.
#[derive(Debug)]
pub struct PipelineResult {
    pub frames: usize,
    pub total_encode_pipeline: Duration,
    pub total_decode_pipeline: Duration,
    pub avg_e2e_latency_us: f64,
    pub pcm_bytes_in: usize,
    pub wire_bytes_out: usize,
    pub overhead_ratio: f64,
}

// ─── Helpers ────────────────────────────────────────────────────────────────

/// Generate a sine wave as 16-bit PCM samples.
pub fn generate_sine_wave(freq_hz: f32, sample_rate: u32, num_samples: usize) -> Vec<i16> {
    (0..num_samples)
        .map(|i| {
            let t = i as f32 / sample_rate as f32;
            (f32::sin(2.0 * std::f32::consts::PI * freq_hz * t) * 16000.0) as i16
        })
        .collect()
}

// ─── Benchmarks ─────────────────────────────────────────────────────────────

/// Measure Opus encode+decode latency and throughput.
///
/// Generates 1000 frames of 440 Hz sine wave (48 kHz, 20 ms frames),
/// encodes each, decodes each, and reports timing and compression ratio.
pub fn bench_codec_roundtrip() -> CodecResult {
    let profile = QualityProfile::GOOD;
    let frame_samples = 960; // 20ms @ 48kHz
    let num_frames = 1000;

    let pcm = generate_sine_wave(440.0, 48_000, frame_samples * num_frames);

    let mut encoder = wzp_codec::create_encoder(profile);
    let mut decoder = wzp_codec::create_decoder(profile);

    let max_enc = encoder.max_frame_bytes();
    let mut enc_buf = vec![0u8; max_enc];
    let mut dec_buf = vec![0i16; frame_samples];

    let mut encoded_frames: Vec<Vec<u8>> = Vec::with_capacity(num_frames);
    let mut total_encoded_bytes: usize = 0;

    // Encode
    let encode_start = Instant::now();
    for i in 0..num_frames {
        let start = i * frame_samples;
        let end = start + frame_samples;
        let n = encoder.encode(&pcm[start..end], &mut enc_buf).unwrap();
        encoded_frames.push(enc_buf[..n].to_vec());
        total_encoded_bytes += n;
    }
    let total_encode = encode_start.elapsed();

    // Decode
    let decode_start = Instant::now();
    for frame in &encoded_frames {
        let _ = decoder.decode(frame, &mut dec_buf).unwrap();
    }
    let total_decode = decode_start.elapsed();

    let total_pcm_bytes = num_frames * frame_samples * 2; // i16 = 2 bytes
    let compression_ratio = total_pcm_bytes as f64 / total_encoded_bytes as f64;
    let total_time = total_encode + total_decode;
    let frames_per_sec = num_frames as f64 / total_time.as_secs_f64();

    CodecResult {
        frames: num_frames,
        total_encode,
        total_decode,
        avg_encode_us: total_encode.as_micros() as f64 / num_frames as f64,
        avg_decode_us: total_decode.as_micros() as f64 / num_frames as f64,
        frames_per_sec,
        compression_ratio,
    }
}

/// Measure FEC encode/decode with simulated packet loss.
///
/// Encodes 100 blocks of 5 frames each, drops `loss_pct`% of packets
/// randomly per block, and measures recovery rate.
pub fn bench_fec_recovery(loss_pct: f32) -> FecResult {
    let profile = QualityProfile::GOOD; // 5 frames/block, 0.2 ratio
    let frames_per_block = profile.frames_per_block as usize;
    let num_blocks = 100;
    // Use a higher FEC ratio for the bench so recovery is possible at higher loss
    let fec_ratio = if loss_pct > 20.0 { 1.0 } else { 0.5 };

    let start = Instant::now();

    let mut blocks_recovered = 0usize;
    let mut total_source_bytes = 0usize;
    let mut total_repair_bytes = 0usize;

    for block_idx in 0..num_blocks {
        let block_id = (block_idx % 256) as u8;

        // Create fresh encoder and decoder for each block
        let mut fec_enc = RaptorQFecEncoder::new(frames_per_block, 256);
        let mut fec_dec = RaptorQFecDecoder::new(frames_per_block, 256);

        // Generate source symbols (simulated encoded audio frames)
        let mut source_symbols: Vec<Vec<u8>> = Vec::new();
        for i in 0..frames_per_block {
            let val = ((block_idx * frames_per_block + i) & 0xFF) as u8;
            let sym = vec![val; 80];
            fec_enc.add_source_symbol(&sym).unwrap();
            source_symbols.push(sym);
        }

        let repairs = fec_enc.generate_repair(fec_ratio).unwrap();

        // Collect all symbols: source + repair
        struct Symbol {
            index: u8,
            is_repair: bool,
            data: Vec<u8>,
        }

        let mut all_symbols: Vec<Symbol> = Vec::new();
        for (i, sym) in source_symbols.iter().enumerate() {
            // For add_symbol we need to provide the raw data; the decoder pads internally
            total_source_bytes += sym.len();
            all_symbols.push(Symbol {
                index: i as u8,
                is_repair: false,
                data: sym.clone(),
            });
        }
        for (idx, data) in &repairs {
            total_repair_bytes += data.len();
            all_symbols.push(Symbol {
                index: *idx,
                is_repair: true,
                data: data.clone(),
            });
        }

        // Simulate loss: drop loss_pct% of symbols
        let drop_count =
            ((all_symbols.len() as f32 * loss_pct / 100.0).round() as usize).min(all_symbols.len());

        // Deterministic shuffle for reproducibility using a simple seed
        // We use a basic Fisher-Yates with a fixed-per-block seed
        let mut indices: Vec<usize> = (0..all_symbols.len()).collect();
        let mut seed = (block_idx as u64).wrapping_mul(6364136223846793005).wrapping_add(1);
        for i in (1..indices.len()).rev() {
            seed = seed.wrapping_mul(6364136223846793005).wrapping_add(1442695040888963407);
            let j = (seed >> 33) as usize % (i + 1);
            indices.swap(i, j);
        }

        // Keep all but `drop_count` symbols
        let keep_indices = &indices[drop_count..];

        for &idx in keep_indices {
            let sym = &all_symbols[idx];
            let _ = fec_dec.add_symbol(block_id, sym.index, sym.is_repair, &sym.data);
        }

        // Try to decode
        if let Ok(Some(_frames)) = fec_dec.try_decode(block_id) {
            blocks_recovered += 1;
        }
    }

    let total_time = start.elapsed();

    FecResult {
        blocks_attempted: num_blocks,
        blocks_recovered,
        recovery_rate_pct: blocks_recovered as f64 / num_blocks as f64 * 100.0,
        total_source_bytes,
        total_repair_bytes,
        overhead_bytes: total_repair_bytes,
        total_time,
    }
}

/// Measure ChaCha20-Poly1305 encrypt+decrypt throughput.
///
/// Creates a crypto session pair and encrypts+decrypts 10000 packets
/// of varying sizes (60, 120, 256 bytes).
pub fn bench_encrypt_decrypt() -> CryptoResult {
    let key = [0x42u8; 32];
    let mut encryptor = ChaChaSession::new(key);
    let mut decryptor = ChaChaSession::new(key);

    let sizes = [60usize, 120, 256];
    let packets_per_size = 10000;
    let total_packets = packets_per_size * sizes.len();

    // Pre-generate payloads
    let payloads: Vec<Vec<u8>> = sizes
        .iter()
        .flat_map(|&sz| {
            (0..packets_per_size).map(move |i| {
                let val = (i & 0xFF) as u8;
                vec![val; sz]
            })
        })
        .collect();

    let header = b"bench-header";
    let mut total_bytes: usize = 0;

    let start = Instant::now();
    for payload in &payloads {
        let mut ciphertext = Vec::with_capacity(payload.len() + 16);
        encryptor.encrypt(header, payload, &mut ciphertext).unwrap();

        let mut plaintext = Vec::with_capacity(payload.len());
        decryptor
            .decrypt(header, &ciphertext, &mut plaintext)
            .unwrap();

        total_bytes += payload.len();
    }
    let total_time = start.elapsed();

    let secs = total_time.as_secs_f64();

    CryptoResult {
        packets: total_packets,
        total_time,
        packets_per_sec: total_packets as f64 / secs,
        megabytes_per_sec: (total_bytes as f64 / (1024.0 * 1024.0)) / secs,
        avg_latency_us: total_time.as_micros() as f64 / total_packets as f64,
    }
}

/// End-to-end pipeline benchmark: PCM -> CallEncoder -> CallDecoder -> PCM.
///
/// Generates PCM, encodes through the full pipeline (codec + FEC),
/// then feeds packets into the decoder side and measures throughput.
pub fn bench_full_pipeline() -> PipelineResult {
    let config = CallConfig::default();
    let mut encoder = CallEncoder::new(&config);
    let mut decoder = CallDecoder::new(&config);

    let frame_samples = 960; // 20ms @ 48kHz
    let num_frames = 50;

    let pcm = generate_sine_wave(440.0, 48_000, frame_samples * num_frames);
    let pcm_bytes_in = num_frames * frame_samples * 2;

    let mut all_packets = Vec::new();
    let mut wire_bytes_out: usize = 0;

    // Encode pipeline
    let enc_start = Instant::now();
    for i in 0..num_frames {
        let start = i * frame_samples;
        let end = start + frame_samples;
        let packets = encoder.encode_frame(&pcm[start..end]).unwrap();
        for pkt in &packets {
            wire_bytes_out += pkt.payload.len();
        }
        all_packets.push(packets);
    }
    let total_encode_pipeline = enc_start.elapsed();

    // Decode pipeline: ingest all packets, then try to decode
    let dec_start = Instant::now();
    let mut dec_pcm = vec![0i16; frame_samples];
    for packets in &all_packets {
        for pkt in packets {
            decoder.ingest(pkt.clone());
        }
        // Attempt to decode after each frame's packets are ingested
        let _ = decoder.decode_next(&mut dec_pcm);
    }
    // Drain any remaining frames
    while decoder.decode_next(&mut dec_pcm).is_some() {}
    let total_decode_pipeline = dec_start.elapsed();

    let total_time = total_encode_pipeline + total_decode_pipeline;
    let overhead_ratio = wire_bytes_out as f64 / pcm_bytes_in as f64;

    PipelineResult {
        frames: num_frames,
        total_encode_pipeline,
        total_decode_pipeline,
        avg_e2e_latency_us: total_time.as_micros() as f64 / num_frames as f64,
        pcm_bytes_in,
        wire_bytes_out,
        overhead_ratio,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sine_wave_generates_correct_length() {
        let pcm = generate_sine_wave(440.0, 48_000, 960);
        assert_eq!(pcm.len(), 960);
        // Should have non-zero samples (it's a sine wave, not silence)
        assert!(pcm.iter().any(|&s| s != 0));
    }

    #[test]
    fn codec_roundtrip_runs() {
        let result = bench_codec_roundtrip();
        assert_eq!(result.frames, 1000);
        assert!(result.frames_per_sec > 0.0);
        assert!(result.compression_ratio > 1.0);
    }

    #[test]
    fn fec_recovery_runs() {
        let result = bench_fec_recovery(10.0);
        assert_eq!(result.blocks_attempted, 100);
        assert!(result.blocks_recovered > 0);
    }

    #[test]
    fn crypto_runs() {
        let result = bench_encrypt_decrypt();
        assert_eq!(result.packets, 30000);
        assert!(result.packets_per_sec > 0.0);
    }

    #[test]
    fn pipeline_runs() {
        let result = bench_full_pipeline();
        assert_eq!(result.frames, 200);
        assert!(result.wire_bytes_out > 0);
    }
}
