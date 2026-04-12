// WarzonePhone Tauri backend — shared between desktop (macOS/Windows/Linux)
// and Tauri mobile (Android/iOS). Platform-specific audio is cfg-gated.

#![cfg_attr(
    all(not(debug_assertions), target_os = "windows"),
    windows_subsystem = "windows"
)]

// Call engine — now compiled on every platform. On desktop it runs the real
// CPAL/VPIO audio pipeline; on Android the engine calls into the standalone
// wzp-native cdylib (via the wzp_native module) for Oboe-backed audio.
mod engine;

// Android runtime binding to libwzp_native.so (Oboe audio backend, built as
// a standalone cdylib with cargo-ndk to avoid the Tauri staticlib symbol
// leak — see docs/incident-tauri-android-init-tcb.md).
#[cfg(target_os = "android")]
mod wzp_native;

// Android AudioManager bridge (routing earpiece / speaker / BT).
#[cfg(target_os = "android")]
mod android_audio;

// Direct-call history store (persisted JSON in app data dir).
mod history;

// CallEngine has a unified impl on both targets now — the Android branch of
// CallEngine::start() routes audio through the standalone wzp-native cdylib
// (loaded via the wzp_native module below), the desktop branch uses CPAL.
use engine::CallEngine;

use serde::Serialize;
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use tauri::{Emitter, Manager};
use tokio::sync::Mutex;
use wzp_proto::MediaTransport;

// ─── Call-flow debug logs (GUI-gated) ────────────────────────────────
//
// Runtime-toggleable verbose logging for every step in the
// signaling + call setup path. When the user enables "Call flow
// debug logs" in the settings panel, `emit_call_debug!` fires a
// `call-debug-log` Tauri event that JS picks up and renders into a
// rolling debug panel so the user can see exactly where a call
// progressed or stalled — no logcat parsing needed.
//
// Mirrors the existing `wzp_codec::dred_verbose_logs` pattern.

static CALL_DEBUG_LOGS: AtomicBool = AtomicBool::new(false);

#[inline]
fn call_debug_logs_enabled() -> bool {
    CALL_DEBUG_LOGS.load(Ordering::Relaxed)
}

fn set_call_debug_logs_internal(on: bool) {
    CALL_DEBUG_LOGS.store(on, Ordering::Relaxed);
}

/// Emit a `call-debug-log` event to the JS side IF the flag is on.
/// Also mirrors to `tracing::info!` so logcat keeps its copy
/// regardless of the flag — the toggle only controls the GUI
/// overlay, not the underlying Android log stream.
pub(crate) fn emit_call_debug(
    app: &tauri::AppHandle,
    step: &str,
    details: serde_json::Value,
) {
    tracing::info!(step, ?details, "call-debug");
    if !call_debug_logs_enabled() {
        return;
    }
    let payload = serde_json::json!({
        "ts_ms": std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .map(|d| d.as_millis() as u64)
            .unwrap_or(0),
        "step": step,
        "details": details,
    });
    let _ = app.emit("call-debug-log", payload);
}

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

/// Toggle DRED verbose logging at runtime (gates the chatty per-frame
/// reconstruction + parse logs in opus_enc and engine.rs). Wired to the
/// "DRED debug logs" checkbox in the GUI settings panel.
#[tauri::command]
fn set_dred_verbose_logs(enabled: bool) {
    wzp_codec::set_dred_verbose_logs(enabled);
    tracing::info!(enabled, "DRED verbose logs toggled");
}

/// Read the current DRED verbose logging flag (so the GUI can hydrate
/// its checkbox on startup without trusting localStorage alone).
#[tauri::command]
fn get_dred_verbose_logs() -> bool {
    wzp_codec::dred_verbose_logs()
}

/// Phase 3.5 call-flow debug logs toggle. Gates the live
/// `call-debug-log` Tauri events that the GUI renders into a
/// rolling debug panel. Does NOT affect logcat — tracing::info
/// always runs regardless so the Android log stream keeps its
/// copy.
#[tauri::command]
fn set_call_debug_logs(enabled: bool) {
    set_call_debug_logs_internal(enabled);
    tracing::info!(enabled, "call-flow debug logs toggled");
}

#[tauri::command]
fn get_call_debug_logs() -> bool {
    call_debug_logs_enabled()
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

#[tauri::command]
async fn connect(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    relay: String,
    room: String,
    alias: String,
    os_aec: bool,
    quality: String,
    // Phase 3 hole-punching: peer's server-reflexive address
    // cross-wired by the relay in CallSetup.peer_direct_addr.
    peer_direct_addr: Option<String>,
    // Phase 5.5: peer's LAN host candidates from CallSetup.
    // Optional so the room-join path (which has no peer addrs)
    // can omit it entirely — it's only populated on direct calls.
    peer_local_addrs: Option<Vec<String>>,
) -> Result<String, String> {
    emit_call_debug(&app, "connect:start", serde_json::json!({
        "relay": relay,
        "room": room,
        "peer_direct_addr": peer_direct_addr,
        "peer_local_addrs": peer_local_addrs,
    }));
    let mut engine_lock = state.engine.lock().await;
    if engine_lock.is_some() {
        emit_call_debug(&app, "connect:already_connected", serde_json::json!({}));
        return Err("already connected".into());
    }

    // Phase 3.5: dual-path QUIC race.
    //
    // If the relay cross-wired a peer_direct_addr into the
    // CallSetup, we read our own reflex addr from SignalState
    // (populated earlier by place_call/answer_call's reflect query)
    // and use determine_role() to decide whether we're the
    // Acceptor (smaller addr, listens) or Dialer (larger addr,
    // dials). Both roles also dial the relay in parallel as a
    // fallback. Whichever transport completes first becomes the
    // media transport we hand to CallEngine::start.
    //
    // If ANY of the inputs is missing (no peer_direct_addr, no
    // own_reflex_addr, unparseable addrs, equal addrs), we skip
    // the race entirely and fall back to the pure-relay path —
    // identical to Phase 0 behavior.
    let (own_reflex_addr, signal_endpoint_for_race) = {
        let sig = state.signal.lock().await;
        (sig.own_reflex_addr.clone(), sig.endpoint.clone())
    };
    let peer_addr_parsed: Option<std::net::SocketAddr> = peer_direct_addr
        .as_deref()
        .and_then(|s| s.parse().ok());
    let relay_addr_parsed: Option<std::net::SocketAddr> = relay.parse().ok();
    let role = wzp_client::reflect::determine_role(
        own_reflex_addr.as_deref(),
        peer_direct_addr.as_deref(),
    );

    // Phase 5.5: build the full peer candidate bundle (reflex +
    // LAN hosts). The dial_order helper will fan them out in
    // priority order for the D-role race.
    let peer_local_addrs_vec = peer_local_addrs.unwrap_or_default();
    let peer_local_parsed: Vec<std::net::SocketAddr> = peer_local_addrs_vec
        .iter()
        .filter_map(|s| s.parse().ok())
        .collect();

    let pre_connected_transport: Option<Arc<wzp_transport::QuinnTransport>> =
        match (role, relay_addr_parsed) {
            (Some(r), Some(relay_sockaddr))
                if peer_addr_parsed.is_some() || !peer_local_parsed.is_empty() =>
            {
                let candidates = wzp_client::dual_path::PeerCandidates {
                    reflexive: peer_addr_parsed,
                    local: peer_local_parsed.clone(),
                };
                tracing::info!(
                    role = ?r,
                    candidates = ?candidates.dial_order(),
                    %relay,
                    %room,
                    own = ?own_reflex_addr,
                    "connect: starting dual-path race"
                );
                emit_call_debug(&app, "connect:dual_path_race_start", serde_json::json!({
                    "role": format!("{:?}", r),
                    "peer_reflex": peer_addr_parsed.map(|a| a.to_string()),
                    "peer_local": peer_local_parsed.iter().map(|a| a.to_string()).collect::<Vec<_>>(),
                    "relay_addr": relay_sockaddr.to_string(),
                    "own_reflex_addr": own_reflex_addr,
                }));
                let room_sni = room.clone();
                let call_sni = format!("call-{room}");
                // Phase 5: pass the signal endpoint so the race
                // reuses ONE socket for listen + dial + relay.
                match wzp_client::dual_path::race(
                    r,
                    candidates,
                    relay_sockaddr,
                    room_sni,
                    call_sni,
                    signal_endpoint_for_race.clone(),
                )
                .await
                {
                    Ok((transport, path)) => {
                        tracing::info!(?path, "connect: dual-path race resolved");
                        emit_call_debug(&app, "connect:dual_path_race_won", serde_json::json!({
                            "path": format!("{:?}", path),
                        }));
                        Some(transport)
                    }
                    Err(e) => {
                        // Both paths failed — surface to the user.
                        // CallEngine::start below with None will try
                        // the relay once more using the old code path
                        // (which reuses the signal endpoint and has a
                        // longer timeout) so we don't unconditionally
                        // fail the call on a transient race blip.
                        tracing::warn!(error = %e, "connect: dual-path race failed, falling back to classic relay connect");
                        emit_call_debug(&app, "connect:dual_path_race_failed", serde_json::json!({
                            "error": e.to_string(),
                        }));
                        None
                    }
                }
            }
            _ => {
                tracing::info!(
                    has_peer_reflex = peer_direct_addr.is_some(),
                    has_peer_local = !peer_local_addrs_vec.is_empty(),
                    has_own = own_reflex_addr.is_some(),
                    ?role,
                    %relay,
                    %room,
                    "connect: skipping dual-path race (missing inputs), relay-only"
                );
                emit_call_debug(&app, "connect:dual_path_skipped", serde_json::json!({
                    "has_peer_reflex": peer_direct_addr.is_some(),
                    "has_peer_local": !peer_local_addrs_vec.is_empty(),
                    "has_own": own_reflex_addr.is_some(),
                    "role": format!("{:?}", role),
                }));
                None
            }
        };

    // If we previously opened a quinn::Endpoint for the signaling connection
    // (direct-call path), reuse it so the media connection shares the same
    // UDP socket. This side-steps the Android issue where a second
    // quinn::Endpoint silently hangs in the QUIC handshake.
    let reuse_endpoint = state.signal.lock().await.endpoint.clone();
    if reuse_endpoint.is_some() && pre_connected_transport.is_none() {
        tracing::info!("connect: reusing existing signal endpoint for media connection");
    }

    let app_clone = app.clone();
    emit_call_debug(&app, "connect:call_engine_starting", serde_json::json!({}));
    let app_for_engine = app.clone();
    match CallEngine::start(relay, room, alias, os_aec, quality, reuse_endpoint, pre_connected_transport, app_for_engine, move |event_kind, message| {
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
            emit_call_debug(&app, "connect:call_engine_started", serde_json::json!({}));
            Ok("connected".into())
        }
        Err(e) => {
            emit_call_debug(&app, "connect:call_engine_failed", serde_json::json!({ "error": e.to_string() }));
            Err(format!("{e}"))
        }
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

// ─── Audio routing (Android-specific, no-op on desktop) ─────────────────────

/// Switch the call audio between earpiece (`on=false`) and loudspeaker
/// (`on=true`). On Android this calls AudioManager.setSpeakerphoneOn via
/// JNI AND then stops and restarts the Oboe streams so AAudio reconfigures
/// with the new routing — without the restart, changing the speakerphone
/// state mid-call silently tears down the running AAudio streams on some
/// OEMs and both capture + playout stop producing data.
///
/// The Rust send/recv tokio tasks keep running during the ~60ms restart
/// window; they just observe empty reads / writes against the
/// process-global ring buffers, which is fine because the ring state
/// is preserved across stop+start.
#[tauri::command]
#[allow(unused_variables)]
async fn set_speakerphone(on: bool) -> Result<(), String> {
    #[cfg(target_os = "android")]
    {
        android_audio::set_speakerphone(on)?;
        if wzp_native::is_loaded() && wzp_native::audio_is_running() {
            tracing::info!(on, "set_speakerphone: restarting Oboe for route change");
            // Oboe's stop/start are sync C-FFI calls that block for ~400ms
            // on Nothing-class devices (Pixel is faster). Calling them
            // directly from an async Tauri command stalls the tokio
            // executor — the send/recv engine tasks were observed to
            // freeze for ~20 seconds across a few rapid speaker toggles,
            // piling up buffered QUIC datagrams and then flooding them
            // all at once when the runtime finally caught up.
            //
            // Fix: run the audio teardown + reopen on a dedicated
            // blocking thread so the runtime keeps scheduling everything
            // else. AAudio's requestStop returns only after the stream
            // is actually in Stopped state, so no explicit inter-call
            // sleep is needed.
            tokio::task::spawn_blocking(|| {
                wzp_native::audio_stop();
                wzp_native::audio_start()
                    .map_err(|code| format!("audio_start after speakerphone toggle: code {code}"))
            })
            .await
            .map_err(|e| format!("spawn_blocking join: {e}"))??;
            tracing::info!("set_speakerphone: Oboe restarted");
        }
        Ok(())
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(())
    }
}

/// Query whether the call is currently routed to the loudspeaker.
#[tauri::command]
async fn is_speakerphone_on() -> Result<bool, String> {
    #[cfg(target_os = "android")]
    {
        android_audio::is_speakerphone_on()
    }
    #[cfg(not(target_os = "android"))]
    {
        Ok(false)
    }
}

// ─── Call history commands ───────────────────────────────────────────────────

#[tauri::command]
fn get_call_history() -> Vec<history::CallHistoryEntry> {
    history::all()
}

#[tauri::command]
fn get_recent_contacts() -> Vec<history::CallHistoryEntry> {
    history::contacts()
}

#[tauri::command]
fn clear_call_history() -> Result<(), String> {
    history::clear();
    Ok(())
}

// ─── Signaling commands — platform independent ───────────────────────────────

struct SignalState {
    transport: Option<Arc<wzp_transport::QuinnTransport>>,
    /// The quinn::Endpoint backing the signal connection. Reused for the
    /// media connection when a direct call is accepted — Android phones
    /// silently drop packets from a second quinn::Endpoint to the same
    /// relay, so every call after register_signal MUST share this socket.
    endpoint: Option<wzp_transport::Endpoint>,
    fingerprint: String,
    signal_status: String,
    incoming_call_id: Option<String>,
    incoming_caller_fp: Option<String>,
    incoming_caller_alias: Option<String>,
    /// Pending `ReflectResponse` channel. When the `get_reflected_address`
    /// Tauri command fires, it drops a `oneshot::Sender<SocketAddr>` here
    /// before sending a `SignalMessage::Reflect`. The spawned recv loop
    /// picks the response off the next bi-stream and fires the sender.
    /// If another Reflect request comes in while one is pending, we
    /// replace the sender — the old receiver sees a `Cancelled` error
    /// and the caller retries.
    pending_reflect: Option<tokio::sync::oneshot::Sender<std::net::SocketAddr>>,
    /// Phase 3.5: this client's own server-reflexive address as last
    /// observed by a Reflect query. Populated by
    /// `try_reflect_own_addr` on success and read by the `connect`
    /// Tauri command to compute the deterministic role for the
    /// dual-path QUIC race against `peer_direct_addr`.
    own_reflex_addr: Option<String>,
    /// The relay address the user currently wants to be registered
    /// against. `Some` means "keep me connected" — the supervisor
    /// will auto-reconnect after unexpected drops. `None` means
    /// "user explicitly deregistered" — do not retry.
    ///
    /// Distinguishing these two cases is what lets relay
    /// restarts + transient network blips be transparent to the
    /// user: the recv loop dies, but because `desired_relay_addr`
    /// is still set, a supervisor task retries the full
    /// connect+register flow with exponential backoff until the
    /// relay is reachable again.
    desired_relay_addr: Option<String>,
    /// Single-flight guard: `true` while the reconnect supervisor
    /// task is actively trying to re-establish the signal
    /// connection. Prevents duplicate supervisors from spawning
    /// (recv loop exit races with a manual register_signal call).
    reconnect_in_progress: bool,
}

#[tauri::command]
async fn register_signal(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    relay: String,
) -> Result<String, String> {
    // Set the desired relay and handle the "already registered to
    // a different relay" transition. This is the public entry
    // point — settings-screen changes come through here.
    let already_same = {
        let sig = state.signal.lock().await;
        sig.transport.is_some()
            && sig.desired_relay_addr.as_deref() == Some(relay.as_str())
    };
    if already_same {
        // Idempotent: user hit "Register" twice on the same relay,
        // or the JS side re-called after a settings save that
        // didn't actually change the relay.
        let sig = state.signal.lock().await;
        return Ok(sig.fingerprint.clone());
    }

    // Tear down any existing registration (different relay → swap).
    internal_deregister(&state.signal, /*keep_desired=*/ false).await;

    // Announce the new desired state so the recv-loop exit path and
    // any running supervisor can see it.
    {
        let mut sig = state.signal.lock().await;
        sig.desired_relay_addr = Some(relay.clone());
    }

    do_register_signal(state.signal.clone(), app, relay).await
}

/// Close the current signal transport + clear derived state.
/// Used by `deregister` (with `keep_desired = false`, clearing
/// `desired_relay_addr`) and by the relay-swap path in
/// `register_signal` (also `keep_desired = false` — the caller
/// is about to set a new desired addr).
async fn internal_deregister(
    signal_state: &Arc<tokio::sync::Mutex<SignalState>>,
    keep_desired: bool,
) {
    let mut sig = signal_state.lock().await;
    if let Some(t) = sig.transport.take() {
        // Dropping the transport Arc closes the quinn connection;
        // calling close() explicitly is a no-op but neat.
        let _ = t.close().await;
    }
    sig.endpoint = None;
    sig.signal_status = "idle".into();
    sig.incoming_call_id = None;
    sig.incoming_caller_fp = None;
    sig.incoming_caller_alias = None;
    sig.pending_reflect = None;
    sig.own_reflex_addr = None;
    if !keep_desired {
        sig.desired_relay_addr = None;
    }
}

/// Core register flow, extracted so the Tauri command AND the
/// reconnect supervisor can both call it. Does the connect +
/// RegisterPresence + spawn-recv-loop dance.
///
/// Contract: `signal_state.desired_relay_addr` must already be
/// set to `Some(relay)` by the caller. On recv-loop exit, the
/// spawned task will check `desired_relay_addr` and (if still
/// Some) trigger the reconnect supervisor.
///
/// Explicit `+ Send` on the return type so the reconnect
/// supervisor (which lives inside a `tokio::spawn`) can await
/// this future without hitting auto-trait inference issues.
fn do_register_signal(
    signal_state: Arc<tokio::sync::Mutex<SignalState>>,
    app: tauri::AppHandle,
    relay: String,
) -> impl std::future::Future<Output = Result<String, String>> + Send {
    async move {
    use wzp_proto::SignalMessage;

    emit_call_debug(&app, "register_signal:start", serde_json::json!({ "relay": relay }));
    let addr: std::net::SocketAddr = relay.parse().map_err(|e| format!("bad address: {e}"))?;
    let _ = rustls::crypto::ring::default_provider().install_default();

    // Load or create seed automatically — no need to "connect to a room first"
    let seed = load_or_create_seed()?;
    let pub_id = seed.derive_identity().public_identity();
    let fp = pub_id.fingerprint.to_string();
    let identity_pub = *pub_id.signing.as_bytes();
    emit_call_debug(&app, "register_signal:identity_loaded", serde_json::json!({ "fingerprint": fp }));

    // Phase 5: single-socket Nebula-style architecture. The signal
    // endpoint is dual-purpose (client + server config). Every outbound
    // flow — signal, reflect probes, relay media dials, direct-P2P
    // dials — uses this same socket, so port-preserving NATs (MikroTik
    // masquerade is the big one) give us a stable external port that
    // peers can actually dial. The same socket also accepts incoming
    // direct-P2P connections during the dual-path race.
    //
    // Was `None` before Phase 5 — that produced a client-only endpoint
    // with a different internal port than later reflect / dual-path
    // endpoints, which made MikroTik look symmetric and broke direct
    // P2P because the advertised reflex port was not the listening
    // port.
    let bind: std::net::SocketAddr = "0.0.0.0:0".parse().unwrap();
    let (server_cfg, _cert_der) = wzp_transport::server_config();
    let endpoint = wzp_transport::create_endpoint(bind, Some(server_cfg))
        .map_err(|e| format!("{e}"))?;
    emit_call_debug(&app, "register_signal:endpoint_created", serde_json::json!({ "bind": bind.to_string() }));
    let conn = wzp_transport::connect(&endpoint, addr, "_signal", wzp_transport::client_config())
        .await
        .map_err(|e| {
            emit_call_debug(&app, "register_signal:connect_failed", serde_json::json!({ "error": e.to_string() }));
            format!("{e}")
        })?;
    let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));
    emit_call_debug(&app, "register_signal:quic_connected", serde_json::json!({ "relay": relay }));

    transport.send_signal(&SignalMessage::RegisterPresence {
        identity_pub, signature: vec![], alias: None,
    }).await.map_err(|e| format!("{e}"))?;
    emit_call_debug(&app, "register_signal:register_presence_sent", serde_json::json!({}));

    match transport.recv_signal().await.map_err(|e| format!("{e}"))? {
        Some(SignalMessage::RegisterPresenceAck { success: true, .. }) => {
            emit_call_debug(&app, "register_signal:ack_received", serde_json::json!({}));
        }
        _ => {
            emit_call_debug(&app, "register_signal:ack_failed", serde_json::json!({}));
            return Err("registration failed".into());
        }
    }

    {
        let mut sig = signal_state.lock().await;
        sig.transport = Some(transport.clone());
        sig.endpoint = Some(endpoint.clone());
        sig.fingerprint = fp.clone();
        sig.signal_status = "registered".into();
    }
    // Let the JS side know we've (re-)entered "registered" so any
    // "reconnecting..." banner can clear.
    let _ = app.emit(
        "signal-event",
        serde_json::json!({ "type": "registered", "fingerprint": fp }),
    );

    tracing::info!(%fp, "signal registered, spawning recv loop");
    emit_call_debug(&app, "register_signal:recv_loop_spawning", serde_json::json!({ "fingerprint": fp }));
    let signal_state_loop = signal_state.clone();
    let app_clone = app.clone();
    tokio::spawn(async move {
        // Capture for the exit-path reconnect trigger below.
        let signal_state = signal_state_loop.clone();
        loop {
            match transport.recv_signal().await {
                Ok(Some(SignalMessage::CallRinging { call_id })) => {
                    tracing::info!(%call_id, "signal: CallRinging");
                    emit_call_debug(&app_clone, "recv:CallRinging", serde_json::json!({ "call_id": call_id }));
                    let mut sig = signal_state.lock().await; sig.signal_status = "ringing".into();
                    let _ = app_clone.emit("signal-event", serde_json::json!({"type":"ringing","call_id":call_id}));
                }
                Ok(Some(SignalMessage::DirectCallOffer { caller_fingerprint, caller_alias, call_id, caller_reflexive_addr, .. })) => {
                    tracing::info!(%call_id, caller = %caller_fingerprint, "signal: DirectCallOffer");
                    emit_call_debug(&app_clone, "recv:DirectCallOffer", serde_json::json!({
                        "call_id": call_id,
                        "caller_fp": caller_fingerprint,
                        "caller_alias": caller_alias,
                        "caller_reflexive_addr": caller_reflexive_addr,
                    }));
                    let mut sig = signal_state.lock().await; sig.signal_status = "incoming".into();
                    sig.incoming_call_id = Some(call_id.clone()); sig.incoming_caller_fp = Some(caller_fingerprint.clone()); sig.incoming_caller_alias = caller_alias.clone();
                    // Log as a Missed entry up-front. If the user accepts
                    // the call, answer_call upgrades it to Received via
                    // history::mark_received_if_pending(call_id). If they
                    // reject or ignore, it stays Missed.
                    history::log(
                        call_id.clone(),
                        caller_fingerprint.clone(),
                        caller_alias.clone(),
                        history::CallDirection::Missed,
                    );
                    let _ = app_clone.emit("signal-event", serde_json::json!({"type":"incoming","call_id":call_id,"caller_fp":caller_fingerprint,"caller_alias":caller_alias}));
                    let _ = app_clone.emit("history-changed", ());
                }
                Ok(Some(SignalMessage::DirectCallAnswer { call_id, accept_mode, callee_reflexive_addr, .. })) => {
                    tracing::info!(%call_id, ?accept_mode, "signal: DirectCallAnswer (forwarded by relay)");
                    emit_call_debug(&app_clone, "recv:DirectCallAnswer", serde_json::json!({
                        "call_id": call_id,
                        "accept_mode": format!("{:?}", accept_mode),
                        "callee_reflexive_addr": callee_reflexive_addr,
                    }));
                }
                Ok(Some(SignalMessage::CallSetup { call_id, room, relay_addr, peer_direct_addr, peer_local_addrs })) => {
                    // Phase 3: peer_direct_addr carries the OTHER party's
                    // reflex addr. Phase 5.5: peer_local_addrs carries
                    // their LAN host candidates (usable for same-LAN
                    // direct dials that can't hairpin through the NAT).
                    tracing::info!(
                        %call_id,
                        %room,
                        %relay_addr,
                        peer_direct = ?peer_direct_addr,
                        peer_local = ?peer_local_addrs,
                        "signal: CallSetup — emitting setup event to JS"
                    );
                    emit_call_debug(&app_clone, "recv:CallSetup", serde_json::json!({
                        "call_id": call_id,
                        "room": room,
                        "relay_addr": relay_addr,
                        "peer_direct_addr": peer_direct_addr,
                        "peer_local_addrs": peer_local_addrs,
                    }));
                    let mut sig = signal_state.lock().await;
                    sig.signal_status = "setup".into();
                    let _ = app_clone.emit(
                        "signal-event",
                        serde_json::json!({
                            "type": "setup",
                            "call_id": call_id,
                            "room": room,
                            "relay_addr": relay_addr,
                            "peer_direct_addr": peer_direct_addr,
                            "peer_local_addrs": peer_local_addrs,
                        }),
                    );
                }
                Ok(Some(SignalMessage::Hangup { reason })) => {
                    tracing::info!(?reason, "signal: Hangup");
                    emit_call_debug(&app_clone, "recv:Hangup", serde_json::json!({ "reason": format!("{:?}", reason) }));
                    let mut sig = signal_state.lock().await; sig.signal_status = "registered".into(); sig.incoming_call_id = None;
                    let _ = app_clone.emit("signal-event", serde_json::json!({"type":"hangup"}));
                }
                Ok(Some(SignalMessage::ReflectResponse { observed_addr })) => {
                    // "STUN for QUIC" response — the relay told us our
                    // own server-reflexive address. If a Tauri command
                    // is currently awaiting this, fire the oneshot;
                    // otherwise log and drop (unsolicited responses
                    // from a confused relay shouldn't crash the loop).
                    tracing::info!(%observed_addr, "signal: ReflectResponse");
                    match observed_addr.parse::<std::net::SocketAddr>() {
                        Ok(parsed) => {
                            let mut sig = signal_state.lock().await;
                            if let Some(tx) = sig.pending_reflect.take() {
                                // `send` returns Err(addr) only if the
                                // receiver was dropped (caller timed out
                                // or canceled). Either way, nothing to
                                // do — the value is gone.
                                let _ = tx.send(parsed);
                            } else {
                                tracing::debug!(%observed_addr, "reflect: unsolicited response (no pending sender)");
                            }
                            let _ = app_clone.emit(
                                "signal-event",
                                serde_json::json!({"type":"reflect","observed_addr":observed_addr}),
                            );
                        }
                        Err(e) => {
                            tracing::warn!(%observed_addr, error = %e, "reflect: relay returned unparseable addr");
                            // Treat unparseable response as a failed
                            // request so the caller doesn't hang.
                            let mut sig = signal_state.lock().await;
                            let _ = sig.pending_reflect.take();
                        }
                    }
                }
                Ok(Some(other)) => {
                    tracing::debug!(?other, "signal: unhandled message");
                }
                Ok(None) => {
                    tracing::warn!("signal recv returned None — peer closed");
                    break;
                }
                Err(wzp_proto::TransportError::Deserialize(e)) => {
                    // Forward-compat: the relay sent us a
                    // SignalMessage variant we don't know yet
                    // (older client against a newer relay).
                    // Log and keep the signal connection alive —
                    // otherwise direct-call registration would
                    // silently die on any protocol bump.
                    tracing::warn!(error = %e, "signal recv: unknown variant, continuing");
                }
                Err(e) => {
                    tracing::warn!(error = %e, "signal recv error — breaking loop");
                    break;
                }
            }
        }
        tracing::warn!("signal recv loop exited — signal_status=idle, transport dropped");
        // Determine whether this was a user-requested close or an
        // unexpected drop. `desired_relay_addr.is_some()` means the
        // user still wants to be registered — spawn the reconnect
        // supervisor with exponential backoff.
        let (should_reconnect, desired_relay, already_reconnecting) = {
            let mut sig = signal_state.lock().await;
            sig.signal_status = "idle".into();
            sig.transport = None;
            (
                sig.desired_relay_addr.is_some(),
                sig.desired_relay_addr.clone(),
                sig.reconnect_in_progress,
            )
        };
        if should_reconnect && !already_reconnecting {
            if let Some(relay) = desired_relay {
                tracing::info!(%relay, "signal recv loop exited unexpectedly — spawning reconnect supervisor");
                emit_call_debug(
                    &app_clone,
                    "signal:reconnect_supervisor_spawning",
                    serde_json::json!({ "relay": relay }),
                );
                let _ = app_clone.emit(
                    "signal-event",
                    serde_json::json!({ "type": "reconnecting", "relay": relay }),
                );
                let state_for_sup = signal_state.clone();
                let app_for_sup = app_clone.clone();
                tokio::spawn(async move {
                    signal_reconnect_supervisor(state_for_sup, app_for_sup, relay).await;
                });
            }
        } else if should_reconnect && already_reconnecting {
            tracing::debug!("signal recv loop exited; reconnect supervisor already running");
        }
    });
    Ok(fp)
    } // end async move
} // end fn do_register_signal

/// Supervisor task: loops with exponential backoff, calling
/// `do_register_signal` until the relay comes back online. Exits
/// as soon as one attempt succeeds (the newly-spawned recv loop
/// owns the connection from that point on) OR the user clears
/// `desired_relay_addr` via `deregister`.
///
/// Backoff schedule: 1s, 2s, 4s, 8s, 15s, 30s (capped). Reset on
/// success or exit.
async fn signal_reconnect_supervisor(
    signal_state: Arc<tokio::sync::Mutex<SignalState>>,
    app: tauri::AppHandle,
    initial_relay: String,
) {
    // Claim the single-flight slot so a second exit-path trigger
    // or a manual register_signal doesn't spawn a duplicate.
    {
        let mut sig = signal_state.lock().await;
        if sig.reconnect_in_progress {
            tracing::debug!("reconnect supervisor: another already running, exiting");
            return;
        }
        sig.reconnect_in_progress = true;
    }

    let backoff_schedule_ms: [u64; 6] = [1_000, 2_000, 4_000, 8_000, 15_000, 30_000];
    let mut attempt: usize = 0;
    let mut current_relay = initial_relay;

    loop {
        // Has the user cleared the desired relay? If so, exit.
        let (desired, transport_is_some) = {
            let sig = signal_state.lock().await;
            (sig.desired_relay_addr.clone(), sig.transport.is_some())
        };
        let Some(desired) = desired else {
            tracing::info!("reconnect supervisor: desired_relay_addr cleared, exiting");
            break;
        };

        // Has something else already re-registered us (manual
        // register_signal won the race)? If so, exit.
        if transport_is_some {
            tracing::info!("reconnect supervisor: transport already set by another path, exiting");
            break;
        }

        // Has the desired relay changed under us? Switch to the new one.
        if desired != current_relay {
            tracing::info!(old = %current_relay, new = %desired, "reconnect supervisor: desired relay changed");
            current_relay = desired.clone();
            attempt = 0;
        }

        // Back off before the retry (skip on attempt 0 so the first
        // reconnect kicks in fast).
        if attempt > 0 {
            let idx = (attempt - 1).min(backoff_schedule_ms.len() - 1);
            let wait_ms = backoff_schedule_ms[idx];
            tracing::info!(
                attempt,
                wait_ms,
                relay = %current_relay,
                "reconnect supervisor: backing off"
            );
            emit_call_debug(
                &app,
                "signal:reconnect_backoff",
                serde_json::json!({ "attempt": attempt, "wait_ms": wait_ms, "relay": current_relay }),
            );
            tokio::time::sleep(std::time::Duration::from_millis(wait_ms)).await;
        }
        attempt += 1;

        // One-shot attempt. do_register_signal will set the
        // transport + spawn a fresh recv loop on success.
        //
        // CRITICAL: release our single-flight guard BEFORE
        // do_register_signal spawns the new recv loop, because that
        // recv loop's exit path also checks `reconnect_in_progress`
        // to decide whether to spawn a supervisor of its own. If we
        // held it here and later exited, the slot would be released
        // too late for the next drop to trigger a fresh supervisor.
        {
            let mut sig = signal_state.lock().await;
            sig.reconnect_in_progress = false;
        }

        emit_call_debug(
            &app,
            "signal:reconnect_attempt",
            serde_json::json!({ "attempt": attempt, "relay": current_relay }),
        );
        match do_register_signal(signal_state.clone(), app.clone(), current_relay.clone()).await {
            Ok(fp) => {
                tracing::info!(%fp, relay = %current_relay, "reconnect supervisor: success");
                emit_call_debug(
                    &app,
                    "signal:reconnect_ok",
                    serde_json::json!({ "fingerprint": fp, "relay": current_relay }),
                );
                return; // recv loop now owns the connection
            }
            Err(e) => {
                tracing::warn!(error = %e, relay = %current_relay, "reconnect supervisor: attempt failed");
                emit_call_debug(
                    &app,
                    "signal:reconnect_failed",
                    serde_json::json!({ "attempt": attempt, "error": e, "relay": current_relay }),
                );
                // Re-claim the single-flight slot for the next iteration.
                let mut sig = signal_state.lock().await;
                sig.reconnect_in_progress = true;
            }
        }
    }

    // Loop exited — clean up the slot if we still hold it.
    let mut sig = signal_state.lock().await;
    sig.reconnect_in_progress = false;
}

#[tauri::command]
async fn place_call(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    target_fp: String,
) -> Result<(), String> {
    use wzp_proto::SignalMessage;

    emit_call_debug(&app, "place_call:start", serde_json::json!({ "target_fp": target_fp }));

    // Phase 3 hole-punching: query our own reflex addr BEFORE the
    // offer so we can advertise it. Best-effort — a failed reflect
    // (old relay, transient error) falls back to `None` which
    // means the callee's CallSetup will have peer_direct_addr=None
    // and the whole call goes through the relay path unchanged.
    //
    // Critical: this call does its own state.signal.lock() usage and
    // MUST NOT be wrapped in an outer lock, or the recv loop's
    // ReflectResponse handler will deadlock on the same mutex.
    emit_call_debug(&app, "place_call:reflect_query_start", serde_json::json!({}));
    let state_inner: Arc<AppState> = (*state).clone();
    let own_reflex = try_reflect_own_addr(&state_inner).await.ok().flatten();
    if let Some(ref a) = own_reflex {
        tracing::info!(%a, "place_call: learned own reflex addr for hole-punching advertisement");
        emit_call_debug(&app, "place_call:reflect_query_ok", serde_json::json!({ "addr": a }));
    } else {
        tracing::info!("place_call: no reflex addr available, falling back to relay-only");
        emit_call_debug(&app, "place_call:reflect_query_none", serde_json::json!({}));
    }

    // Phase 5.5: gather LAN host candidates using the signal
    // endpoint's bound port so incoming dials land on the same
    // socket that's already listening.
    let caller_local_addrs: Vec<String> = {
        let sig = state.signal.lock().await;
        sig.endpoint
            .as_ref()
            .and_then(|ep| ep.local_addr().ok())
            .map(|la| {
                wzp_client::reflect::local_host_candidates(la.port())
                    .into_iter()
                    .map(|a| a.to_string())
                    .collect()
            })
            .unwrap_or_default()
    };
    emit_call_debug(&app, "place_call:host_candidates", serde_json::json!({
        "local_addrs": caller_local_addrs,
    }));

    let sig = state.signal.lock().await;
    let transport = sig.transport.as_ref().ok_or("not registered")?;
    let call_id = format!(
        "{:016x}",
        std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_nanos()
    );
    tracing::info!(%call_id, %target_fp, reflex = ?own_reflex, "place_call: sending DirectCallOffer");
    transport
        .send_signal(&SignalMessage::DirectCallOffer {
            caller_fingerprint: sig.fingerprint.clone(),
            caller_alias: None,
            target_fingerprint: target_fp.clone(),
            call_id: call_id.clone(),
            identity_pub: [0u8; 32],
            ephemeral_pub: [0u8; 32],
            signature: vec![],
            supported_profiles: vec![wzp_proto::QualityProfile::GOOD],
            caller_reflexive_addr: own_reflex.clone(),
            caller_local_addrs: caller_local_addrs.clone(),
        })
        .await
        .map_err(|e| {
            emit_call_debug(&app, "place_call:send_failed", serde_json::json!({ "error": e.to_string() }));
            format!("{e}")
        })?;
    emit_call_debug(&app, "place_call:offer_sent", serde_json::json!({
        "call_id": call_id,
        "target_fp": target_fp,
        "caller_reflexive_addr": own_reflex,
    }));
    history::log(call_id, target_fp, None, history::CallDirection::Placed);
    let _ = app.emit("history-changed", ());
    Ok(())
}

#[tauri::command]
async fn answer_call(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
    call_id: String,
    mode: i32,
) -> Result<(), String> {
    use wzp_proto::SignalMessage;
    let accept_mode = match mode {
        0 => wzp_proto::CallAcceptMode::Reject,
        1 => wzp_proto::CallAcceptMode::AcceptTrusted,
        _ => wzp_proto::CallAcceptMode::AcceptGeneric,
    };
    emit_call_debug(&app, "answer_call:start", serde_json::json!({
        "call_id": call_id,
        "accept_mode": format!("{:?}", accept_mode),
    }));

    // Phase 3 hole-punching: only AcceptTrusted reveals our reflex
    // addr. Privacy-mode (AcceptGeneric) and Reject explicitly do
    // NOT — leaking the callee's IP back to the caller in those
    // modes would defeat the entire point of AcceptGeneric.
    //
    // Like place_call, we MUST NOT hold state.signal.lock() across
    // the reflect await or the recv loop's ReflectResponse handler
    // will deadlock on the same mutex.
    let own_reflex = if accept_mode == wzp_proto::CallAcceptMode::AcceptTrusted {
        emit_call_debug(&app, "answer_call:reflect_query_start", serde_json::json!({}));
        let state_inner: Arc<AppState> = (*state).clone();
        let r = try_reflect_own_addr(&state_inner).await.ok().flatten();
        if let Some(ref a) = r {
            tracing::info!(%call_id, %a, "answer_call: learned own reflex addr for AcceptTrusted");
            emit_call_debug(&app, "answer_call:reflect_query_ok", serde_json::json!({ "addr": a }));
        } else {
            tracing::info!(%call_id, "answer_call: no reflex addr for AcceptTrusted, falling back to relay-only");
            emit_call_debug(&app, "answer_call:reflect_query_none", serde_json::json!({}));
        }
        r
    } else {
        // Reject / AcceptGeneric: keep the IP private.
        emit_call_debug(&app, "answer_call:privacy_mode_skip_reflect", serde_json::json!({}));
        None
    };

    // Phase 5.5: gather LAN host candidates (AcceptTrusted only
    // for symmetry with the reflex addr — privacy mode keeps
    // LAN addrs hidden too).
    let callee_local_addrs: Vec<String> =
        if accept_mode == wzp_proto::CallAcceptMode::AcceptTrusted {
            let sig = state.signal.lock().await;
            sig.endpoint
                .as_ref()
                .and_then(|ep| ep.local_addr().ok())
                .map(|la| {
                    wzp_client::reflect::local_host_candidates(la.port())
                        .into_iter()
                        .map(|a| a.to_string())
                        .collect()
                })
                .unwrap_or_default()
        } else {
            Vec::new()
        };
    emit_call_debug(&app, "answer_call:host_candidates", serde_json::json!({
        "local_addrs": callee_local_addrs,
    }));

    let sig = state.signal.lock().await;
    let transport = sig.transport.as_ref().ok_or_else(|| {
        tracing::warn!("answer_call: not registered (no transport)");
        "not registered".to_string()
    })?;
    tracing::info!(%call_id, ?accept_mode, reflex = ?own_reflex, "answer_call: sending DirectCallAnswer");
    transport
        .send_signal(&SignalMessage::DirectCallAnswer {
            call_id: call_id.clone(),
            accept_mode,
            identity_pub: None,
            ephemeral_pub: None,
            signature: None,
            chosen_profile: Some(wzp_proto::QualityProfile::GOOD),
            callee_reflexive_addr: own_reflex.clone(),
            callee_local_addrs: callee_local_addrs.clone(),
        })
        .await
        .map_err(|e| {
            tracing::error!(%call_id, error = %e, "answer_call: send_signal failed");
            emit_call_debug(&app, "answer_call:send_failed", serde_json::json!({ "error": e.to_string() }));
            format!("{e}")
        })?;
    tracing::info!(%call_id, "answer_call: DirectCallAnswer sent successfully");
    emit_call_debug(&app, "answer_call:answer_sent", serde_json::json!({
        "call_id": call_id,
        "accept_mode": format!("{:?}", accept_mode),
        "callee_reflexive_addr": own_reflex,
    }));
    // Upgrade the pending "Missed" entry to "Received" if the user
    // accepted (mode != Reject). Mode 0 = Reject → leave as Missed.
    if mode != 0 && history::mark_received_if_pending(&call_id) {
        let _ = app.emit("history-changed", ());
    }
    Ok(())
}

/// Internal reflect helper shared by `get_reflected_address` and the
/// hole-punching path in `place_call` / `answer_call`.
///
/// Must be called WITHOUT holding `state.signal.lock()` — the recv
/// loop acquires the same lock to fire the oneshot, so holding it
/// across the await would deadlock.
///
/// Returns `Ok(Some(addr))` on success, `Ok(None)` if reflect is
/// unsupported / timed out / transport failed (caller should
/// gracefully continue with a relay-only path), or `Err` on
/// "not registered" which is a hard precondition failure.
async fn try_reflect_own_addr(
    state: &Arc<AppState>,
) -> Result<Option<String>, String> {
    use wzp_proto::SignalMessage;
    let (tx, rx) = tokio::sync::oneshot::channel::<std::net::SocketAddr>();
    let transport = {
        let mut sig = state.signal.lock().await;
        sig.pending_reflect = Some(tx);
        sig.transport
            .as_ref()
            .ok_or_else(|| "not registered".to_string())?
            .clone()
    };
    if let Err(e) = transport.send_signal(&SignalMessage::Reflect).await {
        let mut sig = state.signal.lock().await;
        sig.pending_reflect = None;
        tracing::warn!(error = %e, "try_reflect_own_addr: send_signal failed, continuing without reflex addr");
        return Ok(None);
    }
    match tokio::time::timeout(std::time::Duration::from_millis(1000), rx).await {
        Ok(Ok(addr)) => {
            // Phase 3.5: cache the result on SignalState so the
            // `connect` command can read it later for role
            // determination without another reflect round-trip.
            let s = addr.to_string();
            {
                let mut sig = state.signal.lock().await;
                sig.own_reflex_addr = Some(s.clone());
            }
            Ok(Some(s))
        }
        Ok(Err(_canceled)) => {
            tracing::warn!("try_reflect_own_addr: oneshot canceled");
            Ok(None)
        }
        Err(_elapsed) => {
            let mut sig = state.signal.lock().await;
            sig.pending_reflect = None;
            tracing::warn!("try_reflect_own_addr: 1s timeout (pre-Phase-1 relay?)");
            Ok(None)
        }
    }
}

/// "STUN for QUIC" — ask the relay what our own public address looks
/// like from its side of the TLS-authenticated signal connection.
///
/// Wire flow:
///   1. We install a `oneshot::Sender` in `SignalState.pending_reflect`
///      (replacing any stale one — last request wins).
///   2. We release the state lock and send `SignalMessage::Reflect`
///      over the existing transport. The relay opens a fresh bi-stream
///      on its side to respond, which the spawned recv loop picks up.
///   3. The recv loop's `ReflectResponse` match arm takes the sender
///      back out and fires it with the parsed `SocketAddr`.
///   4. We await the receiver with a 1s timeout so a non-reflecting
///      relay (pre-Phase-1 build) doesn't hang the UI forever.
///
/// Returns the addr as a string so it can cross the Tauri IPC
/// boundary unchanged — JS-side can display it directly or parse it
/// with `new URL(...)` / a regex if needed.
#[tauri::command]
async fn get_reflected_address(
    state: tauri::State<'_, Arc<AppState>>,
) -> Result<String, String> {
    use wzp_proto::SignalMessage;
    let (tx, rx) = tokio::sync::oneshot::channel::<std::net::SocketAddr>();
    let transport = {
        let mut sig = state.signal.lock().await;
        // Drop any older pending sender — we don't support more than
        // one in-flight Reflect per connection. A prior request whose
        // receiver has timed out will be cleaned up here automatically.
        sig.pending_reflect = Some(tx);
        sig.transport
            .as_ref()
            .ok_or_else(|| "not registered".to_string())?
            .clone()
    };
    if let Err(e) = transport.send_signal(&SignalMessage::Reflect).await {
        // Clean up the pending sender so the next attempt doesn't see
        // a stale channel. Re-acquire the lock inline since we already
        // released it above to release `transport` back to the caller.
        let mut sig = state.signal.lock().await;
        sig.pending_reflect = None;
        return Err(format!("send Reflect: {e}"));
    }

    // 1s is plenty for a same-datacenter relay (< 50ms RTT) and also
    // the ceiling for "something's wrong, tell the user" — any older
    // relay will never reply at all. 1100ms in the integration test.
    match tokio::time::timeout(std::time::Duration::from_millis(1000), rx).await {
        Ok(Ok(addr)) => Ok(addr.to_string()),
        Ok(Err(_canceled)) => {
            // The recv loop dropped the sender (relay returned
            // unparseable addr, or loop exited mid-request).
            Err("reflect channel canceled (signal loop exited or parse error)".into())
        }
        Err(_elapsed) => {
            // Timeout — strip the pending sender so the next attempt
            // starts clean. Old (pre-Phase-1) relays will land here.
            let mut sig = state.signal.lock().await;
            sig.pending_reflect = None;
            Err("reflect timeout (relay may not support reflection)".into())
        }
    }
}

/// Phase 2 of the "STUN for QUIC" rollout — probe multiple relays
/// in parallel to classify this client's NAT type. See
/// `wzp_client::reflect` for the per-probe logic and the pure
/// classifier.
///
/// This does NOT touch the registered `SignalState` — each probe
/// opens a fresh throwaway QUIC endpoint so the OS gives it a
/// fresh ephemeral source port. Sharing one endpoint across probes
/// would make a symmetric NAT look like a cone NAT, which is
/// exactly the failure mode we're trying to detect.
///
/// Takes the relay list from JS because the GUI owns the relay
/// config (localStorage `wzp-settings.relays`). Frontend passes it
/// in; Rust side just does the network work.
#[tauri::command]
async fn detect_nat_type(
    state: tauri::State<'_, Arc<AppState>>,
    relays: Vec<RelayArg>,
) -> Result<serde_json::Value, String> {
    // Parse relay args up front so a single malformed entry fails
    // the whole call cleanly instead of surfacing as a probe error
    // at the end.
    let mut parsed = Vec::with_capacity(relays.len());
    for r in relays {
        let addr: std::net::SocketAddr = r
            .address
            .parse()
            .map_err(|e| format!("bad relay address {:?}: {e}", r.address))?;
        parsed.push((r.name, addr));
    }

    // Phase 5: share the signal endpoint across all probes so
    // they emit from the same source port. Port-preserving NATs
    // (MikroTik, most consumer routers) give a stable external
    // port → classifier correctly sees cone instead of falsely
    // labeling SymmetricPort. Falls back to None (per-probe fresh
    // endpoint) when not registered.
    let shared_endpoint = state.signal.lock().await.endpoint.clone();

    // 1500ms per probe is generous: a same-host probe is < 10ms,
    // a cross-continent probe is typically < 300ms, and we want
    // to tolerate a one-off packet loss during connect.
    let detection = wzp_client::reflect::detect_nat_type(parsed, 1500, shared_endpoint).await;
    serde_json::to_value(&detection).map_err(|e| format!("serialize: {e}"))
}

/// Deserialization shim for the relay list coming from JS. The
/// `wzp-settings.relays` array in localStorage has more fields
/// (rtt, serverFingerprint, knownFingerprint) but we only need
/// name + address here.
#[derive(serde::Deserialize)]
struct RelayArg {
    name: String,
    address: String,
}

#[tauri::command]
async fn get_signal_status(state: tauri::State<'_, Arc<AppState>>) -> Result<serde_json::Value, String> {
    let sig = state.signal.lock().await;
    Ok(serde_json::json!({"status":sig.signal_status,"fingerprint":sig.fingerprint,"incoming_call_id":sig.incoming_call_id,"incoming_caller_fp":sig.incoming_caller_fp}))
}

/// Tear down the signal connection so the user goes back to idle. Called
/// when the user clicks "Deregister" on the direct-call screen. The
/// spawned recv loop will break out naturally when the transport closes,
/// AND — critically — clearing `desired_relay_addr` here tells that
/// exit path NOT to spawn a reconnect supervisor.
#[tauri::command]
async fn deregister(state: tauri::State<'_, Arc<AppState>>) -> Result<(), String> {
    internal_deregister(&state.signal, /*keep_desired=*/ false).await;
    tracing::info!("deregister: user-requested, desired_relay_addr cleared");
    Ok(())
}

/// End the current call, telling the peer via a signal-plane
/// `Hangup` message before tearing down the local media engine.
///
/// Prior to this command existing, the hangup button just called
/// `disconnect` which stopped the local engine but didn't notify
/// the peer — so the OTHER party stayed on the call screen with
/// nothing to hear. The relay DOES notice the media connection
/// closing but doesn't forward anything to the peer on its own,
/// so a real `SignalMessage::Hangup` is the only reliable signal.
///
/// Best-effort: if the signal transport is down (e.g. the relay
/// dropped us mid-call), we still tear down the engine locally
/// and return success. The peer's CallEngine will eventually
/// notice the media side dying and the signal-event hangup
/// handler will fire on receiving it from their signal loop if
/// the relay is still up on their side.
#[tauri::command]
async fn hangup_call(
    state: tauri::State<'_, Arc<AppState>>,
    app: tauri::AppHandle,
) -> Result<(), String> {
    use wzp_proto::SignalMessage;

    emit_call_debug(&app, "hangup_call:start", serde_json::json!({}));

    // Step 1: send Hangup over the signal channel so the relay
    // forwards it to the peer. Do this FIRST so the peer gets
    // the notification even if the engine shutdown takes a beat.
    {
        let sig = state.signal.lock().await;
        if let Some(ref transport) = sig.transport {
            match transport
                .send_signal(&SignalMessage::Hangup {
                    reason: wzp_proto::HangupReason::Normal,
                })
                .await
            {
                Ok(()) => {
                    tracing::info!("hangup_call: Hangup signal sent to relay");
                    emit_call_debug(&app, "hangup_call:signal_sent", serde_json::json!({}));
                }
                Err(e) => {
                    tracing::warn!(error = %e, "hangup_call: failed to send Hangup signal");
                    emit_call_debug(
                        &app,
                        "hangup_call:signal_send_failed",
                        serde_json::json!({ "error": e.to_string() }),
                    );
                }
            }
        } else {
            tracing::debug!("hangup_call: no signal transport, skipping Hangup send");
            emit_call_debug(&app, "hangup_call:no_signal_transport", serde_json::json!({}));
        }
    }

    // Step 2: tear down the local media engine.
    let mut engine_lock = state.engine.lock().await;
    if let Some(engine) = engine_lock.take() {
        engine.stop().await;
        emit_call_debug(&app, "hangup_call:engine_stopped", serde_json::json!({}));
    } else {
        emit_call_debug(&app, "hangup_call:no_engine", serde_json::json!({}));
    }
    Ok(())
}

// ─── App entry point ─────────────────────────────────────────────────────────

/// Shared Tauri app builder. Used by the desktop `main.rs` and the mobile
/// entry point below.
pub fn run() {
    tracing_subscriber::fmt().init();

    let state = Arc::new(AppState {
        engine: Mutex::new(None),
        signal: Arc::new(Mutex::new(SignalState {
            transport: None, endpoint: None, fingerprint: String::new(), signal_status: "idle".into(),
            incoming_call_id: None, incoming_caller_fp: None, incoming_caller_alias: None,
            pending_reflect: None,
            own_reflex_addr: None,
            desired_relay_addr: None,
            reconnect_in_progress: false,
        })),
    });

    tauri::Builder::default()
        .plugin(tauri_plugin_shell::init())
        .plugin(tauri_plugin_notification::init())
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

            // Load the standalone wzp-native cdylib (Oboe audio bridge) and
            // cache its exported function pointers. The library handle is
            // kept alive in a 'static OnceLock for the lifetime of the
            // process, so CallEngine::start() can invoke its audio FFI
            // from anywhere. See src/wzp_native.rs and the incident report
            // in docs/incident-tauri-android-init-tcb.md.
            #[cfg(target_os = "android")]
            {
                match wzp_native::init() {
                    Ok(()) => {
                        tracing::info!(
                            "wzp-native loaded: version={} msg=\"{}\"",
                            wzp_native::version(),
                            wzp_native::hello()
                        );
                    }
                    Err(e) => {
                        tracing::warn!("wzp-native init failed: {e}");
                    }
                }
            }

            Ok(())
        })
        .invoke_handler(tauri::generate_handler![
            ping_relay, get_identity, get_app_info,
            connect, disconnect, toggle_mic, toggle_speaker, get_status,
            register_signal, place_call, answer_call, get_signal_status,
            get_reflected_address, detect_nat_type,
            hangup_call,
            deregister,
            set_speakerphone, is_speakerphone_on,
            get_call_history, get_recent_contacts, clear_call_history,
            set_dred_verbose_logs, get_dred_verbose_logs,
            set_call_debug_logs, get_call_debug_logs,
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
