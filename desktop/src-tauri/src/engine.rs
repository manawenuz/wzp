//! Call engine for the desktop app — wraps wzp-client audio + transport
//! into a clean async interface for Tauri commands.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, AtomicU32, AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Instant;

use tokio::sync::Mutex;
use tracing::{error, info};

use wzp_client::audio_io::{AudioCapture, AudioPlayback};
use wzp_client::call::{CallConfig, CallEncoder};
use wzp_proto::MediaTransport;

const FRAME_SAMPLES: usize = 960;

/// Wrapper to make non-Sync audio handles safe to store in shared state.
/// The audio handle is only accessed from the thread that created it (drop),
/// never shared across threads — Sync is safe.
#[allow(dead_code)]
struct SyncWrapper(Box<dyn std::any::Any + Send>);
unsafe impl Sync for SyncWrapper {}

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
    pub audio_level: u32,
    pub call_duration_secs: f64,
    pub fingerprint: String,
}

pub struct CallEngine {
    running: Arc<AtomicBool>,
    mic_muted: Arc<AtomicBool>,
    spk_muted: Arc<AtomicBool>,
    participants: Arc<Mutex<Vec<ParticipantInfo>>>,
    frames_sent: Arc<AtomicU64>,
    frames_received: Arc<AtomicU64>,
    audio_level: Arc<AtomicU32>,
    transport: Arc<wzp_transport::QuinnTransport>,
    start_time: Instant,
    fingerprint: String,
    /// Keep audio handles alive for the duration of the call.
    /// Wrapped in SyncWrapper because AudioUnit isn't Sync.
    _audio_handle: SyncWrapper,
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
        let fingerprint = fp.to_string();
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

        // Audio I/O — VPIO (OS AEC) on macOS, plain CPAL otherwise.
        // The audio handle must be stored in CallEngine to keep streams alive.
        let (capture_ring, playout_ring, audio_handle): (_, _, Box<dyn std::any::Any + Send>) =
            if _os_aec {
                #[cfg(target_os = "macos")]
                {
                    match wzp_client::audio_vpio::VpioAudio::start() {
                        Ok(v) => {
                            let cr = v.capture_ring().clone();
                            let pr = v.playout_ring().clone();
                            info!("using VoiceProcessingIO (OS AEC)");
                            (cr, pr, Box::new(v))
                        }
                        Err(e) => {
                            info!("VPIO failed ({e}), falling back to CPAL");
                            let capture = AudioCapture::start()?;
                            let playback = AudioPlayback::start()?;
                            let cr = capture.ring().clone();
                            let pr = playback.ring().clone();
                            (cr, pr, Box::new((capture, playback)))
                        }
                    }
                }
                #[cfg(not(target_os = "macos"))]
                {
                    info!("OS AEC not available on this platform, using CPAL");
                    let capture = AudioCapture::start()?;
                    let playback = AudioPlayback::start()?;
                    let cr = capture.ring().clone();
                    let pr = playback.ring().clone();
                    (cr, pr, Box::new((capture, playback)))
                }
            } else {
                let capture = AudioCapture::start()?;
                let playback = AudioPlayback::start()?;
                let cr = capture.ring().clone();
                let pr = playback.ring().clone();
                (cr, pr, Box::new((capture, playback)))
            };

        let running = Arc::new(AtomicBool::new(true));
        let mic_muted = Arc::new(AtomicBool::new(false));
        let spk_muted = Arc::new(AtomicBool::new(false));
        let participants: Arc<Mutex<Vec<ParticipantInfo>>> = Arc::new(Mutex::new(vec![]));
        let frames_sent = Arc::new(AtomicU64::new(0));
        let frames_received = Arc::new(AtomicU64::new(0));
        let audio_level = Arc::new(AtomicU32::new(0));

        // Send task
        let send_t = transport.clone();
        let send_r = running.clone();
        let send_mic = mic_muted.clone();
        let send_fs = frames_sent.clone();
        let send_level = audio_level.clone();
        let send_drops = Arc::new(AtomicU64::new(0));
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

                // Compute RMS audio level for UI meter
                if !buf.is_empty() {
                    let sum_sq: f64 = buf.iter().map(|&s| (s as f64) * (s as f64)).sum();
                    let rms = (sum_sq / buf.len() as f64).sqrt() as u32;
                    send_level.store(rms, Ordering::Relaxed);
                }

                if send_mic.load(Ordering::Relaxed) {
                    buf.fill(0);
                }
                match encoder.encode_frame(&buf) {
                    Ok(pkts) => {
                        for pkt in &pkts {
                            if let Err(e) = send_t.send_media(pkt).await {
                                // Transient congestion (Blocked) — drop packet, keep going
                                send_drops.fetch_add(1, Ordering::Relaxed);
                                if send_drops.load(Ordering::Relaxed) <= 3 {
                                    tracing::warn!("send_media error (dropping packet): {e}");
                                }
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
                        let msg = e.to_string();
                        if msg.contains("closed") || msg.contains("reset") {
                            error!("recv fatal: {e}");
                            break;
                        }
                        // Transient error — continue
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
            audio_level,
            transport,
            start_time: Instant::now(),
            fingerprint,
            _audio_handle: SyncWrapper(audio_handle),
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

    pub async fn status(&self) -> EngineStatus {
        let participants = {
            let parts = self.participants.lock().await;
            parts
                .iter()
                .map(|p| ParticipantInfo {
                    fingerprint: p.fingerprint.clone(),
                    alias: p.alias.clone(),
                })
                .collect()
        }; // lock dropped here
        EngineStatus {
            mic_muted: self.mic_muted.load(Ordering::Relaxed),
            spk_muted: self.spk_muted.load(Ordering::Relaxed),
            participants,
            frames_sent: self.frames_sent.load(Ordering::Relaxed),
            frames_received: self.frames_received.load(Ordering::Relaxed),
            audio_level: self.audio_level.load(Ordering::Relaxed),
            call_duration_secs: self.start_time.elapsed().as_secs_f64(),
            fingerprint: self.fingerprint.clone(),
        }
    }

    pub async fn stop(self) {
        self.running.store(false, Ordering::SeqCst);
        self.transport.close().await.ok();
    }
}
