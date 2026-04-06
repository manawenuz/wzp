//! Real audio I/O via `cpal` — microphone capture and speaker playback.
//!
//! Both structs use 48 kHz, mono, i16 format to match the WarzonePhone codec
//! pipeline. Frames are 960 samples (20 ms at 48 kHz).
//!
//! Audio callbacks are **lock-free**: they read/write directly to an `AudioRing`
//! (atomic SPSC ring buffer). No Mutex, no channel, no allocation on the hot path.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use tracing::{info, warn};

use crate::audio_ring::AudioRing;

/// Number of samples per 20 ms frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;

// ---------------------------------------------------------------------------
// AudioCapture
// ---------------------------------------------------------------------------

/// Captures microphone input via CPAL and writes PCM into a lock-free ring buffer.
///
/// The cpal stream lives on a dedicated OS thread; this handle is `Send + Sync`.
pub struct AudioCapture {
    ring: Arc<AudioRing>,
    running: Arc<AtomicBool>,
}

impl AudioCapture {
    /// Create and start capturing from the default input device at 48 kHz mono.
    pub fn start() -> Result<Self, anyhow::Error> {
        let ring = Arc::new(AudioRing::new());
        let running = Arc::new(AtomicBool::new(true));

        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

        let ring_cb = ring.clone();
        let running_clone = running.clone();

        std::thread::Builder::new()
            .name("wzp-audio-capture".into())
            .spawn(move || {
                let result = (|| -> Result<(), anyhow::Error> {
                    let host = cpal::default_host();
                    let device = host
                        .default_input_device()
                        .ok_or_else(|| anyhow!("no default input audio device found"))?;

                    info!(device = %device.name().unwrap_or_default(), "using input device");

                    let config = StreamConfig {
                        channels: 1,
                        sample_rate: SampleRate(48_000),
                        buffer_size: cpal::BufferSize::Fixed(FRAME_SAMPLES as u32),
                    };

                    let use_f32 = !supports_i16_input(&device)?;

                    let err_cb = |e: cpal::StreamError| {
                        warn!("input stream error: {e}");
                    };

                    let stream = if use_f32 {
                        let ring = ring_cb.clone();
                        let running = running_clone.clone();
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                                if !running.load(Ordering::Relaxed) {
                                    return;
                                }
                                // Batch convert f32 → i16, then write entire slice to ring.
                                // Stack alloc for typical callback sizes (≤ 960 samples).
                                let mut tmp = [0i16; FRAME_SAMPLES];
                                for chunk in data.chunks(FRAME_SAMPLES) {
                                    let n = chunk.len();
                                    for i in 0..n {
                                        tmp[i] = f32_to_i16(chunk[i]);
                                    }
                                    ring.write(&tmp[..n]);
                                }
                            },
                            err_cb,
                            None,
                        )?
                    } else {
                        let ring = ring_cb.clone();
                        let running = running_clone.clone();
                        device.build_input_stream(
                            &config,
                            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                                if !running.load(Ordering::Relaxed) {
                                    return;
                                }
                                ring.write(data);
                            },
                            err_cb,
                            None,
                        )?
                    };

                    stream.play().context("failed to start input stream")?;

                    let _ = init_tx.send(Ok(()));

                    // Keep stream alive until stopped.
                    while running_clone.load(Ordering::Relaxed) {
                        std::thread::park_timeout(std::time::Duration::from_millis(200));
                    }
                    drop(stream);
                    Ok(())
                })();

                if let Err(e) = result {
                    let _ = init_tx.send(Err(e.to_string()));
                }
            })?;

        init_rx
            .recv()
            .map_err(|_| anyhow!("capture thread exited before signaling"))?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(Self { ring, running })
    }

    /// Get a reference to the capture ring buffer for direct polling.
    pub fn ring(&self) -> &Arc<AudioRing> {
        &self.ring
    }

    /// Stop capturing.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for AudioCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// AudioPlayback
// ---------------------------------------------------------------------------

/// Plays PCM through the default output device, reading from a lock-free ring buffer.
///
/// The cpal stream lives on a dedicated OS thread; this handle is `Send + Sync`.
pub struct AudioPlayback {
    ring: Arc<AudioRing>,
    running: Arc<AtomicBool>,
}

impl AudioPlayback {
    /// Create and start playback on the default output device at 48 kHz mono.
    pub fn start() -> Result<Self, anyhow::Error> {
        let ring = Arc::new(AudioRing::new());
        let running = Arc::new(AtomicBool::new(true));

        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

        let ring_cb = ring.clone();
        let running_clone = running.clone();

        std::thread::Builder::new()
            .name("wzp-audio-playback".into())
            .spawn(move || {
                let result = (|| -> Result<(), anyhow::Error> {
                    let host = cpal::default_host();
                    let device = host
                        .default_output_device()
                        .ok_or_else(|| anyhow!("no default output audio device found"))?;

                    info!(device = %device.name().unwrap_or_default(), "using output device");

                    let config = StreamConfig {
                        channels: 1,
                        sample_rate: SampleRate(48_000),
                        buffer_size: cpal::BufferSize::Fixed(FRAME_SAMPLES as u32),
                    };

                    let use_f32 = !supports_i16_output(&device)?;

                    let err_cb = |e: cpal::StreamError| {
                        warn!("output stream error: {e}");
                    };

                    let stream = if use_f32 {
                        let ring = ring_cb.clone();
                        device.build_output_stream(
                            &config,
                            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                let mut tmp = [0i16; FRAME_SAMPLES];
                                for chunk in data.chunks_mut(FRAME_SAMPLES) {
                                    let n = chunk.len();
                                    let read = ring.read(&mut tmp[..n]);
                                    for i in 0..read {
                                        chunk[i] = i16_to_f32(tmp[i]);
                                    }
                                    // Fill remainder with silence if ring underran
                                    for i in read..n {
                                        chunk[i] = 0.0;
                                    }
                                }
                            },
                            err_cb,
                            None,
                        )?
                    } else {
                        let ring = ring_cb.clone();
                        device.build_output_stream(
                            &config,
                            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                                let read = ring.read(data);
                                // Fill remainder with silence if ring underran
                                for sample in &mut data[read..] {
                                    *sample = 0;
                                }
                            },
                            err_cb,
                            None,
                        )?
                    };

                    stream.play().context("failed to start output stream")?;

                    let _ = init_tx.send(Ok(()));

                    // Keep stream alive until stopped.
                    while running_clone.load(Ordering::Relaxed) {
                        std::thread::park_timeout(std::time::Duration::from_millis(200));
                    }
                    drop(stream);
                    Ok(())
                })();

                if let Err(e) = result {
                    let _ = init_tx.send(Err(e.to_string()));
                }
            })?;

        init_rx
            .recv()
            .map_err(|_| anyhow!("playback thread exited before signaling"))?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(Self { ring, running })
    }

    /// Get a reference to the playout ring buffer for direct writing.
    pub fn ring(&self) -> &Arc<AudioRing> {
        &self.ring
    }

    /// Stop playback.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for AudioPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

fn supports_i16_input(device: &cpal::Device) -> Result<bool, anyhow::Error> {
    let supported = device
        .supported_input_configs()
        .context("failed to query input configs")?;
    for cfg in supported {
        if cfg.sample_format() == SampleFormat::I16
            && cfg.min_sample_rate() <= SampleRate(48_000)
            && cfg.max_sample_rate() >= SampleRate(48_000)
            && cfg.channels() >= 1
        {
            return Ok(true);
        }
    }
    Ok(false)
}

fn supports_i16_output(device: &cpal::Device) -> Result<bool, anyhow::Error> {
    let supported = device
        .supported_output_configs()
        .context("failed to query output configs")?;
    for cfg in supported {
        if cfg.sample_format() == SampleFormat::I16
            && cfg.min_sample_rate() <= SampleRate(48_000)
            && cfg.max_sample_rate() >= SampleRate(48_000)
            && cfg.channels() >= 1
        {
            return Ok(true);
        }
    }
    Ok(false)
}

#[inline]
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * i16::MAX as f32) as i16
}

#[inline]
fn i16_to_f32(s: i16) -> f32 {
    s as f32 / i16::MAX as f32
}
