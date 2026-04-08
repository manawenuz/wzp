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
use tracing::{error, info, warn};

use wzp_proto::MediaTransport;
use wzp_relay::config::RelayConfig;
use wzp_relay::metrics::RelayMetrics;
use wzp_relay::pipeline::{PipelineConfig, RelayPipeline};
use wzp_relay::presence::PresenceRegistry;
use wzp_relay::room::{self, RoomManager};
use wzp_relay::session_mgr::SessionManager;

fn parse_args() -> RelayConfig {
    let args: Vec<String> = std::env::args().collect();

    // Check for --config first to use as base
    let mut config_file = None;
    let mut i = 1;
    while i < args.len() {
        if args[i] == "--config" {
            i += 1;
            config_file = args.get(i).cloned();
        }
        i += 1;
    }

    let mut config = if let Some(ref path) = config_file {
        wzp_relay::config::load_config(path)
            .unwrap_or_else(|e| {
                eprintln!("failed to load config from {path}: {e}");
                std::process::exit(1);
            })
    } else {
        RelayConfig::default()
    };

    // CLI flags override config file values
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" => { i += 1; } // already handled
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
            "--probe" => {
                i += 1;
                let addr: SocketAddr = args.get(i)
                    .expect("--probe requires an address")
                    .parse()
                    .expect("invalid --probe address");
                config.probe_targets.push(addr);
            }
            "--probe-mesh" => {
                config.probe_mesh = true;
            }
            "--trunking" => {
                config.trunking_enabled = true;
            }
            "--ws-port" => {
                i += 1;
                config.ws_port = Some(
                    args.get(i).expect("--ws-port requires a port number")
                        .parse().expect("invalid --ws-port number"),
                );
            }
            "--static-dir" => {
                i += 1;
                config.static_dir = Some(
                    args.get(i).expect("--static-dir requires a directory path").to_string(),
                );
            }
            "--global-room" => {
                i += 1;
                config.global_rooms.push(wzp_relay::config::GlobalRoomConfig {
                    name: args.get(i).expect("--global-room requires a room name").to_string(),
                });
            }
            "--debug-tap" => {
                i += 1;
                config.debug_tap = Some(
                    args.get(i).expect("--debug-tap requires a room name (or '*' for all)").to_string(),
                );
            }
            "--mesh-status" => {
                // Print mesh table from a fresh registry and exit.
                // In practice this is useful after the relay has been running;
                // here we just demonstrate the formatter with an empty registry.
                let m = RelayMetrics::new();
                print!("{}", wzp_relay::probe::mesh_summary(m.registry()));
                std::process::exit(0);
            }
            "--help" | "-h" => {
                eprintln!("Usage: wzp-relay [--config <path>] [--listen <addr>] [--remote <addr>] [--auth-url <url>] [--metrics-port <port>] [--probe <addr>]... [--probe-mesh] [--mesh-status]");
                eprintln!();
                eprintln!("Options:");
                eprintln!("  --config <path>        Load configuration from TOML file (peers, listen, etc.)");
                eprintln!("  --listen <addr>        Listen address (default: 0.0.0.0:4433)");
                eprintln!("  --remote <addr>        Remote relay for forwarding (disables room mode)");
                eprintln!("  --auth-url <url>       featherChat auth endpoint (e.g., https://chat.example.com/v1/auth/validate)");
                eprintln!("                         When set, clients must send a bearer token as first signal message.");
                eprintln!("  --metrics-port <port>  Prometheus metrics HTTP port (e.g., 9090). Disabled if not set.");
                eprintln!("  --probe <addr>         Peer relay to probe for health monitoring (repeatable).");
                eprintln!("  --probe-mesh           Enable mesh mode (mark config flag, probes all --probe targets).");
                eprintln!("  --mesh-status          Print mesh health table and exit (diagnostic).");
                eprintln!("  --trunking             Enable trunk batching for outgoing media in room mode.");
                eprintln!("  --global-room <name>   Declare a room as global (bridged across federation). Repeatable.");
                eprintln!("  --debug-tap <room>     Log packet headers for a room ('*' for all rooms).");
                eprintln!("  --ws-port <port>       WebSocket listener port for browser clients (e.g., 8080).");
                eprintln!("  --static-dir <dir>     Directory to serve static files from (HTML/JS/WASM).");
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

/// Detect a non-loopback IP address from local interfaces.
/// Prefers public IPs over private (10.x, 172.16-31.x, 192.168.x).
fn detect_public_ip() -> Option<String> {
    use std::net::UdpSocket;
    // Connect to a public address to find our outbound IP (doesn't actually send anything)
    if let Ok(socket) = UdpSocket::bind("0.0.0.0:0") {
        if socket.connect("8.8.8.8:80").is_ok() {
            if let Ok(addr) = socket.local_addr() {
                return Some(addr.ip().to_string());
            }
        }
    }
    None
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let config = parse_args();
    tracing_subscriber::fmt().init();
    rustls::crypto::ring::default_provider()
        .install_default()
        .expect("failed to install rustls crypto provider");

    // Presence registry
    let presence = Arc::new(Mutex::new(PresenceRegistry::new()));

    // Route resolver
    let route_resolver = Arc::new(wzp_relay::route::RouteResolver::new(config.listen_addr));

    // Prometheus metrics
    let metrics = Arc::new(RelayMetrics::new());
    if let Some(port) = config.metrics_port {
        let m = metrics.clone();
        let p = Some(presence.clone());
        let rr = Some(route_resolver.clone());
        tokio::spawn(wzp_relay::metrics::serve_metrics(port, m, p, rr));
    }

    // Load or generate relay identity — persisted in ~/.wzp/relay-identity
    let relay_seed = {
        let config_dir = dirs::home_dir()
            .unwrap_or_else(|| std::path::PathBuf::from("."))
            .join(".wzp");
        let identity_path = config_dir.join("relay-identity");
        if identity_path.exists() {
            if let Ok(hex) = std::fs::read_to_string(&identity_path) {
                if let Ok(s) = wzp_crypto::Seed::from_hex(hex.trim()) {
                    info!("loaded relay identity from {}", identity_path.display());
                    s
                } else {
                    warn!("corrupt relay identity file, generating new");
                    let s = wzp_crypto::Seed::generate();
                    let hex: String = s.0.iter().map(|b| format!("{b:02x}")).collect();
                    let _ = std::fs::write(&identity_path, &hex);
                    s
                }
            } else {
                let s = wzp_crypto::Seed::generate();
                let hex: String = s.0.iter().map(|b| format!("{b:02x}")).collect();
                let _ = std::fs::write(&identity_path, &hex);
                s
            }
        } else {
            let s = wzp_crypto::Seed::generate();
            let _ = std::fs::create_dir_all(&config_dir);
            let hex: String = s.0.iter().map(|b| format!("{b:02x}")).collect();
            let _ = std::fs::write(&identity_path, &hex);
            info!("generated relay identity at {}", identity_path.display());
            s
        }
    };
    let relay_fp = relay_seed.derive_identity().public_identity().fingerprint;
    info!(addr = %config.listen_addr, fingerprint = %relay_fp, "WarzonePhone relay starting");

    let (server_config, cert_der) = wzp_transport::server_config_from_seed(&relay_seed.0);
    let tls_fp = wzp_transport::tls_fingerprint(&cert_der);
    info!(tls_fingerprint = %tls_fp, "TLS certificate (deterministic from relay identity)");

    // Print federation hint with our public IP + listen port + TLS fingerprint
    let listen_port = config.listen_addr.port();
    let public_ip = detect_public_ip();
    if let Some(ip) = &public_ip {
        info!("federation: to peer with this relay, add to relay.toml:");
        info!("  [[peers]]");
        info!("  url = \"{ip}:{listen_port}\"");
        info!("  fingerprint = \"{tls_fp}\"");
    }

    // Log configured peers and trusted relays
    if !config.peers.is_empty() {
        info!(count = config.peers.len(), "federation peers configured");
        for p in &config.peers {
            info!(url = %p.url, label = ?p.label, "  peer");
        }
    }
    if !config.trusted.is_empty() {
        info!(count = config.trusted.len(), "trusted relays configured");
        for t in &config.trusted {
            info!(fingerprint = %t.fingerprint, label = ?t.label, "  trusted");
        }
    }
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

    // Federation manager
    let global_room_set: std::collections::HashSet<String> = config.global_rooms.iter()
        .map(|g| g.name.clone())
        .collect();

    let federation_mgr = if !config.peers.is_empty() || !config.trusted.is_empty() || !global_room_set.is_empty() {
        let fm = Arc::new(wzp_relay::federation::FederationManager::new(
            config.peers.clone(),
            config.trusted.clone(),
            global_room_set.clone(),
            room_mgr.clone(),
            endpoint.clone(),
            tls_fp.clone(),
        ));
        let fm_run = fm.clone();
        tokio::spawn(async move { fm_run.run().await });
        Some(fm)
    } else {
        None
    };

    // Session manager — enforces max concurrent sessions
    let session_mgr = Arc::new(Mutex::new(SessionManager::new(config.max_sessions)));

    // Spawn inter-relay health probes via ProbeMesh coordinator
    if !config.probe_targets.is_empty() {
        let mesh = wzp_relay::probe::ProbeMesh::new(
            config.probe_targets.clone(),
            metrics.registry(),
            Some(presence.clone()),
        );
        info!(
            targets = mesh.target_count(),
            mesh = config.probe_mesh,
            "spawning probe mesh"
        );
        tokio::spawn(async move { mesh.run_all().await });
    }

    // WebSocket server for browser clients
    if let Some(ws_port) = config.ws_port {
        let ws_state = wzp_relay::ws::WsState {
            room_mgr: room_mgr.clone(),
            session_mgr: session_mgr.clone(),
            auth_url: config.auth_url.clone(),
            metrics: metrics.clone(),
            presence: presence.clone(),
        };
        let static_dir = config.static_dir.clone();
        tokio::spawn(wzp_relay::ws::run_ws_server(ws_port, ws_state, static_dir));
        info!(ws_port, "WebSocket listener enabled for browser clients");
    }

    if let Some(ref url) = config.auth_url {
        info!(url, "auth enabled — clients must present featherChat token");
    } else {
        info!("auth disabled — any client can connect (use --auth-url to enable)");
    }
    if !config.global_rooms.is_empty() {
        info!(count = config.global_rooms.len(), "global rooms configured");
        for g in &config.global_rooms {
            info!(name = %g.name, "  global room");
        }
    }
    if let Some(ref tap) = config.debug_tap {
        info!(filter = %tap, "debug tap enabled — logging packet headers");
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
        let trunking_enabled = config.trunking_enabled;
        let debug_tap = config.debug_tap.as_ref().map(|filter| room::DebugTap { room_filter: filter.clone() });
        let presence = presence.clone();
        let route_resolver = route_resolver.clone();
        let federation_mgr = federation_mgr.clone();

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

            // Ping connections: client just measures QUIC connect RTT.
            // No handshake, no streams — client closes immediately after connecting.
            if room_name == "ping" {
                info!(%addr, "ping connection (RTT probe)");
                return;
            }

            // Probe connections use SNI "_probe" to identify themselves.
            // They skip auth + handshake and just do Ping->Pong + presence gossip.
            if room_name == "_probe" {
                info!(%addr, "probe connection detected, entering Ping/Pong + presence responder");
                loop {
                    match transport.recv_signal().await {
                        Ok(Some(wzp_proto::SignalMessage::Ping { timestamp_ms })) => {
                            if let Err(e) = transport.send_signal(
                                &wzp_proto::SignalMessage::Pong { timestamp_ms },
                            ).await {
                                error!(%addr, "probe pong send error: {e}");
                                break;
                            }
                        }
                        Ok(Some(wzp_proto::SignalMessage::PresenceUpdate { fingerprints, relay_addr })) => {
                            // A peer relay is telling us which fingerprints it has
                            let peer_addr: std::net::SocketAddr = relay_addr.parse().unwrap_or(addr);
                            let fps: std::collections::HashSet<String> = fingerprints.into_iter().collect();
                            {
                                let mut reg = presence.lock().await;
                                reg.update_peer(peer_addr, fps);
                            }
                            // Reply with our own local fingerprints
                            let local_fps: Vec<String> = {
                                let reg = presence.lock().await;
                                reg.local_fingerprints().into_iter().collect()
                            };
                            let reply = wzp_proto::SignalMessage::PresenceUpdate {
                                fingerprints: local_fps,
                                relay_addr: addr.to_string(),
                            };
                            if let Err(e) = transport.send_signal(&reply).await {
                                error!(%addr, "presence reply send error: {e}");
                                break;
                            }
                        }
                        Ok(Some(wzp_proto::SignalMessage::RouteQuery { fingerprint, ttl })) => {
                            // Look up the fingerprint in our local registry
                            let reg = presence.lock().await;
                            let route = route_resolver.resolve(&reg, &fingerprint);
                            drop(reg);

                            let (found, relay_chain) = match route {
                                wzp_relay::route::Route::Local => {
                                    (true, vec![route_resolver.local_addr().to_string()])
                                }
                                wzp_relay::route::Route::DirectPeer(peer_addr) => {
                                    (true, vec![route_resolver.local_addr().to_string(), peer_addr.to_string()])
                                }
                                _ => {
                                    // Not found locally; if ttl > 0 we could forward
                                    // to other peers (future multi-hop). For now, reply not found.
                                    if ttl > 0 {
                                        // TODO: forward RouteQuery to other peers with ttl-1
                                    }
                                    (false, vec![])
                                }
                            };

                            let reply = wzp_proto::SignalMessage::RouteResponse {
                                fingerprint,
                                found,
                                relay_chain,
                            };
                            if let Err(e) = transport.send_signal(&reply).await {
                                error!(%addr, "route response send error: {e}");
                                break;
                            }
                        }
                        Ok(Some(_)) => {
                            // Ignore other signals on probe connections
                        }
                        Ok(None) => {
                            info!(%addr, "probe connection closed");
                            break;
                        }
                        Err(e) => {
                            error!(%addr, "probe recv error: {e}");
                            break;
                        }
                    }
                }
                transport.close().await.ok();
                return;
            }

            // Federation connections use SNI "_federation"
            if room_name == "_federation" {
                if let Some(ref fm) = federation_mgr {
                    // Wait for FederationHello to identify the connecting relay
                    let hello_fp = match tokio::time::timeout(
                        std::time::Duration::from_secs(5),
                        transport.recv_signal(),
                    ).await {
                        Ok(Ok(Some(wzp_proto::SignalMessage::FederationHello { tls_fingerprint }))) => tls_fingerprint,
                        _ => {
                            warn!(%addr, "federation: no hello received, closing");
                            return;
                        }
                    };

                    if let Some(label) = fm.check_inbound_trust(addr, &hello_fp) {
                        let peer_config = wzp_relay::config::PeerConfig {
                            url: addr.to_string(),
                            fingerprint: hello_fp,
                            label: Some(label.clone()),
                        };
                        let fm = fm.clone();
                        info!(%addr, label = %label, "inbound federation accepted (trusted)");
                        fm.handle_inbound(transport, peer_config).await;
                    } else {
                        warn!(%addr, fp = %hello_fp, "unknown relay wants to federate");
                        info!("  to accept, add to relay.toml:");
                        info!("  [[trusted]]");
                        info!("  fingerprint = \"{hello_fp}\"");
                        info!("  label = \"Relay at {addr}\"");
                    }
                } else {
                    info!(%addr, "federation connection rejected (no federation configured)");
                }
                return;
            }

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
            let (_crypto_session, _chosen_profile, caller_fp, caller_alias) = match wzp_relay::handshake::accept_handshake(
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

            // Use the caller's identity fingerprint from the handshake
            let participant_fp = authenticated_fp.clone().unwrap_or(caller_fp);

            // Register in presence registry
            {
                let mut reg = presence.lock().await;
                reg.register_local(&participant_fp, None, Some(room_name.clone()));
            }

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
                    match mgr.join(
                        &room_name,
                        addr,
                        room::ParticipantSender::Quic(transport.clone()),
                        Some(&participant_fp),
                        caller_alias.as_deref(),
                    ) {
                        Ok((id, update, senders)) => {
                            metrics.active_rooms.set(mgr.list().len() as i64);
                            drop(mgr); // release lock before async broadcast
                            room::broadcast_signal(&senders, &update).await;
                            id
                        }
                        Err(e) => {
                            error!(%addr, room = %room_name, "room join denied: {e}");
                            metrics.active_sessions.dec();
                            let mut smgr = session_mgr.lock().await;
                            smgr.remove_session(session_id);
                            transport.close().await.ok();
                            return;
                        }
                    }
                };

                let session_id_str: String = session_id
                    .iter()
                    .map(|b| format!("{b:02x}"))
                    .collect();
                // Set up federation media channel if this is a global room
                let federation_tx = if let Some(ref fm) = federation_mgr {
                    let is_global = fm.is_global_room(&room_name);
                    info!(room = %room_name, is_global, "checking if room is global for federation");
                    if is_global {
                        let (tx, rx) = tokio::sync::mpsc::channel(256);
                        let fm_clone = fm.clone();
                        tokio::spawn(async move {
                            wzp_relay::federation::run_federation_media_egress(fm_clone, rx).await;
                        });
                        info!(room = %room_name, "federation media egress channel created");
                        Some(tx)
                    } else {
                        None
                    }
                } else {
                    None
                };

                room::run_participant(
                    room_mgr.clone(),
                    room_name,
                    participant_id,
                    transport.clone(),
                    metrics.clone(),
                    &session_id_str,
                    trunking_enabled,
                    debug_tap,
                    federation_tx,
                ).await;

                // Participant disconnected — clean up presence + per-session metrics
                if let Some(ref fp) = authenticated_fp {
                    let mut reg = presence.lock().await;
                    reg.unregister_local(fp);
                }
                metrics.remove_session_metrics(&session_id_str);
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
