//! Direct WASAPI microphone capture with Windows's OS-level AEC enabled.
//!
//! Bypasses CPAL and opens the default capture endpoint directly via
//! `IMMDeviceEnumerator` + `IAudioClient2::SetClientProperties`, setting
//! `AudioClientProperties.eCategory = AudioCategory_Communications`. That's
//! the switch that tells Windows "this is a VoIP call" — the OS then
//! enables its communications audio processing chain (AEC, noise
//! suppression, automatic gain control) for the stream. AEC operates at
//! the OS level using the currently-playing audio as the reference
//! signal, so it cancels echo from our CPAL playback (and any other app's
//! audio) without us having to plumb a reference signal ourselves.
//!
//! Platform: Windows only, compiled only when the `windows-aec` feature
//! is enabled. Mirrors the public API of `audio_io::AudioCapture` so
//! `wzp-client`'s lib.rs can transparently re-export either one as
//! `AudioCapture`.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use anyhow::{anyhow, Context};
use tracing::{info, warn};
use windows::core::{Interface, GUID};
use windows::Win32::Foundation::{CloseHandle, BOOL, HANDLE, WAIT_OBJECT_0};
use windows::Win32::Media::Audio::{
    eCapture, eCommunications, AudioCategory_Communications, AudioClientProperties,
    IAudioCaptureClient, IAudioClient, IAudioClient2, IMMDeviceEnumerator, MMDeviceEnumerator,
    AUDCLNT_SHAREMODE_SHARED, AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM,
    AUDCLNT_STREAMFLAGS_EVENTCALLBACK, AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY, WAVEFORMATEX,
    WAVE_FORMAT_PCM,
};
use windows::Win32::System::Com::{
    CoCreateInstance, CoInitializeEx, CoUninitialize, CLSCTX_ALL, COINIT_MULTITHREADED,
};
use windows::Win32::System::Threading::{CreateEventW, WaitForSingleObject, INFINITE};

use crate::audio_ring::AudioRing;

/// 20 ms at 48 kHz, mono. Matches the rest of the audio pipeline.
pub const FRAME_SAMPLES: usize = 960;

/// Microphone capture via WASAPI with Windows's communications AEC enabled.
///
/// The WASAPI capture stream runs on a dedicated OS thread. This handle is
/// `Send + Sync`. Dropping it stops the stream and joins the thread.
pub struct WasapiAudioCapture {
    ring: Arc<AudioRing>,
    running: Arc<AtomicBool>,
    thread: Option<std::thread::JoinHandle<()>>,
}

impl WasapiAudioCapture {
    /// Open the default communications microphone, enable OS AEC, and start
    /// streaming PCM into a lock-free ring buffer.
    ///
    /// Returns only after the capture thread has successfully initialized
    /// the stream, or propagates the error back to the caller.
    pub fn start() -> Result<Self, anyhow::Error> {
        let ring = Arc::new(AudioRing::new());
        let running = Arc::new(AtomicBool::new(true));

        let (init_tx, init_rx) = std::sync::mpsc::sync_channel::<Result<(), String>>(1);
        let ring_cb = ring.clone();
        let running_cb = running.clone();

        let thread = std::thread::Builder::new()
            .name("wzp-audio-capture-wasapi".into())
            .spawn(move || {
                let result = unsafe { capture_thread_main(ring_cb, running_cb.clone(), &init_tx) };
                if let Err(e) = result {
                    warn!("wasapi capture thread exited with error: {e}");
                    // If we failed before signaling init, signal now so the
                    // caller unblocks. Double-send is harmless (channel is
                    // bounded to 1 and we only hit the second send path on
                    // late errors).
                    let _ = init_tx.send(Err(e.to_string()));
                }
            })
            .context("failed to spawn WASAPI capture thread")?;

        init_rx
            .recv()
            .map_err(|_| anyhow!("WASAPI capture thread exited before signaling init"))?
            .map_err(|e| anyhow!("{e}"))?;

        Ok(Self {
            ring,
            running,
            thread: Some(thread),
        })
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

impl Drop for WasapiAudioCapture {
    fn drop(&mut self) {
        self.stop();
        if let Some(handle) = self.thread.take() {
            // Join best-effort. The thread loop polls `running` every 200ms
            // via a short WaitForSingleObject timeout, so it should exit
            // within ~200ms of `stop()`.
            let _ = handle.join();
        }
    }
}

// ---------------------------------------------------------------------------
// WASAPI thread entry point — everything below this line runs on the
// dedicated wzp-audio-capture-wasapi thread.
// ---------------------------------------------------------------------------

unsafe fn capture_thread_main(
    ring: Arc<AudioRing>,
    running: Arc<AtomicBool>,
    init_tx: &std::sync::mpsc::SyncSender<Result<(), String>>,
) -> Result<(), anyhow::Error> {
    // COM init for the capture thread. MULTITHREADED because we're not
    // running a message pump. Must be balanced by CoUninitialize on exit.
    CoInitializeEx(None, COINIT_MULTITHREADED)
        .ok()
        .context("CoInitializeEx failed")?;

    // Use a guard struct so CoUninitialize runs even on early returns.
    struct ComGuard;
    impl Drop for ComGuard {
        fn drop(&mut self) {
            unsafe { CoUninitialize() };
        }
    }
    let _com_guard = ComGuard;

    let enumerator: IMMDeviceEnumerator =
        CoCreateInstance(&MMDeviceEnumerator, None, CLSCTX_ALL)
            .context("CoCreateInstance(MMDeviceEnumerator) failed")?;

    // eCommunications role (not eConsole) — this picks the device the user
    // has designated for communications in Sound Settings. It's the one
    // Windows's AEC is actually tuned for and the one Teams/Zoom use.
    let device = enumerator
        .GetDefaultAudioEndpoint(eCapture, eCommunications)
        .context("GetDefaultAudioEndpoint(eCapture, eCommunications) failed")?;

    if let Ok(name) = device_name(&device) {
        info!(device = %name, "opening WASAPI communications capture endpoint");
    }

    let audio_client: IAudioClient = device
        .Activate(CLSCTX_ALL, None)
        .context("IMMDevice::Activate(IAudioClient) failed")?;

    // IAudioClient2 exposes SetClientProperties, which is the ONLY way to
    // set AudioCategory_Communications pre-Initialize. Calling it on the
    // base IAudioClient would not compile, and setting it after Initialize
    // is a no-op.
    let audio_client2: IAudioClient2 = audio_client
        .cast()
        .context("QueryInterface IAudioClient2 failed")?;

    let mut props = AudioClientProperties {
        cbSize: std::mem::size_of::<AudioClientProperties>() as u32,
        bIsOffload: BOOL(0),
        eCategory: AudioCategory_Communications,
        // 0 = AUDCLNT_STREAMOPTIONS_NONE. The `windows` crate doesn't
        // export the enum constant in all versions, so use 0 directly.
        Options: Default::default(),
    };
    audio_client2
        .SetClientProperties(&mut props as *mut _)
        .context("SetClientProperties(AudioCategory_Communications) failed")?;

    // Request 48 kHz mono i16 directly. AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
    // tells Windows to do any needed format conversion inside the audio
    // engine rather than rejecting our format. SRC_DEFAULT_QUALITY picks
    // the standard Windows resampler quality (fine for voice).
    let wave_format = WAVEFORMATEX {
        wFormatTag: WAVE_FORMAT_PCM as u16,
        nChannels: 1,
        nSamplesPerSec: 48_000,
        nAvgBytesPerSec: 48_000 * 2, // 1 ch * 2 bytes/sample * 48000 Hz
        nBlockAlign: 2,              // 1 ch * 2 bytes/sample
        wBitsPerSample: 16,
        cbSize: 0,
    };

    // 1,000,000 hns = 100 ms buffer (hns = 100-nanosecond units). Windows
    // treats this as the minimum; the engine may give us a larger one.
    const BUFFER_DURATION_HNS: i64 = 1_000_000;

    audio_client
        .Initialize(
            AUDCLNT_SHAREMODE_SHARED,
            AUDCLNT_STREAMFLAGS_EVENTCALLBACK
                | AUDCLNT_STREAMFLAGS_AUTOCONVERTPCM
                | AUDCLNT_STREAMFLAGS_SRC_DEFAULT_QUALITY,
            BUFFER_DURATION_HNS,
            0,
            &wave_format,
            Some(&GUID::zeroed()),
        )
        .context("IAudioClient::Initialize failed — Windows rejected communications-mode 48k mono i16")?;

    // Event-driven capture: Windows signals this handle each time a new
    // audio packet is available. We wait on it from the loop below.
    let event = CreateEventW(None, false, false, None)
        .context("CreateEventW failed")?;
    audio_client
        .SetEventHandle(event)
        .context("SetEventHandle failed")?;

    let capture_client: IAudioCaptureClient = audio_client
        .GetService()
        .context("IAudioClient::GetService(IAudioCaptureClient) failed")?;

    audio_client.Start().context("IAudioClient::Start failed")?;

    // Signal to the parent thread that init succeeded before entering the
    // hot loop. From this point on, errors get logged but don't propagate
    // back to the caller (they'd just cause the ring buffer to stop
    // filling, which the main thread detects as underruns).
    let _ = init_tx.send(Ok(()));
    info!("WASAPI communications-mode capture started with OS AEC enabled");

    let mut logged_first_packet = false;

    // Main capture loop. Exit when `running` goes false (from Drop or an
    // explicit stop() call).
    while running.load(Ordering::Relaxed) {
        // 200 ms timeout so we check `running` regularly even if the audio
        // engine stops delivering packets (e.g. device unplugged).
        let wait = WaitForSingleObject(event, 200);
        if wait.0 != WAIT_OBJECT_0.0 {
            // Timeout or failure — just loop and re-check running.
            continue;
        }

        // Drain all available packets. Windows may have queued more than
        // one since we were last scheduled.
        loop {
            let packet_length = match capture_client.GetNextPacketSize() {
                Ok(n) => n,
                Err(e) => {
                    warn!("GetNextPacketSize failed: {e}");
                    break;
                }
            };
            if packet_length == 0 {
                break;
            }

            let mut buffer_ptr: *mut u8 = std::ptr::null_mut();
            let mut num_frames: u32 = 0;
            let mut flags: u32 = 0;
            let mut device_position: u64 = 0;
            let mut qpc_position: u64 = 0;

            if let Err(e) = capture_client.GetBuffer(
                &mut buffer_ptr,
                &mut num_frames,
                &mut flags,
                Some(&mut device_position),
                Some(&mut qpc_position),
            ) {
                warn!("GetBuffer failed: {e}");
                break;
            }

            if num_frames > 0 && !buffer_ptr.is_null() {
                if !logged_first_packet {
                    info!(
                        frames = num_frames,
                        flags, "WASAPI capture: first packet received"
                    );
                    logged_first_packet = true;
                }

                // Because we asked for 48 kHz mono i16, each frame is
                // exactly one i16. Windows's AUTOCONVERTPCM handles the
                // conversion from whatever the engine mix format is.
                let samples = std::slice::from_raw_parts(
                    buffer_ptr as *const i16,
                    num_frames as usize,
                );
                ring.write(samples);
            }

            if let Err(e) = capture_client.ReleaseBuffer(num_frames) {
                warn!("ReleaseBuffer failed: {e}");
                break;
            }
        }
    }

    info!("WASAPI capture thread stopping");
    let _ = audio_client.Stop();
    let _ = CloseHandle(event);
    // _com_guard drops here, calling CoUninitialize.

    // Silence INFINITE unused-import warning — it's referenced by the
    // `windows` crate's WaitForSingleObject alternative but we use the
    // 200 ms timeout variant instead. Explicit suppression for clarity.
    let _ = INFINITE;

    Ok(())
}

// ---------------------------------------------------------------------------
// Helpers
// ---------------------------------------------------------------------------

/// Best-effort device ID string for logging. Grabbing the friendly name via
/// PKEY_Device_FriendlyName requires IPropertyStore + PROPVARIANT plumbing
/// that's far more ceremony than a log line justifies; the ID is already
/// sufficient to confirm we opened the right endpoint.
unsafe fn device_name(
    device: &windows::Win32::Media::Audio::IMMDevice,
) -> Result<String, anyhow::Error> {
    let id = device.GetId().context("IMMDevice::GetId failed")?;
    Ok(id.to_string().unwrap_or_else(|_| "<non-utf16>".to_string()))
}
