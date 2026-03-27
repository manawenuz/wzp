//! Real audio I/O via `cpal` — microphone capture and speaker playback.
//!
//! Both structs use 48 kHz, mono, i16 format to match the WarzonePhone codec
//! pipeline. Frames are 960 samples (20 ms at 48 kHz).
//!
//! The cpal `Stream` type is not `Send`, so each struct spawns a dedicated OS
//! thread that owns the stream. The public API exposes only `Send + Sync`
//! channel handles.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;
use std::sync::Arc;

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use tracing::{info, warn};

/// Number of samples per 20 ms frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;

// ---------------------------------------------------------------------------
// AudioCapture
// ---------------------------------------------------------------------------

/// Captures microphone input and yields 960-sample PCM frames.
///
/// The cpal stream lives on a dedicated OS thread; this handle is `Send + Sync`.
pub struct AudioCapture {
    rx: mpsc::Receiver<Vec<i16>>,
    running: Arc<AtomicBool>,
}

impl AudioCapture {
    /// Create and start capturing from the default input device at 48 kHz mono.
    pub fn start() -> Result<Self, anyhow::Error> {
        let (tx, rx) = mpsc::sync_channel::<Vec<i16>>(64);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

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
                        buffer_size: cpal::BufferSize::Default,
                    };

                    let use_f32 = !supports_i16_input(&device)?;

                    let buf = Arc::new(std::sync::Mutex::new(
                        Vec::<i16>::with_capacity(FRAME_SAMPLES),
                    ));
                    let err_cb = |e: cpal::StreamError| {
                        warn!("input stream error: {e}");
                    };

                    let stream = if use_f32 {
                        let buf = buf.clone();
                        let tx = tx.clone();
                        let running = running_clone.clone();
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                                if !running.load(Ordering::Relaxed) {
                                    return;
                                }
                                let mut lock = buf.lock().unwrap();
                                for &s in data {
                                    lock.push(f32_to_i16(s));
                                    if lock.len() == FRAME_SAMPLES {
                                        let frame = lock.drain(..).collect();
                                        let _ = tx.try_send(frame);
                                    }
                                }
                            },
                            err_cb,
                            None,
                        )?
                    } else {
                        let buf = buf.clone();
                        let tx = tx.clone();
                        let running = running_clone.clone();
                        device.build_input_stream(
                            &config,
                            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                                if !running.load(Ordering::Relaxed) {
                                    return;
                                }
                                let mut lock = buf.lock().unwrap();
                                for &s in data {
                                    lock.push(s);
                                    if lock.len() == FRAME_SAMPLES {
                                        let frame = lock.drain(..).collect();
                                        let _ = tx.try_send(frame);
                                    }
                                }
                            },
                            err_cb,
                            None,
                        )?
                    };

                    stream.play().context("failed to start input stream")?;

                    // Signal success to the caller before parking.
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

        Ok(Self { rx, running })
    }

    /// Read the next frame of 960 PCM samples (blocking until available).
    ///
    /// Returns `None` when the stream has been stopped or the channel is
    /// disconnected.
    pub fn read_frame(&self) -> Option<Vec<i16>> {
        self.rx.recv().ok()
    }

    /// Stop capturing.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// AudioPlayback
// ---------------------------------------------------------------------------

/// Plays PCM frames through the default output device at 48 kHz mono.
///
/// The cpal stream lives on a dedicated OS thread; this handle is `Send + Sync`.
pub struct AudioPlayback {
    tx: mpsc::SyncSender<Vec<i16>>,
    running: Arc<AtomicBool>,
}

impl AudioPlayback {
    /// Create and start playback on the default output device at 48 kHz mono.
    pub fn start() -> Result<Self, anyhow::Error> {
        let (tx, rx) = mpsc::sync_channel::<Vec<i16>>(64);
        let running = Arc::new(AtomicBool::new(true));
        let running_clone = running.clone();

        let (init_tx, init_rx) = mpsc::sync_channel::<Result<(), String>>(1);

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
                        buffer_size: cpal::BufferSize::Default,
                    };

                    let use_f32 = !supports_i16_output(&device)?;

                    // Shared ring of samples the cpal callback drains from.
                    let ring = Arc::new(std::sync::Mutex::new(
                        std::collections::VecDeque::<i16>::with_capacity(FRAME_SAMPLES * 8),
                    ));

                    // Background drainer: moves frames from the mpsc channel into the ring.
                    {
                        let ring = ring.clone();
                        let running = running_clone.clone();
                        std::thread::Builder::new()
                            .name("wzp-playback-drain".into())
                            .spawn(move || {
                                while running.load(Ordering::Relaxed) {
                                    match rx.recv_timeout(std::time::Duration::from_millis(100)) {
                                        Ok(frame) => {
                                            let mut lock = ring.lock().unwrap();
                                            lock.extend(frame);
                                            while lock.len() > FRAME_SAMPLES * 16 {
                                                lock.pop_front();
                                            }
                                        }
                                        Err(mpsc::RecvTimeoutError::Timeout) => {}
                                        Err(mpsc::RecvTimeoutError::Disconnected) => break,
                                    }
                                }
                            })?;
                    }

                    let err_cb = |e: cpal::StreamError| {
                        warn!("output stream error: {e}");
                    };

                    let stream = if use_f32 {
                        let ring = ring.clone();
                        device.build_output_stream(
                            &config,
                            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                let mut lock = ring.lock().unwrap();
                                for sample in data.iter_mut() {
                                    *sample = match lock.pop_front() {
                                        Some(s) => i16_to_f32(s),
                                        None => 0.0,
                                    };
                                }
                            },
                            err_cb,
                            None,
                        )?
                    } else {
                        let ring = ring.clone();
                        device.build_output_stream(
                            &config,
                            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                                let mut lock = ring.lock().unwrap();
                                for sample in data.iter_mut() {
                                    *sample = lock.pop_front().unwrap_or(0);
                                }
                            },
                            err_cb,
                            None,
                        )?
                    };

                    stream.play().context("failed to start output stream")?;

                    // Signal success to the caller before parking.
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

        Ok(Self { tx, running })
    }

    /// Write a frame of PCM samples for playback.
    pub fn write_frame(&self, pcm: &[i16]) {
        let _ = self.tx.try_send(pcm.to_vec());
    }

    /// Stop playback.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Check if the input device supports i16 at 48 kHz mono.
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

/// Check if the output device supports i16 at 48 kHz mono.
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
