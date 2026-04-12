//! Linux AEC backend: CPAL capture + playback wired through the WebRTC Audio
//! Processing Module (AEC3 + noise suppression + high-pass filter).
//!
//! This is the same algorithm used by Chrome WebRTC, Zoom, Teams, Jitsi, and
//! any other "serious" Linux VoIP app. It runs in-process — no dependency on
//! PulseAudio's module-echo-cancel or PipeWire's filter-chain, so it works
//! identically on ALSA / PulseAudio / PipeWire systems.
//!
//! ## Architecture
//!
//! A single module-level `Arc<Mutex<Processor>>` is shared between the
//! capture and playback paths. On each 20 ms frame (960 samples @ 48 kHz
//! mono):
//!
//! - **Playback path**: `LinuxAecPlayback::start` spawns the usual CPAL
//!   output thread, but wraps each chunk in a call to
//!   `Processor::process_render_frame` **before** handing it to CPAL. That
//!   gives APM an authoritative reference of exactly what's going out to
//!   the speakers (same approach Zoom/Teams/Jitsi use). The AEC then knows
//!   what to cancel when it sees echo in the capture stream.
//!
//! - **Capture path**: `LinuxAecCapture::start` spawns the usual CPAL
//!   input thread, and runs `Processor::process_capture_frame` on each
//!   incoming mic chunk **in place** before pushing it into the ring
//!   buffer. The AEC subtracts the echo using the render reference it
//!   saw on the playback side.
//!
//! APM is strict about frame size: it requires exactly 10 ms = 480 samples
//! per call at 48 kHz. Our pipeline uses 20 ms = 960 samples, so each 20 ms
//! frame is split into two 480-sample halves, APM is called twice, and the
//! halves are stitched back together.
//!
//! APM only accepts f32 samples in `[-1.0, 1.0]`, so we convert i16 → f32
//! before the call and f32 → i16 after (with clamping on the return path).
//!
//! ## Stream delay
//!
//! AEC needs to know roughly how long it takes between a sample being passed
//! to `process_render_frame` and its echo showing up at `process_capture_frame`
//! — i.e. the round trip through CPAL playback → speaker → air → microphone
//! → CPAL capture. AEC3's internal estimator tracks this within a window
//! around whatever hint we give it. We hardcode 60 ms as a reasonable
//! starting point for typical Linux audio stacks; the delay estimator does
//! the fine-tuning automatically.
//!
//! ## Thread safety
//!
//! The 0.3.x line of `webrtc-audio-processing` takes `&mut self` on both
//! `process_capture_frame` and `process_render_frame`, so the `Processor`
//! needs a `Mutex` around it for cross-thread sharing. The capture and
//! playback threads each acquire the lock briefly (sub-millisecond per
//! 10 ms frame) so contention is minimal at our frame rates.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex, OnceLock};

use anyhow::{anyhow, Context};
use cpal::traits::{DeviceTrait, HostTrait, StreamTrait};
use cpal::{SampleFormat, SampleRate, StreamConfig};
use tracing::{info, warn};
use webrtc_audio_processing::{
    Config, EchoCancellation, EchoCancellationSuppressionLevel, InitializationConfig,
    NoiseSuppression, NoiseSuppressionLevel, Processor, NUM_SAMPLES_PER_FRAME,
};

use crate::audio_ring::AudioRing;

/// 20 ms at 48 kHz, mono — matches the rest of the pipeline and the codec.
pub const FRAME_SAMPLES: usize = 960;
/// APM requires strict 10 ms frames at 48 kHz = 480 samples per call.
/// Imported from the webrtc-audio-processing crate so we can't drift out
/// of sync with whatever sample rate / frame length the C++ lib is using.
const APM_FRAME_SAMPLES: usize = NUM_SAMPLES_PER_FRAME as usize;
const APM_NUM_CHANNELS: usize = 1;
/// Round-trip delay hint passed to APM; the estimator refines from here.
/// 60 ms is a reasonable default for CPAL on ALSA / PulseAudio / PipeWire.
#[allow(dead_code)]
const STREAM_DELAY_MS: i32 = 60;

// ---------------------------------------------------------------------------
// Shared APM instance
// ---------------------------------------------------------------------------

/// Module-level lazily-initialized APM. Shared between capture and playback
/// so they operate on the same echo-cancellation state — the render frames
/// pushed by playback are what the capture path subtracts from the mic input.
/// Wrapped in a Mutex because the 0.3.x Processor takes `&mut self` on both
/// process_capture_frame and process_render_frame.
static PROCESSOR: OnceLock<Arc<Mutex<Processor>>> = OnceLock::new();

fn get_or_init_processor() -> anyhow::Result<Arc<Mutex<Processor>>> {
    if let Some(p) = PROCESSOR.get() {
        return Ok(p.clone());
    }
    let init_config = InitializationConfig {
        num_capture_channels: APM_NUM_CHANNELS as i32,
        num_render_channels: APM_NUM_CHANNELS as i32,
        ..Default::default()
    };
    let mut processor = Processor::new(&init_config)
        .map_err(|e| anyhow!("webrtc APM init failed: {e:?}"))?;

    let config = Config {
        echo_cancellation: Some(EchoCancellation {
            suppression_level: EchoCancellationSuppressionLevel::High,
            stream_delay_ms: Some(STREAM_DELAY_MS),
            enable_delay_agnostic: true,
            enable_extended_filter: true,
        }),
        noise_suppression: Some(NoiseSuppression {
            suppression_level: NoiseSuppressionLevel::High,
        }),
        enable_high_pass_filter: true,
        // AGC left off for now — it can fight the Opus encoder's own gain
        // staging and the adaptive-quality controller. Add later if users
        // report low mic levels.
        ..Default::default()
    };
    processor.set_config(config);

    let arc = Arc::new(Mutex::new(processor));
    let _ = PROCESSOR.set(arc.clone());
    info!(
        stream_delay_ms = STREAM_DELAY_MS,
        "webrtc APM initialized (AEC High + NS High + HPF, AGC off)"
    );
    Ok(arc)
}

// ---------------------------------------------------------------------------
// Helpers: i16 ↔ f32 and APM frame processing
// ---------------------------------------------------------------------------

#[inline]
fn i16_to_f32(s: i16) -> f32 {
    s as f32 / 32768.0
}

#[inline]
fn f32_to_i16(s: f32) -> i16 {
    (s.clamp(-1.0, 1.0) * 32767.0) as i16
}

/// Feed a 20 ms (960-sample) playback frame to APM as the render reference.
/// Splits into two 10 ms halves because APM is strict about frame size.
/// Takes the Mutex-wrapped Processor and locks briefly around each call.
fn push_render_frame_20ms(apm: &Mutex<Processor>, pcm: &[i16]) {
    debug_assert_eq!(pcm.len(), FRAME_SAMPLES);
    let mut buf = [0f32; APM_FRAME_SAMPLES];
    for half in pcm.chunks_exact(APM_FRAME_SAMPLES) {
        for (i, &s) in half.iter().enumerate() {
            buf[i] = i16_to_f32(s);
        }
        match apm.lock() {
            Ok(mut p) => {
                if let Err(e) = p.process_render_frame(&mut buf) {
                    warn!("webrtc APM process_render_frame failed: {e:?}");
                }
            }
            Err(_) => {
                warn!("webrtc APM mutex poisoned in render path");
                return;
            }
        }
    }
}

/// Run a 20 ms (960-sample) capture frame through APM's echo cancellation
/// in place. Splits into two 10 ms halves, runs APM on each, stitches
/// results back into the caller's buffer. Briefly holds the Mutex once
/// per 10 ms half.
fn process_capture_frame_20ms(apm: &Mutex<Processor>, pcm: &mut [i16]) {
    debug_assert_eq!(pcm.len(), FRAME_SAMPLES);
    let mut buf = [0f32; APM_FRAME_SAMPLES];
    for half in pcm.chunks_exact_mut(APM_FRAME_SAMPLES) {
        for (i, &s) in half.iter().enumerate() {
            buf[i] = i16_to_f32(s);
        }
        match apm.lock() {
            Ok(mut p) => {
                if let Err(e) = p.process_capture_frame(&mut buf) {
                    warn!("webrtc APM process_capture_frame failed: {e:?}");
                }
            }
            Err(_) => {
                warn!("webrtc APM mutex poisoned in capture path");
                return;
            }
        }
        for (i, d) in half.iter_mut().enumerate() {
            *d = f32_to_i16(buf[i]);
        }
    }
}

// ---------------------------------------------------------------------------
// LinuxAecCapture — CPAL mic + WebRTC AEC capture-side processing
// ---------------------------------------------------------------------------

/// Microphone capture with WebRTC AEC3 applied in place before the codec
/// sees the samples. Mirrors the public API of `audio_io::AudioCapture` so
/// downstream code doesn't change.
pub struct LinuxAecCapture {
    ring: Arc<AudioRing>,
    running: Arc<AtomicBool>,
}

impl LinuxAecCapture {
    pub fn start() -> Result<Self, anyhow::Error> {
        // Eagerly init the APM so the playback side can find it already
        // configured, and so init errors surface on the caller thread
        // instead of silently failing inside the capture thread.
        let apm = get_or_init_processor()?;

        let ring = Arc::new(AudioRing::new());
        let running = Arc::new(AtomicBool::new(true));

        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

        let ring_cb = ring.clone();
        let running_clone = running.clone();
        let apm_capture = apm.clone();

        std::thread::Builder::new()
            .name("wzp-audio-capture-linuxaec".into())
            .spawn(move || {
                let result = (|| -> Result<(), anyhow::Error> {
                    let host = cpal::default_host();
                    let device = host
                        .default_input_device()
                        .ok_or_else(|| anyhow!("no default input audio device found"))?;
                    info!(device = %device.name().unwrap_or_default(), "LinuxAEC: using input device");

                    let config = StreamConfig {
                        channels: 1,
                        sample_rate: SampleRate(48_000),
                        buffer_size: cpal::BufferSize::Default,
                    };

                    let use_f32 = !supports_i16_input(&device)?;

                    let err_cb = |e: cpal::StreamError| {
                        warn!("LinuxAEC input stream error: {e}");
                    };

                    // Leftover buffer for when CPAL gives us partial frames.
                    // We need exactly 960-sample chunks to feed APM.
                    let leftover = std::sync::Mutex::new(Vec::<i16>::with_capacity(FRAME_SAMPLES * 4));

                    let stream = if use_f32 {
                        let ring = ring_cb.clone();
                        let running = running_clone.clone();
                        let apm = apm_capture.clone();
                        device.build_input_stream(
                            &config,
                            move |data: &[f32], _: &cpal::InputCallbackInfo| {
                                if !running.load(Ordering::Relaxed) {
                                    return;
                                }
                                let mut lv = leftover.lock().unwrap();
                                lv.reserve(data.len());
                                for &s in data {
                                    lv.push(f32_to_i16(s));
                                }
                                drain_frames_through_apm(&mut lv, &apm, &ring);
                            },
                            err_cb,
                            None,
                        )?
                    } else {
                        let ring = ring_cb.clone();
                        let running = running_clone.clone();
                        let apm = apm_capture.clone();
                        device.build_input_stream(
                            &config,
                            move |data: &[i16], _: &cpal::InputCallbackInfo| {
                                if !running.load(Ordering::Relaxed) {
                                    return;
                                }
                                let mut lv = leftover.lock().unwrap();
                                lv.extend_from_slice(data);
                                drain_frames_through_apm(&mut lv, &apm, &ring);
                            },
                            err_cb,
                            None,
                        )?
                    };

                    stream.play().context("failed to start LinuxAEC input stream")?;
                    let _ = init_tx.send(Ok(()));
                    info!("LinuxAEC capture started (AEC3 active)");

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
            .map_err(|_| anyhow!("LinuxAEC capture thread exited before signaling"))?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(Self { ring, running })
    }

    pub fn ring(&self) -> &Arc<AudioRing> {
        &self.ring
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for LinuxAecCapture {
    fn drop(&mut self) {
        self.stop();
    }
}

/// Pull whole 960-sample frames out of the leftover buffer, run them through
/// APM's capture-side processing, and push to the ring. Leaves any partial
/// sub-960 remainder in `leftover` for the next callback.
fn drain_frames_through_apm(leftover: &mut Vec<i16>, apm: &Mutex<Processor>, ring: &AudioRing) {
    let mut frame = [0i16; FRAME_SAMPLES];
    while leftover.len() >= FRAME_SAMPLES {
        frame.copy_from_slice(&leftover[..FRAME_SAMPLES]);
        process_capture_frame_20ms(apm, &mut frame);
        ring.write(&frame);
        leftover.drain(..FRAME_SAMPLES);
    }
}

// ---------------------------------------------------------------------------
// LinuxAecPlayback — CPAL speaker output + WebRTC AEC render-side tee
// ---------------------------------------------------------------------------

/// Speaker playback with a render-side tee: each frame written to CPAL is
/// ALSO fed to APM via `process_render_frame` as the echo-cancellation
/// reference signal. This is the "tee the playback ring" approach (Zoom,
/// Teams, Jitsi) — deterministic, does not depend on PulseAudio loopback or
/// PipeWire monitor sources.
pub struct LinuxAecPlayback {
    ring: Arc<AudioRing>,
    running: Arc<AtomicBool>,
}

impl LinuxAecPlayback {
    pub fn start() -> Result<Self, anyhow::Error> {
        let apm = get_or_init_processor()?;

        let ring = Arc::new(AudioRing::new());
        let running = Arc::new(AtomicBool::new(true));

        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);

        let ring_cb = ring.clone();
        let running_clone = running.clone();
        let apm_render = apm.clone();

        std::thread::Builder::new()
            .name("wzp-audio-playback-linuxaec".into())
            .spawn(move || {
                let result = (|| -> Result<(), anyhow::Error> {
                    let host = cpal::default_host();
                    let device = host
                        .default_output_device()
                        .ok_or_else(|| anyhow!("no default output audio device found"))?;
                    info!(device = %device.name().unwrap_or_default(), "LinuxAEC: using output device");

                    let config = StreamConfig {
                        channels: 1,
                        sample_rate: SampleRate(48_000),
                        buffer_size: cpal::BufferSize::Default,
                    };

                    let use_f32 = !supports_i16_output(&device)?;

                    let err_cb = |e: cpal::StreamError| {
                        warn!("LinuxAEC output stream error: {e}");
                    };

                    // Same 960-sample batching approach as the capture side:
                    // CPAL may ask for N samples in a callback where N doesn't
                    // divide 960. We accumulate partial frames in a Vec and
                    // feed APM as soon as we have a whole 20 ms frame.
                    let carry = std::sync::Mutex::new(Vec::<i16>::with_capacity(FRAME_SAMPLES * 4));

                    let stream = if use_f32 {
                        let ring = ring_cb.clone();
                        let apm = apm_render.clone();
                        device.build_output_stream(
                            &config,
                            move |data: &mut [f32], _: &cpal::OutputCallbackInfo| {
                                fill_output_and_tee_f32(data, &ring, &apm, &carry);
                            },
                            err_cb,
                            None,
                        )?
                    } else {
                        let ring = ring_cb.clone();
                        let apm = apm_render.clone();
                        device.build_output_stream(
                            &config,
                            move |data: &mut [i16], _: &cpal::OutputCallbackInfo| {
                                fill_output_and_tee_i16(data, &ring, &apm, &carry);
                            },
                            err_cb,
                            None,
                        )?
                    };

                    stream.play().context("failed to start LinuxAEC output stream")?;
                    let _ = init_tx.send(Ok(()));
                    info!("LinuxAEC playback started (render tee active)");

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
            .map_err(|_| anyhow!("LinuxAEC playback thread exited before signaling"))?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(Self { ring, running })
    }

    pub fn ring(&self) -> &Arc<AudioRing> {
        &self.ring
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for LinuxAecPlayback {
    fn drop(&mut self) {
        self.stop();
    }
}

fn fill_output_and_tee_i16(
    data: &mut [i16],
    ring: &AudioRing,
    apm: &Mutex<Processor>,
    carry: &std::sync::Mutex<Vec<i16>>,
) {
    let read = ring.read(data);
    for s in &mut data[read..] {
        *s = 0;
    }
    tee_render_samples(data, apm, carry);
}

fn fill_output_and_tee_f32(
    data: &mut [f32],
    ring: &AudioRing,
    apm: &Mutex<Processor>,
    carry: &std::sync::Mutex<Vec<i16>>,
) {
    let mut tmp = vec![0i16; data.len()];
    let read = ring.read(&mut tmp);
    for s in &mut tmp[read..] {
        *s = 0;
    }
    for (d, &s) in data.iter_mut().zip(tmp.iter()) {
        *d = i16_to_f32(s);
    }
    tee_render_samples(&tmp, apm, carry);
}

/// Push CPAL-bound samples into APM's render-side input for echo cancellation.
/// Uses a carry buffer to batch into exact 960-sample (20 ms) frames.
fn tee_render_samples(samples: &[i16], apm: &Mutex<Processor>, carry: &std::sync::Mutex<Vec<i16>>) {
    let mut lv = carry.lock().unwrap();
    lv.extend_from_slice(samples);
    while lv.len() >= FRAME_SAMPLES {
        let mut frame = [0i16; FRAME_SAMPLES];
        frame.copy_from_slice(&lv[..FRAME_SAMPLES]);
        push_render_frame_20ms(apm, &frame);
        lv.drain(..FRAME_SAMPLES);
    }
}

// ---------------------------------------------------------------------------
// CPAL format helpers (duplicated from audio_io.rs to keep the modules
// independent — each backend file is a self-contained unit)
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
