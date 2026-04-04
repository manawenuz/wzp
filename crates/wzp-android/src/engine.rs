//! Engine orchestrator — manages the call lifecycle.
//!
//! The engine owns:
//! - The Oboe audio backend (start/stop)
//! - A codec thread running the `Pipeline`
//! - A tokio runtime for async network I/O
//! - Command channel for control from the JNI/UI thread

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use tracing::{error, info, warn};
use wzp_proto::QualityProfile;

use crate::audio_android::{OboeBackend, FRAME_SAMPLES};
use crate::commands::EngineCommand;
use crate::pipeline::Pipeline;
use crate::stats::{CallState, CallStats};

/// Configuration to start a call.
pub struct CallStartConfig {
    /// Initial quality profile.
    pub profile: QualityProfile,
    /// Relay server address (host:port).
    pub relay_addr: String,
    /// Authentication token for the relay.
    pub auth_token: Vec<u8>,
    /// 32-byte identity seed for key derivation.
    pub identity_seed: [u8; 32],
}

impl Default for CallStartConfig {
    fn default() -> Self {
        Self {
            profile: QualityProfile::GOOD,
            relay_addr: String::new(),
            auth_token: Vec::new(),
            identity_seed: [0u8; 32],
        }
    }
}

/// Shared state between the engine owner and background threads.
struct EngineState {
    running: AtomicBool,
    muted: AtomicBool,
    speaker: AtomicBool,
    stats: Mutex<CallStats>,
    command_tx: std::sync::mpsc::Sender<EngineCommand>,
    command_rx: Mutex<Option<std::sync::mpsc::Receiver<EngineCommand>>>,
}

/// The WarzonePhone Android engine.
///
/// Manages the entire call pipeline: audio capture/playout via Oboe,
/// codec encode/decode, FEC, jitter buffer, and network transport.
///
/// Thread model:
/// - **UI/JNI thread**: calls `start_call`, `stop_call`, `set_mute`, etc.
/// - **Codec thread**: runs `Pipeline` encode/decode loop, reads/writes ring buffers
/// - **Tokio runtime** (2 worker threads): async network send/recv
pub struct WzpEngine {
    state: Arc<EngineState>,
    codec_thread: Option<std::thread::JoinHandle<()>>,
    #[allow(unused)]
    tokio_runtime: Option<tokio::runtime::Runtime>,
    call_start: Option<Instant>,
}

impl WzpEngine {
    /// Create a new idle engine.
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state = Arc::new(EngineState {
            running: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            speaker: AtomicBool::new(false),
            stats: Mutex::new(CallStats::default()),
            command_tx: tx,
            command_rx: Mutex::new(Some(rx)),
        });

        Self {
            state,
            codec_thread: None,
            tokio_runtime: None,
            call_start: None,
        }
    }

    /// Start a call with the given configuration.
    ///
    /// This creates the tokio runtime, starts the Oboe audio backend,
    /// and spawns the codec thread.
    pub fn start_call(&mut self, config: CallStartConfig) -> Result<(), anyhow::Error> {
        if self.state.running.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!("call already active"));
        }

        // Update state
        {
            let mut stats = self.state.stats.lock().unwrap();
            *stats = CallStats {
                state: CallState::Connecting,
                ..Default::default()
            };
        }

        // Create tokio runtime with 2 worker threads
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wzp-net")
            .enable_all()
            .build()?;

        // Create async channels for network send/recv
        let (send_tx, mut _send_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (_recv_tx, mut recv_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        // Spawn network tasks (placeholder — will use wzp-transport)
        let _relay_addr = config.relay_addr.clone();
        runtime.spawn(async move {
            // Network send task: reads from send_rx, sends via transport
            // This will be implemented when wzp-transport Android support is added
            while let Some(_packet) = _send_rx.recv().await {
                // TODO: send via wzp-transport
            }
        });

        let recv_tx_clone = _recv_tx.clone();
        runtime.spawn(async move {
            // Network recv task: reads from transport, writes to recv_rx
            // This will be implemented when wzp-transport Android support is added
            let _tx = recv_tx_clone;
            // TODO: recv from wzp-transport and forward
        });

        // Take the command receiver (it can only be taken once)
        let command_rx = self
            .state
            .command_rx
            .lock()
            .unwrap()
            .take()
            .ok_or_else(|| anyhow::anyhow!("command receiver already taken"))?;

        // Start the codec thread
        let state = self.state.clone();
        let profile = config.profile;
        let codec_thread = std::thread::Builder::new()
            .name("wzp-codec".into())
            .spawn(move || {
                // Pin to big cores and set RT priority on Android
                crate::audio_android::pin_to_big_core();
                crate::audio_android::set_realtime_priority();

                // Create audio backend
                let mut audio = OboeBackend::new();
                if let Err(e) = audio.start() {
                    error!("failed to start audio: {e}");
                    state.running.store(false, Ordering::Release);
                    return;
                }

                // Create pipeline
                let mut pipeline = match Pipeline::new(profile) {
                    Ok(p) => p,
                    Err(e) => {
                        error!("failed to create pipeline: {e}");
                        audio.stop();
                        state.running.store(false, Ordering::Release);
                        return;
                    }
                };

                state.running.store(true, Ordering::Release);
                {
                    let mut stats = state.stats.lock().unwrap();
                    stats.state = CallState::Active;
                }

                info!("codec thread started");

                let mut capture_buf = vec![0i16; FRAME_SAMPLES];
                #[allow(unused_assignments)]
                let mut recv_buf: Vec<u8> = Vec::new();

                // Main codec loop: 20ms per iteration
                let frame_duration = std::time::Duration::from_millis(20);

                while state.running.load(Ordering::Relaxed) {
                    let loop_start = Instant::now();

                    // Process commands (non-blocking)
                    while let Ok(cmd) = command_rx.try_recv() {
                        match cmd {
                            EngineCommand::SetMute(m) => {
                                state.muted.store(m, Ordering::Relaxed);
                                info!(muted = m, "mute toggled");
                            }
                            EngineCommand::SetSpeaker(s) => {
                                state.speaker.store(s, Ordering::Relaxed);
                                info!(speaker = s, "speaker toggled");
                            }
                            EngineCommand::ForceProfile(p) => {
                                pipeline.force_profile(p);
                                info!(?p, "profile forced");
                            }
                            EngineCommand::Stop => {
                                info!("stop command received");
                                state.running.store(false, Ordering::Release);
                                break;
                            }
                        }
                    }

                    if !state.running.load(Ordering::Relaxed) {
                        break;
                    }

                    // --- Capture → Encode → Send ---
                    let captured = audio.read_capture(&mut capture_buf);
                    if captured >= FRAME_SAMPLES {
                        let muted = state.muted.load(Ordering::Relaxed);
                        if let Some(encoded) = pipeline.encode_frame(&capture_buf, muted) {
                            // Send to network (best-effort)
                            let _ = send_tx.try_send(encoded);
                        }
                    }

                    // --- Recv → Decode → Playout ---
                    // Drain received packets from the network channel
                    while let Ok(data) = recv_rx.try_recv() {
                        recv_buf = data;
                        // Deserialize the packet and feed to pipeline
                        // For now, feed raw bytes — full MediaPacket deserialization
                        // will be added with the transport integration
                        let _ = &recv_buf; // suppress unused warning
                    }

                    // Decode from jitter buffer
                    if let Some(pcm) = pipeline.decode_frame() {
                        audio.write_playout(&pcm);
                    }

                    // --- Update stats ---
                    {
                        let pstats = pipeline.stats();
                        let mut stats = state.stats.lock().unwrap();
                        stats.frames_encoded = pstats.frames_encoded;
                        stats.frames_decoded = pstats.frames_decoded;
                        stats.underruns = pstats.underruns;
                        stats.jitter_buffer_depth = pstats.jitter_depth;
                        stats.quality_tier = pstats.quality_tier;
                    }

                    // Sleep for remainder of the 20ms frame period
                    let elapsed = loop_start.elapsed();
                    if elapsed < frame_duration {
                        std::thread::sleep(frame_duration - elapsed);
                    }
                }

                // Cleanup
                audio.stop();
                {
                    let mut stats = state.stats.lock().unwrap();
                    stats.state = CallState::Closed;
                }
                info!("codec thread exited");
            })?;

        self.codec_thread = Some(codec_thread);
        self.tokio_runtime = Some(runtime);
        self.call_start = Some(Instant::now());

        info!("call started");
        Ok(())
    }

    /// Stop the current call and clean up all resources.
    pub fn stop_call(&mut self) {
        if !self.state.running.load(Ordering::Acquire) {
            return;
        }

        // Signal stop
        self.state.running.store(false, Ordering::Release);
        let _ = self.state.command_tx.send(EngineCommand::Stop);

        // Join codec thread
        if let Some(handle) = self.codec_thread.take() {
            if let Err(e) = handle.join() {
                warn!("codec thread panicked: {e:?}");
            }
        }

        // Shut down tokio runtime
        if let Some(rt) = self.tokio_runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
        }

        self.call_start = None;
        info!("call stopped");
    }

    /// Set microphone mute state.
    pub fn set_mute(&self, muted: bool) {
        let _ = self.state.command_tx.send(EngineCommand::SetMute(muted));
    }

    /// Set speaker (loudspeaker) mode.
    #[allow(unused)]
    pub fn set_speaker(&self, enabled: bool) {
        let _ = self
            .state
            .command_tx
            .send(EngineCommand::SetSpeaker(enabled));
    }

    /// Force a specific quality profile (overrides adaptive logic).
    #[allow(unused)]
    pub fn force_profile(&self, profile: QualityProfile) {
        let _ = self
            .state
            .command_tx
            .send(EngineCommand::ForceProfile(profile));
    }

    /// Get a snapshot of the current call statistics.
    pub fn get_stats(&self) -> CallStats {
        let mut stats = self.state.stats.lock().unwrap().clone();
        // Update duration from wall clock
        if let Some(start) = self.call_start {
            stats.duration_secs = start.elapsed().as_secs_f64();
        }
        stats
    }

    /// Check if a call is currently active.
    pub fn is_active(&self) -> bool {
        self.state.running.load(Ordering::Acquire)
    }

    /// Destroy the engine, stopping any active call.
    pub fn destroy(mut self) {
        self.stop_call();
        info!("engine destroyed");
    }
}

impl Drop for WzpEngine {
    fn drop(&mut self) {
        self.stop_call();
    }
}
