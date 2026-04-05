//! Engine orchestrator — manages the call lifecycle.
//!
//! IMPORTANT: On Android, pthread_create crashes in shared libraries due to
//! static bionic stubs in the Rust std prebuilt rlibs. ALL work must happen
//! on the JNI calling thread or via the tokio current_thread runtime.
//! No std::thread::spawn or tokio multi_thread allowed.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU16, AtomicU32, Ordering};
use std::sync::{Arc, Mutex};
use std::time::Instant;

use bytes::Bytes;
use tracing::{error, info};
use wzp_crypto::{KeyExchange, WarzoneKeyExchange};
use wzp_proto::{
    CodecId, MediaHeader, MediaPacket, MediaTransport, QualityProfile, SignalMessage,
};

use crate::commands::EngineCommand;
use crate::stats::{CallState, CallStats};

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

struct EngineState {
    running: AtomicBool,
    muted: AtomicBool,
    stats: Mutex<CallStats>,
    command_tx: std::sync::mpsc::Sender<EngineCommand>,
    command_rx: Mutex<Option<std::sync::mpsc::Receiver<EngineCommand>>>,
}

pub struct WzpEngine {
    state: Arc<EngineState>,
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

        // Create single-threaded tokio runtime — NO thread spawning.
        // On Android, pthread_create crashes due to static bionic stubs.
        let runtime = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let relay_addr: SocketAddr = config.relay_addr.parse().map_err(|e| {
            anyhow::anyhow!("invalid relay address '{}': {e}", config.relay_addr)
        })?;

        let room = config.room.clone();
        let identity_seed = config.identity_seed;
        let state = self.state.clone();

        self.state.running.store(true, Ordering::Release);
        self.call_start = Some(Instant::now());

        // Run the entire call on the current thread's tokio runtime.
        // This blocks the JNI thread until the call ends, so Kotlin
        // must call startCall from a background coroutine.
        let state_clone = state.clone();
        runtime.block_on(async move {
            if let Err(e) = run_call(relay_addr, &room, &identity_seed, state_clone).await {
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

    pub fn set_speaker(&self, _enabled: bool) {
        // TODO: route audio via AudioManager on Kotlin side
    }

    pub fn force_profile(&self, _profile: QualityProfile) {
        // TODO: wire to pipeline when codec thread is re-enabled
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

/// Run the full call lifecycle: connect, handshake, send/recv media.
/// All async, no thread spawning.
async fn run_call(
    relay_addr: SocketAddr,
    room: &str,
    identity_seed: &[u8; 32],
    state: Arc<EngineState>,
) -> Result<(), anyhow::Error> {
    // Install rustls crypto provider
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Create QUIC endpoint
    let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
    let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;

    // Connect to relay with room as SNI
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

    // Simple media loop: send silence, recv and count frames.
    // No codec thread, no Oboe — just network I/O to verify connectivity.
    // Audio pipeline will be added once native threading is resolved.
    let seq = AtomicU16::new(0);
    let ts = AtomicU32::new(0);
    let transport_recv = transport.clone();

    let send_task = async {
        let silence = vec![0u8; 20]; // minimal opus silence frame
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            let s = seq.fetch_add(1, Ordering::Relaxed);
            let t = ts.fetch_add(20, Ordering::Relaxed);
            let packet = MediaPacket {
                header: MediaHeader {
                    version: 0,
                    is_repair: false,
                    codec_id: CodecId::Opus24k,
                    has_quality_report: false,
                    fec_ratio_encoded: 0,
                    seq: s,
                    timestamp: t,
                    fec_block: 0,
                    fec_symbol: 0,
                    reserved: 0,
                    csrc_count: 0,
                },
                payload: Bytes::from(silence.clone()),
                quality_report: None,
            };
            if let Err(e) = transport.send_media(&packet).await {
                error!("send error: {e}");
                break;
            }
            // 20ms frame interval
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
        }
    };

    let recv_task = async {
        let mut frames_decoded: u64 = 0;
        loop {
            if !state.running.load(Ordering::Relaxed) {
                break;
            }
            match transport_recv.recv_media().await {
                Ok(Some(_pkt)) => {
                    frames_decoded += 1;
                    let mut stats = state.stats.lock().unwrap();
                    stats.frames_decoded = frames_decoded;
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

    // Update encoded frame count in send task
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
