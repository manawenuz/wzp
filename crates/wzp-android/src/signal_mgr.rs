//! Persistent signal connection manager for direct 1:1 calls.
//!
//! Separate from the media engine — survives across calls.
//! Connects to relay via `_signal` SNI, registers presence,
//! and handles call signaling (offer/answer/setup/hangup).

use std::net::SocketAddr;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, Mutex};

use tracing::{error, info, warn};
use wzp_proto::{MediaTransport, SignalMessage};

/// Signal connection status.
#[derive(Clone, Debug, Default, serde::Serialize)]
pub struct SignalState {
    pub status: String, // "idle", "registered", "ringing", "incoming", "setup"
    pub fingerprint: String,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incoming_call_id: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incoming_caller_fp: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub incoming_caller_alias: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_setup_relay: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_setup_room: Option<String>,
    #[serde(skip_serializing_if = "Option::is_none")]
    pub call_setup_id: Option<String>,
}

/// Manages a persistent `_signal` QUIC connection to a relay.
pub struct SignalManager {
    transport: Arc<wzp_transport::QuinnTransport>,
    state: Arc<Mutex<SignalState>>,
    running: Arc<AtomicBool>,
}

impl SignalManager {
    /// Connect to relay, register presence, and start recv loop.
    /// This creates the SignalManager, spawns a thread that runs FOREVER
    /// (connect + register + recv loop all on ONE thread/runtime to avoid TLS conflicts).
    /// Returns immediately after spawning.
    pub fn start(relay_addr: &str, seed_hex: &str) -> Result<Self, anyhow::Error> {
        let addr: SocketAddr = relay_addr.parse()?;
        let seed = if seed_hex.is_empty() {
            wzp_crypto::Seed::generate()
        } else {
            wzp_crypto::Seed::from_hex(seed_hex).map_err(|e| anyhow::anyhow!(e))?
        };
        let identity = seed.derive_identity();
        let pub_id = identity.public_identity();
        let identity_pub = *pub_id.signing.as_bytes();
        let fp = pub_id.fingerprint.to_string();

        let state = Arc::new(Mutex::new(SignalState {
            status: "connecting".into(),
            fingerprint: fp.clone(),
            ..Default::default()
        }));

        let running = Arc::new(AtomicBool::new(true));

        // We need a transport Arc that's shared with the spawned thread.
        // The thread does connect + register + recv loop. We use a oneshot
        // channel to get the transport back after connect succeeds.
        let (tx, rx) = std::sync::mpsc::channel::<Arc<wzp_transport::QuinnTransport>>();

        let thread_state = Arc::clone(&state);
        let thread_running = Arc::clone(&running);
        let thread_fp = fp.clone();
        let return_state = Arc::clone(&state);
        let return_running = Arc::clone(&running);

        std::thread::Builder::new()
            .name("wzp-signal".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                // ONE runtime for the entire lifetime of this thread.
                // Never dropped until thread exits — avoids TLS cleanup conflicts.
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async move {
                    info!(fingerprint = %thread_fp, relay = %addr, "signal: connecting");

                    let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
                    let endpoint = match wzp_transport::create_endpoint(bind, None) {
                        Ok(e) => e,
                        Err(e) => { error!("signal endpoint: {e}"); return; }
                    };
                    let client_cfg = wzp_transport::client_config();
                    let conn = match wzp_transport::connect(&endpoint, addr, "_signal", client_cfg).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("signal connect failed: {e}");
                            let mut s = thread_state.lock().unwrap();
                            s.status = "idle".into();
                            return;
                        }
                    };
                    let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

                    // Register presence
                    if let Err(e) = transport.send_signal(&SignalMessage::RegisterPresence {
                        identity_pub,
                        signature: vec![],
                        alias: None,
                    }).await {
                        error!("signal register send: {e}");
                        let mut s = thread_state.lock().unwrap();
                        s.status = "idle".into();
                        return;
                    }

                    match transport.recv_signal().await {
                        Ok(Some(SignalMessage::RegisterPresenceAck { success: true, .. })) => {
                            info!(fingerprint = %thread_fp, "signal: registered");
                            let mut s = thread_state.lock().unwrap();
                            s.status = "registered".into();
                        }
                        other => {
                            error!("signal registration failed: {other:?}");
                            let mut s = thread_state.lock().unwrap();
                            s.status = "idle".into();
                            return;
                        }
                    }

                    // Send transport to the caller
                    let _ = tx.send(transport.clone());

                    // Recv loop — runs forever until stopped
            loop {
                if !running.load(Ordering::Relaxed) { break; }

                match transport.recv_signal().await {
                    Ok(Some(SignalMessage::CallRinging { call_id })) => {
                        info!(call_id = %call_id, "signal: ringing");
                        let mut s = state.lock().unwrap();
                        s.status = "ringing".into();
                    }
                    Ok(Some(SignalMessage::DirectCallOffer { caller_fingerprint, caller_alias, call_id, .. })) => {
                        info!(from = %caller_fingerprint, call_id = %call_id, "signal: incoming call");
                        let mut s = state.lock().unwrap();
                        s.status = "incoming".into();
                        s.incoming_call_id = Some(call_id);
                        s.incoming_caller_fp = Some(caller_fingerprint);
                        s.incoming_caller_alias = caller_alias;
                    }
                    Ok(Some(SignalMessage::DirectCallAnswer { call_id, accept_mode, .. })) => {
                        info!(call_id = %call_id, mode = ?accept_mode, "signal: call answered");
                    }
                    Ok(Some(SignalMessage::CallSetup { call_id, room, relay_addr })) => {
                        info!(call_id = %call_id, room = %room, relay = %relay_addr, "signal: call setup");
                        let mut s = state.lock().unwrap();
                        s.status = "setup".into();
                        s.call_setup_relay = Some(relay_addr);
                        s.call_setup_room = Some(room);
                        s.call_setup_id = Some(call_id);
                    }
                    Ok(Some(SignalMessage::Hangup { reason })) => {
                        info!(reason = ?reason, "signal: hangup");
                        let mut s = state.lock().unwrap();
                        s.status = "registered".into();
                        s.incoming_call_id = None;
                        s.incoming_caller_fp = None;
                        s.incoming_caller_alias = None;
                        s.call_setup_relay = None;
                        s.call_setup_room = None;
                        s.call_setup_id = None;
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

            let mut s = thread_state.lock().unwrap();
            s.status = "idle".into();
                }); // block_on — runtime lives until thread exits
            })?; // thread spawn

        // Wait for transport from the spawned thread (with timeout)
        let transport = rx.recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("signal connect timeout"))?;

        Ok(Self {
            transport,
            state: return_state,
            running: return_running,
        })
    }

    /// Get current state (non-blocking).
    pub fn get_state(&self) -> SignalState {
        self.state.lock().unwrap().clone()
    }

    /// Get state as JSON string.
    pub fn get_state_json(&self) -> String {
        serde_json::to_string(&self.get_state()).unwrap_or_else(|_| "{}".into())
    }

    /// Place a direct call.
    pub fn place_call(&self, target_fp: &str) -> Result<(), anyhow::Error> {
        let fp = self.state.lock().unwrap().fingerprint.clone();
        let target = target_fp.to_string();
        let call_id = format!("{:016x}", std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH).unwrap().as_nanos());
        let transport = self.transport.clone();

        // Send on a small thread (async send needs a runtime)
        std::thread::Builder::new()
            .name("wzp-call-send".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all().build().expect("rt");
                rt.block_on(async {
                    let _ = transport.send_signal(&SignalMessage::DirectCallOffer {
                        caller_fingerprint: fp,
                        caller_alias: None,
                        target_fingerprint: target,
                        call_id,
                        identity_pub: [0u8; 32],
                        ephemeral_pub: [0u8; 32],
                        signature: vec![],
                        supported_profiles: vec![wzp_proto::QualityProfile::GOOD],
                    }).await;
                });
            })?;
        Ok(())
    }

    /// Answer an incoming call.
    pub fn answer_call(&self, call_id: &str, mode: wzp_proto::CallAcceptMode) -> Result<(), anyhow::Error> {
        let call_id = call_id.to_string();
        let transport = self.transport.clone();

        std::thread::Builder::new()
            .name("wzp-answer-send".into())
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all().build().expect("rt");
                rt.block_on(async {
                    let _ = transport.send_signal(&SignalMessage::DirectCallAnswer {
                        call_id,
                        accept_mode: mode,
                        identity_pub: None,
                        ephemeral_pub: None,
                        signature: None,
                        chosen_profile: Some(wzp_proto::QualityProfile::GOOD),
                    }).await;
                });
            })?;
        Ok(())
    }

    /// Send hangup.
    pub fn hangup(&self) {
        let transport = self.transport.clone();
        let state = self.state.clone();
        std::thread::spawn(move || {
            let rt = tokio::runtime::Builder::new_current_thread()
                .enable_all().build().expect("rt");
            rt.block_on(async {
                let _ = transport.send_signal(&SignalMessage::Hangup {
                    reason: wzp_proto::HangupReason::Normal,
                }).await;
            });
            let mut s = state.lock().unwrap();
            s.status = "registered".into();
            s.incoming_call_id = None;
            s.incoming_caller_fp = None;
            s.incoming_caller_alias = None;
            s.call_setup_relay = None;
            s.call_setup_room = None;
            s.call_setup_id = None;
        });
    }

    /// Stop the signal connection.
    pub fn stop(&self) {
        self.running.store(false, Ordering::Release);
        self.transport.connection().close(0u32.into(), b"shutdown");
    }
}
