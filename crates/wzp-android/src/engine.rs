//! Engine orchestrator — manages the call lifecycle.
//!
//! The engine owns:
//! - The Oboe audio backend (start/stop)
//! - A codec thread running the `Pipeline`
//! - A tokio runtime for async network I/O
//! - Command channel for control from the JNI/UI thread

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use tracing::{error, info, warn};
use wzp_crypto::{KeyExchange, WarzoneKeyExchange};
use wzp_proto::{
    CodecId, MediaHeader, MediaPacket, MediaTransport, QualityProfile, SignalMessage,
};

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
    /// Room name (passed as SNI).
    pub room: String,
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
            room: String::new(),
            auth_token: Vec::new(),
            identity_seed: [0u8; 32],
        }
    }
}

/// Shared state between the engine owner and background threads.
struct EngineState {
    running: AtomicBool,
    connected: AtomicBool,
    muted: AtomicBool,
    speaker: AtomicBool,
    aec_enabled: AtomicBool,
    agc_enabled: AtomicBool,
    stats: Mutex<CallStats>,
    command_tx: std::sync::mpsc::Sender<EngineCommand>,
    command_rx: Mutex<Option<std::sync::mpsc::Receiver<EngineCommand>>>,
}

/// The WarzonePhone Android engine.
pub struct WzpEngine {
    state: Arc<EngineState>,
    codec_thread: Option<std::thread::JoinHandle<()>>,
    tokio_runtime: Option<tokio::runtime::Runtime>,
    call_start: Option<Instant>,
}

impl WzpEngine {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state = Arc::new(EngineState {
            running: AtomicBool::new(false),
            connected: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            speaker: AtomicBool::new(false),
            aec_enabled: AtomicBool::new(true),
            agc_enabled: AtomicBool::new(true),
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

    pub fn start_call(&mut self, config: CallStartConfig) -> Result<(), anyhow::Error> {
        if self.state.running.load(Ordering::Acquire) {
            return Err(anyhow::anyhow!("call already active"));
        }

        {
            let mut stats = self.state.stats.lock().unwrap();
            *stats = CallStats {
                state: CallState::Connecting,
                ..Default::default()
            };
        }

        // Create tokio runtime
        let runtime = tokio::runtime::Builder::new_multi_thread()
            .worker_threads(2)
            .thread_name("wzp-net")
            .enable_all()
            .build()?;

        // Channels between codec thread and network tasks
        let (send_tx, mut send_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);
        let (recv_tx, recv_rx) = tokio::sync::mpsc::channel::<MediaPacket>(64);

        // Shared sequence counter for outgoing packets
        let seq_counter = Arc::new(AtomicU16::new(0));
        let ts_counter = Arc::new(AtomicU32::new(0));

        // Parse relay address
        let relay_addr: SocketAddr = config.relay_addr.parse().map_err(|e| {
            anyhow::anyhow!("invalid relay address '{}': {e}", config.relay_addr)
        })?;

        let room = config.room.clone();
        let identity_seed = config.identity_seed;
        let state_net = self.state.clone();
        let seq_c = seq_counter.clone();
        let ts_c = ts_counter.clone();

        // Spawn the combined network task (connect + handshake + send/recv)
        runtime.spawn(async move {
            // Install rustls crypto provider
            let _ = rustls::crypto::ring::default_provider().install_default();

            // Create QUIC endpoint
            let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let endpoint = match wzp_transport::create_endpoint(bind_addr, None) {
                Ok(ep) => ep,
                Err(e) => {
                    error!("failed to create QUIC endpoint: {e}");
                    return;
                }
            };

            // Connect to relay with room as SNI
            let sni = if room.is_empty() { "android" } else { &room };
            info!(%relay_addr, sni, "connecting to relay...");
            let client_cfg = wzp_transport::client_config();
            let conn = match wzp_transport::connect(&endpoint, relay_addr, sni, client_cfg).await {
                Ok(c) => c,
                Err(e) => {
                    error!("QUIC connect failed: {e}");
                    return;
                }
            };
            info!("QUIC connected to relay");

            let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

            // Crypto handshake: send CallOffer, receive CallAnswer
            let mut kx = WarzoneKeyExchange::from_identity_seed(&identity_seed);
            let ephemeral_pub = kx.generate_ephemeral();
            let identity_pub = kx.identity_public_key();

            // Sign (ephemeral_pub || "call-offer")
            let mut sign_data = Vec::with_capacity(32 + 10);
            sign_data.extend_from_slice(&ephemeral_pub);
            sign_data.extend_from_slice(b"call-offer");
            let signature = kx.sign(&sign_data);

            let offer = SignalMessage::CallOffer {
                identity_pub,
                ephemeral_pub,
                signature,
                supported_profiles: vec![
                    QualityProfile::GOOD,
                    QualityProfile::DEGRADED,
                    QualityProfile::CATASTROPHIC,
                ],
            };

            if let Err(e) = transport.send_signal(&offer).await {
                error!("failed to send CallOffer: {e}");
                return;
            }
            info!("CallOffer sent, waiting for CallAnswer...");

            // Receive CallAnswer
            let answer = match transport.recv_signal().await {
                Ok(Some(msg)) => msg,
                Ok(None) => {
                    error!("connection closed before CallAnswer");
                    return;
                }
                Err(e) => {
                    error!("failed to receive CallAnswer: {e}");
                    return;
                }
            };

            let (relay_ephemeral_pub, _chosen_profile) = match answer {
                SignalMessage::CallAnswer {
                    ephemeral_pub,
                    chosen_profile,
                    ..
                } => (ephemeral_pub, chosen_profile),
                other => {
                    error!("expected CallAnswer, got {:?}", std::mem::discriminant(&other));
                    return;
                }
            };

            // Derive crypto session (not encrypting media yet for simplicity)
            let _session = match kx.derive_session(&relay_ephemeral_pub) {
                Ok(s) => s,
                Err(e) => {
                    error!("session derivation failed: {e}");
                    return;
                }
            };

            info!("handshake complete, call active");
            state_net.connected.store(true, Ordering::Release);
            {
                let mut stats = state_net.stats.lock().unwrap();
                stats.state = CallState::Active;
            }

            // Spawn recv task
            let recv_transport = transport.clone();
            let recv_handle = tokio::spawn(async move {
                loop {
                    match recv_transport.recv_media().await {
                        Ok(Some(pkt)) => {
                            if recv_tx.send(pkt).await.is_err() {
                                break;
                            }
                        }
                        Ok(None) => {
                            info!("relay disconnected (recv)");
                            break;
                        }
                        Err(e) => {
                            error!("recv_media error: {e}");
                            break;
                        }
                    }
                }
            });

            // Send task runs in this task
            while let Some(encoded) = send_rx.recv().await {
                let seq = seq_c.fetch_add(1, Ordering::Relaxed);
                let ts = ts_c.fetch_add(20, Ordering::Relaxed);
                let packet = MediaPacket {
                    header: MediaHeader {
                        version: 0,
                        is_repair: false,
                        codec_id: CodecId::Opus24k,
                        has_quality_report: false,
                        fec_ratio_encoded: 0,
                        seq,
                        timestamp: ts,
                        fec_block: 0,
                        fec_symbol: 0,
                        reserved: 0,
                        csrc_count: 0,
                    },
                    payload: Bytes::from(encoded),
                    quality_report: None,
                };
                if let Err(e) = transport.send_media(&packet).await {
                    error!("send_media error: {e}");
                    break;
                }
            }

            recv_handle.abort();
            transport.close().await.ok();
        });

        // Take the command receiver
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
                crate::audio_android::pin_to_big_core();
                crate::audio_android::set_realtime_priority();

                let mut audio = OboeBackend::new();
                if let Err(e) = audio.start() {
                    error!("failed to start audio: {e}");
                    state.running.store(false, Ordering::Release);
                    return;
                }

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

                let mut prev_aec = true;
                let mut prev_agc = true;
                let mut capture_buf = vec![0i16; FRAME_SAMPLES];
                let frame_duration = std::time::Duration::from_millis(20);
                let mut recv_rx = recv_rx;

                while state.running.load(Ordering::Relaxed) {
                    let loop_start = Instant::now();

                    // Process commands
                    while let Ok(cmd) = command_rx.try_recv() {
                        match cmd {
                            EngineCommand::SetMute(m) => {
                                state.muted.store(m, Ordering::Relaxed);
                            }
                            EngineCommand::SetSpeaker(s) => {
                                state.speaker.store(s, Ordering::Relaxed);
                            }
                            EngineCommand::ForceProfile(p) => {
                                pipeline.force_profile(p);
                            }
                            EngineCommand::Stop => {
                                state.running.store(false, Ordering::Release);
                                break;
                            }
                        }
                    }

                    // Sync AEC/AGC
                    let cur_aec = state.aec_enabled.load(Ordering::Relaxed);
                    if cur_aec != prev_aec {
                        pipeline.set_aec_enabled(cur_aec);
                        prev_aec = cur_aec;
                    }
                    let cur_agc = state.agc_enabled.load(Ordering::Relaxed);
                    if cur_agc != prev_agc {
                        pipeline.set_agc_enabled(cur_agc);
                        prev_agc = cur_agc;
                    }

                    if !state.running.load(Ordering::Relaxed) {
                        break;
                    }

                    // --- Capture → Encode → Send ---
                    let captured = audio.read_capture(&mut capture_buf);
                    if captured >= FRAME_SAMPLES {
                        let muted = state.muted.load(Ordering::Relaxed);
                        if let Some(encoded) = pipeline.encode_frame(&capture_buf, muted) {
                            let _ = send_tx.try_send(encoded);
                        }
                    }

                    // --- Recv → Decode → Playout ---
                    while let Ok(pkt) = recv_rx.try_recv() {
                        pipeline.feed_packet(pkt);
                    }

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

                    let elapsed = loop_start.elapsed();
                    if elapsed < frame_duration {
                        std::thread::sleep(frame_duration - elapsed);
                    }
                }

                audio.stop();
                {
                    let mut stats = state.stats.lock().unwrap();
                    stats.state = CallState::Closed;
                }
            })?;

        self.codec_thread = Some(codec_thread);
        self.tokio_runtime = Some(runtime);
        self.call_start = Some(Instant::now());
        Ok(())
    }

    pub fn stop_call(&mut self) {
        if !self.state.running.load(Ordering::Acquire) {
            return;
        }
        self.state.running.store(false, Ordering::Release);
        let _ = self.state.command_tx.send(EngineCommand::Stop);

        if let Some(handle) = self.codec_thread.take() {
            let _ = handle.join();
        }
        if let Some(rt) = self.tokio_runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_secs(2));
        }
        self.call_start = None;
    }

    pub fn set_mute(&self, muted: bool) {
        let _ = self.state.command_tx.send(EngineCommand::SetMute(muted));
    }

    pub fn set_speaker(&self, enabled: bool) {
        let _ = self.state.command_tx.send(EngineCommand::SetSpeaker(enabled));
    }

    pub fn set_aec_enabled(&self, enabled: bool) {
        self.state.aec_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn set_agc_enabled(&self, enabled: bool) {
        self.state.agc_enabled.store(enabled, Ordering::Relaxed);
    }

    pub fn force_profile(&self, profile: QualityProfile) {
        let _ = self.state.command_tx.send(EngineCommand::ForceProfile(profile));
    }

    pub fn get_stats(&self) -> CallStats {
        let mut stats = self.state.stats.lock().unwrap().clone();
        if let Some(start) = self.call_start {
            stats.duration_secs = start.elapsed().as_secs_f64();
        }
        stats
    }

    pub fn is_active(&self) -> bool {
        self.state.running.load(Ordering::Acquire)
    }

    pub fn destroy(mut self) {
        self.stop_call();
    }
}

impl Drop for WzpEngine {
    fn drop(&mut self) {
        self.stop_call();
    }
}
