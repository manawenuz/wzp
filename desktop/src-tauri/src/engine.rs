//! Call engine for the desktop app — wraps wzp-client audio + transport
//! into a clean async interface for Tauri commands.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use tokio::sync::Mutex;
use tracing::{error, info};

use wzp_client::audio_io::{AudioCapture, AudioPlayback};
use wzp_client::call::{CallConfig, CallEncoder};
use wzp_proto::MediaTransport;

const FRAME_SAMPLES: usize = 960;

pub struct ParticipantInfo {
    pub fingerprint: String,
    pub alias: Option<String>,
}

pub struct EngineStatus {
    pub mic_muted: bool,
    pub spk_muted: bool,
    pub participants: Vec<ParticipantInfo>,
    pub frames_sent: u64,
    pub frames_received: u64,
}

pub struct CallEngine {
    running: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
    spk_muted: Arc<AtomicBool>,
    participants: Arc<Mutex<Vec<ParticipantInfo>>>,
    frames_sent: Arc<std::sync::atomic::AtomicU64>,
    frames_received: Arc<std::sync::atomic::AtomicU64>,
    transport: Arc<wzp_transport::QuinnTransport>,
}

impl CallEngine {
    pub async fn start<F>(
        relay: String,
        room: String,
        alias: String,
        _os_aec: bool,
        event_cb: F,
    ) -> Result<Self, anyhow::Error>
    where
        F: Fn(&str, &str) + Send + Sync + 'static,
    {
        let _ = rustls::crypto::ring::default_provider().install_default();

        let relay_addr: SocketAddr = relay.parse()?;

        // Load or generate identity
        let seed = {
            let path = {
                let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
                std::path::PathBuf::from(home).join(".wzp").join("identity")
            };
            if path.exists() {
                if let Ok(hex) = std::fs::read_to_string(&path) {
                    if let Ok(s) = wzp_crypto::Seed::from_hex(hex.trim()) {
                        s
                    } else {
                        wzp_crypto::Seed::generate()
                    }
                } else {
                    wzp_crypto::Seed::generate()
                }
            } else {
                let s = wzp_crypto::Seed::generate();
                if let Some(p) = path.parent() {
                    std::fs::create_dir_all(p).ok();
                }
                let hex: String = s.0.iter().map(|b| format!("{b:02x}")).collect();
                std::fs::write(&path, hex).ok();
                s
            }
        };

        let fp = seed.derive_identity().public_identity().fingerprint;
        info!(%fp, "identity loaded");

        // Connect
        let bind_addr: SocketAddr = "0.0.0.0:0".parse().unwrap();
        let endpoint = wzp_transport::create_endpoint(bind_addr, None)?;
        let client_config = wzp_transport::client_config();
        let conn = wzp_transport::connect(&endpoint, relay_addr, &room, client_config).await?;
        let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

        // Handshake
        let _session = wzp_client::handshake::perform_handshake(
            &*transport,
            &seed.0,
            Some(&alias),
        )
        .await?;

        info!("connected to relay, handshake complete");
        event_cb("connected", &format!("joined room {room}"));

        // Audio I/O
        // TODO: support VPIO on macOS when os_aec is true
        let capture = AudioCapture::start()?;
        let playback = AudioPlayback::start()?;
        let capture_ring = capture.ring().clone();
        let playout_ring = playback.ring().clone();
        std::mem::forget(capture);
        std::mem::forget(playback);

        let running = Arc::new(AtomicBool::new(true));
        let mic_muted = Arc::new(AtomicBool::new(false));
        let spk_muted = Arc::new(AtomicBool::new(false));
        let participants: Arc<Mutex<Vec<ParticipantInfo>>> = Arc::new(Mutex::new(vec![]));
        let frames_sent = Arc::new(std::sync::atomic::AtomicU64::new(0));
        let frames_received = Arc::new(std::sync::atomic::AtomicU64::new(0));

        // Send task
        let send_t = transport.clone();
        let send_r = running.clone();
        let send_mic = mic_muted.clone();
        let send_fs = frames_sent.clone();
        tokio::spawn(async move {
            let config = CallConfig {
                noise_suppression: false,
                suppression_enabled: false,
                ..CallConfig::default()
            };
            let mut encoder = CallEncoder::new(&config);
            encoder.set_aec_enabled(false); // OS AEC or none
            let mut buf = vec![0i16; FRAME_SAMPLES];

            loop {
                if !send_r.load(Ordering::Relaxed) {
                    break;
                }
                if capture_ring.available() < FRAME_SAMPLES {
                    tokio::time::sleep(std::time::Duration::from_millis(5)).await;
                    continue;
                }
                capture_ring.read(&mut buf);
                if send_mic.load(Ordering::Relaxed) {
                    buf.fill(0);
                }
                match encoder.encode_frame(&buf) {
                    Ok(pkts) => {
                        for pkt in &pkts {
                            if let Err(e) = send_t.send_media(pkt).await {
                                error!("send: {e}");
                                return;
                            }
                        }
                        send_fs.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(e) => error!("encode: {e}"),
                }
            }
        });

        // Recv task (direct playout)
        let recv_t = transport.clone();
        let recv_r = running.clone();
        let recv_spk = spk_muted.clone();
        let recv_fr = frames_received.clone();
        tokio::spawn(async move {
            let mut opus_dec = wzp_codec::create_decoder(wzp_proto::QualityProfile::GOOD);
            let mut agc = wzp_codec::AutoGainControl::new();
            let mut pcm = vec![0i16; FRAME_SAMPLES];

            loop {
                if !recv_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(100),
                    recv_t.recv_media(),
                )
                .await
                {
                    Ok(Ok(Some(pkt))) => {
                        if !pkt.header.is_repair {
                            if let Ok(n) = opus_dec.decode(&pkt.payload, &mut pcm) {
                                agc.process_frame(&mut pcm[..n]);
                                if !recv_spk.load(Ordering::Relaxed) {
                                    playout_ring.write(&pcm[..n]);
                                }
                            }
                        }
                        recv_fr.fetch_add(1, Ordering::Relaxed);
                    }
                    Ok(Ok(None)) => break,
                    Ok(Err(e)) => {
                        error!("recv: {e}");
                        break;
                    }
                    Err(_) => {}
                }
            }
        });

        // Signal task (presence)
        let sig_t = transport.clone();
        let sig_r = running.clone();
        let sig_p = participants.clone();
        let event_cb = Arc::new(event_cb);
        let sig_cb = event_cb.clone();
        tokio::spawn(async move {
            loop {
                if !sig_r.load(Ordering::Relaxed) {
                    break;
                }
                match tokio::time::timeout(
                    std::time::Duration::from_millis(200),
                    sig_t.recv_signal(),
                )
                .await
                {
                    Ok(Ok(Some(wzp_proto::SignalMessage::RoomUpdate {
                        participants: parts,
                        ..
                    }))) => {
                        let mut seen = std::collections::HashSet::new();
                        let unique: Vec<ParticipantInfo> = parts
                            .into_iter()
                            .filter(|p| seen.insert((p.fingerprint.clone(), p.alias.clone())))
                            .map(|p| ParticipantInfo {
                                fingerprint: p.fingerprint,
                                alias: p.alias,
                            })
                            .collect();
                        let count = unique.len();
                        *sig_p.lock().await = unique;
                        sig_cb("room-update", &format!("{count} participants"));
                    }
                    Ok(Ok(Some(_))) => {}
                    Ok(Ok(None)) => break,
                    Ok(Err(_)) => break,
                    Err(_) => {}
                }
            }
        });

        Ok(Self {
            running,
            mic_muted,
            spk_muted,
            participants,
            frames_sent,
            frames_received,
            transport,
        })
    }

    pub fn toggle_mic(&self) -> bool {
        let was = self.mic_muted.load(Ordering::Relaxed);
        self.mic_muted.store(!was, Ordering::Relaxed);
        !was
    }

    pub fn toggle_speaker(&self) -> bool {
        let was = self.spk_muted.load(Ordering::Relaxed);
        self.spk_muted.store(!was, Ordering::Relaxed);
        !was
    }

    pub fn status(&self) -> EngineStatus {
        let parts = self.participants.blocking_lock();
        EngineStatus {
            mic_muted: self.mic_muted.load(Ordering::Relaxed),
            spk_muted: self.spk_muted.load(Ordering::Relaxed),
            participants: parts
                .iter()
                .map(|p| ParticipantInfo {
                    fingerprint: p.fingerprint.clone(),
                    alias: p.alias.clone(),
                })
                .collect(),
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            frames_received: self.frames_received.load(Ordering::Relaxed),
        }
    }

    pub async fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        self.transport.close().await.ok();
    }
}
