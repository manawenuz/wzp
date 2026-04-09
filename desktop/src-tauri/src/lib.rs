// WarzonePhone Tauri backend — shared between desktop (macOS/Windows/Linux)
// and Tauri mobile (Android/iOS). Platform-specific audio is cfg-gated.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// Call engine — now compiled on every platform. On desktop it runs the real
// CPAL/VPIO audio pipeline; on Android `CallEngine::start()` currently returns
// an error stub (see engine.rs — that's step C of the Oboe integration).
mod engine;

// CallEngine is only referenced from the non-Android connect/disconnect/etc
// commands; the Android stubs return errors directly.
#[cfg(not(target_os = "android"))]
use engine::CallEngine;

use serde::Serialize;
use std::path::PathBuf;
use std::sync::{Arc, OnceLock};
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;
use wzp_proto::MediaTransport;

/// Short git hash captured at compile time by build.rs.
const GIT_HASH: &str = env!("WZP_GIT_HASH");

/// Resolved by `setup()` once we have a Tauri AppHandle. Holds the
/// platform-correct app data dir (e.g. `/data/data/com.wzp.desktop/files` on
/// Android, `~/Library/Application Support/com.wzp.desktop` on macOS).
static APP_DATA_DIR: OnceLock<PathBuf> = OnceLock::new();

/// Adjective list — keep in sync with the noun list below. Both are powers of
/// 2 friendly so the modulo bias is negligible.
const ALIAS_ADJECTIVES: &[&str] = &[
    "Swift", "Silent", "Brave", "Calm", "Dark", "Fierce", "Ghost",
    "Iron", "Lucky", "Noble", "Quick", "Sharp", "Storm", "Wild",
    "Cold", "Bright", "Lone", "Red", "Grey", "Frosty", "Dusty",
    "Rusty", "Neon", "Void", "Solar", "Lunar", "Cyber", "Pixel",
    "Sonic", "Hyper", "Turbo", "Nano", "Mega", "Ultra", "Zinc",
];
const ALIAS_NOUNS: &[&str] = &[
    "Wolf", "Hawk", "Fox", "Bear", "Lynx", "Crow", "Viper",
    "Cobra", "Tiger", "Eagle", "Shark", "Raven", "Falcon", "Otter",
    "Mantis", "Panda", "Jackal", "Badger", "Heron", "Bison",
    "Condor", "Coyote", "Gecko", "Hornet", "Marten", "Osprey",
    "Parrot", "Puma", "Raptor", "Stork", "Toucan", "Walrus",
];

/// Derive a stable human-readable alias from the seed bytes. Same seed →
/// same alias forever, different seeds → effectively random aliases.
fn derive_alias(seed: &wzp_crypto::Seed) -> String {
    let adj_idx = (u16::from_le_bytes([seed.0[0], seed.0[1]]) as usize) % ALIAS_ADJECTIVES.len();
    let noun_idx = (u16::from_le_bytes([seed.0[2], seed.0[3]]) as usize) % ALIAS_NOUNS.len();
    format!("{} {}", ALIAS_ADJECTIVES[adj_idx], ALIAS_NOUNS[noun_idx])
}

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
/// Resolved at startup from Tauri's `path().app_data_dir()` API which gives
/// us the platform-correct app-private location:
///   - Android: `/data/data/<package_id>/files/com.wzp.desktop`
///   - macOS:   `~/Library/Application Support/com.wzp.desktop`
///   - Linux:   `~/.local/share/com.wzp.desktop`
///
/// Falls back to `$HOME/.wzp` on the desktop side if the OnceLock hasn't been
/// initialised yet (shouldn't happen in normal startup, but keeps the fn
/// total).
fn identity_dir() -> PathBuf {
    if let Some(dir) = APP_DATA_DIR.get() {
        return dir.clone();
    }
    #[cfg(target_os = "android")]
    {
        // Last-resort default. The real path is set in setup() below.
        std::path::PathBuf::from("/data/data/com.wzp.desktop/files")
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

/// Build/identity info shown on the home screen so the user can prove which
/// build is installed and what their stable alias is.
#[derive(Clone, Serialize)]
struct AppInfo {
    /// Short git commit hash captured at build time.
    git_hash: &'static str,
    /// Stable adjective+noun derived from the seed.
    alias: String,
    /// Full fingerprint, e.g. "abcd:ef01:..."
    fingerprint: String,
    /// App data dir actually in use — useful for debugging EACCES issues.
    data_dir: String,
}

#[tauri::command]
fn get_app_info() -> Result<AppInfo, String> {
    let seed = load_or_create_seed()?;
    let pub_id = seed.derive_identity().public_identity();
    Ok(AppInfo {
        git_hash: GIT_HASH,
        alias: derive_alias(&seed),
        fingerprint: pub_id.fingerprint.to_string(),
        data_dir: identity_dir().to_string_lossy().into_owned(),
    })
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
        .setup(|app| {
            // Resolve the platform-correct app data dir once at startup so
            // every command can read/write the seed without juggling AppHandle.
            let data_dir = app
                .path()
                .app_data_dir()
                .map(|p| p.join(".wzp"))
                .unwrap_or_else(|_| identity_dir());
            // create_dir_all is a no-op if it already exists.
            if let Err(e) = std::fs::create_dir_all(&data_dir) {
                tracing::warn!("failed to create app data dir {data_dir:?}: {e}");
            }
            tracing::info!("app data dir: {data_dir:?}");
            let _ = APP_DATA_DIR.set(data_dir);

            // Phase 1 smoke test of the separate-cdylib approach to the
            // __init_tcb crash (see docs/incident-tauri-android-init-tcb.md):
            // dlopen the sibling libwzp_native.so that gradle dropped into
            // our jniLibs directory and call its exported wzp_native_version()
            // + wzp_native_hello() functions. If this logs
            //   "wzp-native dlopen OK: version=42 msg=...",
            // we've validated the whole cdylib-split pipeline and can move
            // to Phase 2 (port the Oboe bridge into wzp-native).
            #[cfg(target_os = "android")]
            {
                match unsafe { libloading::Library::new("libwzp_native.so") } {
                    Ok(lib) => {
                        unsafe {
                            match lib.get::<unsafe extern "C" fn() -> i32>(b"wzp_native_version") {
                                Ok(version_fn) => {
                                    let v = version_fn();
                                    let mut buf = [0u8; 64];
                                    let msg = match lib.get::<unsafe extern "C" fn(*mut u8, usize) -> usize>(b"wzp_native_hello") {
                                        Ok(hello_fn) => {
                                            let n = hello_fn(buf.as_mut_ptr(), buf.len());
                                            String::from_utf8_lossy(&buf[..n]).into_owned()
                                        }
                                        Err(e) => format!("<no hello: {e}>"),
                                    };
                                    tracing::info!("wzp-native dlopen OK: version={v} msg=\"{msg}\"");
                                }
                                Err(e) => {
                                    tracing::warn!("wzp-native loaded but dlsym(wzp_native_version) failed: {e}");
                                }
                            }
                        }
                    }
                    Err(e) => {
                        tracing::warn!("wzp-native dlopen failed: {e}");
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ping_relay, get_identity, get_app_info,
            connect, disconnect, toggle_mic, toggle_speaker, get_status,
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
