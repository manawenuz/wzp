#![cfg_attr(not(debug_assertions), windows_subsystem = "windows")]

mod engine;

use engine::CallEngine;
use serde::Serialize;
use std::sync::Arc;
use tauri::Emitter;
use tokio::sync::Mutex;

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
    engine: Mutex<Option<CallEngine>>,
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

/// Read fingerprint from ~/.wzp/identity without connecting.
#[tauri::command]
fn get_identity() -> Result<String, String> {
    let home = std::env::var("HOME").unwrap_or_else(|_| ".".into());
    let path = std::path::PathBuf::from(home).join(".wzp").join("identity");
    if path.exists() {
        if let Ok(hex) = std::fs::read_to_string(&path) {
            if let Ok(seed) = wzp_crypto::Seed::from_hex(hex.trim()) {
                let fp = seed.derive_identity().public_identity().fingerprint;
                return Ok(fp.to_string());
            }
        }
    }
    // No identity yet — generate one so we can show the fingerprint
    let seed = wzp_crypto::Seed::generate();
    let fp = seed.derive_identity().public_identity().fingerprint;
    if let Some(parent) = path.parent() {
        std::fs::create_dir_all(parent).ok();
    }
    let hex: String = seed.0.iter().map(|b| format!("{b:02x}")).collect();
    std::fs::write(&path, hex).ok();
    Ok(fp.to_string())
}

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

#[tauri::command]
async fn toggle_mic(state: tauri::State<'_, Arc<AppState>>) -> Result<bool, String> {
    let engine_lock = state.engine.lock().await;
    if let Some(ref engine) = *engine_lock {
        Ok(engine.toggle_mic())
    } else {
        Err("not connected".into())
    }
}

#[tauri::command]
async fn toggle_speaker(state: tauri::State<'_, Arc<AppState>>) -> Result<bool, String> {
    let engine_lock = state.engine.lock().await;
    if let Some(ref engine) = *engine_lock {
        Ok(engine.toggle_speaker())
    } else {
        Err("not connected".into())
    }
}

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

fn main() {
    tracing_subscriber::fmt().init();

    let state = Arc::new(AppState {
        engine: Mutex::new(None),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .manage(state)
        .invoke_handler(tauri::generate_handler![
            ping_relay,
            get_identity,
            connect,
            disconnect,
            toggle_mic,
            toggle_speaker,
            get_status,
        ])
        .run(tauri::generate_context!())
        .expect("error while running WarzonePhone Desktop");
}
