//! Engine orchestrator — manages the call lifecycle.
//!
//! IMPORTANT: On Android, pthread_create crashes in shared libraries due to
//! static bionic stubs in the Rust std prebuilt rlibs. ALL work must happen
//! on the JNI calling thread or via the tokio current_thread runtime.
//! No std::thread::spawn or tokio multi_thread allowed.
//!
//! Audio capture and playout happen on Kotlin JVM threads via AudioRecord
//! and AudioTrack. PCM samples are transferred through lock-free ring buffers.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU8, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use tracing::{error, info, warn};
use wzp_codec::agc::AutoGainControl;
use wzp_crypto::{KeyExchange, WarzoneKeyExchange};
use wzp_fec::{RaptorQFecDecoder, RaptorQFecEncoder};
use wzp_proto::{
    AdaptiveQualityController, AudioDecoder, AudioEncoder, CodecId, FecDecoder, FecEncoder,
    MediaHeader, MediaPacket, MediaTransport, QualityController, QualityProfile, SignalMessage,
};

use crate::audio_ring::AudioRing;
use crate::commands::EngineCommand;
use crate::stats::{CallState, CallStats};

/// Max frame size at 48kHz mono (40ms = 1920 samples, for Codec2/Opus6k).
const MAX_FRAME_SAMPLES: usize = 1920;

/// Sentinel value: no profile change pending.
const PROFILE_NO_CHANGE: u8 = 0xFF;

/// All quality profiles in index order, for AtomicU8-based signaling.
const PROFILES: [QualityProfile; 6] = [
    QualityProfile::STUDIO_64K,   // 0
    QualityProfile::STUDIO_48K,   // 1
    QualityProfile::STUDIO_32K,   // 2
    QualityProfile::GOOD,         // 3
    QualityProfile::DEGRADED,     // 4
    QualityProfile::CATASTROPHIC, // 5
];

fn profile_to_index(p: &QualityProfile) -> u8 {
    PROFILES.iter().position(|pp| pp.codec == p.codec).map(|i| i as u8).unwrap_or(3)
}

fn index_to_profile(idx: u8) -> Option<QualityProfile> {
    PROFILES.get(idx as usize).copied()
}

/// Compute frame samples at 48kHz for a given profile.
fn frame_samples_for(profile: &QualityProfile) -> usize {
    (profile.frame_duration_ms as usize) * 48 // 48000 / 1000
}

/// Configuration to start a call.
pub struct CallStartConfig {
    pub profile: QualityProfile,
    /// When true, use the relay's chosen_profile from CallAnswer instead of local profile.
    pub auto_profile: bool,
    pub relay_addr: String,
    pub room: String,
    pub auth_token: Vec<u8>,
    pub identity_seed: [u8; 32],
    pub alias: Option<String>,
}

impl Default for CallStartConfig {
    fn default() -> Self {
        Self {
            profile: QualityProfile::GOOD,
            auto_profile: false,
            relay_addr: String::new(),
            room: String::new(),
            auth_token: Vec::new(),
            identity_seed: [0u8; 32],
            alias: None,
        }
    }
}

pub(crate) struct EngineState {
    pub running: AtomicBool,
    pub muted: AtomicBool,
    pub stats: Mutex<CallStats>,
    pub command_tx: std::sync::mpsc::Sender<EngineCommand>,
    pub command_rx: Mutex<Option<std::sync::mpsc::Receiver<EngineCommand>>>,
    /// Ring buffer: Kotlin AudioRecord → Rust encoder
    pub capture_ring: AudioRing,
    /// Ring buffer: Rust decoder → Kotlin AudioTrack
    pub playout_ring: AudioRing,
    /// Current audio level (RMS) for UI display, updated by capture path.
    pub audio_level_rms: AtomicU32,
    /// QUIC transport handle — stored so stop_call() can close it immediately,
    /// triggering relay-side leave + RoomUpdate broadcast.
    pub quic_transport: Mutex<Option<Arc<wzp_transport::QuinnTransport>>>,
}

pub struct WzpEngine {
    pub(crate) state: Arc<EngineState>,
    tokio_runtime: Option<tokio::runtime::Runtime>,
    call_start: Option<Instant>,
}

impl WzpEngine {
    pub fn new() -> Self {
        let (tx, rx) = std::sync::mpsc::channel();
        let state = Arc::new(EngineState {
            running: AtomicBool::new(false),
            muted: AtomicBool::new(false),
            stats: Mutex::new(CallStats::default()),
            command_tx: tx,
            command_rx: Mutex::new(Some(rx)),
            capture_ring: AudioRing::new(),
            playout_ring: AudioRing::new(),
            audio_level_rms: AtomicU32::new(0),
            quic_transport: Mutex::new(None),
        });
        Self {
            state,
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

        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let relay_addr: SocketAddr = config.relay_addr.parse().map_err(|e| {
            anyhow::anyhow!("invalid relay address '{}': {e}", config.relay_addr)
        })?;

        let room = config.room.clone();
        let identity_seed = config.identity_seed;
        let profile = config.profile;
        let auto_profile = config.auto_profile;
        let alias = config.alias.clone();
        let state = self.state.clone();

        self.state.running.store(true, Ordering::Release);
        self.call_start = Some(Instant::now());

        let state_clone = state.clone();
        runtime.block_on(async move {
            if let Err(e) = run_call(relay_addr, &room, &identity_seed, profile, auto_profile, alias.as_deref(), state_clone).await
            {
                error!("call failed: {e}");
            }
        });

        state.running.store(false, Ordering::Release);
        {
            let mut stats = state.stats.lock().unwrap();
            stats.state = CallState::Closed;
        }

        self.tokio_runtime = Some(runtime);
        Ok(())
    }

    pub fn stop_call(&mut self) {
        info!("stop_call: setting running=false");
        self.state.running.store(false, Ordering::Release);
        // Close QUIC connection — this wakes up all blocked recv/send futures
        // inside block_on(run_call(...)) on the JNI thread. run_call will then
        // wait up to 500ms for the peer to acknowledge the close before returning.
        if let Some(transport) = self.state.quic_transport.lock().unwrap().take() {
            info!("stop_call: closing QUIC connection");
            transport.close_now();
        }
        let _ = self.state.command_tx.send(EngineCommand::Stop);
        // Note: the runtime is still blocked in block_on(run_call(...)) on the
        // start_call thread. Once run_call exits (triggered by running=false +
        // connection close above), block_on returns and stores the runtime in
        // self.tokio_runtime. We don't need to shut it down here.
        if let Some(rt) = self.tokio_runtime.take() {
            rt.shutdown_timeout(std::time::Duration::from_millis(100));
        }
        self.call_start = None;
        info!("stop_call: done");
    }

    /// Ping a relay — same pattern as start_call (creates runtime on calling thread).
    /// Returns JSON `{"rtt_ms":N,"server_fingerprint":"hex"}` or error.
    pub fn ping_relay(&self, address: &str) -> Result<String, anyhow::Error> {
        let addr: SocketAddr = address.parse()?;
        let _ = rustls::crypto::ring::default_provider().install_default();

        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let result = rt.block_on(async {
            let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let endpoint = wzp_transport::create_endpoint(bind, None)?;
            let client_cfg = wzp_transport::client_config();
            let start = Instant::now();

            let conn_result = tokio::time::timeout(
                std::time::Duration::from_secs(3),
                wzp_transport::connect(&endpoint, addr, "ping", client_cfg),
            )
            .await;

            // Always close endpoint to prevent resource leaks
            endpoint.close(0u32.into(), b"done");

            let conn = conn_result.map_err(|_| anyhow::anyhow!("timeout"))??;
            let rtt_ms = start.elapsed().as_millis() as u64;
            let server_fp = conn
                .peer_identity()
                .and_then(|id| id.downcast::<Vec<rustls::pki_types::CertificateDer>>().ok())
                .and_then(|certs| certs.first().map(|c| {
                    use std::hash::{Hash, Hasher};
                    let mut h = std::collections::hash_map::DefaultHasher::new();
                    c.as_ref().hash(&mut h);
                    format!("{:016x}", h.finish())
                }))
                .unwrap_or_default();
            conn.close(0u32.into(), b"ping");

            Ok::<_, anyhow::Error>(format!(r#"{{"rtt_ms":{},"server_fingerprint":"{}"}}"#, rtt_ms, server_fp))
        });

        // Shutdown runtime cleanly with timeout
        rt.shutdown_timeout(std::time::Duration::from_millis(500));
        result
    }

    /// Start persistent signaling connection for direct calls.
    /// Spawns a background task that maintains the `_signal` connection.
    pub fn start_signaling(
        &mut self,
        relay_addr: &str,
        seed_hex: &str,
        token: Option<&str>,
        alias: Option<&str>,
    ) -> Result<(), anyhow::Error> {
        // Capture lightweight params only — all crypto work happens on the spawned thread
        let addr_str = relay_addr.to_string();
        let seed_str = seed_hex.to_string();
        let token = token.map(|s| s.to_string());
        let alias = alias.map(|s| s.to_string());
        let state = self.state.clone();

        self.state.running.store(true, Ordering::Release);

        // Spawn on a dedicated thread — Android's Kotlin dispatcher has ~1MB stack,
        // too small for rustls + QUIC + tokio + crypto. Do ALL work on this thread.
        std::thread::Builder::new()
            .name("wzp-signal".into())
            .stack_size(8 * 1024 * 1024) // 8MB stack
            .spawn(move || {
                use wzp_proto::{MediaTransport, SignalMessage};

                let _ = rustls::crypto::ring::default_provider().install_default();

                let addr: SocketAddr = match addr_str.parse() {
                    Ok(a) => a,
                    Err(e) => { error!("bad relay addr: {e}"); return; }
                };
                let seed = if seed_str.is_empty() {
                    wzp_crypto::Seed::generate()
                } else {
                    match wzp_crypto::Seed::from_hex(&seed_str) {
                        Ok(s) => s,
                        Err(e) => { error!("bad seed: {e}"); return; }
                    }
                };
                let identity = seed.derive_identity();
                let pub_id = identity.public_identity();
                let identity_pub = *pub_id.signing.as_bytes();
                let fp = pub_id.fingerprint.to_string();

                info!(fingerprint = %fp, relay = %addr, "starting signaling");

                let signal_state = state.clone();

                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");
                rt.block_on(async move {
            let _ = rustls::crypto::ring::default_provider().install_default();
            let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let endpoint = match wzp_transport::create_endpoint(bind, None) {
                Ok(e) => e,
                Err(e) => { error!("signal endpoint: {e}"); return; }
            };
            let client_cfg = wzp_transport::client_config();
            let conn = match wzp_transport::connect(&endpoint, addr, "_signal", client_cfg).await {
                Ok(c) => c,
                Err(e) => { error!("signal connect: {e}"); return; }
            };
            let transport = std::sync::Arc::new(wzp_transport::QuinnTransport::new(conn));

            // Auth if token provided
            if let Some(ref tok) = token {
                let _ = transport.send_signal(&SignalMessage::AuthToken { token: tok.clone() }).await;
            }

            // Register presence
            let _ = transport.send_signal(&SignalMessage::RegisterPresence {
                identity_pub,
                signature: vec![],
                alias: alias.clone(),
            }).await;

            // Wait for ack
            match transport.recv_signal().await {
                Ok(Some(SignalMessage::RegisterPresenceAck { success: true, .. })) => {
                    info!(fingerprint = %fp, "signal: registered");
                    let mut stats = signal_state.stats.lock().unwrap();
                    stats.state = crate::stats::CallState::Registered;
                }
                other => {
                    error!("signal registration failed: {other:?}");
                    return;
                }
            }

            // Signal recv loop
            loop {
                if !signal_state.running.load(Ordering::Relaxed) {
                    break;
                }
                match transport.recv_signal().await {
                    Ok(Some(SignalMessage::CallRinging { call_id })) => {
                        info!(call_id = %call_id, "signal: ringing");
                        let mut stats = signal_state.stats.lock().unwrap();
                        stats.state = crate::stats::CallState::Ringing;
                    }
                    Ok(Some(SignalMessage::DirectCallOffer { caller_fingerprint, caller_alias, call_id, .. })) => {
                        info!(from = %caller_fingerprint, call_id = %call_id, "signal: incoming call");
                        let mut stats = signal_state.stats.lock().unwrap();
                        stats.state = crate::stats::CallState::IncomingCall;
                        stats.incoming_call_id = Some(call_id);
                        stats.incoming_caller_fp = Some(caller_fingerprint);
                        stats.incoming_caller_alias = caller_alias;
                    }
                    Ok(Some(SignalMessage::DirectCallAnswer { call_id, accept_mode, .. })) => {
                        info!(call_id = %call_id, mode = ?accept_mode, "signal: call answered");
                    }
                    Ok(Some(SignalMessage::CallSetup { call_id, room, relay_addr })) => {
                        info!(call_id = %call_id, room = %room, relay = %relay_addr, "signal: call setup");
                        // Connect to media room via the existing start_call mechanism
                        // Store the room info so Kotlin can call startCall with it
                        let mut stats = signal_state.stats.lock().unwrap();
                        stats.state = crate::stats::CallState::Connecting;
                        // Store call setup info for Kotlin to pick up
                        stats.incoming_call_id = Some(format!("{relay_addr}|{room}"));
                    }
                    Ok(Some(SignalMessage::Hangup { reason })) => {
                        info!(reason = ?reason, "signal: call ended by remote");
                        let mut stats = signal_state.stats.lock().unwrap();
                        stats.state = crate::stats::CallState::Closed;
                        stats.incoming_call_id = None;
                        stats.incoming_caller_fp = None;
                        stats.incoming_caller_alias = None;
                    }
                    Ok(Some(_)) => {}
                    Ok(None) => {
                        info!("signal: connection closed");
                        break;
                    }
                    Err(e) => {
                        error!("signal recv error: {e}");
                        break;
                    }
                }
            }

            let mut stats = signal_state.stats.lock().unwrap();
            stats.state = crate::stats::CallState::Closed;
                }); // block_on
            })?; // thread spawn

        Ok(())
    }

    /// Place a direct call to a target fingerprint via the signal connection.
    pub fn place_call(&self, target_fingerprint: &str) -> Result<(), anyhow::Error> {
        let _ = self.state.command_tx.send(EngineCommand::PlaceCall {
            target_fingerprint: target_fingerprint.to_string(),
        });
        Ok(())
    }

    /// Answer an incoming direct call.
    pub fn answer_call(&self, call_id: &str, mode: wzp_proto::CallAcceptMode) -> Result<(), anyhow::Error> {
        let _ = self.state.command_tx.send(EngineCommand::AnswerCall {
            call_id: call_id.to_string(),
            accept_mode: mode,
        });
        Ok(())
    }

    pub fn set_mute(&self, muted: bool) {
        self.state.muted.store(muted, Ordering::Relaxed);
    }

    pub fn set_speaker(&self, _enabled: bool) {}

    pub fn force_profile(&self, _profile: QualityProfile) {}

    pub fn get_stats(&self) -> CallStats {
        let mut stats = self.state.stats.lock().unwrap().clone();
        if let Some(start) = self.call_start {
            stats.duration_secs = start.elapsed().as_secs_f64();
        }
        stats.audio_level = self.state.audio_level_rms.load(Ordering::Relaxed);
        stats.playout_overflows = self.state.playout_ring.overflow_count();
        stats.playout_underruns = self.state.playout_ring.underrun_count();
        stats.capture_overflows = self.state.capture_ring.overflow_count();
        stats
    }

    pub fn is_active(&self) -> bool {
        self.state.running.load(Ordering::Acquire)
    }

    pub fn write_audio(&self, samples: &[i16]) -> usize {
        if self.state.muted.load(Ordering::Relaxed) {
            return samples.len();
        }
        // Compute RMS for audio level display
        if !samples.is_empty() {
            let sum_sq: f64 = samples.iter().map(|&s| (s as f64) * (s as f64)).sum();
            let rms = (sum_sq / samples.len() as f64).sqrt() as u32;
            self.state.audio_level_rms.store(rms, Ordering::Relaxed);
        }
        self.state.capture_ring.write(samples)
    }

    pub fn read_audio(&self, out: &mut [i16]) -> usize {
        self.state.playout_ring.read(out)
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

/// Run the full call lifecycle: connect, handshake, send/recv media with Opus + FEC.
async fn run_call(
    relay_addr: SocketAddr,
    room: &str,
    identity_seed: &[u8; 32],
    profile: QualityProfile,
    auto_profile: bool,
    alias: Option<&str>,
    state: Arc<EngineState>,
) -> Result<(), anyhow::Error> {
    let _ = rustls::crypto::ring::default_provider().install_default();

    let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;

    let sni = if room.is_empty() { "android" } else { room };
    info!(%relay_addr, sni, "connecting to relay...");
    let client_cfg = wzp_transport::client_config();
    let conn = wzp_transport::connect(&endpoint, relay_addr, sni, client_cfg).await?;
    info!("QUIC connected to relay");

    let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

    // Store transport handle so stop_call() can close the connection immediately
    *state.quic_transport.lock().unwrap() = Some(transport.clone());

    // Crypto handshake
    let mut kx = WarzoneKeyExchange::from_identity_seed(identity_seed);
    let ephemeral_pub = kx.generate_ephemeral();
    let identity_pub = kx.identity_public_key();

    let mut sign_data = Vec::with_capacity(42);
    sign_data.extend_from_slice(&ephemeral_pub);
    sign_data.extend_from_slice(b"call-offer");
    let signature = kx.sign(&sign_data);

    let offer = SignalMessage::CallOffer {
        identity_pub,
        ephemeral_pub,
        signature,
        supported_profiles: vec![
            QualityProfile::STUDIO_64K,
            QualityProfile::STUDIO_48K,
            QualityProfile::STUDIO_32K,
            QualityProfile::GOOD,
            QualityProfile::DEGRADED,
            QualityProfile::CATASTROPHIC,
        ],
        alias: alias.map(|s| s.to_string()),
    };
    transport.send_signal(&offer).await?;
    info!("CallOffer sent, waiting for CallAnswer...");

    let answer = transport
        .recv_signal()
        .await?
        .ok_or_else(|| anyhow::anyhow!("connection closed before CallAnswer"))?;

    let (relay_ephemeral_pub, chosen_profile) = match answer {
        SignalMessage::CallAnswer { ephemeral_pub, chosen_profile, .. } => (ephemeral_pub, chosen_profile),
        other => {
            return Err(anyhow::anyhow!(
                "expected CallAnswer, got {:?}",
                std::mem::discriminant(&other)
            ))
        }
    };

    // Auto mode: use the relay's chosen profile instead of the local preference
    let profile = if auto_profile {
        info!(chosen = ?chosen_profile.codec, "auto mode: using relay's chosen profile");
        chosen_profile
    } else {
        profile
    };

    let _session = kx.derive_session(&relay_ephemeral_pub)?;
    info!(codec = ?profile.codec, "handshake complete, call active");

    {
        let mut stats = state.stats.lock().unwrap();
        stats.state = CallState::Active;
    }

    // Initialize codec (Opus or Codec2 based on profile)
    let mut encoder = wzp_codec::create_encoder(profile);
    let mut decoder = wzp_codec::create_decoder(profile);

    // Initialize FEC encoder/decoder
    let mut fec_enc = wzp_fec::create_encoder(&profile);
    let mut fec_dec = wzp_fec::create_decoder(&profile);

    // AGC: normalize volume on both capture and playout paths
    let mut capture_agc = AutoGainControl::new();
    let mut playout_agc = AutoGainControl::new();

    let mut frame_samples = frame_samples_for(&profile);
    info!(
        codec = ?profile.codec,
        fec_ratio = profile.fec_ratio,
        frames_per_block = profile.frames_per_block,
        frame_ms = profile.frame_duration_ms,
        frame_samples,
        "codec + FEC + AGC initialized"
    );

    {
        let mut stats = state.stats.lock().unwrap();
        stats.current_codec = format!("{:?}", profile.codec);
        stats.auto_mode = auto_profile;
    }

    let seq = AtomicU16::new(0);
    let ts = AtomicU32::new(0);
    let transport_recv = transport.clone();

    // Adaptive quality: shared AtomicU8 between recv task (writer) and send task (reader).
    // 0xFF = no change pending, 0-5 = index into PROFILES array.
    let pending_profile = Arc::new(AtomicU8::new(PROFILE_NO_CHANGE));
    let pending_profile_recv = pending_profile.clone();

    // Pre-allocate buffers (sized for current profile)
    let mut capture_buf = vec![0i16; frame_samples];
    let mut encode_buf = vec![0u8; encoder.max_frame_bytes()];
    let mut frame_in_block: u8 = 0;
    let mut block_id: u8 = 0;
    let mut current_profile = profile;

    // Send task: capture ring → Opus encode → FEC → MediaPackets
    //
    // IMPORTANT: send_media() uses quinn's send_datagram() which is
    // synchronous and returns Err(Blocked) when the congestion window
    // is full. We MUST NOT break on send errors — that would kill the
    // entire call. Instead we drop the packet and keep going.
    let send_task = async {
        info!("send task started (Opus + RaptorQ FEC)");
        let mut send_errors: u64 = 0;
        let mut last_send_error_log = Instant::now();
        let mut last_stats_log = Instant::now();
        let mut frames_sent: u64 = 0;
        let mut frames_dropped: u64 = 0;
        // Per-step timing accumulators (reset every stats log)
        let mut t_agc_us: u64 = 0;
        let mut t_opus_us: u64 = 0;
        let mut t_fec_us: u64 = 0;
        let mut t_send_us: u64 = 0;
        let mut t_frames: u64 = 0;
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }

            // Check for adaptive profile switch from recv task
            if auto_profile {
                let p = pending_profile.swap(PROFILE_NO_CHANGE, Ordering::Acquire);
                if p != PROFILE_NO_CHANGE {
                    if let Some(new_profile) = index_to_profile(p) {
                        info!(
                            from = ?current_profile.codec,
                            to = ?new_profile.codec,
                            "auto: switching encoder profile"
                        );
                        if let Err(e) = encoder.set_profile(new_profile) {
                            warn!("encoder set_profile failed: {e}");
                        } else {
                            fec_enc = wzp_fec::create_encoder(&new_profile);
                            current_profile = new_profile;
                            let new_frame_samples = frame_samples_for(&new_profile);
                            if new_frame_samples != frame_samples {
                                frame_samples = new_frame_samples;
                                capture_buf.resize(frame_samples, 0);
                            }
                            encode_buf.resize(encoder.max_frame_bytes(), 0);
                            // Reset FEC block state for clean switch
                            frame_in_block = 0;
                            block_id = block_id.wrapping_add(1);
                            // Update stats with new codec
                            if let Ok(mut stats) = state.stats.lock() {
                                stats.current_codec = format!("{:?}", new_profile.codec);
                            }
                        }
                    }
                }
            }

            let avail = state.capture_ring.available();
            if avail < frame_samples {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                continue;
            }

            let read = state.capture_ring.read(&mut capture_buf);
            if read < frame_samples {
                continue;
            }

            // Mute: zero out the buffer so Opus encodes silence.
            // We still read from the ring to prevent it from filling up.
            if state.muted.load(Ordering::Relaxed) {
                capture_buf.fill(0);
            }

            // AGC: normalize capture volume before encoding
            let t0 = Instant::now();
            capture_agc.process_frame(&mut capture_buf);
            t_agc_us += t0.elapsed().as_micros() as u64;

            // Opus encode
            let t0 = Instant::now();
            let encoded_len = match encoder.encode(&capture_buf, &mut encode_buf) {
                Ok(n) => n,
                Err(e) => {
                    warn!("opus encode error: {e}");
                    continue;
                }
            };
            t_opus_us += t0.elapsed().as_micros() as u64;
            let encoded = &encode_buf[..encoded_len];

            // Build source packet
            let s = seq.fetch_add(1, Ordering::Relaxed);
            let t = ts.fetch_add(frame_samples as u32, Ordering::Relaxed);

            let source_pkt = MediaPacket {
                header: MediaHeader {
                    version: 0,
                    is_repair: false,
                    codec_id: current_profile.codec,
                    has_quality_report: false,
                    fec_ratio_encoded: MediaHeader::encode_fec_ratio(current_profile.fec_ratio),
                    seq: s,
                    timestamp: t,
                    fec_block: block_id,
                    fec_symbol: frame_in_block,
                    reserved: 0,
                    csrc_count: 0,
                },
                payload: Bytes::copy_from_slice(encoded),
                quality_report: None,
            };

            // Send source packet — drop on error, never break
            let t0 = Instant::now();
            if let Err(e) = transport.send_media(&source_pkt).await {
                send_errors += 1;
                frames_dropped += 1;
                // Log first few errors, then throttle to once per second
                if send_errors <= 3 || last_send_error_log.elapsed().as_secs() >= 1 {
                    warn!(
                        seq = s,
                        send_errors,
                        frames_dropped,
                        "send_media error (dropping packet): {e}"
                    );
                    last_send_error_log = Instant::now();
                }
                // Don't feed to FEC either — the source is lost
                t_send_us += t0.elapsed().as_micros() as u64;
                continue;
            }
            t_send_us += t0.elapsed().as_micros() as u64;
            frames_sent += 1;

            // Feed encoded frame to FEC encoder
            let t0 = Instant::now();
            if let Err(e) = fec_enc.add_source_symbol(encoded) {
                warn!("fec add_source error: {e}");
            }
            frame_in_block += 1;

            // When block is full, generate repair packets
            if frame_in_block >= current_profile.frames_per_block {
                match fec_enc.generate_repair(current_profile.fec_ratio) {
                    Ok(repairs) => {
                        let repair_count = repairs.len();
                        for (sym_idx, repair_data) in repairs {
                            let rs = seq.fetch_add(1, Ordering::Relaxed);
                            let repair_pkt = MediaPacket {
                                header: MediaHeader {
                                    version: 0,
                                    is_repair: true,
                                    codec_id: current_profile.codec,
                                    has_quality_report: false,
                                    fec_ratio_encoded: MediaHeader::encode_fec_ratio(
                                        current_profile.fec_ratio,
                                    ),
                                    seq: rs,
                                    timestamp: t,
                                    fec_block: block_id,
                                    fec_symbol: sym_idx,
                                    reserved: 0,
                                    csrc_count: 0,
                                },
                                payload: Bytes::from(repair_data),
                                quality_report: None,
                            };
                            // Drop repair packets on error — never break
                            if let Err(_e) = transport.send_media(&repair_pkt).await {
                                send_errors += 1;
                                frames_dropped += 1;
                                // Don't log every repair failure — source error log covers it
                            }
                        }
                        if repair_count > 0 && (block_id % 50 == 0 || block_id == 0) {
                            info!(
                                block_id,
                                repair_count,
                                fec_ratio = current_profile.fec_ratio,
                                "FEC block complete"
                            );
                        }
                    }
                    Err(e) => {
                        warn!("fec generate_repair error: {e}");
                    }
                }

                let _ = fec_enc.finalize_block();
                block_id = block_id.wrapping_add(1);
                frame_in_block = 0;
            }
            t_fec_us += t0.elapsed().as_micros() as u64;
            t_frames += 1;

            // Periodic stats every 5 seconds
            if last_stats_log.elapsed().as_secs() >= 5 {
                let avg = |total: u64| if t_frames > 0 { total / t_frames } else { 0 };
                info!(
                    seq = s,
                    block_id,
                    frames_sent,
                    frames_dropped,
                    send_errors,
                    ring_avail = state.capture_ring.available(),
                    capture_overflows = state.capture_ring.overflow_count(),
                    avg_agc_us = avg(t_agc_us),
                    avg_opus_us = avg(t_opus_us),
                    avg_fec_us = avg(t_fec_us),
                    avg_send_us = avg(t_send_us),
                    avg_total_us = avg(t_agc_us + t_opus_us + t_fec_us + t_send_us),
                    "send stats"
                );
                t_agc_us = 0; t_opus_us = 0; t_fec_us = 0; t_send_us = 0; t_frames = 0;
                last_stats_log = Instant::now();
            }
        }
        info!(frames_sent, frames_dropped, send_errors, "send task ended");
    };

    // Pre-allocate decode buffer (max size to handle any incoming codec)
    let mut decode_buf = vec![0i16; MAX_FRAME_SAMPLES];

    // Recv task: MediaPackets → FEC decode → Opus decode → playout ring
    let recv_task = async {
        let mut frames_decoded: u64 = 0;
        let mut fec_recovered: u64 = 0;
        let mut recv_errors: u64 = 0;
        let mut last_recv_instant = Instant::now();
        let mut max_recv_gap_ms: u64 = 0;
        let mut last_stats_log = Instant::now();
        let mut quality_ctrl = AdaptiveQualityController::new();
        let mut last_peer_codec: Option<CodecId> = None;
        info!("recv task started (Opus + RaptorQ FEC)");
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            match transport_recv.recv_media().await {
                Ok(Some(pkt)) => {
                    // Track recv gaps — large gaps indicate network or relay issues
                    let recv_gap_ms = last_recv_instant.elapsed().as_millis() as u64;
                    last_recv_instant = Instant::now();
                    if recv_gap_ms > max_recv_gap_ms {
                        max_recv_gap_ms = recv_gap_ms;
                    }
                    if recv_gap_ms > 500 {
                        warn!(
                            recv_gap_ms,
                            seq = pkt.header.seq,
                            is_repair = pkt.header.is_repair,
                            "large recv gap — possible network stall"
                        );
                    }

                    // Adaptive quality: ingest quality reports from relay
                    if auto_profile {
                        if let Some(ref qr) = pkt.quality_report {
                            if let Some(new_profile) = quality_ctrl.observe(qr) {
                                let idx = profile_to_index(&new_profile);
                                info!(
                                    loss = qr.loss_percent(),
                                    rtt = qr.rtt_ms(),
                                    tier = ?quality_ctrl.tier(),
                                    to = ?new_profile.codec,
                                    "auto: quality adapter recommends switch"
                                );
                                pending_profile_recv.store(idx, Ordering::Release);
                            }
                        }
                    }

                    let is_repair = pkt.header.is_repair;
                    let pkt_block = pkt.header.fec_block;
                    let pkt_symbol = pkt.header.fec_symbol;

                    // Feed every packet (source + repair) to FEC decoder
                    let _ = fec_dec.add_symbol(
                        pkt_block,
                        pkt_symbol,
                        is_repair,
                        &pkt.payload,
                    );

                    // Source packets: decode directly
                    if !is_repair && pkt.header.codec_id != CodecId::ComfortNoise {
                        // Switch decoder to match incoming codec if different
                        if pkt.header.codec_id != decoder.codec_id() {
                            let switch_profile = match pkt.header.codec_id {
                                CodecId::Opus24k => QualityProfile::GOOD,
                                CodecId::Opus6k => QualityProfile::DEGRADED,
                                CodecId::Opus32k => QualityProfile::STUDIO_32K,
                                CodecId::Opus48k => QualityProfile::STUDIO_48K,
                                CodecId::Opus64k => QualityProfile::STUDIO_64K,
                                CodecId::Codec2_1200 => QualityProfile::CATASTROPHIC,
                                CodecId::Codec2_3200 => QualityProfile {
                                    codec: CodecId::Codec2_3200,
                                    fec_ratio: 0.5,
                                    frame_duration_ms: 20,
                                    frames_per_block: 5,
                                },
                                other => QualityProfile { codec: other, ..QualityProfile::GOOD },
                            };
                            info!(from = ?decoder.codec_id(), to = ?pkt.header.codec_id, "recv: switching decoder");
                            let _ = decoder.set_profile(switch_profile);
                        }
                        // Track peer codec for UI display
                        if last_peer_codec != Some(pkt.header.codec_id) {
                            last_peer_codec = Some(pkt.header.codec_id);
                            if let Ok(mut stats) = state.stats.lock() {
                                stats.peer_codec = format!("{:?}", pkt.header.codec_id);
                            }
                        }
                        match decoder.decode(&pkt.payload, &mut decode_buf) {
                            Ok(samples) => {
                                playout_agc.process_frame(&mut decode_buf[..samples]);
                                state.playout_ring.write(&decode_buf[..samples]);
                                frames_decoded += 1;
                            }
                            Err(e) => {
                                warn!("opus decode error: {e}");
                                if let Ok(samples) = decoder.decode_lost(&mut decode_buf) {
                                    playout_agc.process_frame(&mut decode_buf[..samples]);
                                    state.playout_ring.write(&decode_buf[..samples]);
                                }
                            }
                        }
                    }

                    // Try FEC recovery
                    if let Ok(Some(recovered_frames)) = fec_dec.try_decode(pkt_block) {
                        fec_recovered += recovered_frames.len() as u64;
                        if fec_recovered % 50 == 1 {
                            info!(
                                fec_recovered,
                                block = pkt_block,
                                frames = recovered_frames.len(),
                                "FEC block recovered"
                            );
                        }
                    }

                    // Expire old blocks to prevent memory growth
                    if pkt_block > 3 {
                        fec_dec.expire_before(pkt_block.wrapping_sub(3));
                    }

                    let mut stats = state.stats.lock().unwrap();
                    stats.frames_decoded = frames_decoded;
                    stats.fec_recovered = fec_recovered;
                    drop(stats);

                    // Periodic stats every 5 seconds
                    if last_stats_log.elapsed().as_secs() >= 5 {
                        info!(
                            frames_decoded,
                            fec_recovered,
                            recv_errors,
                            max_recv_gap_ms,
                            playout_avail = state.playout_ring.available(),
                            playout_overflows = state.playout_ring.overflow_count(),
                            playout_underruns = state.playout_ring.underrun_count(),
                            "recv stats"
                        );
                        max_recv_gap_ms = 0;
                        last_stats_log = Instant::now();
                    }
                }
                Ok(None) => {
                    info!(frames_decoded, fec_recovered, "relay disconnected (stream ended)");
                    break;
                }
                Err(e) => {
                    recv_errors += 1;
                    // Transient errors: log and keep going
                    let msg = e.to_string();
                    if msg.contains("closed") || msg.contains("reset") {
                        error!(recv_errors, "recv fatal: {e}");
                        break;
                    }
                    // Non-fatal: log throttled
                    if recv_errors <= 3 || recv_errors % 50 == 0 {
                        warn!(recv_errors, "recv error (continuing): {e}");
                    }
                }
            }
        }
        info!(frames_decoded, fec_recovered, recv_errors, "recv task ended");
    };

    // Stats task — polls path quality + quinn RTT every 500ms
    let transport_stats = transport.clone();
    let stats_task = async {
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            // Feed quinn's QUIC-level RTT into our path monitor
            let quic_rtt_ms = transport_stats.connection().stats().path.rtt.as_millis() as u32;
            if quic_rtt_ms > 0 {
                transport_stats.feed_rtt(quic_rtt_ms);
            }
            let pq = transport_stats.path_quality();
            {
                let mut stats = state.stats.lock().unwrap();
                stats.frames_encoded = seq.load(Ordering::Relaxed) as u64;
                stats.loss_pct = pq.loss_pct;
                stats.rtt_ms = quic_rtt_ms;
                stats.jitter_ms = pq.jitter_ms;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    };

    // Signal recv task — listens for RoomUpdate and other signaling messages
    let transport_signal = transport.clone();
    let state_signal = state.clone();
    let signal_task = async {
        loop {
            match transport_signal.recv_signal().await {
                Ok(Some(SignalMessage::RoomUpdate { count, participants })) => {
                    info!(count, "RoomUpdate received");
                    let members: Vec<crate::stats::RoomMember> = participants
                        .iter()
                        .map(|p| crate::stats::RoomMember {
                            fingerprint: p.fingerprint.clone(),
                            alias: p.alias.clone(),
                            relay_label: p.relay_label.clone(),
                        })
                        .collect();
                    let mut stats = state_signal.stats.lock().unwrap();
                    stats.room_participant_count = count;
                    stats.room_participants = members;
                }
                Ok(Some(msg)) => {
                    info!("signal received: {:?}", std::mem::discriminant(&msg));
                }
                Ok(None) => {
                    info!("signal stream closed");
                    break;
                }
                Err(e) => {
                    warn!("signal recv error: {e}");
                    break;
                }
            }
        }
    };

    tokio::select! {
        _ = send_task => info!("send task ended"),
        _ = recv_task => info!("recv task ended"),
        _ = stats_task => info!("stats task ended"),
        _ = signal_task => info!("signal task ended"),
    }

    // Send CONNECTION_CLOSE and wait up to 500ms for the peer to acknowledge.
    // This ensures the relay sees the close even if the first packet is lost.
    info!("closing QUIC connection...");
    transport.close_now();
    match tokio::time::timeout(
        std::time::Duration::from_millis(500),
        transport.connection().closed(),
    ).await {
        Ok(_) => info!("QUIC connection closed cleanly"),
        Err(_) => info!("QUIC close timed out (relay may not have ack'd)"),
    }
    Ok(())
}
