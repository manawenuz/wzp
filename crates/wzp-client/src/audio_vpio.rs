//! macOS Voice Processing I/O — uses Apple's VoiceProcessingIO audio unit
//! for hardware-accelerated echo cancellation, AGC, and noise suppression.
//!
//! VoiceProcessingIO is a combined input+output unit that knows what's going
//! to the speaker, so it can cancel the echo from the mic signal internally.
//! This is the same engine FaceTime and other Apple apps use.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

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
}

impl VpioAudio {
    /// Start VoiceProcessingIO with AEC enabled.
    pub fn start() -> Result<Self, anyhow::Error> {
        let capture_ring = Arc::new(AudioRing::new());
        let playout_ring = Arc::new(AudioRing::new());
        let running = Arc::new(AtomicBool::new(true));

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
        let logged = Arc::new(AtomicBool::new(false));
        au.set_input_callback(
            move |args: render_callback::Args<data::NonInterleaved<f32>>| {
                if !cap_running.load(Ordering::Relaxed) {
                    return Ok(());
                }
                let mut buffers = args.data.channels();
                if let Some(ch) = buffers.next() {
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
        au.set_render_callback(
            move |mut args: render_callback::Args<data::NonInterleaved<f32>>| {
                let mut buffers = args.data.channels_mut();
                if let Some(ch) = buffers.next() {
                    let mut tmp = [0i16; FRAME_SAMPLES];
                    for chunk in ch.chunks_mut(FRAME_SAMPLES) {
                        let n = chunk.len();
                        let read = play_ring.read(&mut tmp[..n]);
                        for i in 0..read {
                            chunk[i] = tmp[i] as f32 / i16::MAX as f32;
                        }
                        for i in read..n {
                            chunk[i] = 0.0;
                        }
                    }
                }
                Ok(())
            },
        )
        .context("failed to set render callback")?;

        au.initialize().context("failed to initialize VoiceProcessingIO")?;
        au.start().context("failed to start VoiceProcessingIO")?;

        info!("VoiceProcessingIO started (OS-level AEC enabled)");

        Ok(Self {
            capture_ring,
            playout_ring,
            _audio_unit: au,
            running,
        })
    }

    pub fn capture_ring(&self) -> &Arc<AudioRing> {
        &self.capture_ring
    }

    pub fn playout_ring(&self) -> &Arc<AudioRing> {
        &self.playout_ring
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
