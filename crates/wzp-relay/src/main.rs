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
use tracing::{debug, error, info, warn};

use wzp_proto::{MediaTransport, SignalMessage};
use wzp_relay::config::RelayConfig;
use wzp_relay::metrics::RelayMetrics;
use wzp_relay::pipeline::{PipelineConfig, RelayPipeline};
use wzp_relay::presence::PresenceRegistry;
use wzp_relay::room::{self, RoomManager};
use wzp_relay::session_mgr::SessionManager;

/// Parsed CLI result — config + identity path.
struct CliResult {
    config: RelayConfig,
    identity_path: Option<String>,
    config_file: Option<String>,
    config_needs_create: bool,
}

fn parse_args() -> CliResult {
    let args: Vec<String> = std::env::args().collect();

    // First pass: extract --config and --identity
    let mut config_file = None;
    let mut identity_path = None;
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => { i += 1; config_file = args.get(i).cloned(); }
            "--identity" | "-i" => { i += 1; identity_path = args.get(i).cloned(); }
            _ => {}
        }
        i += 1;
    }

    // Track if we need to create the config after identity is known
    let config_needs_create = config_file.as_ref().map(|p| !std::path::Path::new(p).exists()).unwrap_or(false);

    let mut config = if let Some(ref path) = config_file {
        if config_needs_create {
            // Will be re-created with personalized info after identity is loaded
            RelayConfig::default()
        } else {
            wzp_relay::config::load_config(path)
                .unwrap_or_else(|e| {
                    eprintln!("failed to load config from {path}: {e}");
                    std::process::exit(1);
                })
        }
    } else {
        RelayConfig::default()
    };

    // CLI flags override config file values
    let mut i = 1;
    while i < args.len() {
        match args[i].as_str() {
            "--config" | "-c" => { i += 1; } // already handled
            "--identity" | "-i" => { i += 1; } // already handled
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
            "--event-log" => {
                i += 1;
                config.event_log = Some(
                    args.get(i).expect("--event-log requires a file path").to_string(),
                );
            }
            "--version" | "-V" => {
                println!("wzp-relay {}", env!("WZP_BUILD_HASH"));
                std::process::exit(0);
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
                eprintln!("  -c, --config <path>    Load config from TOML file (creates example if missing)");
                eprintln!("  -i, --identity <path>  Identity file path (creates if missing, uses OsRng)");
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
    CliResult { config, identity_path, config_file, config_needs_create }
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

/// Build-time git hash, set by build.rs or env.
const BUILD_GIT_HASH: &str = env!("WZP_BUILD_HASH");

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let CliResult { config, identity_path, config_file, config_needs_create } = parse_args();
    tracing_subscriber::fmt().init();
    info!(version = BUILD_GIT_HASH, "wzp-relay build");
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

    // Load or generate relay identity
    let relay_seed = {
        let id_path = match identity_path {
            Some(ref p) => std::path::PathBuf::from(p),
            None => dirs::home_dir()
                .unwrap_or_else(|| std::path::PathBuf::from("."))
                .join(".wzp")
                .join("relay-identity"),
        };
        if id_path.exists() {
            if let Ok(hex) = std::fs::read_to_string(&id_path) {
                if let Ok(s) = wzp_crypto::Seed::from_hex(hex.trim()) {
                    info!("loaded relay identity from {}", id_path.display());
                    s
                } else {
                    warn!("corrupt identity file {}, generating new", id_path.display());
                    let s = wzp_crypto::Seed::generate();
                    let hex: String = s.0.iter().map(|b| format!("{b:02x}")).collect();
                    let _ = std::fs::write(&id_path, &hex);
                    s
                }
            } else {
                let s = wzp_crypto::Seed::generate();
                let hex: String = s.0.iter().map(|b| format!("{b:02x}")).collect();
                let _ = std::fs::write(&id_path, &hex);
                s
            }
        } else {
            let s = wzp_crypto::Seed::generate();
            if let Some(parent) = id_path.parent() {
                let _ = std::fs::create_dir_all(parent);
            }
            let hex: String = s.0.iter().map(|b| format!("{b:02x}")).collect();
            let _ = std::fs::write(&id_path, &hex);
            info!("generated relay identity at {}", id_path.display());
            s
        }
    };
    let relay_fp = relay_seed.derive_identity().public_identity().fingerprint;
    info!(addr = %config.listen_addr, fingerprint = %relay_fp, "WarzonePhone relay starting");

    let (server_config, cert_der) = wzp_transport::server_config_from_seed(&relay_seed.0);
    let tls_fp = wzp_transport::tls_fingerprint(&cert_der);
    info!(tls_fingerprint = %tls_fp, "TLS certificate (deterministic from relay identity)");

    // Create personalized config file if it was missing
    let public_ip = detect_public_ip();
    if config_needs_create {
        if let Some(ref path) = config_file {
            let info = wzp_relay::config::RelayInfo {
                listen_addr: config.listen_addr.to_string(),
                tls_fingerprint: tls_fp.clone(),
                public_ip: public_ip.clone(),
            };
            if let Err(e) = wzp_relay::config::load_or_create_config(path, Some(&info)) {
                warn!("failed to create config: {e}");
            }
        }
    }

    // Print federation hint with our public IP + listen port + TLS fingerprint
    let listen_port = config.listen_addr.port();
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

    // Compute the IP address we should advertise in CallSetup for direct
    // calls. If the relay is bound to a specific IP, use it as-is; if bound
    // to 0.0.0.0, use the trick of "connect" a UDP socket to an arbitrary
    // external address and read its local_addr — the OS binds to whichever
    // local interface IP would route packets to that destination, which is
    // the primary outbound interface. This is the same IP clients on the
    // LAN use to reach us.
    let advertised_ip: std::net::IpAddr = {
        let listen_ip = config.listen_addr.ip();
        if !listen_ip.is_unspecified() {
            listen_ip
        } else {
            // Probe via a dummy "connected" UDP socket. Never actually sends.
            match std::net::UdpSocket::bind("0.0.0.0:0")
                .and_then(|s| { s.connect("8.8.8.8:80").map(|_| s) })
                .and_then(|s| s.local_addr())
            {
                Ok(a) if !a.ip().is_loopback() => a.ip(),
                _ => std::net::IpAddr::from([127u8, 0, 0, 1]),
            }
        }
    };
    let advertised_addr_str = format!("{}:{}", advertised_ip, config.listen_addr.port());
    info!(%advertised_addr_str, "relay advertised address for CallSetup");

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

    // Event log for protocol analysis
    let event_log = wzp_relay::event_log::start_event_log(
        config.event_log.as_ref().map(std::path::PathBuf::from)
    );

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
            metrics.clone(),
            event_log.clone(),
        ));
        let fm_run = fm.clone();
        tokio::spawn(async move { fm_run.run().await });
        Some(fm)
    } else {
        None
    };

    // Session manager — enforces max concurrent sessions
    let session_mgr = Arc::new(Mutex::new(SessionManager::new(config.max_sessions)));

    // Signal hub + call registry for direct 1:1 calls
    let signal_hub = Arc::new(Mutex::new(wzp_relay::signal_hub::SignalHub::new()));
    let call_registry = Arc::new(Mutex::new(wzp_relay::call_registry::CallRegistry::new()));

    // Phase 4: cross-relay direct-call signal dispatcher.
    //
    // The federation layer unwraps incoming
    // `SignalMessage::FederatedSignalForward` envelopes and pushes
    // (inner, origin_relay_fp) onto this channel. A dedicated task
    // further down reads from it and routes the inner message
    // through signal_hub / call_registry exactly as if it had
    // arrived on a local signal transport — with the extra
    // context that a peer relay is on the other side of the call.
    let (cross_relay_tx, mut cross_relay_rx) =
        tokio::sync::mpsc::channel::<(wzp_proto::SignalMessage, String)>(32);
    if let Some(ref fm) = federation_mgr {
        fm.set_cross_relay_tx(cross_relay_tx.clone()).await;
    }

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

    // Phase 4: cross-relay direct-call dispatcher task.
    //
    // Reads unwrapped (inner, origin_relay_fp) tuples that the
    // federation layer pushes out of its `handle_signal` arm for
    // `FederatedSignalForward`, and routes the inner message
    // through the local signal_hub / call_registry exactly as if
    // the message had arrived on a local client signal transport.
    //
    // In Phase 4 MVP the dispatcher handles:
    //   * DirectCallOffer  — if target is local, stash in registry
    //                        with peer_relay_fp and deliver to
    //                        local callee via signal_hub.
    //   * DirectCallAnswer — stash callee addr, forward answer to
    //                        local caller, emit local CallSetup.
    //   * CallRinging     — forward to local caller for UX.
    //   * Hangup          — forward to the local participant(s).
    // Everything else is dropped.
    {
        let signal_hub_d = signal_hub.clone();
        let call_registry_d = call_registry.clone();
        let advertised_addr_d = advertised_addr_str.clone();
        let federation_mgr_d = federation_mgr.clone();
        tokio::spawn(async move {
            use wzp_proto::{CallAcceptMode, SignalMessage};
            while let Some((inner, origin_relay_fp)) = cross_relay_rx.recv().await {
                match inner {
                    SignalMessage::DirectCallOffer {
                        ref target_fingerprint,
                        ref caller_fingerprint,
                        ref call_id,
                        ref caller_reflexive_addr,
                        ref caller_local_addrs,
                        ..
                    } => {
                        // Is the target on THIS relay? If not, drop —
                        // Phase 4 MVP is single-hop federation only.
                        let online = {
                            let hub = signal_hub_d.lock().await;
                            hub.is_online(target_fingerprint)
                        };
                        if !online {
                            tracing::debug!(
                                target = %target_fingerprint,
                                %origin_relay_fp,
                                "cross-relay: offer target not local, dropping (no multi-hop)"
                            );
                            continue;
                        }
                        // Stash in local registry so the answer path
                        // can find the call + route the reply back
                        // through the same federation link. Include
                        // Phase 5.5 LAN host candidates too.
                        {
                            let mut reg = call_registry_d.lock().await;
                            reg.create_call(
                                call_id.clone(),
                                caller_fingerprint.clone(),
                                target_fingerprint.clone(),
                            );
                            reg.set_caller_reflexive_addr(call_id, caller_reflexive_addr.clone());
                            reg.set_caller_local_addrs(call_id, caller_local_addrs.clone());
                            reg.set_peer_relay_fp(call_id, Some(origin_relay_fp.clone()));
                        }
                        // Deliver the offer to the local target.
                        let hub = signal_hub_d.lock().await;
                        if let Err(e) = hub.send_to(target_fingerprint, &inner).await {
                            tracing::warn!(
                                target = %target_fingerprint,
                                error = %e,
                                "cross-relay: failed to deliver forwarded offer"
                            );
                        }
                    }

                    SignalMessage::DirectCallAnswer {
                        ref call_id,
                        accept_mode,
                        ref callee_reflexive_addr,
                        ref callee_local_addrs,
                        ..
                    } => {
                        // Look up the local caller fp from the registry.
                        let caller_fp = {
                            let reg = call_registry_d.lock().await;
                            reg.get(call_id).map(|c| c.caller_fingerprint.clone())
                        };
                        let Some(caller_fp) = caller_fp else {
                            tracing::debug!(%call_id, "cross-relay: answer for unknown call, dropping");
                            continue;
                        };

                        if accept_mode == CallAcceptMode::Reject {
                            // Forward hangup to local caller + clean up registry.
                            let hub = signal_hub_d.lock().await;
                            let _ = hub
                                .send_to(
                                    &caller_fp,
                                    &SignalMessage::Hangup {
                                        reason: wzp_proto::HangupReason::Normal,
                                        call_id: None,
                                    },
                                )
                                .await;
                            drop(hub);
                            let mut reg = call_registry_d.lock().await;
                            reg.end_call(call_id);
                            continue;
                        }

                        // Accept — stash the callee's reflex addr + LAN
                        // host candidates + mark the call active,
                        // then read back everything needed to cross-
                        // wire peer_direct_addr + peer_local_addrs in
                        // the local CallSetup.
                        // Also set peer_relay_fp so the originating
                        // relay knows where to forward MediaPathReport.
                        let room_name = format!("call-{call_id}");
                        let (callee_addr_for_setup, callee_local_for_setup) = {
                            let mut reg = call_registry_d.lock().await;
                            reg.set_active(call_id, accept_mode, room_name.clone());
                            reg.set_peer_relay_fp(call_id, Some(origin_relay_fp.clone()));
                            reg.set_callee_reflexive_addr(
                                call_id,
                                callee_reflexive_addr.clone(),
                            );
                            reg.set_callee_local_addrs(call_id, callee_local_addrs.clone());
                            let c = reg.get(call_id);
                            (
                                c.and_then(|c| c.callee_reflexive_addr.clone()),
                                c.map(|c| c.callee_local_addrs.clone()).unwrap_or_default(),
                            )
                        };

                        // Forward the raw answer to the local caller so
                        // the JS side sees DirectCallAnswer (fires any
                        // "call answered" UX that looks at this message).
                        {
                            let hub = signal_hub_d.lock().await;
                            let _ = hub.send_to(&caller_fp, &inner).await;
                        }

                        // Emit the LOCAL CallSetup to our local caller.
                        // relay_addr = our own advertised addr so if P2P
                        // fails the caller will at least dial OUR relay
                        // (single-relay fallback — Phase 4.1 will wire
                        // federated media so that actually reaches the
                        // peer). peer_direct_addr = the callee's reflex
                        // addr carried in the answer. peer_local_addrs
                        // = callee's LAN host candidates (Phase 5.5 ICE).
                        let setup = SignalMessage::CallSetup {
                            call_id: call_id.clone(),
                            room: room_name.clone(),
                            relay_addr: advertised_addr_d.clone(),
                            peer_direct_addr: callee_addr_for_setup,
                            peer_local_addrs: callee_local_for_setup,
                        };
                        let hub = signal_hub_d.lock().await;
                        let _ = hub.send_to(&caller_fp, &setup).await;

                        tracing::info!(
                            %call_id,
                            %caller_fp,
                            %origin_relay_fp,
                            "cross-relay: delivered answer + CallSetup to local caller"
                        );
                    }

                    SignalMessage::CallRinging { ref call_id } => {
                        // Forward to local caller for "ringing..." UX.
                        let caller_fp = {
                            let reg = call_registry_d.lock().await;
                            reg.get(call_id).map(|c| c.caller_fingerprint.clone())
                        };
                        if let Some(fp) = caller_fp {
                            let hub = signal_hub_d.lock().await;
                            let _ = hub.send_to(&fp, &inner).await;
                        }
                    }

                    // Phase 6: MediaPathReport forwarded across
                    // federation — deliver to the LOCAL participant.
                    // The report comes from the remote side, so we
                    // deliver to whichever participant is local. In
                    // the cross-relay case, one is local and one is
                    // remote. Try both — send_to is a no-op if the
                    // target isn't connected to this relay.
                    SignalMessage::MediaPathReport { ref call_id, .. } => {
                        let (caller_fp, callee_fp) = {
                            let reg = call_registry_d.lock().await;
                            match reg.get(call_id) {
                                Some(c) => (
                                    Some(c.caller_fingerprint.clone()),
                                    Some(c.callee_fingerprint.clone()),
                                ),
                                None => (None, None),
                            }
                        };
                        let hub = signal_hub_d.lock().await;
                        if let Some(fp) = caller_fp {
                            let _ = hub.send_to(&fp, &inner).await;
                        }
                        if let Some(fp) = callee_fp {
                            let _ = hub.send_to(&fp, &inner).await;
                        }
                    }

                    SignalMessage::Hangup { .. } => {
                        // Best-effort: broadcast the hangup to every
                        // local participant of any call that currently
                        // has this origin as its peer_relay_fp.
                        // The forwarded hangup doesn't carry a call_id
                        // so we can't target precisely — Phase 4.1 will
                        // tighten this once hangup tracking is stricter.
                        tracing::debug!(
                            %origin_relay_fp,
                            "cross-relay: forwarded Hangup (Phase 4.1 will target by call_id)"
                        );
                    }

                    _ => {
                        tracing::debug!(
                            %origin_relay_fp,
                            "cross-relay: dispatcher ignoring unsupported inner variant"
                        );
                    }
                }
            }
            // Suppress the warning if federation_mgr_d is unused —
            // it's held here so the Arc doesn't drop during the
            // dispatcher's lifetime.
            drop(federation_mgr_d);
        });
    }

    info!("Listening for connections...");

    loop {
        // Pull the next Incoming off the queue. Deliberately do NOT await
        // the QUIC handshake here — move that into the per-connection
        // spawned task below. Previously we used wzp_transport::accept
        // which did both, which meant a single slow handshake would block
        // the entire accept loop and prevent ALL subsequent connections
        // from being processed. Surfaced as direct-call hangs where the
        // callee's call-* connection never completes its QUIC handshake.
        let incoming = match endpoint.accept().await {
            Some(inc) => inc,
            None => {
                error!("endpoint.accept() returned None — endpoint closed");
                break;
            }
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
        let signal_hub = signal_hub.clone();
        let call_registry = call_registry.clone();
        let advertised_addr_str = advertised_addr_str.clone();
        // Phase 4: per-task clone of this relay's federation TLS
        // fingerprint so the FederatedSignalForward envelopes the
        // spawned signal handler builds carry `origin_relay_fp`.
        let tls_fp = tls_fp.clone();

        let incoming_addr = incoming.remote_address();
        info!(%incoming_addr, "accept queue: new Incoming, spawning handshake task");

        tokio::spawn(async move {
            // Drive the QUIC handshake inside the spawned task so that
            // slow or hung handshakes never block the outer accept loop.
            let connection = match incoming.await {
                Ok(c) => c,
                Err(e) => {
                    error!(%incoming_addr, "QUIC handshake failed: {e}");
                    return;
                }
            };
            info!(%incoming_addr, "QUIC handshake complete");
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
            if room_name == "ping" {
                info!(%addr, "ping connection (RTT probe)");
                return;
            }

            // Version query: respond with build hash over a uni stream.
            if room_name == "version" {
                if let Ok(mut send) = transport.connection().open_uni().await {
                    let _ = send.write_all(BUILD_GIT_HASH.as_bytes()).await;
                    let _ = send.finish();
                    // Wait for client to read before closing
                    tokio::time::sleep(std::time::Duration::from_millis(100)).await;
                }
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

            // Direct calling: persistent signaling connection
            if room_name == "_signal" {
                info!(%addr, "signal connection");

                // Optional auth
                let auth_fp: Option<String> = if let Some(ref url) = auth_url {
                    match transport.recv_signal().await {
                        Ok(Some(SignalMessage::AuthToken { token })) => {
                            match wzp_relay::auth::validate_token(url, &token).await {
                                Ok(client) => Some(client.fingerprint),
                                Err(e) => {
                                    error!(%addr, "signal auth failed: {e}");
                                    return;
                                }
                            }
                        }
                        _ => { warn!(%addr, "signal: expected AuthToken"); return; }
                    }
                } else {
                    None
                };

                // Wait for RegisterPresence
                let (client_fp, client_alias) = match tokio::time::timeout(
                    std::time::Duration::from_secs(10),
                    transport.recv_signal(),
                ).await {
                    Ok(Ok(Some(SignalMessage::RegisterPresence { identity_pub, signature: _, alias }))) => {
                        // Compute fingerprint: SHA-256(Ed25519 pub key)[:16], same as Fingerprint type
                        let fp = {
                            use sha2::{Sha256, Digest};
                            let hash = Sha256::digest(&identity_pub);
                            let fingerprint = wzp_crypto::Fingerprint([
                                hash[0], hash[1], hash[2], hash[3], hash[4], hash[5], hash[6], hash[7],
                                hash[8], hash[9], hash[10], hash[11], hash[12], hash[13], hash[14], hash[15],
                            ]);
                            fingerprint.to_string()
                        };
                        let fp = auth_fp.unwrap_or(fp);
                        (fp, alias)
                    }
                    _ => {
                        warn!(%addr, "signal: no RegisterPresence received");
                        return;
                    }
                };

                // Register in signal hub + presence
                {
                    let mut hub = signal_hub.lock().await;
                    hub.register(client_fp.clone(), transport.clone(), client_alias.clone());
                }
                {
                    let mut reg = presence.lock().await;
                    reg.register_local(&client_fp, client_alias.clone(), None);
                }

                // Send ack
                let _ = transport.send_signal(&SignalMessage::RegisterPresenceAck {
                    success: true,
                    error: None,
                    relay_build: Some(BUILD_GIT_HASH.to_string()),
                }).await;

                info!(%addr, fingerprint = %client_fp, alias = ?client_alias, "signal client registered");

                // Signal recv loop
                loop {
                    match transport.recv_signal().await {
                        Ok(Some(msg)) => {
                            match msg {
                                SignalMessage::DirectCallOffer {
                                    ref target_fingerprint,
                                    ref call_id,
                                    ref caller_reflexive_addr,
                                    ref caller_local_addrs,
                                    ..
                                } => {
                                    let target_fp = target_fingerprint.clone();
                                    let call_id = call_id.clone();
                                    let caller_addr_for_registry = caller_reflexive_addr.clone();
                                    let caller_local_for_registry = caller_local_addrs.clone();

                                    // Check if target is online
                                    let online = {
                                        let hub = signal_hub.lock().await;
                                        hub.is_online(&target_fp)
                                    };
                                    if !online {
                                        // Phase 4: maybe the target is on a
                                        // federation peer. Wrap the offer in
                                        // FederatedSignalForward and broadcast
                                        // it over every active peer link —
                                        // whichever relay has the target will
                                        // unwrap and dispatch locally. We also
                                        // stash the call in OUR registry so
                                        // the eventual answer coming back via
                                        // federation has a matching entry.
                                        let forwarded = if let Some(ref fm) = federation_mgr {
                                            let forward = SignalMessage::FederatedSignalForward {
                                                inner: Box::new(msg.clone()),
                                                origin_relay_fp: tls_fp.clone(),
                                            };
                                            let count = fm.broadcast_signal(&forward).await;
                                            if count > 0 {
                                                info!(
                                                    %addr,
                                                    target = %target_fp,
                                                    peers = count,
                                                    "direct-call offer forwarded to federation peers"
                                                );
                                                true
                                            } else {
                                                false
                                            }
                                        } else {
                                            false
                                        };

                                        if !forwarded {
                                            info!(%addr, target = %target_fp, "call target not online (no federation route)");
                                            let _ = transport.send_signal(&SignalMessage::Hangup {
                                                reason: wzp_proto::HangupReason::Normal,
                                                call_id: None,
                                            }).await;
                                            continue;
                                        }

                                        // Create call in registry with the
                                        // caller's reflex addr + LAN host
                                        // candidates, and mark it as
                                        // cross-relay so the answer path knows
                                        // to route the CallSetup's
                                        // peer_direct_addr from what the
                                        // federated answer carries. peer_relay_fp
                                        // stays None here because we broadcast —
                                        // the receiving relay picks itself as
                                        // the answer source and its forwarded
                                        // answer will identify itself there.
                                        {
                                            let mut reg = call_registry.lock().await;
                                            reg.create_call(
                                                call_id.clone(),
                                                client_fp.clone(),
                                                target_fp.clone(),
                                            );
                                            reg.set_caller_reflexive_addr(
                                                &call_id,
                                                caller_addr_for_registry.clone(),
                                            );
                                            reg.set_caller_local_addrs(
                                                &call_id,
                                                caller_local_for_registry.clone(),
                                            );
                                        }

                                        // Send ringing to caller immediately
                                        // so the UI shows feedback while the
                                        // federated delivery is in flight.
                                        let _ = transport.send_signal(&SignalMessage::CallRinging {
                                            call_id: call_id.clone(),
                                        }).await;
                                        continue;
                                    }

                                    // Create call in registry + stash the caller's
                                    // reflex addr (Phase 3 hole-punching) AND its
                                    // LAN host candidates (Phase 5.5 ICE). The
                                    // relay treats both as opaque. Both are
                                    // injected later into the callee's CallSetup.
                                    {
                                        let mut reg = call_registry.lock().await;
                                        reg.create_call(call_id.clone(), client_fp.clone(), target_fp.clone());
                                        reg.set_caller_reflexive_addr(&call_id, caller_addr_for_registry);
                                        reg.set_caller_local_addrs(&call_id, caller_local_for_registry);
                                    }

                                    // Forward offer to callee
                                    info!(caller = %client_fp, callee = %target_fp, call_id = %call_id, "routing direct call offer");
                                    let hub = signal_hub.lock().await;
                                    if let Err(e) = hub.send_to(&target_fp, &msg).await {
                                        warn!("failed to forward call offer: {e}");
                                    }

                                    // Send ringing to caller
                                    drop(hub);
                                    let _ = transport.send_signal(&SignalMessage::CallRinging {
                                        call_id: call_id.clone(),
                                    }).await;
                                }

                                SignalMessage::DirectCallAnswer {
                                    ref call_id,
                                    ref accept_mode,
                                    ref callee_reflexive_addr,
                                    ref callee_local_addrs,
                                    ..
                                } => {
                                    let call_id = call_id.clone();
                                    let mode = *accept_mode;
                                    let callee_addr_for_registry = callee_reflexive_addr.clone();
                                    let callee_local_for_registry = callee_local_addrs.clone();

                                    // Phase 4: look up peer fingerprint AND
                                    // peer_relay_fp in one lock acquisition.
                                    // peer_relay_fp being Some means the
                                    // caller is on a remote federation peer
                                    // and we have to route the answer /
                                    // hangup back through that link instead
                                    // of local signal_hub.
                                    let (peer_fp, peer_relay_fp) = {
                                        let reg = call_registry.lock().await;
                                        match reg.get(&call_id) {
                                            Some(c) => (
                                                Some(reg.peer_fingerprint(&call_id, &client_fp).map(|s| s.to_string())),
                                                c.peer_relay_fp.clone(),
                                            ),
                                            None => (None, None),
                                        }
                                    };

                                    let Some(Some(peer_fp)) = peer_fp else {
                                        warn!(call_id = %call_id, "answer for unknown call");
                                        continue;
                                    };

                                    if mode == wzp_proto::CallAcceptMode::Reject {
                                        info!(call_id = %call_id, "call rejected");
                                        let mut reg = call_registry.lock().await;
                                        reg.end_call(&call_id);
                                        drop(reg);

                                        // Phase 4: cross-relay reject —
                                        // forward the hangup to the origin
                                        // relay instead of local signal_hub.
                                        if let Some(ref origin_fp) = peer_relay_fp {
                                            if let Some(ref fm) = federation_mgr {
                                                let hangup = SignalMessage::Hangup {
                                                    reason: wzp_proto::HangupReason::Normal,
                                                    call_id: Some(call_id.clone()),
                                                };
                                                let forward = SignalMessage::FederatedSignalForward {
                                                    inner: Box::new(hangup),
                                                    origin_relay_fp: tls_fp.clone(),
                                                };
                                                if let Err(e) = fm.send_signal_to_peer(origin_fp, &forward).await {
                                                    warn!(%call_id, %origin_fp, error = %e, "cross-relay reject forward failed");
                                                }
                                            }
                                        } else {
                                            let hub = signal_hub.lock().await;
                                            let _ = hub.send_to(&peer_fp, &SignalMessage::Hangup {
                                                reason: wzp_proto::HangupReason::Normal,
                                                call_id: Some(call_id.clone()),
                                            }).await;
                                        }
                                    } else {
                                        // Accept — create private room + stash the
                                        // callee's reflex addr if it advertised one
                                        // (AcceptTrusted only — privacy-mode answers
                                        // leave it None by design). Then read back
                                        // BOTH parties' addrs so we can cross-wire
                                        // peer_direct_addr on the CallSetups below.
                                        let room = format!("call-{call_id}");
                                        let (caller_addr, callee_addr, caller_local, callee_local) = {
                                            let mut reg = call_registry.lock().await;
                                            reg.set_active(&call_id, mode, room.clone());
                                            reg.set_callee_reflexive_addr(&call_id, callee_addr_for_registry);
                                            reg.set_callee_local_addrs(&call_id, callee_local_for_registry.clone());
                                            let call = reg.get(&call_id);
                                            (
                                                call.and_then(|c| c.caller_reflexive_addr.clone()),
                                                call.and_then(|c| c.callee_reflexive_addr.clone()),
                                                call.map(|c| c.caller_local_addrs.clone()).unwrap_or_default(),
                                                call.map(|c| c.callee_local_addrs.clone()).unwrap_or_default(),
                                            )
                                        };
                                        info!(
                                            call_id = %call_id,
                                            room = %room,
                                            ?mode,
                                            p2p_viable = caller_addr.is_some() && callee_addr.is_some(),
                                            "call accepted, creating room"
                                        );

                                        let relay_addr_for_setup = advertised_addr_str.clone();

                                        if let Some(ref origin_fp) = peer_relay_fp {
                                            // Phase 4 cross-relay: the caller
                                            // is on a remote peer. Forward the
                                            // raw answer (which carries the
                                            // callee's reflex addr) back over
                                            // federation — the peer's
                                            // cross-relay dispatcher will
                                            // deliver it to the local caller
                                            // AND emit a CallSetup on that
                                            // side with peer_direct_addr =
                                            // callee_addr.
                                            //
                                            // Here we emit only the LOCAL
                                            // CallSetup (to our callee) with
                                            // peer_direct_addr = caller_addr.
                                            if let Some(ref fm) = federation_mgr {
                                                let forward = SignalMessage::FederatedSignalForward {
                                                    inner: Box::new(msg.clone()),
                                                    origin_relay_fp: tls_fp.clone(),
                                                };
                                                if let Err(e) = fm.send_signal_to_peer(origin_fp, &forward).await {
                                                    warn!(
                                                        %call_id,
                                                        %origin_fp,
                                                        error = %e,
                                                        "cross-relay answer forward failed"
                                                    );
                                                }
                                            }

                                            let setup_for_callee = SignalMessage::CallSetup {
                                                call_id: call_id.clone(),
                                                room: room.clone(),
                                                relay_addr: relay_addr_for_setup,
                                                peer_direct_addr: caller_addr.clone(),
                                                peer_local_addrs: caller_local.clone(),
                                            };
                                            let hub = signal_hub.lock().await;
                                            let _ = hub.send_to(&client_fp, &setup_for_callee).await;
                                        } else {
                                            // Local call (existing Phase 3 path).
                                            // Forward answer to caller
                                            {
                                                let hub = signal_hub.lock().await;
                                                let _ = hub.send_to(&peer_fp, &msg).await;
                                            }

                                            // Send CallSetup to BOTH parties with
                                            // cross-wired peer_direct_addr +
                                            // peer_local_addrs (Phase 5.5 ICE).
                                            let setup_for_caller = SignalMessage::CallSetup {
                                                call_id: call_id.clone(),
                                                room: room.clone(),
                                                relay_addr: relay_addr_for_setup.clone(),
                                                peer_direct_addr: callee_addr.clone(),
                                                peer_local_addrs: callee_local.clone(),
                                            };
                                            let setup_for_callee = SignalMessage::CallSetup {
                                                call_id: call_id.clone(),
                                                room: room.clone(),
                                                relay_addr: relay_addr_for_setup,
                                                peer_direct_addr: caller_addr.clone(),
                                                peer_local_addrs: caller_local.clone(),
                                            };
                                            let hub = signal_hub.lock().await;
                                            let _ = hub.send_to(&peer_fp, &setup_for_caller).await;
                                            let _ = hub.send_to(&client_fp, &setup_for_callee).await;
                                        }
                                    }
                                }

                                SignalMessage::Hangup { ref call_id, .. } => {
                                    // If the client sent a call_id, only end
                                    // that specific call. Otherwise (old clients)
                                    // fall back to ending ALL active calls for
                                    // this user — which can race with new calls.
                                    let calls = {
                                        let reg = call_registry.lock().await;
                                        if let Some(cid) = call_id {
                                            // Targeted hangup: only the named call
                                            reg.get(cid)
                                                .map(|c| vec![(c.call_id.clone(), if c.caller_fingerprint == client_fp {
                                                    c.callee_fingerprint.clone()
                                                } else {
                                                    c.caller_fingerprint.clone()
                                                })])
                                                .unwrap_or_default()
                                        } else {
                                            // Legacy: end all calls for this user
                                            reg.calls_for_fingerprint(&client_fp)
                                                .iter()
                                                .map(|c| (c.call_id.clone(), if c.caller_fingerprint == client_fp {
                                                    c.callee_fingerprint.clone()
                                                } else {
                                                    c.caller_fingerprint.clone()
                                                }))
                                                .collect::<Vec<_>>()
                                        }
                                    };
                                    for (cid, peer_fp) in &calls {
                                        let hub = signal_hub.lock().await;
                                        let _ = hub.send_to(peer_fp, &msg).await;
                                        drop(hub);
                                        let mut reg = call_registry.lock().await;
                                        reg.end_call(cid);
                                    }
                                }

                                // Phase 6: forward MediaPathReport to the
                                // call peer so both sides can negotiate
                                // the media path before committing.
                                SignalMessage::MediaPathReport { ref call_id, .. } => {
                                    // Look up peer AND check if this is a
                                    // cross-relay call (same pattern as
                                    // DirectCallAnswer).
                                    let (peer_fp, peer_relay_fp) = {
                                        let reg = call_registry.lock().await;
                                        match reg.get(call_id) {
                                            Some(c) => (
                                                reg.peer_fingerprint(call_id, &client_fp)
                                                    .map(|s| s.to_string()),
                                                c.peer_relay_fp.clone(),
                                            ),
                                            None => (None, None),
                                        }
                                    };

                                    if let Some(fp) = peer_fp {
                                        if let Some(ref origin_fp) = peer_relay_fp {
                                            // Cross-relay: wrap and forward
                                            if let Some(ref fm) = federation_mgr {
                                                let forward = SignalMessage::FederatedSignalForward {
                                                    inner: Box::new(msg.clone()),
                                                    origin_relay_fp: tls_fp.clone(),
                                                };
                                                if let Err(e) = fm.send_signal_to_peer(origin_fp, &forward).await {
                                                    warn!(
                                                        %call_id,
                                                        %origin_fp,
                                                        error = %e,
                                                        "cross-relay MediaPathReport forward failed"
                                                    );
                                                }
                                            }
                                        } else {
                                            // Local call
                                            let hub = signal_hub.lock().await;
                                            let _ = hub.send_to(&fp, &msg).await;
                                        }
                                    }
                                }

                                SignalMessage::Ping { timestamp_ms } => {
                                    let _ = transport.send_signal(&SignalMessage::Pong { timestamp_ms }).await;
                                }

                                // QUIC-native NAT reflection ("STUN for QUIC").
                                // The client asks "what source address do you
                                // see for me?" and we reply with whatever
                                // quinn reports as this connection's remote
                                // address — i.e. the post-NAT public address
                                // as observed from the server side of the TLS
                                // session. Used by the P2P path to learn the
                                // client's server-reflexive address without
                                // running a separate STUN server. No auth or
                                // rate-limit in Phase 1 — the client is
                                // already TLS-authenticated by the time it
                                // reaches this match arm.
                                SignalMessage::Reflect => {
                                    let observed_addr = addr.to_string();
                                    if let Err(e) = transport.send_signal(
                                        &SignalMessage::ReflectResponse {
                                            observed_addr: observed_addr.clone(),
                                        },
                                    ).await {
                                        warn!(%addr, error = %e, "reflect: failed to send response");
                                    } else {
                                        debug!(%addr, %observed_addr, "reflect: responded");
                                    }
                                }

                                other => {
                                    warn!(%addr, "signal: unexpected message: {:?}", std::mem::discriminant(&other));
                                }
                            }
                        }
                        Ok(None) => {
                            info!(%addr, "signal connection closed");
                            break;
                        }
                        Err(wzp_proto::TransportError::Deserialize(e)) => {
                            // Forward-compat: the peer sent a
                            // SignalMessage variant we don't know
                            // (newer client, newer federation peer).
                            // Log and continue — tearing down the
                            // connection on unknown variants would
                            // silently kill interop across minor
                            // protocol version bumps.
                            warn!(%addr, "signal deserialize (unknown variant?), continuing: {e}");
                        }
                        Err(e) => {
                            warn!(%addr, "signal recv error: {e}");
                            break;
                        }
                    }
                }

                // Cleanup: unregister + end active calls
                let active_calls = {
                    let reg = call_registry.lock().await;
                    reg.calls_for_fingerprint(&client_fp)
                        .iter()
                        .map(|c| (c.call_id.clone(), if c.caller_fingerprint == client_fp {
                            c.callee_fingerprint.clone()
                        } else {
                            c.caller_fingerprint.clone()
                        }))
                        .collect::<Vec<_>>()
                };
                for (call_id, peer_fp) in &active_calls {
                    let hub = signal_hub.lock().await;
                    let _ = hub.send_to(peer_fp, &SignalMessage::Hangup {
                        reason: wzp_proto::HangupReason::Normal,
                        call_id: Some(call_id.clone()),
                    }).await;
                    drop(hub);
                    let mut reg = call_registry.lock().await;
                    reg.end_call(call_id);
                }

                {
                    let mut hub = signal_hub.lock().await;
                    hub.unregister(&client_fp);
                }
                {
                    let mut reg = presence.lock().await;
                    reg.unregister_local(&client_fp);
                }

                transport.close().await.ok();
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

            // ACL: call rooms (call-*) are restricted to the two authorized participants.
            // Only the relay's call orchestrator creates these rooms — random clients can't join.
            if room_name.starts_with("call-") {
                let call_id = &room_name[5..]; // strip "call-" prefix
                let authorized = {
                    let reg = call_registry.lock().await;
                    match reg.get(call_id) {
                        Some(call) => {
                            call.caller_fingerprint == participant_fp
                                || call.callee_fingerprint == participant_fp
                        }
                        None => false, // unknown call — reject
                    }
                };
                if !authorized {
                    warn!(%addr, room = %room_name, fp = %participant_fp, "rejected: not authorized for this call room");
                    transport.close().await.ok();
                    return;
                }
                info!(%addr, room = %room_name, fp = %participant_fp, "authorized for call room");
            }

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

                // Call rooms: enforce 2-participant limit
                if room_name.starts_with("call-") {
                    let mgr = room_mgr.lock().await;
                    if mgr.room_size(&room_name) >= 2 {
                        drop(mgr);
                        warn!(%addr, room = %room_name, "call room full (max 2 participants)");
                        metrics.active_sessions.dec();
                        let mut smgr = session_mgr.lock().await;
                        smgr.remove_session(session_id);
                        transport.close().await.ok();
                        return;
                    }
                }

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

                            // Merge federated participants into RoomUpdate if this is a global room
                            let merged_update = if let Some(ref fm) = federation_mgr {
                                if fm.is_global_room(&room_name) {
                                    if let SignalMessage::RoomUpdate { count: _, participants: mut local_parts } = update {
                                        let remote = fm.get_remote_participants(&room_name).await;
                                        local_parts.extend(remote);
                                        // Deduplicate by fingerprint
                                        let mut seen = std::collections::HashSet::new();
                                        local_parts.retain(|p| seen.insert(p.fingerprint.clone()));
                                        SignalMessage::RoomUpdate {
                                            count: local_parts.len() as u32,
                                            participants: local_parts,
                                        }
                                    } else { update }
                                } else { update }
                            } else { update };

                            room::broadcast_signal(&senders, &merged_update).await;
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
                let (federation_tx, federation_room_hash) = if let Some(ref fm) = federation_mgr {
                    let is_global = fm.is_global_room(&room_name);
                    if is_global {
                        let canonical_hash = fm.global_room_hash(&room_name);
                        let (tx, rx) = tokio::sync::mpsc::channel(256);
                        let fm_clone = fm.clone();
                        tokio::spawn(async move {
                            wzp_relay::federation::run_federation_media_egress(fm_clone, rx).await;
                        });
                        info!(room = %room_name, canonical = ?fm.resolve_global_room(&room_name), "federation egress created (global room)");
                        (Some(tx), Some(canonical_hash))
                    } else {
                        (None, None)
                    }
                } else {
                    (None, None)
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
                    federation_room_hash,
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
    Ok(())
}
