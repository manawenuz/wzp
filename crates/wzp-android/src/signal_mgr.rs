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
    /// Create SignalManager and start connect+register+recv on a background thread.
    /// Returns immediately. The internal thread runs forever.
    /// CRITICAL: tokio runtime must never be dropped on Android (libcrypto TLS conflict).
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

        // Channel to receive transport after connect succeeds
        let (transport_tx, transport_rx) = std::sync::mpsc::channel();

        let bg_state = Arc::clone(&state);
        let bg_running = Arc::clone(&running);
        let ret_state = Arc::clone(&state);
        let ret_running = Arc::clone(&running);

        // ONE thread, ONE runtime, NEVER dropped.
        // Connect + register + recv loop all happen here.
        std::thread::Builder::new()
            .name("wzp-signal".into())
            .stack_size(4 * 1024 * 1024)
            .spawn(move || {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_all()
                    .build()
                    .expect("tokio runtime");

                rt.block_on(async move {
                    info!(fingerprint = %fp, relay = %addr, "signal: connecting");

                    let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
                    let endpoint = match wzp_transport::create_endpoint(bind, None) {
                        Ok(e) => e,
                        Err(e) => {
                            error!("signal endpoint: {e}");
                            bg_state.lock().unwrap().status = "idle".into();
                            return;
                        }
                    };
                    let client_cfg = wzp_transport::client_config();
                    let conn = match wzp_transport::connect(&endpoint, addr, "_signal", client_cfg).await {
                        Ok(c) => c,
                        Err(e) => {
                            error!("signal connect: {e}");
                            bg_state.lock().unwrap().status = "idle".into();
                            return;
                        }
                    };
                    let transport = Arc::new(wzp_transport::QuinnTransport::new(conn));

                    // Register
                    if let Err(e) = transport.send_signal(&SignalMessage::RegisterPresence {
                        identity_pub, signature: vec![], alias: None,
                    }).await {
                        error!("signal register: {e}");
                        bg_state.lock().unwrap().status = "idle".into();
                        return;
                    }

                    match transport.recv_signal().await {
                        Ok(Some(SignalMessage::RegisterPresenceAck { success: true, .. })) => {
                            info!(fingerprint = %fp, "signal: registered");
                            bg_state.lock().unwrap().status = "registered".into();
                            // Send transport to caller
                            let _ = transport_tx.send(transport.clone());
                        }
                        other => {
                            error!("signal registration failed: {other:?}");
                            bg_state.lock().unwrap().status = "idle".into();
                            return;
                        }
                    }

                    // Recv loop — runs forever
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

            bg_state.lock().unwrap().status = "idle".into();
                }); // block_on

                // Runtime intentionally NOT dropped — lives until thread exits.
                // This prevents ring/libcrypto TLS cleanup conflict on Android.
                // The thread is parked here forever (block_on returned = connection lost).
                std::thread::park();
            })?; // thread spawn

        // Wait for transport (up to 10s)
        let transport = transport_rx.recv_timeout(std::time::Duration::from_secs(10))
            .map_err(|_| anyhow::anyhow!("signal connect timeout — check relay address"))?;

        Ok(Self { transport, state: ret_state, running: ret_running })
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
