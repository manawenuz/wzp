//! macOS Voice Processing I/O — uses Apple's VoiceProcessingIO audio unit
//! for hardware-accelerated echo cancellation, AGC, and noise suppression.
//!
//! VoiceProcessingIO is a combined input+output unit that knows what's going
//! to the speaker, so it can cancel the echo from the mic signal internally.
//! This is the same engine FaceTime and other Apple apps use.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

use anyhow::Context;
use coreaudio::audio_unit::audio_format::LinearPcmFlags;
use coreaudio::audio_unit::render_callback::{self, data};
use coreaudio::audio_unit::{AudioUnit, Element, IOType, SampleFormat, Scope, StreamFormat};
use coreaudio::sys;
use tracing::info;

use crate::audio_ring::AudioRing;

/// Number of samples per 20 ms frame at 48 kHz mono.
pub const FRAME_SAMPLES: usize = 960;

/// Combined capture + playback via macOS VoiceProcessingIO.
///
/// The OS handles AEC internally — no manual far-end feeding needed.
pub struct VpioAudio {
    capture_ring: Arc<AudioRing>,
    playout_ring: Arc<AudioRing>,
    _audio_unit: AudioUnit,
    running: Arc<AtomicBool>,
    stats: Arc<VpioStats>,
}

/// Render/capture counters for diagnosing macOS VoiceProcessingIO.
///
/// These are atomics because CoreAudio callbacks run on realtime audio
/// threads. The Tauri engine polls snapshots from a normal async task and
/// emits them to the call debug log.
#[derive(Default)]
pub struct VpioStats {
    capture_callbacks: AtomicU64,
    capture_samples: AtomicU64,
    render_callbacks: AtomicU64,
    render_requested_samples: AtomicU64,
    render_read_samples: AtomicU64,
    render_underrun_callbacks: AtomicU64,
    render_nonzero_callbacks: AtomicU64,
    render_last_requested: AtomicU64,
    render_last_read: AtomicU64,
    render_last_rms: AtomicU64,
    render_last_ring_available: AtomicU64,
}

#[derive(Clone, Copy, Debug)]
pub struct VpioStatsSnapshot {
    pub capture_callbacks: u64,
    pub capture_samples: u64,
    pub render_callbacks: u64,
    pub render_requested_samples: u64,
    pub render_read_samples: u64,
    pub render_underrun_callbacks: u64,
    pub render_nonzero_callbacks: u64,
    pub render_last_requested: u64,
    pub render_last_read: u64,
    pub render_last_rms: u64,
    pub render_last_ring_available: u64,
}

impl VpioStats {
    pub fn snapshot(&self) -> VpioStatsSnapshot {
        VpioStatsSnapshot {
            capture_callbacks: self.capture_callbacks.load(Ordering::Relaxed),
            capture_samples: self.capture_samples.load(Ordering::Relaxed),
            render_callbacks: self.render_callbacks.load(Ordering::Relaxed),
            render_requested_samples: self.render_requested_samples.load(Ordering::Relaxed),
            render_read_samples: self.render_read_samples.load(Ordering::Relaxed),
            render_underrun_callbacks: self.render_underrun_callbacks.load(Ordering::Relaxed),
            render_nonzero_callbacks: self.render_nonzero_callbacks.load(Ordering::Relaxed),
            render_last_requested: self.render_last_requested.load(Ordering::Relaxed),
            render_last_read: self.render_last_read.load(Ordering::Relaxed),
            render_last_rms: self.render_last_rms.load(Ordering::Relaxed),
            render_last_ring_available: self.render_last_ring_available.load(Ordering::Relaxed),
        }
    }
}

impl VpioAudio {
    /// Start VoiceProcessingIO with AEC enabled.
    pub fn start() -> Result<Self, anyhow::Error> {
        let capture_ring = Arc::new(AudioRing::new());
        let playout_ring = Arc::new(AudioRing::new());
        let running = Arc::new(AtomicBool::new(true));
        let stats = Arc::new(VpioStats::default());

        let mut au = AudioUnit::new(IOType::VoiceProcessingIO)
            .context("failed to create VoiceProcessingIO audio unit")?;

        // Must uninitialize before configuring properties.
        au.uninitialize()
            .context("failed to uninitialize VPIO for configuration")?;

        // Enable input (mic) on Element::Input (bus 1).
        let enable: u32 = 1;
        au.set_property(
            sys::kAudioOutputUnitProperty_EnableIO,
            Scope::Input,
            Element::Input,
            Some(&enable),
        )
        .context("failed to enable VPIO input")?;

        // Output (speaker) is enabled by default on VPIO, but be explicit.
        au.set_property(
            sys::kAudioOutputUnitProperty_EnableIO,
            Scope::Output,
            Element::Output,
            Some(&enable),
        )
        .context("failed to enable VPIO output")?;

        // Configure stream format: 48kHz mono f32 non-interleaved
        let stream_format = StreamFormat {
            sample_rate: 48_000.0,
            sample_format: SampleFormat::F32,
            flags: LinearPcmFlags::IS_FLOAT
                | LinearPcmFlags::IS_PACKED
                | LinearPcmFlags::IS_NON_INTERLEAVED,
            channels: 1,
        };

        let asbd = stream_format.to_asbd();

        // Input: set format on Output scope of Input element
        // (= the format the AU delivers to us from the mic)
        au.set_property(
            sys::kAudioUnitProperty_StreamFormat,
            Scope::Output,
            Element::Input,
            Some(&asbd),
        )
        .context("failed to set input stream format")?;

        // Output: set format on Input scope of Output element
        // (= the format we feed to the AU for the speaker)
        au.set_property(
            sys::kAudioUnitProperty_StreamFormat,
            Scope::Input,
            Element::Output,
            Some(&asbd),
        )
        .context("failed to set output stream format")?;

        // Set up input callback (mic capture with AEC applied)
        let cap_ring = capture_ring.clone();
        let cap_running = running.clone();
        let cap_stats = stats.clone();
        let logged = Arc::new(AtomicBool::new(false));
        au.set_input_callback(
            move |args: render_callback::Args<data::NonInterleaved<f32>>| {
                if !cap_running.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let mut buffers = args.data.channels();
                if let Some(ch) = buffers.next() {
                    cap_stats.capture_callbacks.fetch_add(1, Ordering::Relaxed);
                    cap_stats
                        .capture_samples
                        .fetch_add(ch.len() as u64, Ordering::Relaxed);
                    if !logged.swap(true, Ordering::Relaxed) {
                        eprintln!("[vpio] capture callback: {} f32 samples", ch.len());
                    }
                    let mut tmp = [0i16; FRAME_SAMPLES];
                    for chunk in ch.chunks(FRAME_SAMPLES) {
                        let n = chunk.len();
                        for i in 0..n {
                            tmp[i] = (chunk[i].clamp(-1.0, 1.0) * i16::MAX as f32) as i16;
                        }
                        cap_ring.write(&tmp[..n]);
                    }
                }
                Ok(())
            },
        )
        .context("failed to set input callback")?;

        // Set up output callback (speaker playback — AEC uses this as reference)
        let play_ring = playout_ring.clone();
        let render_stats = stats.clone();
        let logged_render = Arc::new(AtomicBool::new(false));
        au.set_render_callback(
            move |mut args: render_callback::Args<data::NonInterleaved<f32>>| {
                let mut buffers = args.data.channels_mut();
                if let Some(ch) = buffers.next() {
                    render_stats
                        .render_callbacks
                        .fetch_add(1, Ordering::Relaxed);
                    render_stats
                        .render_requested_samples
                        .fetch_add(ch.len() as u64, Ordering::Relaxed);
                    render_stats
                        .render_last_requested
                        .store(ch.len() as u64, Ordering::Relaxed);
                    let mut tmp = [0i16; FRAME_SAMPLES];
                    let mut total_read = 0usize;
                    let mut sum_sq = 0u64;
                    let ring_available = play_ring.available();
                    for chunk in ch.chunks_mut(FRAME_SAMPLES) {
                        let n = chunk.len();
                        let read = play_ring.read(&mut tmp[..n]);
                        total_read += read;
                        for i in 0..read {
                            let s = tmp[i] as i64;
                            sum_sq = sum_sq.saturating_add((s * s) as u64);
                            chunk[i] = tmp[i] as f32 / i16::MAX as f32;
                        }
                        for i in read..n {
                            chunk[i] = 0.0;
                        }
                    }
                    render_stats
                        .render_read_samples
                        .fetch_add(total_read as u64, Ordering::Relaxed);
                    render_stats
                        .render_last_read
                        .store(total_read as u64, Ordering::Relaxed);
                    render_stats
                        .render_last_ring_available
                        .store(ring_available as u64, Ordering::Relaxed);
                    if total_read == 0 {
                        render_stats
                            .render_underrun_callbacks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    let rms = if total_read > 0 {
                        ((sum_sq as f64 / total_read as f64).sqrt()) as u64
                    } else {
                        0
                    };
                    render_stats.render_last_rms.store(rms, Ordering::Relaxed);
                    if rms > 0 {
                        render_stats
                            .render_nonzero_callbacks
                            .fetch_add(1, Ordering::Relaxed);
                    }
                    if !logged_render.swap(true, Ordering::Relaxed) {
                        eprintln!(
                            "[vpio] render callback: {} f32 samples, ring_available={}, ring_read={}, rms={}",
                            ch.len(),
                            ring_available,
                            total_read,
                            rms
                        );
                    }
                }
                Ok(())
            },
        )
        .context("failed to set render callback")?;

        au.initialize()
            .context("failed to initialize VoiceProcessingIO")?;
        au.start().context("failed to start VoiceProcessingIO")?;

        info!("VoiceProcessingIO started (OS-level AEC enabled)");

        Ok(Self {
            capture_ring,
            playout_ring,
            _audio_unit: au,
            running,
            stats,
        })
    }

    pub fn capture_ring(&self) -> &Arc<AudioRing> {
        &self.capture_ring
    }

    pub fn playout_ring(&self) -> &Arc<AudioRing> {
        &self.playout_ring
    }

    pub fn stats(&self) -> Arc<VpioStats> {
        self.stats.clone()
    }

    pub fn stop(&self) {
        self.running.store(false, Ordering::Relaxed);
    }
}

impl Drop for VpioAudio {
    fn drop(&mut self) {
        self.stop();
    }
}
