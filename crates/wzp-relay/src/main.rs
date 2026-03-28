//! WarzonePhone relay daemon entry point.
//!
//! Supports two modes:
//! - **Room mode** (default): clients join named rooms, packets forwarded to all others (SFU)
//! - **Forward mode** (--remote): all traffic forwarded to a remote relay
//!
//! Room names are passed via the QUIC SNI (server_name) field.
//! The web bridge connects with room name as SNI.

use std::net::SocketAddr;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::Arc;
use std::time::Duration;

use tokio::sync::Mutex;
use tracing::{error, info};

use wzp_proto::MediaTransport;
use wzp_relay::config::RelayConfig;
use wzp_relay::metrics::RelayMetrics;
use wzp_relay::pipeline::{PipelineConfig, RelayPipeline};
use wzp_relay::room::{self, RoomManager};
use wzp_relay::session_mgr::SessionManager;

fn parse_args() -> RelayConfig {
    let mut config = RelayConfig::default();
    let args: Vec<String> = std::env::args().collect();
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--listen" => {
                i += 1;
                config.listen_addr = args.get(i).expect("--listen requires an address")
                    .parse().expect("invalid --listen address");
            }
            "--remote" => {
                i += 1;
                config.remote_relay = Some(
                    args.get(i).expect("--remote requires an address")
                        .parse().expect("invalid --remote address"),
                );
            }
            "--auth-url" => {
                i += 1;
                config.auth_url = Some(
                    args.get(i).expect("--auth-url requires a URL").to_string(),
                );
            }
            "--metrics-port" => {
                i += 1;
                config.metrics_port = Some(
                    args.get(i).expect("--metrics-port requires a port number")
                        .parse().expect("invalid --metrics-port number"),
                );
            }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-relay [--listen <addr>] [--remote <addr>] [--auth-url <url>] [--metrics-port <port>]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --listen <addr>        Listen address (default: 0.0.0.0:4433)");
                eprintln!("  --remote <addr>        Remote relay for forwarding (disables room mode)");
                eprintln!("  --auth-url <url>       featherChat auth endpoint (e.g., https://chat.example.com/v1/auth/validate)");
                eprintln!("                         When set, clients must send a bearer token as first signal message.");
                eprintln!("  --metrics-port <port>  Prometheus metrics HTTP port (e.g., 9090). Disabled if not set.");
                eprintln!();
                eprintln!("Room mode (default):");
                eprintln!("  Clients join rooms by name. Packets forwarded to all others (SFU).");
                std::process::exit(0);
            }
            other => {
                eprintln!("unknown argument: {other}");
                std::process::exit(1);
            }
        }
        i += 1;
    }
    config
}

struct RelayStats {
    upstream_packets: AtomicU64,
    downstream_packets: AtomicU64,
}

async fn run_upstream(
    client: Arc<wzp_transport::QuinnTransport>,
    remote: Arc<wzp_transport::QuinnTransport>,
    pipeline: Arc<Mutex<RelayPipeline>>,
    stats: Arc<RelayStats>,
) {
    loop {
        match client.recv_media().await {
            Ok(Some(pkt)) => {
                let outbound = {
                    let mut pipe = pipeline.lock().await;
                    let decoded = pipe.ingest(pkt);
                    let mut out = Vec::new();
                    for p in decoded { out.extend(pipe.prepare_outbound(p)); }
                    out
                };
                for p in &outbound {
                    if let Err(e) = remote.send_media(p).await {
                        error!("upstream send: {e}");
                        return;
                    }
                }
                stats.upstream_packets.fetch_add(outbound.len() as u64, Ordering::Relaxed);
            }
            Ok(None) => { info!("client disconnected (upstream)"); break; }
            Err(e) => { error!("upstream recv: {e}"); break; }
        }
    }
}

async fn run_downstream(
    client: Arc<wzp_transport::QuinnTransport>,
    remote: Arc<wzp_transport::QuinnTransport>,
    pipeline: Arc<Mutex<RelayPipeline>>,
    stats: Arc<RelayStats>,
) {
    loop {
        match remote.recv_media().await {
            Ok(Some(pkt)) => {
                let outbound = {
                    let mut pipe = pipeline.lock().await;
                    let decoded = pipe.ingest(pkt);
                    let mut out = Vec::new();
                    for p in decoded { out.extend(pipe.prepare_outbound(p)); }
                    out
                };
                for p in &outbound {
                    if let Err(e) = client.send_media(p).await {
                        error!("downstream send: {e}");
                        return;
                    }
                }
                stats.downstream_packets.fetch_add(outbound.len() as u64, Ordering::Relaxed);
            }
            Ok(None) => { info!("remote disconnected (downstream)"); break; }
            Err(e) => { error!("downstream recv: {e}"); break; }
        }
    }
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = parse_args();
    tracing_subscriber::fmt().init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // Prometheus metrics
    let metrics = Arc::new(RelayMetrics::new());
    if let Some(port) = config.metrics_port {
        let m = metrics.clone();
        tokio::spawn(wzp_relay::metrics::serve_metrics(port, m));
    }

    // Generate ephemeral relay identity for crypto handshake
    let relay_seed = wzp_crypto::Seed::generate();
    let relay_fp = relay_seed.derive_identity().public_identity().fingerprint;
    info!(addr = %config.listen_addr, fingerprint = %relay_fp, "WarzonePhone relay starting");

    let (server_config, _cert) = wzp_transport::server_config();
    let endpoint = wzp_transport::create_endpoint(config.listen_addr, Some(server_config))?;

    // Forward mode
    let remote_transport: Option<Arc<wzp_transport::QuinnTransport>> =
        if let Some(remote_addr) = config.remote_relay {
            info!(%remote_addr, "forward mode → remote relay");
            let client_cfg = wzp_transport::client_config();
            let conn = wzp_transport::connect(&endpoint, remote_addr, "localhost", client_cfg).await?;
            Some(Arc::new(wzp_transport::QuinnTransport::new(conn)))
        } else {
            info!("room mode — clients join named rooms (SFU)");
            None
        };

    // Room manager (room mode only)
    let room_mgr = Arc::new(Mutex::new(RoomManager::new()));

    // Session manager — enforces max concurrent sessions
    let session_mgr = Arc::new(Mutex::new(SessionManager::new(config.max_sessions)));

    if let Some(ref url) = config.auth_url {
        info!(url, "auth enabled — clients must present featherChat token");
    } else {
        info!("auth disabled — any client can connect (use --auth-url to enable)");
    }

    info!("Listening for connections...");

    loop {
        let connection = match wzp_transport::accept(&endpoint).await {
            Ok(conn) => conn,
            Err(e) => { error!("accept: {e}"); continue; }
        };

        let remote_transport = remote_transport.clone();
        let room_mgr = room_mgr.clone();
        let session_mgr = session_mgr.clone();
        let auth_url = config.auth_url.clone();
        let relay_seed_bytes = relay_seed.0;
        let metrics = metrics.clone();

        tokio::spawn(async move {
            let addr = connection.remote_address();

            let room_name = connection
                .handshake_data()
                .and_then(|hd| {
                    hd.downcast::<quinn::crypto::rustls::HandshakeData>().ok()
                })
                .and_then(|hd| hd.server_name.clone())
                .unwrap_or_else(|| "default".to_string());

            let transport = Arc::new(wzp_transport::QuinnTransport::new(connection));

            // Auth check: if --auth-url is set, expect first signal message to be a token
            // Auth: if --auth-url is set, expect AuthToken as first signal
            let authenticated_fp: Option<String> = if let Some(ref url) = auth_url {
                info!(%addr, "waiting for auth token...");
                match transport.recv_signal().await {
                    Ok(Some(wzp_proto::SignalMessage::AuthToken { token })) => {
                        match wzp_relay::auth::validate_token(url, &token).await {
                            Ok(client) => {
                                metrics.auth_attempts.with_label_values(&["ok"]).inc();
                                info!(
                                    %addr,
                                    fingerprint = %client.fingerprint,
                                    alias = ?client.alias,
                                    "authenticated"
                                );
                                Some(client.fingerprint)
                            }
                            Err(e) => {
                                metrics.auth_attempts.with_label_values(&["fail"]).inc();
                                error!(%addr, "auth failed: {e}");
                                transport.close().await.ok();
                                return;
                            }
                        }
                    }
                    Ok(Some(_)) => {
                        error!(%addr, "expected AuthToken as first signal, got something else");
                        transport.close().await.ok();
                        return;
                    }
                    Ok(None) => {
                        error!(%addr, "connection closed before auth");
                        return;
                    }
                    Err(e) => {
                        error!(%addr, "signal recv error during auth: {e}");
                        transport.close().await.ok();
                        return;
                    }
                }
            } else {
                None
            };

            // Crypto handshake: verify client identity + negotiate quality profile
            let handshake_start = std::time::Instant::now();
            let (_crypto_session, _chosen_profile) = match wzp_relay::handshake::accept_handshake(
                &*transport,
                &relay_seed_bytes,
            ).await {
                Ok(result) => {
                    let elapsed = handshake_start.elapsed().as_secs_f64();
                    metrics.handshake_duration.observe(elapsed);
                    info!(%addr, elapsed_ms = %(elapsed * 1000.0), "crypto handshake complete");
                    result
                }
                Err(e) => {
                    error!(%addr, "handshake failed: {e}");
                    transport.close().await.ok();
                    return;
                }
            };

            info!(%addr, room = %room_name, "client joining");

            if let Some(remote) = remote_transport {
                // Forward mode — same as before
                let stats = Arc::new(RelayStats {
                    upstream_packets: AtomicU64::new(0),
                    downstream_packets: AtomicU64::new(0),
                });
                let up_pipe = Arc::new(Mutex::new(RelayPipeline::new(PipelineConfig::default())));
                let dn_pipe = Arc::new(Mutex::new(RelayPipeline::new(PipelineConfig::default())));

                let stats_log = stats.clone();
                let stats_handle = tokio::spawn(async move {
                    let mut interval = tokio::time::interval(Duration::from_secs(5));
                    loop {
                        interval.tick().await;
                        info!(
                            up = stats_log.upstream_packets.load(Ordering::Relaxed),
                            down = stats_log.downstream_packets.load(Ordering::Relaxed),
                            "forward stats"
                        );
                    }
                });

                let up = tokio::spawn(run_upstream(transport.clone(), remote.clone(), up_pipe, stats.clone()));
                let dn = tokio::spawn(run_downstream(transport.clone(), remote.clone(), dn_pipe, stats));

                tokio::select! { _ = up => {} _ = dn => {} }
                stats_handle.abort();
                transport.close().await.ok();
            } else {
                // Room mode — enforce max sessions, then join room
                let session_id = {
                    let mut smgr = session_mgr.lock().await;
                    match smgr.create_session(&room_name, authenticated_fp.clone()) {
                        Ok(id) => id,
                        Err(e) => {
                            error!(%addr, room = %room_name, "session rejected: {e}");
                            transport.close().await.ok();
                            return;
                        }
                    }
                };

                metrics.active_sessions.inc();

                let participant_id = {
                    let mut mgr = room_mgr.lock().await;
                    match mgr.join(&room_name, addr, transport.clone(), authenticated_fp.as_deref()) {
                        Ok(id) => {
                            metrics.active_rooms.set(mgr.list().len() as i64);
                            id
                        }
                        Err(e) => {
                            error!(%addr, room = %room_name, "room join denied: {e}");
                            // Clean up the session we just created
                            metrics.active_sessions.dec();
                            let mut smgr = session_mgr.lock().await;
                            smgr.remove_session(session_id);
                            transport.close().await.ok();
                            return;
                        }
                    }
                };

                room::run_participant(
                    room_mgr.clone(),
                    room_name,
                    participant_id,
                    transport.clone(),
                    metrics.clone(),
                ).await;

                // Participant disconnected — clean up session
                metrics.active_sessions.dec();
                {
                    let mgr = room_mgr.lock().await;
                    metrics.active_rooms.set(mgr.list().len() as i64);
                }
                {
                    let mut smgr = session_mgr.lock().await;
                    smgr.remove_session(session_id);
                }

                transport.close().await.ok();
            }
        });
    }
}
