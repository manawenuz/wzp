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
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use tracing::{error, info, warn};
use wzp_codec::agc::AutoGainControl;
use wzp_codec::opus_dec::OpusDecoder;
use wzp_codec::opus_enc::OpusEncoder;
use wzp_crypto::{KeyExchange, WarzoneKeyExchange};
use wzp_fec::{RaptorQFecDecoder, RaptorQFecEncoder};
use wzp_proto::{
    AudioDecoder, AudioEncoder, CodecId, FecDecoder, FecEncoder,
    MediaHeader, MediaPacket, MediaTransport, QualityProfile, SignalMessage,
};

use crate::audio_ring::AudioRing;
use crate::commands::EngineCommand;
use crate::stats::{CallState, CallStats};

/// Opus frame size at 48kHz mono, 20ms = 960 samples.
const FRAME_SAMPLES: usize = 960;

/// Configuration to start a call.
pub struct CallStartConfig {
    pub profile: QualityProfile,
    pub relay_addr: String,
    pub room: String,
    pub auth_token: Vec<u8>,
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
        let state = self.state.clone();

        self.state.running.store(true, Ordering::Release);
        self.call_start = Some(Instant::now());

        let state_clone = state.clone();
        runtime.block_on(async move {
            if let Err(e) = run_call(relay_addr, &room, &identity_seed, profile, state_clone).await
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
        self.state.running.store(false, Ordering::Release);
        let _ = self.state.command_tx.send(EngineCommand::Stop);
        if let Some(rt) = self.tokio_runtime.take() {
            rt.shutdown_background();
        }
        self.call_start = None;
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
        // Include current audio level
        stats.audio_level = self.state.audio_level_rms.load(Ordering::Relaxed);
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
            QualityProfile::GOOD,
            QualityProfile::DEGRADED,
            QualityProfile::CATASTROPHIC,
        ],
    };
    transport.send_signal(&offer).await?;
    info!("CallOffer sent, waiting for CallAnswer...");

    let answer = transport
        .recv_signal()
        .await?
        .ok_or_else(|| anyhow::anyhow!("connection closed before CallAnswer"))?;

    let relay_ephemeral_pub = match answer {
        SignalMessage::CallAnswer { ephemeral_pub, .. } => ephemeral_pub,
        other => {
            return Err(anyhow::anyhow!(
                "expected CallAnswer, got {:?}",
                std::mem::discriminant(&other)
            ))
        }
    };

    let _session = kx.derive_session(&relay_ephemeral_pub)?;
    info!("handshake complete, call active");

    {
        let mut stats = state.stats.lock().unwrap();
        stats.state = CallState::Active;
    }

    // Initialize Opus codec
    let mut encoder =
        OpusEncoder::new(profile).map_err(|e| anyhow::anyhow!("opus encoder init: {e}"))?;
    let mut decoder =
        OpusDecoder::new(profile).map_err(|e| anyhow::anyhow!("opus decoder init: {e}"))?;

    // Initialize FEC encoder/decoder
    let mut fec_enc = wzp_fec::create_encoder(&profile);
    let mut fec_dec = wzp_fec::create_decoder(&profile);

    // AGC: normalize volume on both capture and playout paths
    let mut capture_agc = AutoGainControl::new();
    let mut playout_agc = AutoGainControl::new();

    info!(
        fec_ratio = profile.fec_ratio,
        frames_per_block = profile.frames_per_block,
        "codec + FEC + AGC initialized (48kHz mono, 20ms frames)"
    );

    let seq = AtomicU16::new(0);
    let ts = AtomicU32::new(0);
    let transport_recv = transport.clone();

    // Pre-allocate buffers
    let mut capture_buf = vec![0i16; FRAME_SAMPLES];
    let mut encode_buf = vec![0u8; encoder.max_frame_bytes()];
    let mut frame_in_block: u8 = 0;
    let mut block_id: u8 = 0;

    // Send task: capture ring → Opus encode → FEC → MediaPackets
    let send_task = async {
        info!("send task started (Opus + RaptorQ FEC)");
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }

            let avail = state.capture_ring.available();
            if avail < FRAME_SAMPLES {
                tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                continue;
            }

            let read = state.capture_ring.read(&mut capture_buf);
            if read < FRAME_SAMPLES {
                continue;
            }

            // AGC: normalize capture volume before encoding
            capture_agc.process_frame(&mut capture_buf);

            // Opus encode
            let encoded_len = match encoder.encode(&capture_buf, &mut encode_buf) {
                Ok(n) => n,
                Err(e) => {
                    warn!("opus encode error: {e}");
                    continue;
                }
            };
            let encoded = &encode_buf[..encoded_len];

            // Build source packet
            let s = seq.fetch_add(1, Ordering::Relaxed);
            let t = ts.fetch_add(FRAME_SAMPLES as u32, Ordering::Relaxed);

            let source_pkt = MediaPacket {
                header: MediaHeader {
                    version: 0,
                    is_repair: false,
                    codec_id: profile.codec,
                    has_quality_report: false,
                    fec_ratio_encoded: MediaHeader::encode_fec_ratio(profile.fec_ratio),
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

            // Send source packet
            if let Err(e) = transport.send_media(&source_pkt).await {
                error!("send error: {e}");
                break;
            }

            // Feed encoded frame to FEC encoder
            if let Err(e) = fec_enc.add_source_symbol(encoded) {
                warn!("fec add_source error: {e}");
            }
            frame_in_block += 1;

            // When block is full, generate repair packets
            if frame_in_block >= profile.frames_per_block {
                match fec_enc.generate_repair(profile.fec_ratio) {
                    Ok(repairs) => {
                        let repair_count = repairs.len();
                        for (sym_idx, repair_data) in repairs {
                            let rs = seq.fetch_add(1, Ordering::Relaxed);
                            let repair_pkt = MediaPacket {
                                header: MediaHeader {
                                    version: 0,
                                    is_repair: true,
                                    codec_id: profile.codec,
                                    has_quality_report: false,
                                    fec_ratio_encoded: MediaHeader::encode_fec_ratio(
                                        profile.fec_ratio,
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
                            if let Err(e) = transport.send_media(&repair_pkt).await {
                                error!("send repair error: {e}");
                                break;
                            }
                        }
                        if repair_count > 0 && (block_id % 50 == 0 || block_id == 0) {
                            info!(
                                block_id,
                                repair_count,
                                fec_ratio = profile.fec_ratio,
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

            if s % 500 == 0 {
                info!(seq = s, block_id, frame_in_block, "sending");
            }
        }
    };

    // Pre-allocate decode buffer
    let mut decode_buf = vec![0i16; FRAME_SAMPLES];

    // Recv task: MediaPackets → FEC decode → Opus decode → playout ring
    let recv_task = async {
        let mut frames_decoded: u64 = 0;
        let mut fec_recovered: u64 = 0;
        info!("recv task started (Opus + RaptorQ FEC)");
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            match transport_recv.recv_media().await {
                Ok(Some(pkt)) => {
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
                    if !is_repair {
                        match decoder.decode(&pkt.payload, &mut decode_buf) {
                            Ok(samples) => {
                                // AGC on playout — normalizes received audio volume
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

                    // Try FEC recovery for this block
                    // (useful when source packets were lost but repair arrived)
                    if let Ok(Some(recovered_frames)) = fec_dec.try_decode(pkt_block) {
                        // FEC recovered the block — any previously missing frames
                        // are now available. In a full jitter buffer implementation,
                        // we'd insert recovered frames at the right position.
                        // For now, log recovery for telemetry.
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

                    if frames_decoded == 1 || frames_decoded % 500 == 0 {
                        info!(frames_decoded, fec_recovered, "recv stats");
                    }

                    let mut stats = state.stats.lock().unwrap();
                    stats.frames_decoded = frames_decoded;
                    stats.fec_recovered = fec_recovered;
                }
                Ok(None) => {
                    info!("relay disconnected");
                    break;
                }
                Err(e) => {
                    error!("recv error: {e}");
                    break;
                }
            }
        }
    };

    // Stats task
    let stats_task = async {
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            {
                let mut stats = state.stats.lock().unwrap();
                stats.frames_encoded = seq.load(Ordering::Relaxed) as u64;
            }
            tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        }
    };

    tokio::select! {
        _ = send_task => {}
        _ = recv_task => {}
        _ = stats_task => {}
    }

    transport.close().await.ok();
    Ok(())
}
