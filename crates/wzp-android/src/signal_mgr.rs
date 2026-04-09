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
    /// Connect to relay and register. Returns immediately with Self.
    /// Then call `run()` on a separate thread to start the recv loop.
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

        info!(fingerprint = %fp, relay = %addr, "signal: connecting");

        // Connect + register synchronously
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()?;

        let transport = rt.block_on(async {
            let bind: SocketAddr = "0.0.0.0:0".parse().unwrap();
            let endpoint = wzp_transport::create_endpoint(bind, None)?;
            let client_cfg = wzp_transport::client_config();
            let conn = wzp_transport::connect(&endpoint, addr, "_signal", client_cfg).await?;
            Ok::<_, anyhow::Error>(Arc::new(wzp_transport::QuinnTransport::new(conn)))
        })?;

        // Register presence (still on the same runtime)
        rt.block_on(async {
            transport.send_signal(&SignalMessage::RegisterPresence {
                identity_pub,
                signature: vec![],
                alias: None,
            }).await?;

            match transport.recv_signal().await? {
                Some(SignalMessage::RegisterPresenceAck { success: true, .. }) => {
                    info!(fingerprint = %fp, "signal: registered");
                    Ok(())
                }
                other => Err(anyhow::anyhow!("registration failed: {other:?}")),
            }
        })?;

        // Don't drop runtime — keep it for run()
        let state = Arc::new(Mutex::new(SignalState {
            status: "registered".into(),
            fingerprint: fp,
            ..Default::default()
        }));
        let running = Arc::new(AtomicBool::new(true));

        Ok(Self {
            transport,
            state,
            running,
        })
    }

    /// Blocking recv loop. Run on a dedicated thread after start().
    /// Never returns until the connection drops or stop() is called.
    pub fn run(&self) {
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .expect("tokio runtime");

        let transport = self.transport.clone();
        let state = self.state.clone();
        let running = self.running.clone();

        rt.block_on(async move {
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

            let mut s = state.lock().unwrap();
            s.status = "idle".into();
        }); // block_on
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
