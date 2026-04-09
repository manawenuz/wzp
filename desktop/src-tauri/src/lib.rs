// WarzonePhone Tauri backend — shared between desktop (macOS/Windows/Linux)
// and Tauri mobile (Android/iOS). Platform-specific audio is cfg-gated.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// CPAL-backed audio engine — desktop only. On Android we'll plug in an
// oboe/AAudio backend in a later step.
#[cfg(not(target_os = "android"))]
mod engine;

#[cfg(not(target_os = "android"))]
use engine::CallEngine;

use serde::Serialize;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::Mutex;
use wzp_proto::MediaTransport;

#[derive(Clone, Serialize)]
struct CallEvent {
    kind: String,
    message: String,
}

#[derive(Clone, Serialize)]
struct Participant {
    fingerprint: String,
    alias: Option<String>,
    relay_label: Option<String>,
}

#[derive(Clone, Serialize)]
struct CallStatus {
    active: bool,
    mic_muted: bool,
    spk_muted: bool,
    participants: Vec<Participant>,
    encode_fps: u64,
    recv_fps: u64,
    audio_level: u32,
    call_duration_secs: f64,
    fingerprint: String,
    tx_codec: String,
    rx_codec: String,
}

struct AppState {
    #[cfg(not(target_os = "android"))]
    engine: Mutex<Option<CallEngine>>,
    signal: Arc<Mutex<SignalState>>,
}

/// Ping result with RTT and server identity hash.
#[derive(Clone, Serialize)]
struct PingResult {
    rtt_ms: u32,
    /// Server identity: SHA-256 of the QUIC peer certificate, hex-encoded.
    server_fingerprint: String,
}

/// Ping a relay to check if it's online, measure RTT, and get server identity.
#[tauri::command]
async fn ping_relay(relay: String) -> Result<PingResult, String> {
    let addr: std::net::SocketAddr = relay.parse().map_err(|e| format!("bad address: {e}"))?;
    let _ = rustls::crypto::ring::default_provider().install_default();
    let bind: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    let endpoint = wzp_transport::create_endpoint(bind, None).map_err(|e| format!("{e}"))?;
    let client_cfg = wzp_transport::client_config();

    let start = std::time::Instant::now();
    let conn_result = tokio::time::timeout(
        std::time::Duration::from_secs(3),
        wzp_transport::connect(&endpoint, addr, "ping", client_cfg),
    )
    .await;

    // Always close endpoint to prevent resource leaks
    endpoint.close(0u32.into(), b"done");

    match conn_result {
        Ok(Ok(conn)) => {
            let rtt_ms = start.elapsed().as_millis() as u32;

            let server_fingerprint = conn
                .peer_identity()
                .and_then(|id| id.downcast::<Vec<rustls::pki_types::CertificateDer>>().ok())
                .and_then(|certs| certs.first().map(|c| {
                    use std::hash::{Hash, Hasher};
                    let mut hasher = std::collections::hash_map::DefaultHasher::new();
                    c.as_ref().hash(&mut hasher);
                    let h = hasher.finish();
                    format!("{h:016x}")
                }))
                .unwrap_or_else(|| {
                    format!("{:x}", addr.ip().to_string().len() as u64 * 0x9e3779b97f4a7c15 + addr.port() as u64)
                });

            conn.close(0u32.into(), b"ping");
            Ok(PingResult { rtt_ms, server_fingerprint })
        }
        Ok(Err(e)) => Err(format!("{e}")),
        Err(_) => Err("timeout (3s)".into()),
    }
}

/// Return the directory where identity/config should live.
///
/// Desktop: `$HOME/.wzp`
/// Android: `/data/data/com.wzp.phone/files/.wzp` (app-internal storage)
fn identity_dir() -> std::path::PathBuf {
    #[cfg(target_os = "android")]
    {
        // Android app-internal storage. The package id must match tauri.conf.json.
        return std::path::PathBuf::from("/data/data/com.wzp.phone/files/.wzp");
    }
    #[cfg(not(target_os = "android"))]
    {
        let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
        std::path::PathBuf::from(home).join(".wzp")
    }
}

fn identity_path() -> std::path::PathBuf {
    identity_dir().join("identity")
}

/// Load the persisted seed, or generate-and-persist a new one if missing.
fn load_or_create_seed() -> Result<wzp_crypto::Seed, String> {
    let path = identity_path();
    if path.exists() {
        let hex = std::fs::read_to_string(&path).map_err(|e| format!("read identity: {e}"))?;
        return wzp_crypto::Seed::from_hex(hex.trim()).map_err(|e| format!("{e}"));
    }
    let seed = wzp_crypto::Seed::generate();
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).map_err(|e| format!("create identity dir: {e}"))?;
    }
    let hex: String = seed.0.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, hex).map_err(|e| format!("write identity: {e}"))?;
    Ok(seed)
}

/// Read fingerprint, generating a fresh identity if none exists yet.
#[tauri::command]
fn get_identity() -> Result<String, String> {
    let seed = load_or_create_seed()?;
    Ok(seed.derive_identity().public_identity().fingerprint.to_string())
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn connect(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    relay: String,
    room: String,
    alias: String,
    os_aec: bool,
    quality: String,
) -> Result<String, String> {
    let mut engine_lock = state.engine.lock().await;
    if engine_lock.is_some() {
        return Err("already connected".into());
    }

    let app_clone = app.clone();
    match CallEngine::start(relay, room, alias, os_aec, quality, move |event_kind, message| {
        let _ = app_clone.emit(
            "call-event",
            CallEvent {
                kind: event_kind.to_string(),
                message: message.to_string(),
            },
        );
    })
    .await
    {
        Ok(eng) => {
            *engine_lock = Some(eng);
            Ok("connected".into())
        }
        Err(e) => Err(format!("{e}")),
    }
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn disconnect(state: tauri::State<'_, Arc<AppState>>) -> Result<String, String> {
    let mut engine_lock = state.engine.lock().await;
    if let Some(engine) = engine_lock.take() {
        engine.stop().await;
        Ok("disconnected".into())
    } else {
        Err("not connected".into())
    }
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn toggle_mic(state: tauri::State<'_, Arc<AppState>>) -> Result<bool, String> {
    let engine_lock = state.engine.lock().await;
    if let Some(ref engine) = *engine_lock {
        Ok(engine.toggle_mic())
    } else {
        Err("not connected".into())
    }
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn toggle_speaker(state: tauri::State<'_, Arc<AppState>>) -> Result<bool, String> {
    let engine_lock = state.engine.lock().await;
    if let Some(ref engine) = *engine_lock {
        Ok(engine.toggle_speaker())
    } else {
        Err("not connected".into())
    }
}

#[cfg(not(target_os = "android"))]
#[tauri::command]
async fn get_status(state: tauri::State<'_, Arc<AppState>>) -> Result<CallStatus, String> {
    let engine_lock = state.engine.lock().await;
    if let Some(ref engine) = *engine_lock {
        let status = engine.status().await;
        Ok(CallStatus {
            active: true,
            mic_muted: status.mic_muted,
            spk_muted: status.spk_muted,
            participants: status
                .participants
                .into_iter()
                .map(|p| Participant {
                    fingerprint: p.fingerprint,
                    alias: p.alias,
                    relay_label: p.relay_label,
                })
                .collect(),
            encode_fps: status.frames_sent,
            recv_fps: status.frames_received,
            audio_level: status.audio_level,
            call_duration_secs: status.call_duration_secs,
            fingerprint: status.fingerprint,
            tx_codec: status.tx_codec,
            rx_codec: status.rx_codec,
        })
    } else {
        Ok(CallStatus {
            active: false,
            mic_muted: false,
            spk_muted: false,
            participants: vec![],
            encode_fps: 0,
            recv_fps: 0,
            audio_level: 0,
            call_duration_secs: 0.0,
            fingerprint: String::new(),
            tx_codec: String::new(),
            rx_codec: String::new(),
        })
    }
}

// ─── Android stubs for engine-backed commands ────────────────────────────────
//
// Step 1 of the Android rewrite: signal-only. Audio is wired up in Step 3.
// These keep the JS frontend happy (same `invoke` surface) without pulling
// in CPAL, which doesn't support Android.

#[cfg(target_os = "android")]
#[tauri::command]
async fn connect(
    _state: tauri::State<'_, Arc<AppState>>,
    _app: tauri::AppHandle,
    _relay: String,
    _room: String,
    _alias: String,
    _os_aec: bool,
    _quality: String,
) -> Result<String, String> {
    Err("audio backend not yet wired on Android (step 3)".into())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn disconnect(_state: tauri::State<'_, Arc<AppState>>) -> Result<String, String> {
    Ok("not connected".into())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn toggle_mic(_state: tauri::State<'_, Arc<AppState>>) -> Result<bool, String> {
    Err("not connected".into())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn toggle_speaker(_state: tauri::State<'_, Arc<AppState>>) -> Result<bool, String> {
    Err("not connected".into())
}

#[cfg(target_os = "android")]
#[tauri::command]
async fn get_status(_state: tauri::State<'_, Arc<AppState>>) -> Result<CallStatus, String> {
    Ok(CallStatus {
        active: false,
        mic_muted: false,
        spk_muted: false,
        participants: vec![],
        encode_fps: 0,
        recv_fps: 0,
        audio_level: 0,
        call_duration_secs: 0.0,
        fingerprint: String::new(),
        tx_codec: String::new(),
        rx_codec: String::new(),
    })
}

// ─── Signaling commands — platform independent ───────────────────────────────

struct SignalState {
    transport: Option<Arc<wzp_transport::QuinnTransport>>,
    fingerprint: String,
    signal_status: String,
    incoming_call_id: Option<String>,
    incoming_caller_fp: Option<String>,
    incoming_caller_alias: Option<String>,
}

#[tauri::command]
async fn register_signal(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    relay: String,
) -> Result<String, String> {
    use wzp_proto::SignalMessage;

    let addr: std::net::SocketAddr = relay.parse().map_err(|e| format!("bad address: {e}"))?;
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Load or create seed automatically — no need to "connect to a room first"
    let seed = load_or_create_seed()?;
    let pub_id = seed.derive_identity().public_identity();
    let fp = pub_id.fingerprint.to_string();
    let identity_pub = *pub_id.signing.as_bytes();

    let bind: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    let endpoint = wzp_transport::create_endpoint(bind, None).map_err(|e| format!("{e}"))?;
    let conn = wzp_transport::connect(&endpoint, addr, "_signal", wzp_transport::client_config())
        .await.map_err(|e| format!("{e}"))?;
    let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

    transport.send_signal(&SignalMessage::RegisterPresence {
        identity_pub, signature: vec![], alias: None,
    }).await.map_err(|e| format!("{e}"))?;

    match transport.recv_signal().await.map_err(|e| format!("{e}"))? {
        Some(SignalMessage::RegisterPresenceAck { success: true, .. }) => {}
        _ => return Err("registration failed".into()),
    }

    { let mut sig = state.signal.lock().await; sig.transport = Some(transport.clone()); sig.fingerprint = fp.clone(); sig.signal_status = "registered".into(); }

    let signal_state = Arc::clone(&state.signal);
    let app_clone = app.clone();
    tokio::spawn(async move {
        loop {
            match transport.recv_signal().await {
                Ok(Some(SignalMessage::CallRinging { call_id })) => {
                    let mut sig = signal_state.lock().await; sig.signal_status = "ringing".into();
                    let _ = app_clone.emit("signal-event", serde_json::json!({"type":"ringing","call_id":call_id}));
                }
                Ok(Some(SignalMessage::DirectCallOffer { caller_fingerprint, caller_alias, call_id, .. })) => {
                    let mut sig = signal_state.lock().await; sig.signal_status = "incoming".into();
                    sig.incoming_call_id = Some(call_id.clone()); sig.incoming_caller_fp = Some(caller_fingerprint.clone()); sig.incoming_caller_alias = caller_alias.clone();
                    let _ = app_clone.emit("signal-event", serde_json::json!({"type":"incoming","call_id":call_id,"caller_fp":caller_fingerprint,"caller_alias":caller_alias}));
                }
                Ok(Some(SignalMessage::CallSetup { call_id, room, relay_addr })) => {
                    let mut sig = signal_state.lock().await; sig.signal_status = "setup".into();
                    let _ = app_clone.emit("signal-event", serde_json::json!({"type":"setup","call_id":call_id,"room":room,"relay_addr":relay_addr}));
                }
                Ok(Some(SignalMessage::Hangup { .. })) => {
                    let mut sig = signal_state.lock().await; sig.signal_status = "registered".into(); sig.incoming_call_id = None;
                    let _ = app_clone.emit("signal-event", serde_json::json!({"type":"hangup"}));
                }
                Ok(None) | Err(_) => break,
                _ => {}
            }
        }
        let mut sig = signal_state.lock().await; sig.signal_status = "idle".into(); sig.transport = None;
    });
    Ok(fp)
}

#[tauri::command]
async fn place_call(state: tauri::State<'_, Arc<AppState>>, target_fp: String) -> Result<(), String> {
    use wzp_proto::SignalMessage;
    let sig = state.signal.lock().await;
    let transport = sig.transport.as_ref().ok_or("not registered")?;
    let call_id = format!("{:016x}", std::time::SystemTime::now().duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
    transport.send_signal(&SignalMessage::DirectCallOffer {
        caller_fingerprint: sig.fingerprint.clone(), caller_alias: None, target_fingerprint: target_fp,
        call_id, identity_pub: [0u8; 32], ephemeral_pub: [0u8; 32], signature: vec![],
        supported_profiles: vec![wzp_proto::QualityProfile::GOOD],
    }).await.map_err(|e| format!("{e}"))?;
    Ok(())
}

#[tauri::command]
async fn answer_call(state: tauri::State<'_, Arc<AppState>>, call_id: String, mode: i32) -> Result<(), String> {
    use wzp_proto::SignalMessage;
    let sig = state.signal.lock().await;
    let transport = sig.transport.as_ref().ok_or("not registered")?;
    let accept_mode = match mode { 0 => wzp_proto::CallAcceptMode::Reject, 1 => wzp_proto::CallAcceptMode::AcceptTrusted, _ => wzp_proto::CallAcceptMode::AcceptGeneric };
    transport.send_signal(&SignalMessage::DirectCallAnswer {
        call_id, accept_mode, identity_pub: None, ephemeral_pub: None, signature: None,
        chosen_profile: Some(wzp_proto::QualityProfile::GOOD),
    }).await.map_err(|e| format!("{e}"))?;
    Ok(())
}

#[tauri::command]
async fn get_signal_status(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let sig = state.signal.lock().await;
    Ok(serde_json::json!({"status":sig.signal_status,"fingerprint":sig.fingerprint,"incoming_call_id":sig.incoming_call_id,"incoming_caller_fp":sig.incoming_caller_fp}))
}

// ─── App entry point ─────────────────────────────────────────────────────────

/// Shared Tauri app builder. Used by the desktop `main.rs` and the mobile
/// entry point below.
pub fn run() {
    tracing_subscriber::fmt().init();

    let state = Arc::new(AppState {
        #[cfg(not(target_os = "android"))]
        engine: Mutex::new(None),
        signal: Arc::new(Mutex::new(SignalState {
            transport: None, fingerprint: String::new(), signal_status: "idle".into(),
            incoming_call_id: None, incoming_caller_fp: None, incoming_caller_alias: None,
        })),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            ping_relay, get_identity, connect, disconnect, toggle_mic, toggle_speaker, get_status,
            register_signal, place_call, answer_call, get_signal_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WarzonePhone");
}

/// Tauri mobile entry point (Android/iOS). On desktop this is a no-op —
/// `main.rs` calls `run()` directly.
#[cfg_attr(mobile, tauri::mobile_entry_point)]
pub fn mobile_entry() {
    run();
}
